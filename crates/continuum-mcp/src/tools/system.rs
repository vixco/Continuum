//! # System info tools (`mcp__continuum__system_*`)
//!
//! - [`current_time`] — ISO timestamp + timezone offset
//! - [`active_window`] — foreground window title and process name
//! - [`clipboard_get`] — Windows clipboard text read (best-effort)
//! - [`show_notification`] — Windows toast via `tauri-winrt-notification`
//!
//! Every function here is synchronous and side-effect-free except for
//! `clipboard_get` (opens the Windows clipboard) and `show_notification`
//! (posts a toast + consults an in-process rate limiter). Errors are swallowed
//! into `None`/empty values — the MCP layer decides how to surface them, and
//! per CLAUDE.md these tools must never crash.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{Local, Offset};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request/response types
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct EmptyRequest {}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CurrentTimeResponse {
    /// ISO-8601 with timezone offset, e.g. `2026-04-12T14:33:07+02:00`.
    pub iso8601: String,
    /// Timezone offset in minutes east of UTC (e.g. 120 for CEST).
    pub tz_offset_minutes: i32,
    /// Milliseconds since Unix epoch.
    pub epoch_ms: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ActiveWindowResponse {
    /// Window title (empty if no focused window).
    pub title: String,
    /// Process executable name (e.g. "Code.exe"). Empty if lookup failed.
    pub process_name: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ClipboardResponse {
    /// `None` if the clipboard is empty or holds non-text data.
    pub text: Option<String>,
}

// ---------------------------------------------------------------------------
// current_time
// ---------------------------------------------------------------------------

pub fn current_time() -> CurrentTimeResponse {
    let now = Local::now();
    CurrentTimeResponse {
        iso8601: now.to_rfc3339(),
        tz_offset_minutes: now.offset().fix().local_minus_utc() / 60,
        epoch_ms: now.timestamp_millis(),
    }
}

// ---------------------------------------------------------------------------
// active_window
// ---------------------------------------------------------------------------

pub fn active_window() -> ActiveWindowResponse {
    let (title, process_name) = continuum_core::senses::context::foreground_window();
    ActiveWindowResponse {
        title,
        process_name,
    }
}

// ---------------------------------------------------------------------------
// clipboard_get
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub fn clipboard_get() -> ClipboardResponse {
    ClipboardResponse {
        text: clipboard_win::get_text(),
    }
}

#[cfg(not(windows))]
pub fn clipboard_get() -> ClipboardResponse {
    ClipboardResponse { text: None }
}

#[cfg(windows)]
mod clipboard_win {
    use tracing::trace;
    use windows::Win32::Foundation::{HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    /// Windows CF_UNICODETEXT format identifier (hardcoded to avoid pulling in
    /// the full Ole feature gate for one constant).
    const CF_UNICODETEXT: u32 = 13;

    /// Best-effort clipboard read. Returns `None` if:
    /// - Another app holds the clipboard
    /// - The clipboard holds non-text data
    /// - Any Win32 call fails
    pub fn get_text() -> Option<String> {
        unsafe {
            // SAFETY: OpenClipboard takes an optional owner HWND; passing null
            // is documented as "associate with current task". Must be paired
            // with CloseClipboard in every return path below.
            if OpenClipboard(Some(HWND(std::ptr::null_mut()))).is_err() {
                trace!("clipboard: OpenClipboard failed");
                return None;
            }

            // GetClipboardData returns a borrowed HANDLE to the current data;
            // we do not free it.
            let handle = match GetClipboardData(CF_UNICODETEXT) {
                Ok(h) => h,
                Err(_) => {
                    let _ = CloseClipboard();
                    return None;
                }
            };

            // GlobalLock pins the memory and returns a pointer; pair with
            // GlobalUnlock before returning.
            let hglobal = HGLOBAL(handle.0);
            let ptr = GlobalLock(hglobal) as *const u16;
            if ptr.is_null() {
                let _ = CloseClipboard();
                return None;
            }

            // Walk to the null terminator, capped at 10 M wide chars to avoid
            // walking forever on corrupt handles.
            let mut len = 0usize;
            let max_chars = 10 * 1024 * 1024;
            while len < max_chars && *ptr.add(len) != 0 {
                len += 1;
            }

            let slice = std::slice::from_raw_parts(ptr, len);
            let text = String::from_utf16_lossy(slice);

            let _ = GlobalUnlock(hglobal);
            let _ = CloseClipboard();
            Some(text)
        }
    }
}

// ---------------------------------------------------------------------------
// show_notification
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct NotificationRequest {
    /// Title of the toast (truncated at 64 characters).
    pub title: String,
    /// Body of the toast (truncated at 200 characters).
    pub body: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct NotificationResponse {
    /// True if a toast was actually shown. False if the per-process rate
    /// limiter suppressed it (≥10s must elapse between notifications).
    pub shown: bool,
    /// Brief reason when `shown` is false.
    pub reason: Option<String>,
}

/// Minimum time between consecutive notifications from the same continuum-mcp
/// process. Prevents the orchestrator from spamming the tray.
const NOTIFICATION_RATE_LIMIT: Duration = Duration::from_secs(10);
const TITLE_MAX_CHARS: usize = 64;
const BODY_MAX_CHARS: usize = 200;

static LAST_NOTIFICATION: Mutex<Option<Instant>> = Mutex::new(None);

/// Safely truncates a string to at most `max` Unicode scalars without
/// cutting into a multi-byte character.
fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Posts a Windows toast. Returns `(shown, reason_if_not_shown)`.
pub fn show_notification(title: &str, body: &str) -> NotificationResponse {
    // Rate limit first — do NOT consume the slot if we're about to skip.
    {
        let guard = LAST_NOTIFICATION.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(prev) = *guard {
            if prev.elapsed() < NOTIFICATION_RATE_LIMIT {
                return NotificationResponse {
                    shown: false,
                    reason: Some("rate-limited: previous notification shown <10s ago".into()),
                };
            }
        }
    }

    let title = clip(title, TITLE_MAX_CHARS);
    let body = clip(body, BODY_MAX_CHARS);

    match toast_impl::post(&title, &body) {
        Ok(()) => {
            let mut guard = LAST_NOTIFICATION.lock().unwrap_or_else(|p| p.into_inner());
            *guard = Some(Instant::now());
            NotificationResponse {
                shown: true,
                reason: None,
            }
        }
        Err(e) => NotificationResponse {
            shown: false,
            reason: Some(format!("toast backend failed: {e}")),
        },
    }
}

#[cfg(windows)]
mod toast_impl {
    use tauri_winrt_notification::Toast;

    pub fn post(title: &str, body: &str) -> Result<(), String> {
        Toast::new(Toast::POWERSHELL_APP_ID)
            .title(title)
            .text1(body)
            .show()
            .map_err(|e| e.to_string())
    }
}

#[cfg(not(windows))]
mod toast_impl {
    pub fn post(_title: &str, _body: &str) -> Result<(), String> {
        // No-op on non-Windows — the MCP server is Windows-only at runtime,
        // but we keep the module compilable elsewhere for dev/CI.
        Err("notifications are only supported on Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_time_populates_fields() {
        let t = current_time();
        assert!(t.iso8601.contains('T'));
        assert!(t.epoch_ms > 1_700_000_000_000); // > 2023
    }

    #[test]
    fn active_window_does_not_panic() {
        // On CI there may be no desktop session; empty strings are OK.
        let _ = active_window();
    }

    #[test]
    fn clip_handles_multibyte_chars() {
        // "héllo" has 5 chars; clipping to 2 should yield "hé", not break on byte boundary.
        let c = clip("héllo", 2);
        assert_eq!(c, "hé");
    }

    #[test]
    fn clip_short_strings_are_unchanged() {
        assert_eq!(clip("hi", 10), "hi");
    }
}
