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

use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use tracing::{debug, error, trace};

use crate::config::ContextConfig;
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
    use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
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
        // SAFETY: GetModuleBaseNameW reads the base name of the first module
        // (the exe) into `name_buf`. We pass the handle obtained from
        // OpenProcess and a None module handle to get the exe name. The buffer
        // is valid and large enough for any path component.
        let name_len = unsafe { GetModuleBaseNameW(handle, None, &mut name_buf) };

        // SAFETY: CloseHandle is safe to call on any valid handle. We obtained
        // this handle from OpenProcess above and have not closed it yet.
        let _ = unsafe { CloseHandle(handle) };

        if name_len == 0 {
            trace!(
                layer = "senses",
                component = "context",
                pid = pid,
                "GetModuleBaseNameW returned 0 chars"
            );
            return String::new();
        }

        String::from_utf16_lossy(&name_buf[..name_len as usize])
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
    //! even though Kairo is a Windows-only application.

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
    /// The loop sleeps for [`ContextConfig::poll_interval_secs`] between polls.
    /// Each iteration calls [`poll_once`](Self::poll_once) to capture the
    /// current desktop state and pushes the result through the channel.
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
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let interval = Duration::from_secs(self.config.poll_interval_secs);

        debug!(
            layer = "senses",
            component = "context",
            interval_secs = self.config.poll_interval_secs,
            "Context poller loop starting"
        );

        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    let obs = self.poll_once();

                    trace!(
                        layer = "senses",
                        component = "context",
                        window_title = %obs.foreground_window_title,
                        process_name = %obs.foreground_process_name,
                        idle_seconds = obs.idle_seconds,
                        in_call = obs.in_call,
                        "Polled context"
                    );

                    if let Err(e) = tx.send(obs).await {
                        error!(
                            layer = "senses",
                            component = "context",
                            error = %e,
                            "Observation channel closed, stopping context poller"
                        );
                        return Err(e.into());
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
        let (title, process_name) = win::get_foreground_window_info();
        let idle_seconds = win::get_idle_seconds();
        let in_call = is_in_call(&process_name, &title);

        ContextObservation {
            foreground_window_title: title,
            foreground_process_name: process_name,
            idle_seconds,
            in_call,
            ts: Utc::now(),
        }
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
        assert!(!is_in_call("Code.exe", "main.rs - kairo-ai"));
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
        assert!(process.len() < 512, "Process name should be reasonable length");
    }

    #[tokio::test]
    async fn test_context_watcher_run_with_shutdown() {
        let watcher = ContextWatcher::new(ContextConfig {
            poll_interval_secs: 1,
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
        shutdown_tx.send(true).expect("Shutdown send should succeed");

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

        assert!(result.is_err(), "Watcher should return error on closed channel");
    }
}
