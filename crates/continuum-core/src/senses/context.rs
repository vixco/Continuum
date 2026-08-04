//! # Context watcher
//!
//! Pure Rust code that polls Windows APIs once per second. Captures:
//! - Foreground window title and process name
//! - Idle time since last user input
//! - Whether the user is in a call (Discord, Teams, Zoom, Meet)
//!
//! This layer uses no AI. It is structured polling — cheap, fast, deterministic.
//! Produces [`ContextObservation`] structs that feed into the perception frame
//! builder.
//!
//! # Platform support
//!
//! All Windows API calls are gated behind `#[cfg(windows)]`. On non-Windows
//! platforms, stub implementations return empty observations so the crate
//! remains compilable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use tracing::{debug, error, trace};

use crate::config::ContextConfig;
use crate::senses::live_context::{
    InputActivityWorldState, LiveContextHub, PrivacyDisposition, ProjectWorldState,
    WindowWorldState,
};
use crate::senses::types::ContextObservation;

/// Processes whose presence in the foreground strongly indicate the user is
/// in a voice/video call.
const CALL_PROCESSES: &[&str] = &[
    "discord.exe",
    "teams.exe",
    "ms-teams.exe",
    "zoom.exe",
    "slack.exe",
];

/// Substrings in the foreground window title that indicate a browser-based
/// call (Google Meet, Zoom web, etc.) when the foreground process is a browser.
const CALL_TITLE_KEYWORDS: &[&str] = &["meet", "zoom"];

/// Browser process names to check for title-based call detection.
const BROWSER_PROCESSES: &[&str] = &["chrome.exe", "msedge.exe", "firefox.exe", "brave.exe"];

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    //! Windows-specific FFI wrappers for context polling.
    //!
    //! Every function in this module calls Windows APIs through the `windows`
    //! crate and returns safe Rust types. All `unsafe` blocks carry a
    //! `// SAFETY:` comment explaining why the call is sound.

    use tracing::{trace, warn};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };

    /// Returns `(window_title, process_name)` for the current foreground window.
    ///
    /// If no window is focused or any API call fails, returns empty strings
    /// instead of propagating errors — the poller should never crash over a
    /// transient desktop state.
    pub fn get_foreground_window_info() -> (String, String) {
        // SAFETY: GetForegroundWindow has no preconditions and returns a null
        // HWND when no window is focused.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            trace!(
                layer = "senses",
                component = "context",
                "No foreground window (HWND is null)"
            );
            return (String::new(), String::new());
        }

        let title = get_window_title(hwnd);
        let process_name = get_process_name_for_window(hwnd);

        (title, process_name)
    }

    /// Reads the title text of the given window handle.
    fn get_window_title(hwnd: windows::Win32::Foundation::HWND) -> String {
        let mut buf = [0u16; 512];
        // SAFETY: GetWindowTextW writes at most `buf.len()` wide chars into
        // `buf` and returns the number of chars written (excluding the null
        // terminator). The buffer is stack-allocated and valid for the call.
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if len == 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }

    /// Resolves the process name (e.g. "Code.exe") for the process that owns
    /// the given window.
    fn get_process_name_for_window(hwnd: windows::Win32::Foundation::HWND) -> String {
        let mut pid: u32 = 0;
        // SAFETY: GetWindowThreadProcessId writes the owning process ID into
        // `pid`. A null return (thread id 0) is non-fatal — the pid may
        // still have been written.
        let _thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0 {
            warn!(
                layer = "senses",
                component = "context",
                "GetWindowThreadProcessId returned PID 0"
            );
            return String::new();
        }

        // SAFETY: OpenProcess with PROCESS_QUERY_LIMITED_INFORMATION is a
        // low-privilege request. It returns an Err result if access is denied,
        // which we handle below.
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(h) => h,
            Err(e) => {
                trace!(
                    layer = "senses",
                    component = "context",
                    pid = pid,
                    error = %e,
                    "OpenProcess failed for PID"
                );
                return String::new();
            }
        };

        let mut name_buf = [0u16; 260]; // MAX_PATH
        let mut name_len = name_buf.len() as u32;
        // SAFETY: QueryFullProcessImageNameW writes the full executable path
        // into `name_buf` and updates `name_len` with the number of chars
        // written (excluding the null terminator). Unlike GetModuleBaseNameW,
        // this function works with PROCESS_QUERY_LIMITED_INFORMATION access
        // rights, which is the low-privilege handle we opened above.
        // We pass PWSTR wrapping the buffer pointer directly.
        let result = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_FORMAT(0),
                windows::core::PWSTR(name_buf.as_mut_ptr()),
                &mut name_len,
            )
        };

        // SAFETY: CloseHandle is safe to call on any valid handle. We obtained
        // this handle from OpenProcess above and have not closed it yet.
        let _ = unsafe { CloseHandle(handle) };

        if result.is_err() || name_len == 0 {
            trace!(
                layer = "senses",
                component = "context",
                pid = pid,
                "QueryFullProcessImageNameW failed or returned 0 chars"
            );
            return String::new();
        }

        let full_path = String::from_utf16_lossy(&name_buf[..name_len as usize]);
        // Extract just the filename (e.g. "Code.exe") from the full path.
        full_path
            .rsplit('\\')
            .next()
            .unwrap_or(&full_path)
            .to_string()
    }

    /// Returns the number of seconds since the user last provided keyboard or
    /// mouse input.
    pub fn get_idle_seconds() -> u64 {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };

        // SAFETY: GetLastInputInfo fills the struct if cbSize is set correctly.
        // We set cbSize above. Returns TRUE on success.
        let ok = unsafe { GetLastInputInfo(&mut info) };
        if !ok.as_bool() {
            warn!(
                layer = "senses",
                component = "context",
                "GetLastInputInfo failed"
            );
            return 0;
        }

        // SAFETY: GetTickCount has no preconditions and returns the number of
        // milliseconds since system start. It wraps every ~49.7 days.
        let now = unsafe { GetTickCount() };

        // Handle tick count wrap-around. Because both values are u32, wrapping
        // subtraction gives the correct elapsed time even across a wrap.
        let elapsed_ms = now.wrapping_sub(info.dwTime);
        u64::from(elapsed_ms) / 1000
    }
}

// ---------------------------------------------------------------------------
// Non-Windows stubs
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod win {
    //! Stub implementations for non-Windows platforms.
    //!
    //! These allow the crate to compile on Linux/macOS for CI and testing,
    //! even though Continuum is a Windows-only application.

    /// Returns empty strings on non-Windows platforms.
    pub fn get_foreground_window_info() -> (String, String) {
        (String::new(), String::new())
    }

    /// Returns 0 on non-Windows platforms.
    pub fn get_idle_seconds() -> u64 {
        0
    }
}

// ---------------------------------------------------------------------------
// Public helpers for external crates
// ---------------------------------------------------------------------------

/// Returns `(window_title, process_name)` for the current foreground window.
///
/// On non-Windows platforms both strings are empty. Errors at the Win32 layer
/// are swallowed and return empty strings — consumers should treat this as a
/// best-effort lookup. Added in Phase 4 to back the `system_active_window`
/// MCP tool.
pub fn foreground_window() -> (String, String) {
    win::get_foreground_window_info()
}

// ---------------------------------------------------------------------------
// Call detection
// ---------------------------------------------------------------------------

/// Determines whether the user appears to be in a voice/video call based on the
/// foreground window's process name and title.
///
/// This is a heuristic check — it cannot detect calls that are running in the
/// background or in a minimized window. For Phase 1, foreground-only detection
/// is sufficient.
///
/// # Detection rules
///
/// 1. If the foreground process is a known call application (Discord, Teams,
///    Zoom, Slack), report `true`.
/// 2. If the foreground process is a browser and the window title contains
///    "Meet" or "Zoom" (case-insensitive), report `true`.
/// 3. Otherwise, report `false`.
fn is_in_call(process_name: &str, window_title: &str) -> bool {
    let process_lower = process_name.to_lowercase();
    let title_lower = window_title.to_lowercase();

    // Direct match against known call processes.
    if CALL_PROCESSES.iter().any(|&p| process_lower == p) {
        return true;
    }

    // Browser-based call detection via title keywords.
    if BROWSER_PROCESSES.iter().any(|&b| process_lower == b)
        && CALL_TITLE_KEYWORDS
            .iter()
            .any(|&kw| title_lower.contains(kw))
    {
        return true;
    }

    false
}

// ---------------------------------------------------------------------------
// ContextWatcher
// ---------------------------------------------------------------------------

/// Watches the Windows desktop context by polling system APIs at a
/// configurable interval.
///
/// The watcher runs as a long-lived async task and sends
/// [`ContextObservation`] values to the frame builder through a channel.
///
/// # Layer
///
/// Layer 1 — Senses. This component is pure polling with no AI involvement.
///
/// # Self-healing
///
/// The watcher logs every poll cycle via `tracing` with structured fields
/// (`layer = "senses"`, `component = "context"`). If polling fails, the error
/// is logged and the watcher continues on the next tick. The repair agent can
/// detect prolonged failures by reading the log and restart the component.
pub struct ContextWatcher {
    /// Configuration for the polling interval.
    config: ContextConfig,
}

impl ContextWatcher {
    /// Creates a new context watcher with the given configuration.
    pub fn new(config: ContextConfig) -> Self {
        debug!(
            layer = "senses",
            component = "context",
            poll_interval_secs = config.poll_interval_secs,
            "ContextWatcher created"
        );
        Self { config }
    }

    /// Runs the context poller loop, sending observations to `tx` until the
    /// shutdown signal fires.
    ///
    /// The loop polls immediately, then every
    /// [`ContextConfig::poll_interval_secs`]. Each observation updates shared
    /// live context before a non-blocking send to the legacy frame channel, so
    /// a busy downstream consumer cannot pause context continuity.
    ///
    /// # Shutdown
    ///
    /// The watcher monitors `shutdown` and exits cleanly when the value
    /// changes to `true`. It logs its exit so the repair agent can distinguish
    /// a graceful shutdown from a crash.
    ///
    /// # Errors
    ///
    /// Returns an error only if the observation channel is closed (receiver
    /// dropped), which typically means the frame builder has been shut down.
    pub async fn run(
        &self,
        tx: tokio::sync::mpsc::Sender<ContextObservation>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        self.run_inner(tx, shutdown, None).await
    }

    /// Run while also projecting safe window, activity, terminal, and project
    /// events into the shared agent-facing world-state.
    pub async fn run_with_live_context(
        &self,
        tx: tokio::sync::mpsc::Sender<ContextObservation>,
        shutdown: tokio::sync::watch::Receiver<bool>,
        live_context: LiveContextHub,
    ) -> Result<()> {
        self.run_inner(tx, shutdown, Some(live_context)).await
    }

    async fn run_inner(
        &self,
        tx: tokio::sync::mpsc::Sender<ContextObservation>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        live_context: Option<LiveContextHub>,
    ) -> Result<()> {
        let interval = Duration::from_secs(self.config.poll_interval_secs.max(1));
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!(
            layer = "senses",
            component = "context",
            interval_secs = self.config.poll_interval_secs,
            "Context poller loop starting"
        );

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let (obs, privacy) = self.poll_once_with_privacy();

                    if let Some(hub) = &live_context {
                        hub.record_context(
                            WindowWorldState {
                                process_name: obs.foreground_process_name.clone(),
                                title: obs.foreground_window_title.clone(),
                                observed_at: obs.ts,
                                in_call: obs.in_call,
                                privacy,
                            },
                            InputActivityWorldState {
                                observed_at: obs.ts,
                                idle_seconds: obs.idle_seconds,
                                active: obs.idle_seconds < 2,
                            },
                        );
                        hub.record_project(project_world_state(
                            &obs.foreground_process_name,
                            &self.config.terminal_process_names,
                            obs.ts,
                        ));
                    }

                    trace!(
                        layer = "senses",
                        component = "context",
                        window_title = %obs.foreground_window_title,
                        process_name = %obs.foreground_process_name,
                        idle_seconds = obs.idle_seconds,
                        in_call = obs.in_call,
                        "Polled context"
                    );

                    match tx.try_send(obs) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            if let Some(hub) = &live_context {
                                hub.record_output_drop(1);
                            }
                            tracing::warn!(
                                layer = "senses",
                                component = "context",
                                "Legacy frame channel full; shared live context remains current"
                            );
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            error!(
                                layer = "senses",
                                component = "context",
                                "Observation channel closed, stopping context poller"
                            );
                            return Err(anyhow::anyhow!("context observation channel closed"));
                        }
                    }
                }
                result = shutdown.changed() => {
                    match result {
                        Ok(()) if *shutdown.borrow() => {
                            debug!(
                                layer = "senses",
                                component = "context",
                                "Shutdown signal received, stopping context poller"
                            );
                            return Ok(());
                        }
                        Ok(()) => {
                            // Value changed but is not true; keep running.
                            continue;
                        }
                        Err(_) => {
                            // Sender dropped — treat as shutdown.
                            debug!(
                                layer = "senses",
                                component = "context",
                                "Shutdown watch sender dropped, stopping context poller"
                            );
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// Polls the current desktop state once and returns a [`ContextObservation`].
    ///
    /// This is a synchronous function that calls into platform-specific FFI.
    /// It never panics — all errors are logged and produce empty/default values.
    pub fn poll_once(&self) -> ContextObservation {
        self.poll_once_with_privacy().0
    }

    fn poll_once_with_privacy(&self) -> (ContextObservation, PrivacyDisposition) {
        let (title, process_name) = win::get_foreground_window_info();
        let idle_seconds = win::get_idle_seconds();
        let in_call = is_in_call(&process_name, &title);
        let privacy = classify_privacy(&self.config, &process_name, &title);
        let safe_title =
            if privacy == PrivacyDisposition::Redacted && self.config.redact_sensitive_titles {
                "[redacted sensitive window]".to_string()
            } else {
                title
            };

        (
            ContextObservation {
                foreground_window_title: safe_title,
                foreground_process_name: process_name,
                idle_seconds,
                in_call,
                ts: Utc::now(),
            },
            privacy,
        )
    }

    /// Returns `true` if the watcher appears to be in a healthy state.
    ///
    /// For the context poller this is always `true` since it relies only on
    /// Windows APIs that are always available. A future version could track
    /// consecutive poll failures and return `false` after a threshold.
    pub fn is_healthy(&self) -> bool {
        true
    }

    /// Returns `true` if the watcher should be restarted by the repair agent.
    ///
    /// Currently always returns `false`. A future version could set an
    /// internal flag after repeated API failures.
    pub fn should_restart(&self) -> bool {
        false
    }
}

fn classify_privacy(
    config: &ContextConfig,
    process_name: &str,
    window_title: &str,
) -> PrivacyDisposition {
    let process = process_name.to_ascii_lowercase();
    let title = window_title.to_ascii_lowercase();
    let sensitive_process = config
        .sensitive_process_names
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(&process));
    let sensitive_title = config
        .sensitive_title_keywords
        .iter()
        .any(|keyword| title.contains(&keyword.to_ascii_lowercase()));
    if sensitive_process || sensitive_title {
        PrivacyDisposition::Redacted
    } else {
        PrivacyDisposition::Visible
    }
}

fn project_world_state(
    foreground_process: &str,
    terminal_processes: &[String],
    observed_at: chrono::DateTime<Utc>,
) -> ProjectWorldState {
    let terminal_active = terminal_processes
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(foreground_process));
    let project_root = std::env::current_dir().ok().and_then(find_git_root);
    let git_head = project_root.as_deref().and_then(read_git_head);
    let project_name = project_root
        .as_deref()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned());
    ProjectWorldState {
        observed_at,
        terminal_active,
        terminal_process: terminal_active.then(|| foreground_process.to_string()),
        project_root: project_root.map(|root| root.to_string_lossy().into_owned()),
        project_name,
        git_head,
    }
}

fn find_git_root(start: PathBuf) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(Path::to_path_buf)
}

fn read_git_head(root: &Path) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let marker = std::fs::read_to_string(dot_git).ok()?;
        let path = marker.trim().strip_prefix("gitdir:")?.trim();
        let candidate = PathBuf::from(path);
        if candidate.is_absolute() {
            candidate
        } else {
            root.join(candidate)
        }
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    Some(
        head.trim()
            .strip_prefix("ref: refs/heads/")
            .unwrap_or(head.trim())
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_watcher_creation() {
        let config = ContextConfig {
            poll_interval_secs: 2,
            ..ContextConfig::default()
        };
        let watcher = ContextWatcher::new(config);
        assert_eq!(watcher.config.poll_interval_secs, 2);
    }

    #[test]
    fn test_context_watcher_default_config() {
        let config = ContextConfig::default();
        let watcher = ContextWatcher::new(config);
        assert_eq!(watcher.config.poll_interval_secs, 1);
    }

    #[test]
    fn sensitive_titles_are_classified_before_publication() {
        let config = ContextConfig::default();
        assert_eq!(
            classify_privacy(&config, "chrome.exe", "Enter password"),
            PrivacyDisposition::Redacted
        );
        assert_eq!(
            classify_privacy(&config, "Code.exe", "vision.rs - Continuum"),
            PrivacyDisposition::Visible
        );
    }

    #[test]
    fn terminal_projection_never_contains_terminal_text() {
        let config = ContextConfig::default();
        let state = project_world_state("pwsh.exe", &config.terminal_process_names, Utc::now());
        assert!(state.terminal_active);
        assert_eq!(state.terminal_process.as_deref(), Some("pwsh.exe"));
    }

    #[test]
    fn test_poll_once_returns_valid_observation() {
        let watcher = ContextWatcher::new(ContextConfig::default());
        let obs = watcher.poll_once();

        // On any platform, poll_once should return a valid observation.
        // The timestamp should be recent (within the last second).
        let now = Utc::now();
        let diff = now.signed_duration_since(obs.ts);
        assert!(diff.num_seconds() < 2, "Timestamp should be recent");
    }

    #[test]
    fn test_is_in_call_discord() {
        assert!(is_in_call("Discord.exe", "General - Discord"));
        assert!(is_in_call("discord.exe", "Voice Channel"));
    }

    #[test]
    fn test_is_in_call_teams() {
        assert!(is_in_call("Teams.exe", "Meeting | Microsoft Teams"));
        assert!(is_in_call("ms-teams.exe", "Chat"));
    }

    #[test]
    fn test_is_in_call_zoom() {
        assert!(is_in_call("Zoom.exe", "Zoom Meeting"));
    }

    #[test]
    fn test_is_in_call_browser_meet() {
        assert!(is_in_call("chrome.exe", "Meeting - Google Meet"));
        assert!(is_in_call("msedge.exe", "Google Meet - abc-defg-hij"));
    }

    #[test]
    fn test_is_in_call_browser_zoom_web() {
        assert!(is_in_call("chrome.exe", "Zoom - Web Client"));
    }

    #[test]
    fn test_is_not_in_call_regular_browser() {
        assert!(!is_in_call("chrome.exe", "GitHub - Google Chrome"));
        assert!(!is_in_call("msedge.exe", "Bing - Microsoft Edge"));
    }

    #[test]
    fn test_is_not_in_call_editor() {
        assert!(!is_in_call("Code.exe", "main.rs - continuum-ai"));
        assert!(!is_in_call("notepad.exe", "Untitled - Notepad"));
    }

    #[test]
    fn test_is_not_in_call_empty() {
        assert!(!is_in_call("", ""));
    }

    #[test]
    fn test_is_in_call_slack() {
        assert!(is_in_call("Slack.exe", "Huddle - #general"));
    }

    #[test]
    fn test_is_in_call_case_insensitive() {
        // Process name matching should be case-insensitive.
        assert!(is_in_call("DISCORD.EXE", "Voice"));
        assert!(is_in_call("CHROME.EXE", "Google Meet"));
    }

    #[test]
    fn test_health_check() {
        let watcher = ContextWatcher::new(ContextConfig::default());
        assert!(watcher.is_healthy());
        assert!(!watcher.should_restart());
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_idle_seconds() {
        // On a Windows machine, idle time should be a small number (the test
        // itself is providing input by running).
        let idle = win::get_idle_seconds();
        // Just verify it doesn't panic and returns something reasonable.
        // Just verify it returns a plausible value. In automated environments
        // the system may have been idle for a long time.
        assert!(idle < 86_400, "Idle time should be under 24 hours");
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_foreground_window() {
        // On a Windows machine, there should typically be some foreground window.
        // We just verify the function doesn't panic.
        let (title, process) = win::get_foreground_window_info();
        // Both may be empty if we're running headless, but they should be valid strings.
        assert!(title.len() < 1024, "Title should be reasonable length");
        assert!(
            process.len() < 512,
            "Process name should be reasonable length"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_foreground_process_name_is_nonempty() {
        // Regression test: QueryFullProcessImageNameW must return a non-empty
        // process name for the foreground window. The previous implementation
        // used GetModuleBaseNameW with PROCESS_QUERY_LIMITED_INFORMATION, which
        // silently returned 0 on every call because that access right is
        // insufficient for GetModuleBaseNameW.
        let (_title, process) = win::get_foreground_window_info();
        // In a desktop session there is always a foreground window.
        // This test will be skipped in headless CI (no foreground window → empty
        // is expected), but on a real desktop it must be non-empty.
        if !_title.is_empty() {
            assert!(
                !process.is_empty(),
                "foreground_process_name should not be empty when a window is focused"
            );
            assert!(
                process.ends_with(".exe") || process.ends_with(".EXE"),
                "process name should end with .exe, got: {process}"
            );
        }
    }

    #[tokio::test]
    async fn test_context_watcher_run_with_shutdown() {
        let watcher = ContextWatcher::new(ContextConfig {
            poll_interval_secs: 1,
            ..ContextConfig::default()
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Spawn the watcher.
        let handle = tokio::spawn(async move { watcher.run(tx, shutdown_rx).await });

        // Wait for at least one observation.
        let obs = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("Should receive an observation within 3 seconds")
            .expect("Channel should not be closed");

        // Verify the observation has a valid timestamp.
        let now = Utc::now();
        let diff = now.signed_duration_since(obs.ts);
        assert!(diff.num_seconds() < 5);

        // Signal shutdown.
        shutdown_tx
            .send(true)
            .expect("Shutdown send should succeed");

        // The watcher should exit cleanly.
        let result = tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("Watcher should shut down within 3 seconds")
            .expect("Watcher task should not panic");

        assert!(result.is_ok(), "Watcher should exit without error");
    }

    #[tokio::test]
    async fn test_context_watcher_stops_on_channel_close() {
        let watcher = ContextWatcher::new(ContextConfig {
            poll_interval_secs: 1,
            ..ContextConfig::default()
        });
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Drop the receiver immediately.
        drop(rx);

        // The watcher should exit with an error since the channel is closed.
        let handle = tokio::spawn(async move { watcher.run(tx, shutdown_rx).await });

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("Watcher should stop within 5 seconds")
            .expect("Watcher task should not panic");

        assert!(
            result.is_err(),
            "Watcher should return error on closed channel"
        );
    }
}
