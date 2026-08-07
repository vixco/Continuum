//! # Context watcher
//!
//! Pure Rust code that polls Windows APIs once per second. Captures:
//! - Foreground window title and process name
//! - Process enrichment: pid, executable path (scrubbed), hosting monitor,
//!   and focus dwell (`active_since_secs`) — context engine spec §4.2
//! - Idle time since last user input
//! - Whether the user is in a call (Discord, Teams, Zoom, Meet)
//!
//! This layer uses no AI. It is structured polling — cheap, fast, deterministic.
//! Produces [`ContextObservation`] structs that feed into the perception frame
//! builder, plus `focus_switch` [`ContextEvent`]s (from consecutive-poll
//! diffs via the sentinel-aware dwell tracker) into the events channel
//! (Task A6): the runtime clones its [`EventSender`] in via
//! [`ContextWatcher::with_event_sender`]; standalone construction gets a
//! log-only sink.
//!
//! # Platform support
//!
//! All Windows API calls are gated behind `#[cfg(windows)]`. On non-Windows
//! platforms, stub implementations return empty observations so the crate
//! remains compilable.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::Serialize;
use tracing::{debug, error, trace};

use crate::config::{ContextConfig, ObservationToggles, PrivacyConfig};
use crate::context::project::{CurrentProject, CurrentProjectHandle};
use crate::memory::events::{
    fill_project_id_from_handle, ContextEvent, EventSender, EventSensitivity, EventSource,
    EventType, COLLECTOR_EVENT_IMPORTANCE,
};
use crate::senses::live_context::{
    InputActivityWorldState, LiveContextHub, MonitorGeometry, PrivacyDisposition,
    ProjectWorldState, WindowWorldState,
};
use crate::senses::privacy::{
    emit_system_event, source_enabled, strictest, ObservedSource, PrivacyFilter, Zone,
    EXCLUDED_PROCESS, EXCLUDED_TITLE,
};
use crate::senses::toggles::ToggleControl;
use crate::senses::types::ContextObservation;

/// Replacement title used for `local_only` windows when
/// `[context].redact_sensitive_titles` is enabled — preserves the legacy
/// "observed but redacted" behavior (spec §4.1 legacy migration).
///
/// `pub(crate)` because downstream zone re-derivation needs it: once the
/// title has been replaced by this literal, the title keyword that made
/// the window `local_only` is gone, so consumers that only see the
/// sanitized observation (Task B3's classification consumption) recognize
/// the literal itself as the local_only marker.
pub(crate) const REDACTED_TITLE: &str = "[redacted sensitive window]";

/// Raw foreground-window facts straight from the Win32 layer, before the
/// privacy choke point. Never leaves this module unsanitized —
/// [`sanitize_observation`] is the only consumer.
#[derive(Debug, Clone, Default)]
pub struct RawForegroundInfo {
    /// Raw window title (may contain secrets — free text).
    pub title: String,
    /// Process image basename (e.g. `"Code.exe"`).
    pub process_name: String,
    /// Owning process id, when the lookup succeeded.
    pub pid: Option<u32>,
    /// Full executable path (raw — scrubbed at emit).
    pub exe_path: Option<String>,
    /// `(left, top, right, bottom)` of the monitor hosting the window,
    /// via `MonitorFromWindow` (DPI-safe, spec §4.2).
    pub monitor_rect: Option<(i32, i32, i32, i32)>,
}

/// One visible, non-minimized top-level window from the per-monitor sweep
/// (spec §4.1). Only what zone matching needs: title, process, rect. No
/// child windows, no UIA — deliberately cheap.
#[derive(Debug, Clone)]
pub struct VisibleWindow {
    /// Window title (raw — used only for zone matching, never emitted).
    pub title: String,
    /// Owning process image basename (raw — zone matching only).
    pub process_name: String,
    /// `(left, top, right, bottom)` in virtual-desktop coordinates.
    pub rect: (i32, i32, i32, i32),
}

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
    use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetForegroundWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
        IsIconic, IsWindowVisible,
    };

    use super::{RawForegroundInfo, VisibleWindow};

    /// Enumerates visible, non-minimized top-level windows with a
    /// non-empty title: title + owning process + window rect. This backs
    /// the per-monitor privacy sweep (spec §4.1); it runs at the 1 Hz
    /// context-poll cadence and deliberately stays cheap — no UIA, no
    /// child windows, no z-order analysis.
    ///
    /// Returns `None` when the enumeration itself fails — a partial window
    /// list could miss a sensitive window, so the caller must keep the
    /// previous zones rather than relax them from incomplete data.
    pub fn enumerate_visible_windows() -> Option<Vec<VisibleWindow>> {
        unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
            // SAFETY: `lparam` is the pointer to the Vec passed to
            // EnumWindows below; it outlives this synchronous enumeration.
            let out = unsafe { &mut *(lparam.0 as *mut Vec<VisibleWindow>) };
            // SAFETY: `hwnd` is a valid handle supplied by EnumWindows.
            let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
            // SAFETY: same handle validity as above.
            let minimized = unsafe { IsIconic(hwnd) }.as_bool();
            if !visible || minimized {
                return true.into();
            }
            let title = get_window_title(hwnd);
            if title.is_empty() {
                return true.into();
            }
            let mut rect = RECT::default();
            // SAFETY: GetWindowRect writes into the stack-allocated RECT.
            if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
                return true.into();
            }
            let process_name = get_process_name_for_window(hwnd);
            out.push(VisibleWindow {
                title,
                process_name,
                rect: (rect.left, rect.top, rect.right, rect.bottom),
            });
            true.into()
        }

        let mut result: Vec<VisibleWindow> = Vec::new();
        // SAFETY: the callback only dereferences the Vec pointer for the
        // duration of this synchronous call; EnumWindows does not retain it.
        let enumerated =
            unsafe { EnumWindows(Some(callback), LPARAM(&mut result as *mut _ as isize)) };
        if let Err(error) = enumerated {
            trace!(
                layer = "senses",
                component = "context",
                error = %error,
                "EnumWindows sweep failed; monitor zones keep their previous value"
            );
            return None;
        }
        Some(result)
    }

    /// Returns `(window_title, process_name)` for the current foreground window.
    ///
    /// If no window is focused or any API call fails, returns empty strings
    /// instead of propagating errors — the poller should never crash over a
    /// transient desktop state.
    pub fn get_foreground_window_info() -> (String, String) {
        let info = get_foreground_info();
        (info.title, info.process_name)
    }

    /// Returns the full raw foreground facts (title, process, pid, exe
    /// path, hosting-monitor rect) for the current foreground window
    /// (spec §4.2 enrichment). All failures degrade to empty/`None`
    /// fields — never an error.
    pub fn get_foreground_info() -> RawForegroundInfo {
        // SAFETY: GetForegroundWindow has no preconditions and returns a null
        // HWND when no window is focused.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            trace!(
                layer = "senses",
                component = "context",
                "No foreground window (HWND is null)"
            );
            return RawForegroundInfo::default();
        }

        let title = get_window_title(hwnd);
        let (pid, exe_path) = get_process_info_for_window(hwnd);
        let process_name = exe_path.as_deref().map(image_basename).unwrap_or_default();

        RawForegroundInfo {
            title,
            process_name,
            pid,
            exe_path,
            monitor_rect: monitor_rect_for_window(hwnd),
        }
    }

    /// Rect of the monitor hosting the window, via `MonitorFromWindow`
    /// (`MONITOR_DEFAULTTONEAREST` — DPI-safe, spec §4.2). `None` when the
    /// monitor lookup fails.
    fn monitor_rect_for_window(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
        // SAFETY: MonitorFromWindow has no preconditions; with
        // MONITOR_DEFAULTTONEAREST it returns the nearest monitor even for
        // off-screen windows, or a null handle only in pathological cases.
        let hmonitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
        if hmonitor.0.is_null() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: GetMonitorInfoW fills the struct when cbSize is set
        // correctly, which it is above; the pointer is a valid stack
        // allocation for the duration of the call.
        let ok = unsafe { GetMonitorInfoW(hmonitor, &mut info) };
        if !ok.as_bool() {
            return None;
        }
        let rect = info.rcMonitor;
        Some((rect.left, rect.top, rect.right, rect.bottom))
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
        get_process_info_for_window(hwnd)
            .1
            .as_deref()
            .map(image_basename)
            .unwrap_or_default()
    }

    /// Extracts the image basename (e.g. "Code.exe") from a full
    /// executable path.
    fn image_basename(full_path: &str) -> String {
        full_path
            .rsplit('\\')
            .next()
            .unwrap_or(full_path)
            .to_string()
    }

    /// Resolves `(pid, full executable path)` for the process that owns the
    /// given window. The pid is returned even when the path lookup fails
    /// (access denied on elevated processes); both are `None` when the
    /// window has no resolvable owner.
    fn get_process_info_for_window(
        hwnd: windows::Win32::Foundation::HWND,
    ) -> (Option<u32>, Option<String>) {
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
            return (None, None);
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
                return (Some(pid), None);
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
            return (Some(pid), None);
        }

        let full_path = String::from_utf16_lossy(&name_buf[..name_len as usize]);
        (Some(pid), Some(full_path))
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

    /// Returns an empty observation on non-Windows platforms.
    pub fn get_foreground_info() -> super::RawForegroundInfo {
        super::RawForegroundInfo::default()
    }

    /// Returns 0 on non-Windows platforms.
    pub fn get_idle_seconds() -> u64 {
        0
    }

    /// Returns an empty sweep on non-Windows platforms.
    pub fn enumerate_visible_windows() -> Option<Vec<super::VisibleWindow>> {
        Some(Vec::new())
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

/// Health snapshot of the context poller (Task A8, spec §7), readable by
/// the repair agent through the runtime's `state.json` publish loop.
/// This replaces the old dead hooks: freshness is judged from
/// `last_poll_at`, which the 1 Hz loop stamps on every tick.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ContextWatchHealth {
    /// Whether the poll loop is running. `false` + a reason is the
    /// *disabled-with-reason* state (`pause_all`) — still healthy.
    pub enabled: bool,
    /// Why the poller is disabled, when it is.
    pub disabled_reason: Option<String>,
    /// When the last poll completed (stamped every tick).
    pub last_poll_at: Option<DateTime<Utc>>,
    /// Total completed polls.
    pub polls: u64,
}

impl ContextWatchHealth {
    /// Real freshness check (spec §7): healthy while disabled-with-reason,
    /// while the loop hasn't produced its first poll yet, or while the
    /// last poll is within 3 poll intervals (min 5 s).
    pub fn is_healthy(&self, now: DateTime<Utc>, poll_interval: Duration) -> bool {
        if !self.enabled {
            return true;
        }
        let Some(last) = self.last_poll_at else {
            // Booting: the first tick fires immediately after spawn, so
            // a missing timestamp is a startup state, not a stall — the
            // restart check below covers a loop that never starts.
            return self.polls == 0;
        };
        let threshold_ms = (poll_interval.as_millis() as i64)
            .saturating_mul(3)
            .max(5_000);
        now.signed_duration_since(last).num_milliseconds() <= threshold_ms
    }

    /// Restart only on a sustained stall (10 poll intervals, min 30 s) of
    /// an enabled poller — mirrors `LiveContextHealth::should_restart`'s
    /// shape. Disabled-with-reason never restarts.
    pub fn should_restart(&self, now: DateTime<Utc>, poll_interval: Duration) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(last) = self.last_poll_at else {
            return false;
        };
        let threshold_ms = (poll_interval.as_millis() as i64)
            .saturating_mul(10)
            .max(30_000);
        now.signed_duration_since(last).num_milliseconds() > threshold_ms
    }
}

/// Shared handle onto the poller's health snapshot, for callers that
/// outlive the moved watcher (the runtime's health publisher, tests).
pub type SharedContextWatchHealth = Arc<RwLock<ContextWatchHealth>>;

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
    /// The privacy choke point (spec §4.1). Every title/process/path is
    /// zoned and scrubbed through this filter at collector emit.
    privacy: Arc<PrivacyFilter>,
    /// Honest per-source observation toggles; the window poll is gated by
    /// `pause_all` only.
    toggles: ObservationToggles,
    /// Live toggle control (Task C5): when attached, the loop re-reads the
    /// toggle values every tick instead of honouring the boot-time copy.
    toggle_control: Option<ToggleControl>,
    /// Shared read handle onto the project resolver's current project
    /// (Task A4). `ProjectWorldState` derives its root from this — never
    /// from `current_dir()`. `None` (tests, tools) projects no project.
    project: Option<CurrentProjectHandle>,
    /// Events-channel producer handle (Task A6): `focus_switch` events go
    /// here, non-blocking. Log-only until the runtime injects the real
    /// sender via [`ContextWatcher::with_event_sender`].
    events: EventSender,
    /// Shared health snapshot (Task A8, spec §7): the run loop stamps
    /// every poll; the runtime's health publisher reads the handle.
    health: SharedContextWatchHealth,
}

impl ContextWatcher {
    /// Creates a new context watcher with the given configuration.
    ///
    /// Standalone construction (tests, tools) synthesizes a privacy filter
    /// from the legacy `[context]` lists plus a default `[privacy]`
    /// section. The runtime shares its boot-time filter via
    /// [`ContextWatcher::with_privacy`] instead.
    pub fn new(config: ContextConfig) -> Self {
        debug!(
            layer = "senses",
            component = "context",
            poll_interval_secs = config.poll_interval_secs,
            "ContextWatcher created"
        );
        let privacy = Arc::new(PrivacyFilter::from_config(
            &config,
            &PrivacyConfig::default(),
        ));
        Self {
            config,
            privacy,
            toggles: ObservationToggles::default(),
            toggle_control: None,
            project: None,
            events: EventSender::log_only(),
            health: SharedContextWatchHealth::default(),
        }
    }

    /// Shared handle onto the health snapshot, for callers that outlive
    /// the moved watcher (the runtime's health publisher, tests). Grab it
    /// before `tokio::spawn` moves the watcher (Task A8, spec §7).
    pub fn health_handle(&self) -> SharedContextWatchHealth {
        self.health.clone()
    }

    /// Attaches the shared boot-time privacy filter and observation
    /// toggles (spec §4.1). Called once at senses spawn.
    pub fn with_privacy(mut self, filter: Arc<PrivacyFilter>, toggles: ObservationToggles) -> Self {
        self.privacy = filter;
        self.toggles = toggles;
        self
    }

    /// Attaches the shared **live** toggle control (Task C5, spec §4.13).
    ///
    /// Without it the watcher honours the boot-time
    /// [`ObservationToggles`] copy it was given; with it, every loop
    /// iteration re-reads the current value, so a Context-page switch
    /// takes effect without a restart.
    pub fn with_toggle_control(mut self, control: ToggleControl) -> Self {
        self.toggle_control = Some(control);
        self
    }

    /// The toggle values to honour right now: the live control when one is
    /// attached, else the boot-time copy.
    fn live_toggles(&self) -> ObservationToggles {
        match &self.toggle_control {
            Some(control) => control.snapshot(),
            None => self.toggles.clone(),
        }
    }

    /// Attaches the project resolver's shared current-project handle
    /// (Task A4) so `ProjectWorldState` reflects the resolved project
    /// instead of the daemon's working directory.
    pub fn with_project_handle(mut self, handle: CurrentProjectHandle) -> Self {
        self.project = Some(handle);
        self
    }

    /// Attaches the events-channel producer handle (Task A6, spec §3).
    /// Called once at senses spawn in the runtime binary; without it the
    /// watcher's `focus_switch` events are log-only.
    pub fn with_event_sender(mut self, sender: EventSender) -> Self {
        self.events = sender;
        self
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
        // Honest toggles (spec §4.1): the window poll has no dedicated
        // toggle, so only `pause_all` gates it. Defense in depth alongside
        // the frame-loop gate in the runtime binary.
        //
        // With a live toggle control attached (Task C5) the loop re-checks
        // per tick instead of parking, so unpausing from the Context page
        // takes effect within one poll interval.
        if !source_enabled(&self.live_toggles(), ObservedSource::Window) {
            emit_system_event(
                "toggle_change",
                "context/window observation paused by [privacy.toggles].pause_all",
            );
            {
                let mut health = self.health.write();
                health.enabled = false;
                health.disabled_reason = Some("paused by [privacy.toggles].pause_all".to_string());
            }
            if self.toggle_control.is_none() {
                while !*shutdown.borrow() {
                    if shutdown.changed().await.is_err() {
                        break;
                    }
                }
                return Ok(());
            }
        }
        {
            let mut health = self.health.write();
            health.enabled = true;
            health.disabled_reason = None;
        }

        let interval = Duration::from_secs(self.config.poll_interval_secs.max(1));
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!(
            layer = "senses",
            component = "context",
            interval_secs = self.config.poll_interval_secs,
            "Context poller loop starting"
        );

        // Sentinel-aware dwell bookkeeping (spec §4.1): entering or leaving
        // a never_observe window resets dwell, and consecutive excluded
        // windows collapse into one `[excluded]` bucket. Its transitions
        // drive the focus_switch events below (spec §4.2) — the sentinel
        // bucket provides the synthetic from/to `[excluded]` endpoints.
        let mut dwell = DwellTracker::default();

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // Honest toggles, live (spec §4.1, Task C5): a paused
                    // window source polls nothing at all this tick — no
                    // Win32 call, no observation, no event.
                    if !source_enabled(&self.live_toggles(), ObservedSource::Window) {
                        let mut health = self.health.write();
                        if health.enabled {
                            health.enabled = false;
                            health.disabled_reason =
                                Some("paused by [privacy.toggles].pause_all".to_string());
                        }
                        continue;
                    }
                    {
                        let mut health = self.health.write();
                        if !health.enabled {
                            health.enabled = true;
                            health.disabled_reason = None;
                        }
                    }
                    let PolledContext {
                        observation: mut obs,
                        privacy,
                        monitor_rect,
                    } = self.poll_once_with_privacy();
                    {
                        // Health stamp (Task A8): every completed poll
                        // refreshes the freshness timestamp the runtime's
                        // health publisher judges this component by.
                        let mut health = self.health.write();
                        health.last_poll_at = Some(obs.ts);
                        health.polls = health.polls.saturating_add(1);
                    }
                    let sample = dwell.observe(
                        &obs.foreground_process_name,
                        &obs.foreground_window_title,
                        privacy,
                        Instant::now(),
                    );
                    obs.active_since_secs = sample.active_secs;

                    // Switch-event detection (spec §4.2): a dwell
                    // transition becomes a focus_switch ContextEvent on
                    // the events channel (Task A6). The watcher stamps
                    // the resolver's current project itself — it holds
                    // the shared handle — so the event arrives at the
                    // writer already attributed.
                    if let Some(switch) = &sample.switch {
                        if let Some(mut event) = focus_switch_event(switch, &obs, privacy) {
                            fill_project_id_from_handle(&mut event, self.project.as_ref());
                            self.events.send(event);
                        }
                    }

                    if let Some(hub) = &live_context {
                        let geometries = hub.monitor_geometries();

                        // Monitor enrichment (spec §4.2): join the
                        // MonitorFromWindow rect to the hub's monitor id
                        // scheme (`display-N`). The sentinel already
                        // cleared its rect; unmatched rects stay None.
                        if let Some(rect) = monitor_rect {
                            obs.monitor_id = monitor_id_for_rect(rect, &geometries);
                        }

                        hub.record_context(
                            WindowWorldState {
                                process_name: obs.foreground_process_name.clone(),
                                title: obs.foreground_window_title.clone(),
                                observed_at: obs.ts,
                                in_call: obs.in_call,
                                privacy,
                                // Schema v4 (Task C3): publish the Task A2
                                // enrichment so the MCP `context_window`
                                // tool sees what the frame loop sees.
                                pid: obs.pid,
                                exe_path: obs.exe_path.clone(),
                                monitor_id: obs.monitor_id.clone(),
                                active_since_secs: obs.active_since_secs,
                            },
                            InputActivityWorldState {
                                observed_at: obs.ts,
                                idle_seconds: obs.idle_seconds,
                                active: obs.idle_seconds < 2,
                            },
                        );
                        // Project projection (Task A4): derived from the
                        // resolver's current project — never current_dir().
                        let current_project =
                            self.project.as_ref().and_then(|handle| handle.read().clone());
                        hub.record_project(project_world_state(
                            &obs.foreground_process_name,
                            &self.config.terminal_process_names,
                            obs.ts,
                            &self.privacy,
                            current_project.as_ref(),
                        ));

                        // Per-monitor visible-window sweep (spec §4.1): a
                        // monitor showing any never_observe/local_only
                        // top-level window inherits that zone for
                        // capture/caption purposes. A failed enumeration
                        // keeps the previous zones (never relax privacy
                        // from incomplete data).
                        if !geometries.is_empty() {
                            if let Some(visible) = win::enumerate_visible_windows() {
                                hub.set_monitor_zones(compute_monitor_zones(
                                    &self.privacy,
                                    &visible,
                                    &geometries,
                                ));
                            }
                        }
                    }

                    trace!(
                        layer = "senses",
                        component = "context",
                        window_title = %obs.foreground_window_title,
                        process_name = %obs.foreground_process_name,
                        idle_seconds = obs.idle_seconds,
                        in_call = obs.in_call,
                        dwell_secs = sample.active_secs,
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
        self.poll_once_with_privacy().observation
    }

    fn poll_once_with_privacy(&self) -> PolledContext {
        let raw = win::get_foreground_info();
        let idle_seconds = win::get_idle_seconds();
        sanitize_observation(&self.privacy, &self.config, raw, idle_seconds)
    }

    /// Returns `true` if the watcher appears to be in a healthy state
    /// (Task A8: the formerly-dead hook is now a real freshness check —
    /// see [`ContextWatchHealth::is_healthy`]).
    pub fn is_healthy(&self) -> bool {
        self.health.read().is_healthy(
            Utc::now(),
            Duration::from_secs(self.config.poll_interval_secs.max(1)),
        )
    }

    /// Returns `true` if the watcher should be restarted by the repair
    /// agent: only on a sustained poll stall of an enabled loop (Task A8
    /// — see [`ContextWatchHealth::should_restart`]).
    pub fn should_restart(&self) -> bool {
        self.health.read().should_restart(
            Utc::now(),
            Duration::from_secs(self.config.poll_interval_secs.max(1)),
        )
    }
}

/// One sanitized poll: the observation, its privacy disposition, and the
/// raw monitor rect (cleared for excluded windows so the sentinel cannot
/// leak *which monitor* an excluded app is on).
#[derive(Debug, Clone)]
struct PolledContext {
    observation: ContextObservation,
    privacy: PrivacyDisposition,
    monitor_rect: Option<(i32, i32, i32, i32)>,
}

/// Builds the emitted observation for one poll (spec §4.1) — the single
/// place where the raw foreground facts become collector output.
///
/// Zone semantics:
/// - `never_observe` → the **sentinel observation**: process
///   [`EXCLUDED_PROCESS`], title [`EXCLUDED_TITLE`] (empty),
///   `in_call = false` (call detection would leak which excluded app is
///   focused), disposition `Excluded`. The enrichment fields (`pid`,
///   `exe_path`, `monitor_id`, monitor rect) are cleared — they identify
///   the excluded app or its location. The frame keeps flowing with
///   sentinel content so latest-wins consumers never persist the
///   *previous* window as current (the stale-frame bug the sentinel
///   exists to prevent). `idle_seconds` stays real — coarse input
///   activity is not window content.
/// - `local_only` → observed; the title is replaced by the legacy
///   redaction literal when `[context].redact_sensitive_titles` is set
///   (today's in-place redaction for cloud-bound contexts), otherwise
///   scrubbed. Disposition `Redacted`.
/// - `cloud_allowed` → observed; title scrubbed. Disposition `Visible`.
///
/// The process name and executable path are filesystem-derived structured
/// fields and pass through [`PrivacyFilter::scrub_path`]
/// (username-component redaction), never `scrub_text`.
///
/// `active_since_secs` and `monitor_id` are filled by the caller (from the
/// dwell tracker and the hub's monitor geometry respectively).
fn sanitize_observation(
    privacy: &PrivacyFilter,
    config: &ContextConfig,
    raw: RawForegroundInfo,
    idle_seconds: u64,
) -> PolledContext {
    let ts = Utc::now();
    match privacy.resolve_zone(&raw.process_name, &raw.title) {
        Zone::NeverObserve => PolledContext {
            observation: ContextObservation {
                foreground_window_title: EXCLUDED_TITLE.to_string(),
                foreground_process_name: EXCLUDED_PROCESS.to_string(),
                idle_seconds,
                in_call: false,
                pid: None,
                exe_path: None,
                active_since_secs: 0,
                monitor_id: None,
                privacy: Some(PrivacyDisposition::Excluded),
                ts,
            },
            privacy: PrivacyDisposition::Excluded,
            monitor_rect: None,
        },
        zone @ (Zone::LocalOnly | Zone::CloudAllowed) => {
            let in_call = is_in_call(&raw.process_name, &raw.title);
            let title = if zone == Zone::LocalOnly && config.redact_sensitive_titles {
                REDACTED_TITLE.to_string()
            } else {
                privacy.scrub_text(&raw.title)
            };
            let disposition = if zone == Zone::LocalOnly {
                PrivacyDisposition::Redacted
            } else {
                PrivacyDisposition::Visible
            };
            PolledContext {
                observation: ContextObservation {
                    foreground_window_title: title,
                    foreground_process_name: privacy.scrub_path(&raw.process_name),
                    idle_seconds,
                    in_call,
                    pid: raw.pid,
                    exe_path: raw.exe_path.as_deref().map(|path| privacy.scrub_path(path)),
                    active_since_secs: 0,
                    monitor_id: None,
                    // The zone tag rides *on the observation* (fixwave 2,
                    // I3): the cloud gate must not have to re-derive it
                    // from `REDACTED_TITLE`, which only exists when the
                    // legacy `redact_sensitive_titles` knob is on.
                    privacy: Some(disposition),
                    ts,
                },
                privacy: disposition,
                monitor_rect: raw.monitor_rect,
            }
        }
    }
}

/// Maps a monitor rect (from `MonitorFromWindow`) to the hub's monitor id
/// scheme (`display-N`, the same keys `set_monitor_zones` uses) by exact
/// geometry match. Returns `None` when no projected monitor matches —
/// callers must treat that as "monitor unknown", never guess.
fn monitor_id_for_rect(rect: (i32, i32, i32, i32), monitors: &[MonitorGeometry]) -> Option<String> {
    let (left, top, right, bottom) = rect;
    let width = i64::from(right) - i64::from(left);
    let height = i64::from(bottom) - i64::from(top);
    monitors
        .iter()
        .find(|monitor| {
            monitor.x == left
                && monitor.y == top
                && i64::from(monitor.width) == width
                && i64::from(monitor.height) == height
        })
        .map(|monitor| monitor.monitor_id.clone())
}

/// Sentinel-aware focus-dwell bookkeeping (spec §4.1).
///
/// Tracks how long the current (process, title) pair has been focused and
/// reports focus transitions. Because the tracker observes *sanitized*
/// observations, every `never_observe` window maps to the same
/// `([excluded], "")` key: dwell resets on any sentinel↔real transition,
/// and consecutive excluded windows collapse into one `[excluded]` span —
/// so a switch touching an excluded endpoint surfaces as exactly one
/// synthetic switch from/to the literal `[excluded]` bucket (spec §4.2),
/// and excluded→excluded transitions are suppressed entirely.
#[derive(Debug, Default)]
struct DwellTracker {
    key: Option<(String, String)>,
    disposition: PrivacyDisposition,
    since: Option<Instant>,
}

/// One dwell-tracker poll result: the current focus target's accumulated
/// dwell plus the transition that ended the previous target, if any.
#[derive(Debug, Clone)]
struct DwellSample {
    /// Whole seconds the current (process, title) pair has been focused.
    active_secs: u64,
    /// The focus switch this poll observed (`None` while focus is stable
    /// and on the very first poll).
    switch: Option<FocusSwitch>,
}

/// A completed focus span: what was focused, how it was zoned, and for how
/// long — the raw material for a `focus_switch` [`ContextEvent`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct FocusSwitch {
    /// Sanitized process name of the window focus left (may be the
    /// literal `[excluded]` bucket).
    from_process: String,
    /// Sanitized/redacted title of the window focus left.
    from_title: String,
    /// Privacy disposition of the departed window.
    from_disposition: PrivacyDisposition,
    /// Whole seconds the departed window held focus.
    dwell_secs: u64,
}

impl DwellTracker {
    /// Records the sanitized (process, title) pair for this poll and
    /// returns the dwell sample: current dwell seconds plus the focus
    /// switch that ended the previous span, if this poll changed focus.
    fn observe(
        &mut self,
        process: &str,
        title: &str,
        disposition: PrivacyDisposition,
        now: Instant,
    ) -> DwellSample {
        let changed = self
            .key
            .as_ref()
            .is_none_or(|(p, t)| p != process || t != title);
        if !changed {
            self.disposition = disposition;
            return DwellSample {
                active_secs: self
                    .since
                    .map(|since| now.duration_since(since).as_secs())
                    .unwrap_or(0),
                switch: None,
            };
        }
        let switch = match (self.key.take(), self.since) {
            (Some((from_process, from_title)), Some(since)) => Some(FocusSwitch {
                from_process,
                from_title,
                from_disposition: self.disposition,
                dwell_secs: now.duration_since(since).as_secs(),
            }),
            _ => None,
        };
        self.key = Some((process.to_string(), title.to_string()));
        self.disposition = disposition;
        self.since = Some(now);
        DwellSample {
            active_secs: 0,
            switch,
        }
    }
}

/// Builds the `focus_switch` [`ContextEvent`] for a dwell transition
/// (spec §4.2), or `None` when either endpoint has no process (no window
/// was focused — startup, desktop, secure-desktop flashes).
///
/// Encoding (stable template — consumers parse the summary): summary is
/// `"<from_app> → <to_app> after <dwell>s"`, `application`/`window_title`
/// are the *destination* (already scrubbed upstream), sensitivity is
/// `cloud_allowed` only when **both** endpoints are `Visible` — a
/// `local_only` or excluded endpoint makes the whole switch `local_only`
/// (strictest-zone propagation, spec §4.1). Excluded endpoints appear only
/// as the literal `[excluded]` bucket, never as the real app.
///
/// `project_id` is `None` at emit: the builder runs below the resolver.
/// The watcher stamps the resolver's current project from its shared
/// [`CurrentProjectHandle`] right before sending (Task A6) — see the
/// `run_inner` push site.
fn focus_switch_event(
    switch: &FocusSwitch,
    observation: &ContextObservation,
    to_disposition: PrivacyDisposition,
) -> Option<ContextEvent> {
    if switch.from_process.is_empty() || observation.foreground_process_name.is_empty() {
        return None;
    }
    let sensitivity = if switch.from_disposition == PrivacyDisposition::Visible
        && to_disposition == PrivacyDisposition::Visible
    {
        EventSensitivity::CloudAllowed
    } else {
        EventSensitivity::LocalOnly
    };
    Some(ContextEvent {
        ts: observation.ts,
        source: EventSource::Window,
        application: observation.foreground_process_name.clone(),
        window_title: observation.foreground_window_title.clone(),
        project_id: None,
        event_type: EventType::FocusSwitch,
        summary: format!(
            "{} → {} after {}s",
            switch.from_process, observation.foreground_process_name, switch.dwell_secs
        ),
        importance: COLLECTOR_EVENT_IMPORTANCE,
        confidence: 1.0,
        sensitivity,
        raw_reference: None,
    })
}

/// Positive intersection area between a window rect and a monitor, or 0.
fn intersection_area(rect: (i32, i32, i32, i32), monitor: &MonitorGeometry) -> i64 {
    let (left, top, right, bottom) = rect;
    let m_right = monitor.x.saturating_add(monitor.width as i32);
    let m_bottom = monitor.y.saturating_add(monitor.height as i32);
    let width = i64::from(right.min(m_right)) - i64::from(left.max(monitor.x));
    let height = i64::from(bottom.min(m_bottom)) - i64::from(top.max(monitor.y));
    if width <= 0 || height <= 0 {
        0
    } else {
        width * height
    }
}

/// Computes the strictest privacy zone per monitor from the visible-window
/// sweep (spec §4.1). Every monitor a sensitive window overlaps inherits
/// that window's zone — a window straddling two monitors tightens both
/// (stricter is always safe). Monitors without sensitive windows are
/// absent from the map (implicitly `cloud_allowed`).
fn compute_monitor_zones(
    privacy: &PrivacyFilter,
    windows: &[VisibleWindow],
    monitors: &[MonitorGeometry],
) -> BTreeMap<String, Zone> {
    let mut zones: BTreeMap<String, Zone> = BTreeMap::new();
    for window in windows {
        let zone = privacy.resolve_zone(&window.process_name, &window.title);
        if zone == Zone::CloudAllowed {
            continue;
        }
        for monitor in monitors {
            if intersection_area(window.rect, monitor) > 0 {
                let entry = zones
                    .entry(monitor.monitor_id.clone())
                    .or_insert(Zone::CloudAllowed);
                *entry = strictest([*entry, zone]);
            }
        }
    }
    zones
}

/// Builds the shared project projection from the resolver's current
/// project (Task A4, spec §4.3). Before the resolver existed this derived
/// the root from the daemon's `current_dir()` — always Continuum's own
/// repo, never the user's actual project. Now: no resolved project → no
/// project fields, ever.
///
/// Git facts (`git_head`, `branch`, counts, last commit) are **owned by
/// the git collector** (`senses::git_watch`, Task A5) and merged into the
/// hub via [`LiveContextHub::record_git_facts`]; this projection leaves
/// them empty and [`LiveContextHub::record_project`] carries the
/// collector's values forward for an unchanged root.
fn project_world_state(
    foreground_process: &str,
    terminal_processes: &[String],
    observed_at: chrono::DateTime<Utc>,
    privacy: &PrivacyFilter,
    current: Option<&CurrentProject>,
) -> ProjectWorldState {
    let terminal_active = terminal_processes
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(foreground_process));
    let project_root = current.and_then(|project| project.root_path.clone());
    let project_name = current.map(|project| project.name.clone());
    ProjectWorldState {
        observed_at,
        terminal_active,
        terminal_process: terminal_active.then(|| foreground_process.to_string()),
        // Paths are scrubbed at collector emit (spec §4.1): home prefix →
        // `~`, username components redacted.
        project_root: project_root.map(|root| privacy.scrub_path(&root.to_string_lossy())),
        project_name: project_name.map(|name| privacy.scrub_path(&name)),
        git_head: None,
        branch: None,
        dirty: 0,
        staged: 0,
        untracked: 0,
        ahead: 0,
        behind: 0,
        conflicts: 0,
        last_commit_id: None,
        last_commit_subject: None,
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

    fn test_filter() -> PrivacyFilter {
        PrivacyFilter::from_config(&ContextConfig::default(), &PrivacyConfig::default())
            .with_environment(None, None)
    }

    fn raw(title: &str, process: &str) -> RawForegroundInfo {
        RawForegroundInfo {
            title: title.to_string(),
            process_name: process.to_string(),
            pid: Some(4321),
            exe_path: Some(format!("C:\\Apps\\{process}")),
            monitor_rect: Some((0, 0, 1920, 1080)),
        }
    }

    #[test]
    fn sensitive_titles_are_classified_before_publication() {
        let filter = test_filter();
        let config = ContextConfig::default();
        // Legacy title keyword → local_only → observed but redacted.
        let polled = sanitize_observation(&filter, &config, raw("Enter password", "chrome.exe"), 0);
        assert_eq!(polled.privacy, PrivacyDisposition::Redacted);
        assert_eq!(polled.observation.foreground_window_title, REDACTED_TITLE);
        assert_eq!(polled.observation.foreground_process_name, "chrome.exe");
        // Non-sensitive window → visible, title intact.
        let polled = sanitize_observation(
            &filter,
            &config,
            raw("vision.rs - Continuum", "Code.exe"),
            0,
        );
        assert_eq!(polled.privacy, PrivacyDisposition::Visible);
        assert_eq!(
            polled.observation.foreground_window_title,
            "vision.rs - Continuum"
        );
    }

    #[test]
    fn never_observe_process_yields_the_sentinel_observation() {
        // Spec §4.1: a never_observe window emits the sentinel — process
        // "[excluded]", empty title, Excluded — instead of a redacted
        // title. The observation keeps flowing so latest-wins consumers
        // never persist the previous window as current.
        let filter = test_filter();
        let config = ContextConfig::default();
        let polled = sanitize_observation(
            &filter,
            &config,
            raw("1Password — my vault", "1password.exe"),
            7,
        );
        let obs = &polled.observation;
        assert_eq!(polled.privacy, PrivacyDisposition::Excluded);
        assert_eq!(obs.foreground_process_name, EXCLUDED_PROCESS);
        assert_eq!(obs.foreground_window_title, EXCLUDED_TITLE);
        assert!(!obs.in_call, "call detection must not leak excluded apps");
        assert_eq!(obs.idle_seconds, 7, "input activity is not window content");
        // Enrichment fields identify the excluded app or its location and
        // must be cleared on the sentinel (spec §4.2).
        assert_eq!(obs.pid, None);
        assert_eq!(obs.exe_path, None);
        assert_eq!(obs.monitor_id, None);
        assert_eq!(polled.monitor_rect, None, "rect would leak the monitor");
    }

    #[test]
    fn cloud_allowed_titles_are_scrubbed_for_secrets() {
        let filter = test_filter();
        let config = ContextConfig::default();
        let polled = sanitize_observation(
            &filter,
            &config,
            raw(
                "token ghp_AbCd1234EfGh5678IjKl9012MnOp3456QrSt — Notepad",
                "notepad.exe",
            ),
            0,
        );
        assert_eq!(polled.privacy, PrivacyDisposition::Visible);
        assert!(!polled.observation.foreground_window_title.contains("ghp_"));
        assert!(polled
            .observation
            .foreground_window_title
            .contains("[REDACTED]"));
    }

    #[test]
    fn local_only_title_scrubbed_when_redaction_disabled() {
        let filter = test_filter();
        let config = ContextConfig {
            redact_sensitive_titles: false,
            ..ContextConfig::default()
        };
        let polled = sanitize_observation(
            &filter,
            &config,
            raw("password sk-live1234567890abcdef", "chrome.exe"),
            0,
        );
        assert_eq!(polled.privacy, PrivacyDisposition::Redacted);
        assert!(!polled
            .observation
            .foreground_window_title
            .contains("sk-live"));
    }

    // --- Window/process enrichment (spec §4.2) ---

    #[test]
    fn enrichment_fields_pass_through_with_scrubbed_exe_path() {
        let filter = test_filter().with_environment(
            Some("C:\\Users\\testuser".to_string()),
            Some("testuser".to_string()),
        );
        let config = ContextConfig::default();
        let mut info = raw("main.rs - Continuum", "Code.exe");
        info.exe_path = Some("C:\\Users\\testuser\\AppData\\Code\\Code.exe".to_string());
        let polled = sanitize_observation(&filter, &config, info, 0);
        assert_eq!(polled.observation.pid, Some(4321));
        assert_eq!(
            polled.observation.exe_path.as_deref(),
            Some("~\\AppData\\Code\\Code.exe"),
            "exe_path must be path-scrubbed at collector emit"
        );
        assert_eq!(polled.monitor_rect, Some((0, 0, 1920, 1080)));
        // monitor_id and active_since_secs are filled by the caller.
        assert_eq!(polled.observation.monitor_id, None);
        assert_eq!(polled.observation.active_since_secs, 0);
    }

    #[test]
    fn monitor_id_maps_by_exact_geometry_with_none_fallback() {
        let monitors = [monitor("display-1", 0, 0), monitor("display-2", 1920, 0)];
        // Exact geometry match → the hub's monitor id.
        assert_eq!(
            monitor_id_for_rect((0, 0, 1920, 1080), &monitors).as_deref(),
            Some("display-1")
        );
        assert_eq!(
            monitor_id_for_rect((1920, 0, 3840, 1080), &monitors).as_deref(),
            Some("display-2")
        );
        // Unknown rect (hot-plugged monitor not yet projected, DPI
        // mismatch) → None, never a guess.
        assert_eq!(monitor_id_for_rect((5000, 0, 6920, 1080), &monitors), None);
        // No projected monitors at all → None.
        assert_eq!(monitor_id_for_rect((0, 0, 1920, 1080), &[]), None);
    }

    // --- Dwell bookkeeping (spec §4.1 sentinel semantics) ---

    #[test]
    fn dwell_resets_on_sentinel_transitions() {
        let mut dwell = DwellTracker::default();
        let t0 = Instant::now();
        let visible = PrivacyDisposition::Visible;
        let excluded = PrivacyDisposition::Excluded;
        assert_eq!(
            dwell
                .observe("Code.exe", "main.rs", visible, t0)
                .active_secs,
            0
        );
        assert_eq!(
            dwell
                .observe("Code.exe", "main.rs", visible, t0 + Duration::from_secs(5))
                .active_secs,
            5
        );
        // Entering an excluded window resets dwell.
        assert_eq!(
            dwell
                .observe(
                    EXCLUDED_PROCESS,
                    EXCLUDED_TITLE,
                    excluded,
                    t0 + Duration::from_secs(6)
                )
                .active_secs,
            0
        );
        // Consecutive excluded windows share one [excluded] bucket: two
        // different never_observe apps in a row do NOT reset each other.
        assert_eq!(
            dwell
                .observe(
                    EXCLUDED_PROCESS,
                    EXCLUDED_TITLE,
                    excluded,
                    t0 + Duration::from_secs(9)
                )
                .active_secs,
            3
        );
        // Leaving the sentinel resets again.
        assert_eq!(
            dwell
                .observe("Code.exe", "main.rs", visible, t0 + Duration::from_secs(10))
                .active_secs,
            0
        );
    }

    // --- Switch detection (spec §4.2) ---

    #[test]
    fn dwell_reports_switches_with_departed_span() {
        let mut dwell = DwellTracker::default();
        let t0 = Instant::now();
        let visible = PrivacyDisposition::Visible;
        // First observation ever: no switch (nothing was focused before).
        assert!(dwell
            .observe("Code.exe", "main.rs", visible, t0)
            .switch
            .is_none());
        // Stable focus: no switch.
        assert!(dwell
            .observe("Code.exe", "main.rs", visible, t0 + Duration::from_secs(5))
            .switch
            .is_none());
        // Focus change: the switch carries the departed window + its dwell.
        let sample = dwell.observe(
            "chrome.exe",
            "GitHub",
            visible,
            t0 + Duration::from_secs(12),
        );
        let switch = sample.switch.expect("focus change must report a switch");
        assert_eq!(switch.from_process, "Code.exe");
        assert_eq!(switch.from_title, "main.rs");
        assert_eq!(switch.from_disposition, PrivacyDisposition::Visible);
        assert_eq!(switch.dwell_secs, 12);
        assert_eq!(sample.active_secs, 0, "new window starts at zero dwell");
    }

    #[test]
    fn excluded_boundary_switches_are_synthetic_and_collapsed() {
        // Spec §4.1/§4.2 sentinel semantics: switches touching a
        // never_observe window surface only as the synthetic [excluded]
        // bucket, and excluded→excluded transitions are suppressed
        // entirely (the sanitized key is identical, so no switch fires).
        let mut dwell = DwellTracker::default();
        let t0 = Instant::now();
        let visible = PrivacyDisposition::Visible;
        let excluded = PrivacyDisposition::Excluded;

        dwell.observe("Code.exe", "main.rs", visible, t0);
        // Real → excluded: one synthetic switch out of Code.exe.
        let sample = dwell.observe(
            EXCLUDED_PROCESS,
            EXCLUDED_TITLE,
            excluded,
            t0 + Duration::from_secs(5),
        );
        let switch = sample.switch.expect("entering the sentinel is a switch");
        assert_eq!(switch.from_process, "Code.exe");
        // Excluded → excluded (a *different* never_observe app): sanitized
        // to the same bucket, so NO switch event — the two excluded apps
        // must not be distinguishable even by event count.
        assert!(dwell
            .observe(
                EXCLUDED_PROCESS,
                EXCLUDED_TITLE,
                excluded,
                t0 + Duration::from_secs(9)
            )
            .switch
            .is_none());
        // Excluded → real: one synthetic switch out of the bucket.
        let sample = dwell.observe(
            "chrome.exe",
            "GitHub",
            visible,
            t0 + Duration::from_secs(11),
        );
        let switch = sample.switch.expect("leaving the sentinel is a switch");
        assert_eq!(switch.from_process, EXCLUDED_PROCESS);
        assert_eq!(switch.from_title, EXCLUDED_TITLE);
        assert_eq!(switch.from_disposition, PrivacyDisposition::Excluded);
        assert_eq!(switch.dwell_secs, 6, "one collapsed [excluded] span");
    }

    fn obs_for(process: &str, title: &str) -> ContextObservation {
        ContextObservation {
            foreground_window_title: title.to_string(),
            foreground_process_name: process.to_string(),
            ts: Utc::now(),
            ..Default::default()
        }
    }

    #[test]
    fn focus_switch_event_uses_the_stable_summary_template() {
        let switch = FocusSwitch {
            from_process: "Code.exe".into(),
            from_title: "main.rs".into(),
            from_disposition: PrivacyDisposition::Visible,
            dwell_secs: 42,
        };
        let obs = obs_for("chrome.exe", "GitHub - PR #7");
        let event = focus_switch_event(&switch, &obs, PrivacyDisposition::Visible)
            .expect("visible→visible switch emits an event");
        assert_eq!(event.summary, "Code.exe → chrome.exe after 42s");
        assert_eq!(event.source, EventSource::Window);
        assert_eq!(event.event_type, EventType::FocusSwitch);
        assert!(event.event_type.valid_for(event.source));
        assert_eq!(event.application, "chrome.exe");
        assert_eq!(event.window_title, "GitHub - PR #7");
        assert_eq!(event.sensitivity, EventSensitivity::CloudAllowed);
        assert_eq!(
            event.project_id, None,
            "project_id is stamped from the watcher's project handle at send, not at build"
        );
        assert_eq!(event.confidence, 1.0);
    }

    #[test]
    fn focus_switch_event_sensitivity_follows_strictest_endpoint() {
        let obs = obs_for("chrome.exe", "GitHub");
        let visible_switch = FocusSwitch {
            from_process: "msedge.exe".into(),
            from_title: REDACTED_TITLE.into(),
            from_disposition: PrivacyDisposition::Redacted,
            dwell_secs: 3,
        };
        // local_only FROM endpoint → whole switch local_only.
        let event = focus_switch_event(&visible_switch, &obs, PrivacyDisposition::Visible).unwrap();
        assert_eq!(event.sensitivity, EventSensitivity::LocalOnly);
        // local_only TO endpoint → local_only too.
        let from_visible = FocusSwitch {
            from_process: "Code.exe".into(),
            from_title: "main.rs".into(),
            from_disposition: PrivacyDisposition::Visible,
            dwell_secs: 3,
        };
        let event = focus_switch_event(&from_visible, &obs, PrivacyDisposition::Redacted).unwrap();
        assert_eq!(event.sensitivity, EventSensitivity::LocalOnly);
        // Excluded endpoint (the synthetic [excluded] switch) is
        // local_only as well — never cloud-bound.
        let sentinel_obs = obs_for(EXCLUDED_PROCESS, EXCLUDED_TITLE);
        let event =
            focus_switch_event(&from_visible, &sentinel_obs, PrivacyDisposition::Excluded).unwrap();
        assert_eq!(event.sensitivity, EventSensitivity::LocalOnly);
        assert_eq!(event.summary, "Code.exe → [excluded] after 3s");
    }

    #[test]
    fn focus_switch_event_suppressed_for_windowless_endpoints() {
        let switch = FocusSwitch {
            from_process: String::new(),
            from_title: String::new(),
            from_disposition: PrivacyDisposition::Visible,
            dwell_secs: 2,
        };
        let obs = obs_for("chrome.exe", "GitHub");
        assert!(
            focus_switch_event(&switch, &obs, PrivacyDisposition::Visible).is_none(),
            "no-foreground-window spans must not produce switch events"
        );
        let from_real = FocusSwitch {
            from_process: "chrome.exe".into(),
            from_title: "GitHub".into(),
            from_disposition: PrivacyDisposition::Visible,
            dwell_secs: 2,
        };
        let empty_obs = obs_for("", "");
        assert!(focus_switch_event(&from_real, &empty_obs, PrivacyDisposition::Visible).is_none());
    }

    #[tokio::test]
    async fn switch_events_are_stamped_and_sent_on_the_events_channel() {
        // The Task A6 transport: the watcher stamps its project handle
        // onto the built event and sends it through the EventSender —
        // this test exercises exactly what the run_inner push site does.
        use crate::context::project::CurrentProject;
        use crate::context::project::ProjectStatus;

        let (sender, mut rx) = crate::memory::events::EventSender::bounded(4);
        let handle: CurrentProjectHandle =
            Arc::new(parking_lot::RwLock::new(Some(CurrentProject {
                id: "continuum".into(),
                name: "Continuum".into(),
                root_path: None,
                confidence: 0.9,
                source_tier: 1,
                zone: None,
                status: ProjectStatus::Confirmed,
            })));

        let switch = FocusSwitch {
            from_process: "a6-test-from.exe".into(),
            from_title: "from".into(),
            from_disposition: PrivacyDisposition::Visible,
            dwell_secs: 7,
        };
        let obs = obs_for("a6-test-to.exe", "to");
        let mut event = focus_switch_event(&switch, &obs, PrivacyDisposition::Visible).unwrap();
        fill_project_id_from_handle(&mut event, Some(&handle));
        sender.send(event);

        let received = rx.try_recv().expect("switch event must reach the channel");
        assert_eq!(
            received.summary,
            "a6-test-from.exe → a6-test-to.exe after 7s"
        );
        assert_eq!(received.project_id.as_deref(), Some("continuum"));
    }

    // --- Per-monitor visible-window sweep (spec §4.1) ---

    fn monitor(id: &str, x: i32, y: i32) -> MonitorGeometry {
        MonitorGeometry {
            monitor_id: id.to_string(),
            x,
            y,
            width: 1920,
            height: 1080,
        }
    }

    fn window(process: &str, title: &str, rect: (i32, i32, i32, i32)) -> VisibleWindow {
        VisibleWindow {
            title: title.to_string(),
            process_name: process.to_string(),
            rect,
        }
    }

    #[test]
    fn monitor_inherits_strictest_zone_of_its_visible_windows() {
        let filter = test_filter();
        let monitors = [monitor("display-1", 0, 0), monitor("display-2", 1920, 0)];
        // Sensitive password manager on display-2; benign editor on
        // display-1; a local_only (private browsing) window also on
        // display-2 — never_observe must win there.
        let windows = [
            window("Code.exe", "main.rs - Continuum", (100, 100, 900, 800)),
            window("1password.exe", "1Password", (2000, 100, 2800, 800)),
            window("chrome.exe", "site - Incognito", (2100, 200, 2900, 900)),
        ];
        let zones = compute_monitor_zones(&filter, &windows, &monitors);
        assert_eq!(zones.get("display-1"), None, "benign monitor stays open");
        assert_eq!(zones.get("display-2"), Some(&Zone::NeverObserve));
    }

    #[test]
    fn straddling_window_tightens_every_overlapped_monitor() {
        let filter = test_filter();
        let monitors = [monitor("display-1", 0, 0), monitor("display-2", 1920, 0)];
        // A never_observe window spanning the boundary between monitors.
        let windows = [window("keepassxc.exe", "KeePassXC", (1500, 100, 2400, 800))];
        let zones = compute_monitor_zones(&filter, &windows, &monitors);
        assert_eq!(zones.get("display-1"), Some(&Zone::NeverObserve));
        assert_eq!(zones.get("display-2"), Some(&Zone::NeverObserve));
    }

    #[test]
    fn local_only_window_marks_monitor_local_only() {
        let filter = test_filter();
        let monitors = [monitor("display-1", 0, 0)];
        let windows = [window(
            "msedge.exe",
            "Docs - InPrivate",
            (100, 100, 900, 800),
        )];
        let zones = compute_monitor_zones(&filter, &windows, &monitors);
        assert_eq!(zones.get("display-1"), Some(&Zone::LocalOnly));
    }

    #[test]
    fn offscreen_window_affects_no_monitor() {
        let filter = test_filter();
        let monitors = [monitor("display-1", 0, 0)];
        let windows = [window("1password.exe", "1Password", (-2000, 0, -100, 500))];
        let zones = compute_monitor_zones(&filter, &windows, &monitors);
        assert!(zones.is_empty());
    }

    #[test]
    fn terminal_projection_never_contains_terminal_text() {
        let config = ContextConfig::default();
        let filter = test_filter();
        let state = project_world_state(
            "pwsh.exe",
            &config.terminal_process_names,
            Utc::now(),
            &filter,
            None,
        );
        assert!(state.terminal_active);
        assert_eq!(state.terminal_process.as_deref(), Some("pwsh.exe"));
    }

    #[test]
    fn project_world_state_derives_from_resolver_never_current_dir() {
        // Task A4 (spec §4.3): with no resolved project there are NO
        // project fields — the old behavior derived them from the daemon's
        // own current_dir(), which was always wrong.
        let filter = test_filter();
        let state = project_world_state("Code.exe", &[], Utc::now(), &filter, None);
        assert_eq!(state.project_root, None);
        assert_eq!(state.project_name, None);
        assert_eq!(state.git_head, None);

        // With a resolved project, root/name come from the resolver. Git
        // facts are the git collector's (Task A5) — this projection leaves
        // them empty and the hub merges/carries them.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("myproj");
        std::fs::create_dir_all(&repo).unwrap();
        let current = CurrentProject {
            id: "myproj".to_string(),
            name: "MyProj".to_string(),
            root_path: Some(repo.clone()),
            confidence: 0.9,
            source_tier: 1,
            zone: None,
            status: crate::context::project::ProjectStatus::Configured,
        };
        let state = project_world_state("Code.exe", &[], Utc::now(), &filter, Some(&current));
        assert_eq!(
            state.project_root.as_deref(),
            Some(repo.to_string_lossy().as_ref())
        );
        assert_eq!(state.project_name.as_deref(), Some("MyProj"));
        assert_eq!(state.git_head, None, "git facts belong to the collector");
        assert_eq!(state.branch, None);
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

    /// Task A8: the formerly-dead hooks are real now — freshness from
    /// `last_poll_at`, restart only on a sustained stall of an enabled
    /// loop, disabled-with-reason always healthy (spec §7).
    #[test]
    fn context_watch_health_freshness_and_restart() {
        let interval = Duration::from_secs(1);
        let now = Utc::now();

        // Booting: enabled, no poll yet → healthy, no restart.
        let health = ContextWatchHealth {
            enabled: true,
            ..ContextWatchHealth::default()
        };
        assert!(health.is_healthy(now, interval));
        assert!(!health.should_restart(now, interval));

        // Fresh poll → healthy.
        let health = ContextWatchHealth {
            enabled: true,
            last_poll_at: Some(now - chrono::Duration::seconds(2)),
            polls: 2,
            ..ContextWatchHealth::default()
        };
        assert!(health.is_healthy(now, interval));
        assert!(!health.should_restart(now, interval));

        // Stale beyond the freshness window (min 5 s) but under the
        // restart threshold (min 30 s) → unhealthy, no restart yet.
        let health = ContextWatchHealth {
            enabled: true,
            last_poll_at: Some(now - chrono::Duration::seconds(10)),
            polls: 10,
            ..ContextWatchHealth::default()
        };
        assert!(!health.is_healthy(now, interval));
        assert!(!health.should_restart(now, interval));

        // Sustained stall → restart.
        let health = ContextWatchHealth {
            enabled: true,
            last_poll_at: Some(now - chrono::Duration::seconds(60)),
            polls: 10,
            ..ContextWatchHealth::default()
        };
        assert!(!health.is_healthy(now, interval));
        assert!(health.should_restart(now, interval));

        // Disabled-with-reason: healthy, never restarts (spec §7).
        let health = ContextWatchHealth {
            enabled: false,
            disabled_reason: Some("paused by [privacy.toggles].pause_all".into()),
            last_poll_at: Some(now - chrono::Duration::seconds(600)),
            polls: 3,
        };
        assert!(health.is_healthy(now, interval));
        assert!(!health.should_restart(now, interval));
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
