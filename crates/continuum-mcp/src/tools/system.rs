//! # System info tools (`mcp__continuum__system_*`)
//!
//! - [`current_time`] — ISO timestamp + timezone offset
//! - [`active_window`] — foreground window title and process name
//! - [`live_context`] — compact shared local world-state
//! - [`clipboard_get`] — Windows clipboard text read (best-effort)
//! - [`show_notification`] — Windows toast via `tauri-winrt-notification`
//!
//! Every function here is synchronous and side-effect-free except for
//! `clipboard_get` (opens the Windows clipboard) and `show_notification`
//! (posts a toast + consults an in-process rate limiter). Errors are swallowed
//! into `None`/empty values — the MCP layer decides how to surface them, and
//! per CLAUDE.md these tools must never crash.
//!
//! ## Privacy boundary (context engine spec §5.1, Task C2)
//!
//! The three **observation** tools — [`active_window`], [`clipboard_get`],
//! and [`live_context`] — used to read Windows state directly and hand it
//! to the orchestrator, which meant a zone the user configured
//! (`never_observe` on their password manager, `local_only` on private
//! browsing) was enforced only on the runtime's own frame loop and could be
//! side-stepped by calling the MCP tool instead. They now route every byte
//! they return through the same
//! [`PrivacyFilter`](continuum_core::senses::privacy::PrivacyFilter) the
//! runtime uses, built from the same `config.toml`.
//!
//! **This is content filtering, not a schema change.** Every request and
//! response type in this module is byte-identical to before: same tool
//! names, same field names, same types, same optionality. Non-negotiable #7
//! (never break the published MCP API) holds — a caller sees the same shape,
//! with private content replaced by the §4.1 sentinel/redaction literals.

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{Local, Offset};
use continuum_core::senses::live_context::REDACTED_LOCAL_ONLY;
use continuum_core::senses::privacy::{PrivacyFilter, Zone, EXCLUDED_PROCESS, EXCLUDED_TITLE};
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

#[derive(Debug, Serialize, JsonSchema)]
pub struct LiveContextResponse {
    /// False before the runtime has published its first snapshot.
    pub available: bool,
    /// True when the published snapshot is older than ten seconds.
    pub stale: bool,
    /// Compact source-attributed text for models without image input.
    pub compact: Option<String>,
    /// Full structured world-state when available.
    #[schemars(with = "Option<serde_json::Value>")]
    pub state: Option<continuum_core::senses::live_context::LiveWorldState>,
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

/// Foreground window title + process name, **zone-gated** (spec §5.1).
///
/// - `never_observe` → the §4.1 sentinel ([`EXCLUDED_PROCESS`] / empty
///   title). The real title is never returned, not even redacted: titles of
///   excluded windows are content.
/// - `local_only` → the process name survives (it is not window content)
///   and the title collapses to the redaction literal — this response is
///   cloud-bound, and `local_only` content never leaves the machine.
/// - `cloud_allowed` → the title is scrubbed as free text, the process name
///   as a path (an exe path can carry the username).
pub fn active_window(filter: &PrivacyFilter) -> ActiveWindowResponse {
    let (title, process_name) = continuum_core::senses::context::foreground_window();
    gate_active_window(filter, &process_name, &title)
}

/// The pure gating decision behind [`active_window`], split out so it can
/// be tested without a desktop session.
///
/// `pub` since Task C6: the safety-redaction harness
/// (`continuum-redaction-bench`) must push its secret corpus through this
/// exact decision, and the live foreground window of the machine running
/// the bench is not a corpus anyone can control.
pub fn gate_active_window(
    filter: &PrivacyFilter,
    process_name: &str,
    title: &str,
) -> ActiveWindowResponse {
    match filter.resolve_zone(process_name, title) {
        Zone::NeverObserve => ActiveWindowResponse {
            title: EXCLUDED_TITLE.to_string(),
            process_name: EXCLUDED_PROCESS.to_string(),
        },
        Zone::LocalOnly => ActiveWindowResponse {
            title: REDACTED_LOCAL_ONLY.to_string(),
            process_name: filter.scrub_path(process_name),
        },
        Zone::CloudAllowed => ActiveWindowResponse {
            title: filter.scrub_text(title),
            process_name: filter.scrub_path(process_name),
        },
    }
}

/// Read the runtime's atomically-published shared live world-state.
///
/// Both returned fields are privacy-gated (spec §5.1): the structured
/// `state` goes through
/// [`LiveWorldState::cloud_view`](continuum_core::senses::live_context::LiveWorldState::cloud_view)
/// — the egress-point cloud gate — and `compact` is rendered **from the
/// gated state**, so the two can never disagree. Schema unchanged.
pub fn live_context(
    data_dir: &Path,
    filter: &PrivacyFilter,
) -> Result<LiveContextResponse, String> {
    let path = data_dir.join("live-context.json");
    if !path.exists() {
        return Ok(LiveContextResponse {
            available: false,
            stale: false,
            compact: None,
            state: None,
        });
    }
    let metadata = fs::metadata(&path).map_err(|error| error.to_string())?;
    if metadata.len() > 2 * 1024 * 1024 {
        return Err("live-context.json exceeds the 2 MiB safety cap".into());
    }
    let contents = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let state: continuum_core::senses::live_context::LiveWorldState =
        serde_json::from_str(&contents).map_err(|error| error.to_string())?;
    let stale = chrono::Utc::now()
        .signed_duration_since(state.generated_at)
        .num_seconds()
        > 10;
    let state = state.cloud_view(filter);
    let compact = Some(state.compact_for_agents(4_000));
    Ok(LiveContextResponse {
        available: true,
        stale,
        compact,
        state: Some(state),
    })
}

// ---------------------------------------------------------------------------
// clipboard_get
// ---------------------------------------------------------------------------

/// Best-effort clipboard read, gated three ways (spec §5.1):
///
/// 1. `enabled` is the `[context_tools] clipboard_tool_enabled` kill-switch.
///    When false the clipboard is **never opened** — nothing can leak even
///    in scrubbed form — and the tool answers `text: None`, which is
///    already its documented "nothing to return" answer.
/// 2. The foreground zone gates it. Reading the clipboard while a
///    `never_observe` window is focused is the exact zone bypass §5.1
///    exists to close: the user excluded that app, and the clipboard is
///    very likely holding what they just copied out of it.
/// 3. Whatever survives is scrubbed as free text, always.
pub fn clipboard_get(filter: &PrivacyFilter, enabled: bool) -> ClipboardResponse {
    if !enabled {
        return ClipboardResponse { text: None };
    }
    let (title, process_name) = continuum_core::senses::context::foreground_window();
    let zone = filter.resolve_zone(&process_name, &title);
    if zone == Zone::NeverObserve {
        return ClipboardResponse { text: None };
    }
    gate_clipboard(filter, read_clipboard_text())
}

/// The pure scrub behind [`clipboard_get`], split out so it can be tested
/// without touching the Windows clipboard — and, since Task C6, so the
/// safety-redaction harness can feed it the secret corpus.
pub fn gate_clipboard(filter: &PrivacyFilter, text: Option<String>) -> ClipboardResponse {
    ClipboardResponse {
        text: text.map(|text| filter.scrub_text(&text)),
    }
}

#[cfg(windows)]
fn read_clipboard_text() -> Option<String> {
    clipboard_win::get_text()
}

#[cfg(not(windows))]
fn read_clipboard_text() -> Option<String> {
    None
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
    use continuum_core::config::{ContextConfig, PrivacyConfig};
    use continuum_core::senses::privacy::{ZoneRule, REDACTED};

    fn test_filter() -> PrivacyFilter {
        PrivacyFilter::from_config(&ContextConfig::default(), &PrivacyConfig::default())
            .with_environment(
                Some("C:\\Users\\testuser".to_string()),
                Some("testuser".to_string()),
            )
    }

    #[test]
    fn current_time_populates_fields() {
        let t = current_time();
        assert!(t.iso8601.contains('T'));
        assert!(t.epoch_ms > 1_700_000_000_000); // > 2023
    }

    #[test]
    fn active_window_does_not_panic() {
        // On CI there may be no desktop session; empty strings are OK.
        let _ = active_window(&test_filter());
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

    #[test]
    fn live_context_is_unavailable_before_runtime_publish() {
        let dir = tempfile::tempdir().expect("temp dir");
        let response = live_context(dir.path(), &test_filter()).expect("unavailable response");
        assert!(!response.available);
        assert!(response.state.is_none());
    }

    #[test]
    fn live_context_round_trips_the_shared_projection() {
        let dir = tempfile::tempdir().expect("temp dir");
        let hub = continuum_core::senses::live_context::LiveContextHub::default();
        continuum_core::senses::live_context::write_snapshot(
            &dir.path().join("live-context.json"),
            &hub.snapshot(),
        )
        .expect("publish live context");
        let response = live_context(dir.path(), &test_filter()).expect("read live context");
        assert!(response.available);
        assert!(!response.stale);
        assert!(response.compact.as_deref().is_some_and(|text| {
            text.starts_with(&format!(
                "live-context/v{}",
                continuum_core::senses::live_context::LIVE_CONTEXT_SCHEMA_VERSION
            ))
        }));
    }

    // -----------------------------------------------------------------------
    // §5.1 privacy retrofit (Task C2)
    // -----------------------------------------------------------------------

    #[test]
    fn active_window_returns_the_sentinel_for_a_never_observe_zone() {
        let filter = test_filter();
        // 1password.exe is a legacy `[context].sensitive_process_names`
        // entry, synthesised into a never_observe zone rule.
        let response = gate_active_window(&filter, "1password.exe", "Personal — Bank login");
        assert_eq!(response.process_name, EXCLUDED_PROCESS);
        assert_eq!(response.title, EXCLUDED_TITLE);
        assert!(
            !response.title.contains("Bank"),
            "an excluded window title must never be returned, not even redacted"
        );
    }

    #[test]
    fn active_window_redacts_a_local_only_title_but_keeps_the_process() {
        let filter = test_filter();
        let response = gate_active_window(&filter, "msedge.exe", "Health results - InPrivate");
        assert_eq!(response.process_name, "msedge.exe");
        assert_eq!(response.title, REDACTED_LOCAL_ONLY);
        assert!(!response.title.contains("Health results"));
    }

    #[test]
    fn active_window_scrubs_secrets_out_of_an_ordinary_title() {
        let filter = test_filter();
        let response = gate_active_window(
            &filter,
            "C:\\Users\\testuser\\bin\\code.exe",
            "deploy.sh — sk-ant-api03-AbCdEf0123456789AbCdEf0123456789",
        );
        assert!(
            !response.title.contains("sk-ant-api03"),
            "secret survived in the window title: {}",
            response.title
        );
        assert!(response.title.contains(REDACTED));
        assert!(response.title.starts_with("deploy.sh"));
        assert_eq!(
            response.process_name, "~\\bin\\code.exe",
            "the exe path must be username-scrubbed"
        );
    }

    #[test]
    fn active_window_honors_an_explicitly_configured_zone() {
        // A user-configured `[privacy].zones` rule must bind the MCP tool
        // exactly like it binds the runtime's frame loop — that equivalence
        // is the whole point of the §5.1 retrofit.
        let privacy = PrivacyConfig {
            zones: vec![ZoneRule {
                match_process: Some("notepad.exe".to_string()),
                match_title_keyword: None,
                zone: continuum_core::senses::privacy::Zone::NeverObserve,
            }],
            ..PrivacyConfig::default()
        };
        let filter = PrivacyFilter::from_config(&ContextConfig::default(), &privacy);
        let response = gate_active_window(&filter, "notepad.exe", "diary.txt");
        assert_eq!(response.process_name, EXCLUDED_PROCESS);
        assert_eq!(response.title, EXCLUDED_TITLE);
    }

    #[test]
    fn clipboard_text_is_always_scrubbed() {
        let filter = test_filter();
        let response = gate_clipboard(
            &filter,
            Some("token ghp_AbCd1234EfGh5678IjKl9012MnOp3456QrSt for the deploy".to_string()),
        );
        let text = response.text.expect("clipboard text");
        assert!(!text.contains("ghp_"), "clipboard leaked a token: {text}");
        assert!(text.contains(REDACTED));
        // Non-secret content survives — this is filtering, not blanking.
        assert!(text.contains("for the deploy"));
    }

    #[test]
    fn clipboard_kill_switch_returns_none_without_reading_the_clipboard() {
        let response = clipboard_get(&test_filter(), false);
        assert!(
            response.text.is_none(),
            "[context_tools] clipboard_tool_enabled = false must yield text: null"
        );
    }

    #[test]
    fn clipboard_empty_stays_none() {
        assert!(gate_clipboard(&test_filter(), None).text.is_none());
    }

    #[test]
    fn live_context_filters_both_compact_and_state() {
        use continuum_core::senses::live_context::{
            write_snapshot, LiveWorldState, PrivacyDisposition, WindowWorldState,
        };

        let dir = tempfile::tempdir().expect("temp dir");
        let hub = continuum_core::senses::live_context::LiveContextHub::default();
        let mut snapshot: LiveWorldState = hub.snapshot();
        snapshot.window = Some(WindowWorldState {
            process_name: "code.exe".into(),
            title: "deploy — sk-ant-api03-AbCdEf0123456789AbCdEf0123456789".into(),
            observed_at: chrono::Utc::now(),
            in_call: false,
            privacy: PrivacyDisposition::Visible,
            pid: Some(1234),
            exe_path: Some("C:\\Users\\testuser\\bin\\code.exe".into()),
            monitor_id: Some("display-1".into()),
            active_since_secs: 12,
        });
        snapshot.health.last_error =
            Some("upload failed with Bearer abcdefghij1234567890XYZ".to_string());
        write_snapshot(&dir.path().join("live-context.json"), &snapshot)
            .expect("publish live context");

        let response = live_context(dir.path(), &test_filter()).expect("read live context");
        assert!(response.available);

        // BOTH fields are gated (spec §5.1 says so explicitly).
        let compact = response.compact.expect("compact");
        let state = serde_json::to_string(&response.state.expect("state")).expect("serialize");
        for secret in ["sk-ant-api03", "abcdefghij1234567890XYZ"] {
            assert!(!compact.contains(secret), "compact leaked {secret}");
            assert!(!state.contains(secret), "state leaked {secret}");
        }
    }

    #[test]
    fn live_context_applies_the_window_sentinel_in_the_state_field() {
        use continuum_core::senses::live_context::{
            write_snapshot, LiveWorldState, PrivacyDisposition, WindowWorldState,
        };

        let dir = tempfile::tempdir().expect("temp dir");
        let hub = continuum_core::senses::live_context::LiveContextHub::default();
        let mut snapshot: LiveWorldState = hub.snapshot();
        snapshot.window = Some(WindowWorldState {
            process_name: "1password.exe".into(),
            title: "Personal Vault".into(),
            observed_at: chrono::Utc::now(),
            in_call: true,
            privacy: PrivacyDisposition::Visible,
            pid: Some(9001),
            exe_path: Some("C:\\Program Files\\1Password\\1password.exe".into()),
            monitor_id: Some("display-2".into()),
            active_since_secs: 30,
        });
        write_snapshot(&dir.path().join("live-context.json"), &snapshot)
            .expect("publish live context");

        let response = live_context(dir.path(), &test_filter()).expect("read live context");
        let window = response
            .state
            .expect("state")
            .window
            .expect("window entry present");
        assert_eq!(window.process_name, EXCLUDED_PROCESS);
        assert_eq!(window.title, EXCLUDED_TITLE);
        assert!(!window.in_call);
        // Schema v4 enrichment: pid/exe path/monitor ARE the identity and
        // placement of the excluded app — the sentinel carries none of it.
        assert!(window.pid.is_none());
        assert!(window.exe_path.is_none());
        assert!(window.monitor_id.is_none());
        assert_eq!(window.active_since_secs, 0);
    }

    /// Non-negotiable #7: the retrofit changes **content**, never shape.
    /// This pins the exact serialized field set of all three retrofitted
    /// responses — a rename or an added field fails here before it can
    /// reach a released tool schema.
    #[test]
    fn retrofitted_response_schemas_are_unchanged() {
        /// Serialized field names, sorted (serde_json orders map keys, so
        /// this is a set comparison, not a field-order one).
        fn keys(value: &serde_json::Value) -> Vec<String> {
            let mut keys: Vec<String> = value
                .as_object()
                .expect("response serializes to an object")
                .keys()
                .cloned()
                .collect();
            keys.sort();
            keys
        }

        let filter = test_filter();
        let active = serde_json::to_value(gate_active_window(&filter, "code.exe", "main.rs"))
            .expect("serialize active_window");
        assert_eq!(
            keys(&active),
            vec!["process_name", "title"],
            "system_active_window response schema changed"
        );

        let clipboard = serde_json::to_value(gate_clipboard(&filter, Some("hi".into())))
            .expect("serialize clipboard");
        assert_eq!(
            keys(&clipboard),
            vec!["text"],
            "system_clipboard_get response schema changed"
        );

        let dir = tempfile::tempdir().expect("temp dir");
        let live = serde_json::to_value(
            live_context(dir.path(), &filter).expect("unavailable live context"),
        )
        .expect("serialize live context");
        assert_eq!(
            keys(&live),
            vec!["available", "compact", "stale", "state"],
            "system_live_context response schema changed"
        );
    }
}
