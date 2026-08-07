//! # Context tools — published-state family (`mcp__continuum__context_*`)
//!
//! Context engine spec §5.2, Task C3. The maintainer's headline
//! requirement for the context engine is that **every context source is
//! also a tool**: whatever the runtime observes and publishes, the
//! orchestrator can ask for on demand instead of waiting for it to be
//! packaged into a wake.
//!
//! This module holds the five *published-state* tools — the ones whose
//! source is a file the runtime already writes, or the Projects table:
//!
//! | Tool | Source |
//! |---|---|
//! | [`session`] | `state.json` → `session_state` (Task C1) |
//! | [`window`] | `live-context.json` + `context_events` (focus switches) |
//! | [`screen`] | `live-context.json` monitors + compact render |
//! | [`audio`] | `state.json` voice telemetry + `live-context.json` call flag |
//! | [`projects`] | `projects` table, opened read-only |
//!
//! and the five *events/git/package* tools Task C4 added, which share the
//! same [`Gate`] and the same readers:
//!
//! | Tool | Source |
//! |---|---|
//! | [`timeline`] | `context_events`, filtered (read-only) |
//! | [`search`] | `context_events_fts` (read-only) |
//! | [`files`] | `context_events` restricted to the file collector |
//! | [`git`] | `live-context.json`, or an on-demand bounded probe of a **confirmed** named project |
//! | [`package`] | the §4.9 `ContextPackage`, mcp-published profile |
//!
//! ## Two extra rules the event-shaped tools obey
//!
//! 5. **`local_only` rows never leave.** Spec §4.1 says `local_only`
//!    content is "stripped from everything cloud-bound", and an MCP tool
//!    response is cloud-bound by destination. Those rows are therefore
//!    omitted entirely — but the count is reported as `omitted_private`,
//!    so the orchestrator can tell "nothing happened" from "something
//!    happened that you may not see" without learning what it was.
//! 6. **A source whose toggle is off is not replayed.** The honest-toggle
//!    rule (§4.1) applies to the log too: with `mic = false` the audio
//!    classifier's rows stop being returned, exactly as `context_audio`
//!    stops returning transcripts. The rows stay on disk (they are the
//!    user's local memory); they just stop being an egress path.
//!
//! ## The four rules every tool in this family obeys (spec §5.2 preamble)
//!
//! 1. **Read-only.** Nothing here writes anything, anywhere. The SQLite
//!    handle comes from [`RawLog::open_read_only`] (`PRAGMA
//!    query_only = ON`, no DDL) — the runtime stays the single writer.
//! 2. **Privacy-gated.** Every response passes the same cloud gate the
//!    §5.1 retrofit put in front of the existing observation tools:
//!    [`LiveWorldState::cloud_view`] for anything derived from
//!    `live-context.json`, [`gate_session_state`] for the raw session
//!    state `state.json` publishes, and `scrub_text`/`scrub_path` for
//!    any free text this module assembles itself.
//! 3. **Degrade, never fail.** A missing file, an unparseable snapshot, a
//!    database that does not exist yet, a runtime that is not running —
//!    all of these answer `{available: false, …}` or `{stale: true, …}`
//!    with empty data. None of them returns an error: an error here would
//!    surface as a failed tool call in the middle of a wake, and a wake
//!    must never die because a context source was cold.
//! 4. **Switchable.** `[context_tools] enabled = false` turns the whole
//!    family into empty answers ([`Gate::disabled`]), and the per-source
//!    observation toggles (`[privacy.toggles]`, spec §4.1 "honest
//!    toggles") do the same per tool: a user who turned the microphone
//!    off must not get transcripts back from a tool just because the
//!    runtime published one before the toggle flipped.
//!
//! ## Staleness
//!
//! Two clocks, because the two published files are written on different
//! rules:
//!
//! - `live-context.json` is **content-versioned** (spec §4.11): the
//!   publisher skips the write when nothing meaningful changed. Its
//!   `generated_at` therefore ages on a genuinely static screen, and
//!   [`LIVE_CONTEXT_STALE_AFTER_SECS`] (10 s — the same threshold the
//!   existing `system_live_context` tool uses) means "this snapshot is
//!   old", not necessarily "the runtime is dead".
//! - `state.json` is written on a **fixed 2 s tick** regardless of
//!   content, so [`RUNTIME_STATE_STALE_AFTER_SECS`] (30 s — fifteen
//!   missed ticks) genuinely does mean "the runtime is not publishing".

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use continuum_core::config::{ContextPackageConfig, GitContextConfig, ObservationToggles};
use continuum_core::context::package::{
    ContextPackage, CurrentMoment, DropStep, MemoryLine, PackageBudget, PackageSection,
    SessionSection, VaultNoteLine,
};
use continuum_core::context::project::ProjectStatus;
use continuum_core::memory::events::{
    event_enum_token, parse_event_enum, EventSensitivity, EventSource, EventType,
};
use continuum_core::memory::raw_log::{
    ContextEventRow, EventQuery, RawLog, RawLogError, EVENT_QUERY_MAX_LIMIT, EVENT_SEARCH_MAX_LIMIT,
};
use continuum_core::orchestrator::wake_context::split_event_sections;
use continuum_core::runtime_publish::RuntimeSnapshot;
use continuum_core::senses::git_watch::{probe_last_commit, probe_repo_status};
use continuum_core::senses::live_context::{
    gate_session_state, LiveWorldState, PrivacyDisposition,
};
use continuum_core::senses::privacy::{
    source_enabled, ObservedSource, PrivacyFilter, Zone, EXCLUDED_PROCESS,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Age after which a `live-context.json` snapshot is reported `stale`.
/// Matches `system_live_context` so the two never disagree.
pub const LIVE_CONTEXT_STALE_AFTER_SECS: i64 = 10;

/// Age after which a `state.json` snapshot is reported `stale`. The
/// runtime republishes every 2 s, so this is fifteen missed ticks.
pub const RUNTIME_STATE_STALE_AFTER_SECS: i64 = 30;

/// Safety cap on a published JSON file we are willing to parse.
const MAX_PUBLISHED_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Default number of focus switches `context_window` returns.
pub const WINDOW_SWITCH_DEFAULT_LIMIT: u32 = 10;

/// Hard cap on `context_window`'s `limit`.
pub const WINDOW_SWITCH_MAX_LIMIT: u32 = 50;

/// Default number of events `context_timeline` returns.
pub const TIMELINE_DEFAULT_LIMIT: u32 = 50;

/// Default number of hits `context_search` returns.
pub const SEARCH_DEFAULT_LIMIT: u32 = 20;

/// Default number of file events `context_files` returns.
pub const FILES_DEFAULT_LIMIT: u32 = 20;

/// Hard cap on `context_files`' `limit`.
pub const FILES_MAX_LIMIT: u32 = 100;

/// Lower bound on `context_package`'s `token_budget` — below this the
/// never-drop sections alone blow the budget and the answer is noise.
pub const PACKAGE_MIN_TOKEN_BUDGET: u32 = 200;

/// Upper bound on `context_package`'s `token_budget`.
pub const PACKAGE_MAX_TOKEN_BUDGET: u32 = 8_000;

/// How many rows `context_package` over-fetches before privacy filtering,
/// so that dropping `local_only` rows does not silently shrink a section
/// below its configured cap.
const PACKAGE_EVENT_OVERFETCH: usize = 4;

// ---------------------------------------------------------------------------
// Package section names (spec §4.9 per-consumer matrix)
// ---------------------------------------------------------------------------

/// Stable section tokens `context_package` reports in `sections_present` /
/// `sections_omitted`. They name the [`ContextPackage`] fields, not the
/// rendered headings, so a heading reword never breaks a caller.
const SECTION_CURRENT_MOMENT: &str = "current_moment";
const SECTION_SESSION: &str = "session";
const SECTION_JUST_BEFORE: &str = "just_before";
const SECTION_MEMORIES: &str = "memories";
const SECTION_VAULT_NOTES: &str = "vault_notes";
const SECTION_RECENT_CHANGES: &str = "recent_changes";
const SECTION_FAILED_ATTEMPTS: &str = "failed_attempts";
const SECTION_LAST_SUCCESS: &str = "last_success";

/// Sections this profile can never fill, in the order they are reported.
///
/// The first two are the spec §4.9 matrix's explicit omissions: there is
/// no wake to explain (`why_woken`) and no trigger frame to describe
/// (`trigger_frame_moment` — the live-context snapshot replaces it). The
/// rest are omissions of *fact*: the MCP process does not know which tools
/// the caller was granted, does not run the continuation resolver (the
/// runtime does, at wake time), and does not surface the vault's pending
/// decisions or the semantic fact set here — `memory_vault_search`,
/// `memory_vault_resolve` and `memory_list_facts` are the tools for those,
/// and duplicating them inside a budgeted package would just crowd out the
/// sections only this tool can provide.
const PROFILE_OMITTED_SECTIONS: [&str; 6] = [
    "why_woken",
    "trigger_frame_moment",
    "tools",
    "recommended_next_step",
    "pending_decisions",
    "facts",
];

// ---------------------------------------------------------------------------
// Shared gate
// ---------------------------------------------------------------------------

/// Everything a context tool needs to answer: where to read from, how to
/// filter what it read, and which switches are off.
///
/// Built once per call from the server state; passed by reference so the
/// handlers stay free functions that are testable without an MCP server.
pub struct Gate<'a> {
    /// Continuum data directory (`state.json`, `live-context.json`).
    pub data_dir: &'a Path,
    /// Path to the raw-log SQLite database (`[storage] db_path`).
    pub db_path: &'a Path,
    /// The §5.1 cloud gate.
    pub filter: &'a PrivacyFilter,
    /// `[privacy.toggles]` — per-source honest toggles (spec §4.1).
    pub toggles: &'a ObservationToggles,
    /// `[git_context]` — the enable switch and the subprocess timeout the
    /// `context_git` named-project probe honours (spec §4.4/§6).
    pub git_context: &'a GitContextConfig,
    /// `[context_package]` — the budget and per-section caps
    /// `context_package` renders with (spec §4.9/§6).
    pub package_config: &'a ContextPackageConfig,
    /// `[context_tools] enabled` — the family master switch (spec §5.2).
    pub enabled: bool,
}

impl Gate<'_> {
    /// Whether the family master switch is off. When it is, every tool
    /// answers as if nothing had ever been observed — same schema, empty
    /// content (non-negotiable #7: a switch never changes a shape).
    fn disabled(&self) -> bool {
        !self.enabled
    }

    /// Whether a given observation source may be reported at all.
    /// `pause_all` is folded in by [`source_enabled`].
    fn source_on(&self, source: ObservedSource) -> bool {
        source_enabled(self.toggles, source)
    }
}

// ---------------------------------------------------------------------------
// Shared readers
// ---------------------------------------------------------------------------

/// Reads `live-context.json`, applies the cloud gate, and reports
/// staleness. `None` means "nothing to report" — the file is absent,
/// oversized, or unparseable — never an error (rule 3).
pub fn read_live_context(
    data_dir: &Path,
    filter: &PrivacyFilter,
) -> Option<(LiveWorldState, bool)> {
    let state: LiveWorldState = read_published_json(&data_dir.join("live-context.json"))?;
    let stale = age_secs(state.generated_at) > LIVE_CONTEXT_STALE_AFTER_SECS;
    Some((state.cloud_view(filter), stale))
}

/// Reads `state.json` (the [`RuntimeSnapshot`] contract) and reports
/// staleness from its `last_update` stamp, falling back to the file's
/// modification time when the stamp is missing or unparseable. `None`
/// means "nothing to report" (rule 3).
///
/// The snapshot is returned **ungated** — the publisher writes raw state
/// on purpose (Task C1) and each consumer owns its own egress point.
/// Callers must gate whatever they project out of it.
pub fn read_runtime_snapshot(data_dir: &Path) -> Option<(RuntimeSnapshot, bool)> {
    let path = data_dir.join("state.json");
    let snapshot: RuntimeSnapshot = read_published_json(&path)?;
    let published_at = DateTime::parse_from_rfc3339(&snapshot.last_update)
        .map(|ts| ts.with_timezone(&Utc))
        .ok()
        .or_else(|| file_modified_at(&path));
    // No usable stamp at all: say stale rather than imply freshness.
    let stale = published_at
        .map(|ts| age_secs(ts) > RUNTIME_STATE_STALE_AFTER_SECS)
        .unwrap_or(true);
    Some((snapshot, stale))
}

/// Opens the raw-log database read-only for the context-events reads.
/// [`RawLogError::NotYetCreated`] is the normal cold-start answer, not a
/// failure — callers turn it into an empty list.
pub async fn open_events_read_only(db_path: &Path) -> Result<RawLog, RawLogError> {
    RawLog::open_read_only(&db_path.to_string_lossy()).await
}

fn read_published_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_PUBLISHED_FILE_BYTES {
        tracing::warn!(
            layer = "mcp",
            component = "context_tools",
            path = %path.display(),
            bytes = metadata.len(),
            "Published state file exceeds the safety cap — reporting unavailable"
        );
        return None;
    }
    let contents = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&contents) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(
                layer = "mcp",
                component = "context_tools",
                path = %path.display(),
                error = %error,
                "Published state file did not parse — reporting unavailable"
            );
            None
        }
    }
}

fn file_modified_at(path: &Path) -> Option<DateTime<Utc>> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .map(DateTime::<Utc>::from)
}

fn age_secs(ts: DateTime<Utc>) -> i64 {
    Utc::now().signed_duration_since(ts).num_seconds()
}

/// Stable snake_case token for a privacy disposition, used as the `zone`
/// marker on monitors and windows.
fn zone_token(disposition: PrivacyDisposition) -> &'static str {
    match disposition {
        PrivacyDisposition::Visible => "cloud_allowed",
        PrivacyDisposition::Redacted => "local_only",
        PrivacyDisposition::Excluded => "never_observe",
    }
}

/// Re-gates one **process name** against the live zone rules (spec §5.1:
/// a rule added after the row was written must still bind). Mirrors the
/// `system_active_window` decision: a `never_observe` process collapses
/// to the sentinel, everything else survives path-scrubbed — a process
/// name is not window *content*.
fn gate_process(filter: &PrivacyFilter, process: &str) -> String {
    match filter.resolve_zone(process, "") {
        Zone::NeverObserve => EXCLUDED_PROCESS.to_string(),
        Zone::LocalOnly | Zone::CloudAllowed => filter.scrub_path(process),
    }
}

// ---------------------------------------------------------------------------
// context_session
// ---------------------------------------------------------------------------

/// A stamped free-text field of the session state.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StampedTextView {
    /// The text, cloud-gated.
    pub text: String,
    /// When it was recorded.
    pub at: DateTime<Utc>,
}

/// The cloud-gated projection of Continuum's live session state
/// (spec §4.8).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionStateView {
    /// Resolved current project id, or null when no project is resolved.
    pub project: Option<String>,
    /// Inferred larger goal; null when unknown or below the confidence
    /// floor. Generalized when the session is `local_only`.
    pub goal: Option<String>,
    /// Inferred concrete task; same semantics as `goal`.
    pub task: Option<String>,
    /// Foreground process name at the last observation.
    pub app: Option<String>,
    /// Foreground window title at the last observation.
    pub window_title: Option<String>,
    /// Best-effort recently-touched files, most recent first. Never
    /// authoritative — derived from editor titles and file events.
    pub open_files: Vec<String>,
    /// Most recent observed error.
    pub last_error: Option<StampedTextView>,
    /// Most recent observed success.
    pub last_success: Option<StampedTextView>,
    /// Most recent thing the user actually asked for.
    pub last_user_command: Option<StampedTextView>,
    /// Confidence in `goal`/`task`, in [0.0, 1.0].
    pub confidence: f32,
    /// True when the inference window contained `local_only` content, so
    /// `goal`/`task` above are generalized rather than verbatim.
    pub local_only: bool,
    /// When the current session began (boot, or the last project change).
    pub since: DateTime<Utc>,
    /// When any field last changed.
    pub updated: DateTime<Utc>,
}

/// `context_session` response.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ContextSessionResponse {
    /// False when the runtime has never published a session state, or the
    /// tools/observation are switched off.
    pub available: bool,
    /// True when `state.json` is older than 30 s (the runtime is probably
    /// not running).
    pub stale: bool,
    /// The session state, cloud-gated.
    pub session: Option<SessionStateView>,
}

/// Reads the runtime's live session state out of `state.json`.
pub fn session(gate: &Gate<'_>) -> ContextSessionResponse {
    // Window observation is the source every mechanical session field
    // comes from; `pause_all` turns it off.
    if gate.disabled() || !gate.source_on(ObservedSource::Window) {
        return ContextSessionResponse {
            available: false,
            stale: false,
            session: None,
        };
    }
    let Some((snapshot, stale)) = read_runtime_snapshot(gate.data_dir) else {
        return ContextSessionResponse {
            available: false,
            stale: false,
            session: None,
        };
    };
    let Some(raw) = snapshot.session_state else {
        return ContextSessionResponse {
            available: false,
            stale,
            session: None,
        };
    };
    // `state.json` carries the RAW state by contract (Task C1) — this is
    // the egress point, so the gate runs here.
    let view = gate_session_state(gate.filter, &raw);
    ContextSessionResponse {
        available: true,
        stale,
        session: Some(SessionStateView {
            project: view.active_project,
            goal: view.current_goal,
            task: view.current_task,
            app: view.active_app,
            window_title: view.window_title,
            open_files: view.open_files,
            last_error: view.last_error.map(stamped),
            last_success: view.last_success.map(stamped),
            last_user_command: view.last_user_command.map(stamped),
            confidence: view.confidence,
            local_only: view.local_only,
            since: view.since,
            updated: view.updated,
        }),
    }
}

fn stamped(value: continuum_core::context::session_state::StampedText) -> StampedTextView {
    StampedTextView {
        text: value.text,
        at: value.at,
    }
}

// ---------------------------------------------------------------------------
// context_window
// ---------------------------------------------------------------------------

/// `context_window` request.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct ContextWindowRequest {
    /// How many recent focus switches to return. Default 10, clamped to
    /// 50.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// The current foreground window, cloud-gated.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ActiveWindowView {
    /// Process name, or `[excluded]` for a `never_observe` window.
    pub process: String,
    /// Window title; empty for an excluded window, the redaction literal
    /// for a `local_only` one, secret-scrubbed otherwise.
    pub title: String,
    /// Zone this window resolved to: `cloud_allowed` | `local_only` |
    /// `never_observe`.
    pub zone: String,
    /// Whether the user appears to be in a call.
    pub in_call: bool,
    /// Owning process id; null for an excluded window or a failed lookup.
    pub pid: Option<u32>,
    /// Executable path (home/username-scrubbed); null for an excluded
    /// window or a failed lookup.
    pub exe_path: Option<String>,
    /// Monitor the window sits on (`display-N`); null when unknown.
    pub monitor_id: Option<String>,
    /// Seconds this window had been focused as of `observed_at`.
    pub active_since_secs: u64,
    /// When the foreground was last observed.
    pub observed_at: DateTime<Utc>,
}

/// One completed focus span.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WindowSwitchView {
    /// Process focus left.
    pub from_app: String,
    /// Process focus moved to.
    pub to_app: String,
    /// When the switch happened.
    pub at: DateTime<Utc>,
    /// How long the departed window had held focus.
    pub dwell_secs: u64,
}

/// `context_window` response.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ContextWindowResponse {
    /// False before the runtime publishes, or when observation is off.
    pub available: bool,
    /// True when the live-context snapshot is older than 10 s, or the
    /// event database is not readable yet.
    pub stale: bool,
    /// The current foreground window.
    pub active: Option<ActiveWindowView>,
    /// Recent focus switches, oldest first. Empty when the event
    /// database does not exist yet.
    pub recent_switches: Vec<WindowSwitchView>,
}

/// Reads the foreground window from `live-context.json` and recent focus
/// switches from `context_events`.
pub async fn window(gate: &Gate<'_>, request: &ContextWindowRequest) -> ContextWindowResponse {
    if gate.disabled() || !gate.source_on(ObservedSource::Window) {
        return ContextWindowResponse {
            available: false,
            stale: false,
            active: None,
            recent_switches: Vec::new(),
        };
    }
    let live = read_live_context(gate.data_dir, gate.filter);
    let limit = request
        .limit
        .unwrap_or(WINDOW_SWITCH_DEFAULT_LIMIT)
        .clamp(1, WINDOW_SWITCH_MAX_LIMIT);
    let (switches, events_stale) = read_focus_switches(gate, limit as usize).await;

    let Some((state, live_stale)) = live else {
        return ContextWindowResponse {
            available: false,
            stale: events_stale,
            active: None,
            recent_switches: switches,
        };
    };
    let active = state.window.map(|window| ActiveWindowView {
        process: window.process_name,
        title: window.title,
        zone: zone_token(window.privacy).to_string(),
        in_call: window.in_call,
        pid: window.pid,
        exe_path: window.exe_path,
        monitor_id: window.monitor_id,
        active_since_secs: window.active_since_secs,
        observed_at: window.observed_at,
    });
    ContextWindowResponse {
        available: true,
        stale: live_stale || events_stale,
        active,
        recent_switches: switches,
    }
}

/// Reads `focus_switch` rows from `context_events` over a read-only
/// handle. Returns `(switches, stale)`; a database that does not exist
/// yet is `(vec![], true)` per the spec §5.2 preamble.
async fn read_focus_switches(gate: &Gate<'_>, limit: usize) -> (Vec<WindowSwitchView>, bool) {
    let log = match open_events_read_only(gate.db_path).await {
        Ok(log) => log,
        Err(RawLogError::NotYetCreated { .. }) => return (Vec::new(), true),
        Err(error) => {
            tracing::warn!(
                layer = "mcp",
                component = "context_tools",
                error = %error,
                "Read-only open of the raw log failed — reporting no focus switches"
            );
            return (Vec::new(), true);
        }
    };
    let query = EventQuery {
        types: vec![EventType::FocusSwitch],
        source: Some(EventSource::Window),
        limit,
        ..EventQuery::default()
    };
    let rows = log.query_context_events(&query).await;
    log.close().await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                layer = "mcp",
                component = "context_tools",
                error = %error,
                "context_events query failed — reporting no focus switches"
            );
            return (Vec::new(), true);
        }
    };
    let switches = rows
        .iter()
        .filter_map(|row| {
            let (from_app, to_app, dwell_secs) = parse_focus_switch_summary(&row.summary)?;
            // The row's destination column is authoritative; the summary
            // is the only place the *source* app is recorded.
            let to_app = if row.application.is_empty() {
                to_app
            } else {
                row.application.clone()
            };
            // Spec §4.1 propagation: a switch touching a `local_only` or
            // excluded endpoint is stored with `local_only` sensitivity,
            // and its excluded endpoints were already collapsed to the
            // `[excluded]` bucket at emit. Both endpoints are re-gated
            // here against the *live* rules (a rule added since the row
            // was written still binds), which keeps the same call
            // `system_active_window` makes: a process name is not window
            // content. The row itself is never dropped — "you switched
            // away from X after N seconds" is exactly the timing the
            // continuation resolver reasons over.
            Some(WindowSwitchView {
                from_app: gate_process(gate.filter, &from_app),
                to_app: gate_process(gate.filter, &to_app),
                at: row.ts_last,
                dwell_secs,
            })
        })
        .collect();
    (switches, false)
}

/// Parses the stable `focus_switch` summary template emitted by the
/// context watcher: `"<from_app> → <to_app> after <dwell>s"`. Returns
/// `None` for anything that does not match, so a future template change
/// degrades to "no switches" instead of to garbage.
fn parse_focus_switch_summary(summary: &str) -> Option<(String, String, u64)> {
    let (from_app, rest) = summary.split_once(" → ")?;
    let (to_app, dwell) = rest.rsplit_once(" after ")?;
    let dwell_secs = dwell.strip_suffix('s')?.parse::<u64>().ok()?;
    if from_app.is_empty() || to_app.is_empty() {
        return None;
    }
    Some((from_app.to_string(), to_app.to_string(), dwell_secs))
}

// ---------------------------------------------------------------------------
// context_screen
// ---------------------------------------------------------------------------

/// One monitor's latest local-vision caption, cloud-gated.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorView {
    /// Hub monitor id (`display-N`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Whether this is the primary monitor.
    pub primary: bool,
    /// The latest local vision summary. Empty string for a
    /// `never_observe` monitor (sentinel semantics: no caption at all,
    /// not even a redaction marker); the redaction literal for a
    /// `local_only` one.
    pub caption: String,
    /// Zone this monitor resolved to: `cloud_allowed` | `local_only` |
    /// `never_observe`.
    pub zone: String,
}

/// `context_screen` response.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ContextScreenResponse {
    /// False before the runtime publishes, or when screen observation is
    /// switched off.
    pub available: bool,
    /// True when the live-context snapshot is older than 10 s.
    pub stale: bool,
    /// Every connected monitor, including excluded ones (present with an
    /// empty caption and a `never_observe` zone marker, so the caller can
    /// see that a screen exists and is deliberately not described).
    pub monitors: Vec<MonitorView>,
    /// The same compact, source-attributed text `system_live_context`
    /// returns, rendered from the gated state.
    pub world_compact: Option<String>,
}

/// Reads per-monitor captions and the compact world render from
/// `live-context.json`.
pub fn screen(gate: &Gate<'_>) -> ContextScreenResponse {
    if gate.disabled() || !gate.source_on(ObservedSource::Screen) {
        return ContextScreenResponse {
            available: false,
            stale: false,
            monitors: Vec::new(),
            world_compact: None,
        };
    }
    let Some((state, stale)) = read_live_context(gate.data_dir, gate.filter) else {
        return ContextScreenResponse {
            available: false,
            stale: false,
            monitors: Vec::new(),
            world_compact: None,
        };
    };
    let world_compact = Some(state.compact_for_agents(4_000));
    let monitors = state
        .monitors
        .into_iter()
        .map(|monitor| MonitorView {
            id: monitor.monitor_id,
            name: monitor.name,
            primary: monitor.is_primary,
            caption: monitor.description,
            zone: zone_token(monitor.privacy).to_string(),
        })
        .collect();
    ContextScreenResponse {
        available: true,
        stale,
        monitors,
        world_compact,
    }
}

// ---------------------------------------------------------------------------
// context_audio
// ---------------------------------------------------------------------------

/// `context_audio` response.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ContextAudioResponse {
    /// False when the runtime is not publishing, or the microphone
    /// toggle (`[privacy.toggles] mic`, or `pause_all`) is off.
    pub available: bool,
    /// True when `state.json` is older than 30 s.
    pub stale: bool,
    /// The most recent transcript the voice pipeline published, scrubbed.
    /// Null when nothing has been heard.
    pub transcript: Option<String>,
    /// When that transcript was published.
    pub at: Option<DateTime<Utc>>,
    /// Whether the user appears to be in a call (from the foreground
    /// window observation).
    pub in_call: bool,
    /// Whether ambient mute is currently suppressing voice output.
    pub muted: bool,
}

/// Reads the voice pipeline's published transcript + call/mute state.
pub fn audio(gate: &Gate<'_>) -> ContextAudioResponse {
    if gate.disabled() || !gate.source_on(ObservedSource::Mic) {
        return ContextAudioResponse {
            available: false,
            stale: false,
            transcript: None,
            at: None,
            in_call: false,
            muted: false,
        };
    }
    let Some((snapshot, stale)) = read_runtime_snapshot(gate.data_dir) else {
        return ContextAudioResponse {
            available: false,
            stale: false,
            transcript: None,
            at: None,
            in_call: false,
            muted: false,
        };
    };
    // Transcripts arrive scrubbed from the voice pipeline; scrubbing
    // again is idempotent by PrivacyFilter's contract and keeps this
    // egress point self-sufficient.
    let transcript = snapshot
        .partial_transcript
        .as_deref()
        .map(|text| gate.filter.scrub_text(text))
        .filter(|text| !text.trim().is_empty());
    let at = transcript.as_ref().and_then(|_| {
        DateTime::parse_from_rfc3339(&snapshot.last_update)
            .ok()
            .map(|ts| ts.with_timezone(&Utc))
    });
    // `in_call` comes from the (gated) foreground observation — an
    // excluded window reports false by construction.
    let in_call = read_live_context(gate.data_dir, gate.filter)
        .and_then(|(state, _)| state.window.map(|window| window.in_call))
        .unwrap_or(false);
    ContextAudioResponse {
        available: true,
        stale,
        transcript,
        at,
        in_call,
        muted: snapshot.ambient_mute_active.unwrap_or(false),
    }
}

// ---------------------------------------------------------------------------
// context_projects
// ---------------------------------------------------------------------------

/// One row of the Projects table (spec §4.3).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProjectView {
    /// Slug id.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// `configured` (from `[[projects.known]]`), `discovered` (an
    /// auto-discovery proposal that never participates in resolution
    /// until confirmed), or `confirmed`.
    pub status: String,
    /// Absolute root directories, home/username-scrubbed.
    pub root_paths: Vec<String>,
    /// Per-project privacy zone, when one is configured.
    pub zone: Option<String>,
    /// Whether this is the session's currently resolved project.
    pub active: bool,
    /// Last time the resolver attributed a frame to this project.
    pub last_active_ts: Option<String>,
}

/// `context_projects` response.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ContextProjectsResponse {
    /// False when the projects database does not exist yet (the runtime
    /// has never booted against this data directory) or the tools are
    /// switched off.
    pub available: bool,
    /// Configured and confirmed projects first, then discovered
    /// proposals; the active project is first within its group.
    pub projects: Vec<ProjectView>,
}

/// Reads the Projects table read-only and marks the active project from
/// the published session state.
pub async fn projects(gate: &Gate<'_>) -> ContextProjectsResponse {
    if gate.disabled() {
        return ContextProjectsResponse {
            available: false,
            projects: Vec::new(),
        };
    }
    let log = match open_events_read_only(gate.db_path).await {
        Ok(log) => log,
        Err(RawLogError::NotYetCreated { .. }) => {
            return ContextProjectsResponse {
                available: false,
                projects: Vec::new(),
            }
        }
        Err(error) => {
            tracing::warn!(
                layer = "mcp",
                component = "context_tools",
                error = %error,
                "Read-only open of the raw log failed — reporting no projects"
            );
            return ContextProjectsResponse {
                available: false,
                projects: Vec::new(),
            };
        }
    };
    let rows = log.list_projects().await;
    log.close().await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                layer = "mcp",
                component = "context_tools",
                error = %error,
                "projects query failed — reporting no projects"
            );
            return ContextProjectsResponse {
                available: false,
                projects: Vec::new(),
            };
        }
    };

    let active_id = read_runtime_snapshot(gate.data_dir)
        .and_then(|(snapshot, _)| snapshot.session_state)
        .and_then(|session| session.active_project);

    let mut projects: Vec<ProjectView> = rows
        .into_iter()
        .map(|row| {
            let active = active_id.as_deref() == Some(row.entry.id.as_str());
            ProjectView {
                id: row.entry.id,
                // Names are user-authored free text; roots are paths.
                name: gate.filter.scrub_text(&row.entry.name),
                status: row.entry.status.as_str().to_string(),
                root_paths: row
                    .entry
                    .root_paths
                    .iter()
                    .map(|path| gate.filter.scrub_path(path))
                    .collect(),
                zone: row
                    .entry
                    .zone
                    .map(|zone| project_zone_token(zone).to_string()),
                active,
                last_active_ts: row.last_active_ts,
            }
        })
        .collect();
    // Active first, then the resolver-participating rows, then the
    // discovery proposals — the order the Context page and the
    // orchestrator both want to read.
    projects.sort_by_key(|project| {
        (
            !project.active,
            project.status == "discovered",
            project.id.clone(),
        )
    });
    ContextProjectsResponse {
        available: true,
        projects,
    }
}

fn project_zone_token(zone: Zone) -> &'static str {
    match zone {
        Zone::NeverObserve => "never_observe",
        Zone::LocalOnly => "local_only",
        Zone::CloudAllowed => "cloud_allowed",
    }
}

// ===========================================================================
// Task C4 — the events / git / package half of the family
// ===========================================================================

// ---------------------------------------------------------------------------
// Shared event projection
// ---------------------------------------------------------------------------

/// One deduped `context_events` row, cloud-gated.
///
/// `count`/`ts_first`/`ts_last` are the §4.6 collapse bookkeeping: a
/// repeated failure is **one** row with `count: 14` spanning a window, not
/// fourteen lines. Read it that way — "build failed ×14 over 3 minutes" is
/// a much stronger signal than a single occurrence.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EventView {
    /// When the first collapsed occurrence happened.
    pub ts_first: DateTime<Utc>,
    /// When the most recent collapsed occurrence happened.
    pub ts_last: DateTime<Utc>,
    /// How many occurrences collapsed into this row.
    pub count: i64,
    /// Collector family: `window` | `git` | `file` | `screen` | `audio` |
    /// `system` | `voice`.
    pub source: String,
    /// What happened, as the closed-registry token (`focus_switch`,
    /// `commit`, `error`, …).
    pub event_type: String,
    /// Resolved project id, when one was known at write time.
    pub project: Option<String>,
    /// Application the event is about (path-scrubbed).
    pub application: String,
    /// Window title at the first occurrence (secret-scrubbed).
    pub window_title: String,
    /// One-line description (secret-scrubbed).
    pub summary: String,
    /// Importance in \[0.0, 1.0\].
    pub importance: f32,
    /// Confidence in \[0.0, 1.0\].
    pub confidence: f32,
}

/// The response shape shared by `context_timeline`, `context_search` and
/// `context_files` — one schema, so a caller that can read one can read
/// all three.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ContextEventsResponse {
    /// False when the event database does not exist yet, the tools are
    /// switched off, or the source this tool reads was toggled off.
    pub available: bool,
    /// True when the event database could not be read (cold runtime,
    /// unreadable file). Never an error — see the module docs, rule 3.
    pub stale: bool,
    /// Matching events, oldest first (`context_search` returns them
    /// best-match first instead).
    pub events: Vec<EventView>,
    /// How many matching rows were withheld because they are `local_only`
    /// (spec §4.1: stripped from everything cloud-bound) or because a live
    /// privacy rule now covers them. The count is deliberate: it tells the
    /// orchestrator that something exists without telling it what.
    pub omitted_private: u32,
}

impl ContextEventsResponse {
    /// The "switched off" answer: same schema, no content.
    fn off() -> Self {
        Self {
            available: false,
            stale: false,
            events: Vec::new(),
            omitted_private: 0,
        }
    }

    /// The "nothing to read yet" answer (spec §5.2: missing DB →
    /// `{stale: true, events: []}`).
    fn cold() -> Self {
        Self {
            available: false,
            stale: true,
            events: Vec::new(),
            omitted_private: 0,
        }
    }
}

/// The observation toggle that governs a collector family, or `None` for
/// families that only `pause_all` can silence.
fn toggle_for_source(source: EventSource) -> Option<ObservedSource> {
    match source {
        EventSource::Window => Some(ObservedSource::Window),
        EventSource::Git => Some(ObservedSource::Git),
        EventSource::File => Some(ObservedSource::Files),
        EventSource::Screen => Some(ObservedSource::Screen),
        // Voice rides the microphone: silencing the mic must silence the
        // replay of what it heard, not only the live transcript.
        EventSource::Audio | EventSource::Voice => Some(ObservedSource::Mic),
        // System rows are runtime bookkeeping (idle, wakes, toggle
        // changes, drops) — not an observation of the user. `pause_all`
        // still stops the whole family upstream.
        EventSource::System => None,
    }
}

/// Projects rows into cloud-gated [`EventView`]s, returning the views and
/// the number of rows withheld for privacy.
///
/// Three filters run, in this order:
///
/// 1. **Toggle.** A row from a source whose `[privacy.toggles]` switch is
///    off is dropped silently — it is not "private", it is switched off,
///    and counting it would leak that the source is producing.
/// 2. **Recorded sensitivity.** `EventSensitivity::LocalOnly` rows are
///    withheld and counted (rule 5).
/// 3. **Live zone rules.** The row's `(application, window_title)` pair is
///    re-resolved against the *current* `PrivacyFilter`, so a zone the
///    user added after the row was written still binds. Anything that no
///    longer resolves `cloud_allowed` is withheld and counted.
///
/// What survives is scrubbed: application as a path, title and summary as
/// free text.
fn event_views(gate: &Gate<'_>, rows: &[ContextEventRow]) -> (Vec<EventView>, u32) {
    let mut omitted = 0u32;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(source) = toggle_for_source(row.source) {
            if !gate.source_on(source) {
                continue;
            }
        } else if gate.toggles.pause_all {
            continue;
        }
        if row.sensitivity == EventSensitivity::LocalOnly {
            omitted += 1;
            continue;
        }
        if gate
            .filter
            .resolve_zone(&row.application, &row.window_title)
            != Zone::CloudAllowed
        {
            omitted += 1;
            continue;
        }
        // A summary is free text assembled from observations; re-resolving
        // it on its own catches a never_observe/local_only keyword that
        // survived inside it (the fail-closed check C2 established).
        if gate.filter.resolve_zone("", &row.summary) != Zone::CloudAllowed {
            omitted += 1;
            continue;
        }
        views.push(EventView {
            ts_first: row.ts_first,
            ts_last: row.ts_last,
            count: row.count,
            source: event_enum_token(&row.source),
            event_type: event_enum_token(&row.event_type),
            project: row.project_id.clone(),
            application: gate.filter.scrub_path(&row.application),
            window_title: gate.filter.scrub_text(&row.window_title),
            summary: gate.filter.scrub_text(&row.summary),
            importance: row.importance,
            confidence: row.confidence,
        });
    }
    (views, omitted)
}

/// Opens the log read-only and runs `op`, mapping every failure mode onto
/// the degrade-never-fail contract. `None` means "answer cold".
async fn with_events_log<T, F, Fut>(gate: &Gate<'_>, what: &'static str, op: F) -> Option<T>
where
    F: FnOnce(RawLog) -> Fut,
    Fut: std::future::Future<Output = (RawLog, anyhow::Result<T>)>,
{
    let log = match open_events_read_only(gate.db_path).await {
        Ok(log) => log,
        Err(RawLogError::NotYetCreated { .. }) => return None,
        Err(error) => {
            tracing::warn!(
                layer = "mcp",
                component = "context_tools",
                tool = what,
                error = %error,
                "Read-only open of the raw log failed — reporting no events"
            );
            return None;
        }
    };
    let (log, outcome) = op(log).await;
    log.close().await;
    match outcome {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(
                layer = "mcp",
                component = "context_tools",
                tool = what,
                error = %error,
                "context_events read failed — reporting no events"
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// context_timeline
// ---------------------------------------------------------------------------

/// `context_timeline` request.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct ContextTimelineRequest {
    /// Only events whose latest occurrence is at or after this RFC 3339
    /// timestamp. An unparseable value is ignored (no filter) rather than
    /// erroring.
    #[serde(default)]
    pub since: Option<String>,
    /// Only events whose first occurrence is at or before this RFC 3339
    /// timestamp. Unparseable values are ignored.
    #[serde(default)]
    pub until: Option<String>,
    /// Registry event-type tokens to keep (`error`, `commit`,
    /// `focus_switch`, …). Unknown tokens are dropped; a filter made
    /// entirely of unknown tokens matches nothing.
    #[serde(default)]
    pub types: Option<Vec<String>>,
    /// Restrict to one resolved project id.
    #[serde(default)]
    pub project: Option<String>,
    /// Restrict to one collector family (`window` | `git` | `file` |
    /// `screen` | `audio` | `system` | `voice`).
    #[serde(default)]
    pub source: Option<String>,
    /// Max events returned. Default 50, clamped to 200.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Reads deduped context events with the spec §5.2 filter set.
pub async fn timeline(gate: &Gate<'_>, request: &ContextTimelineRequest) -> ContextEventsResponse {
    if gate.disabled() || gate.toggles.pause_all {
        return ContextEventsResponse::off();
    }
    let limit = request
        .limit
        .unwrap_or(TIMELINE_DEFAULT_LIMIT)
        .clamp(1, EVENT_QUERY_MAX_LIMIT as u32) as usize;

    // An explicit filter the registry does not recognise must narrow to
    // nothing, never widen to "everything": the model asked for something
    // specific and got it wrong, and silently answering a broader question
    // is the worse failure.
    let types: Vec<EventType> = match &request.types {
        Some(tokens) => {
            let parsed: Vec<EventType> = tokens
                .iter()
                .filter_map(|token| parse_event_enum(token.trim()))
                .collect();
            if parsed.is_empty() {
                return ContextEventsResponse {
                    available: true,
                    stale: false,
                    events: Vec::new(),
                    omitted_private: 0,
                };
            }
            parsed
        }
        None => Vec::new(),
    };
    let source: Option<EventSource> = match &request.source {
        Some(token) => match parse_event_enum(token.trim()) {
            Some(source) => Some(source),
            None => {
                return ContextEventsResponse {
                    available: true,
                    stale: false,
                    events: Vec::new(),
                    omitted_private: 0,
                }
            }
        },
        None => None,
    };

    let query = EventQuery {
        since: parse_timestamp(request.since.as_deref()),
        until: parse_timestamp(request.until.as_deref()),
        types,
        project: request.project.clone(),
        source,
        limit,
    };
    let rows = with_events_log(gate, "context_timeline", |log| async move {
        let rows = log.query_context_events(&query).await;
        (log, rows)
    })
    .await;
    let Some(rows) = rows else {
        return ContextEventsResponse::cold();
    };
    let (events, omitted_private) = event_views(gate, &rows);
    ContextEventsResponse {
        available: true,
        stale: false,
        events,
        omitted_private,
    }
}

/// Parses an optional RFC 3339 timestamp. A malformed value degrades to
/// "no filter" with a warning — a bad argument must not fail a tool call
/// in the middle of a wake (rule 3).
fn parse_timestamp(value: Option<&str>) -> Option<DateTime<Utc>> {
    let raw = value?.trim();
    if raw.is_empty() {
        return None;
    }
    match DateTime::parse_from_rfc3339(raw) {
        Ok(ts) => Some(ts.with_timezone(&Utc)),
        Err(error) => {
            tracing::warn!(
                layer = "mcp",
                component = "context_tools",
                value = raw,
                error = %error,
                "Ignoring unparseable timestamp filter"
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// context_search
// ---------------------------------------------------------------------------

/// `context_search` request.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct ContextSearchRequest {
    /// Free-text query. Punctuation is stripped and each word becomes a
    /// prefix term, so `build fail` matches "build failed".
    pub query: String,
    /// Max hits returned. Default 20, clamped to 50.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Full-text search over event summaries and window titles, best match
/// first.
pub async fn search(gate: &Gate<'_>, request: &ContextSearchRequest) -> ContextEventsResponse {
    if gate.disabled() || gate.toggles.pause_all {
        return ContextEventsResponse::off();
    }
    let limit = request
        .limit
        .unwrap_or(SEARCH_DEFAULT_LIMIT)
        .clamp(1, EVENT_SEARCH_MAX_LIMIT as u32) as usize;
    let query = request.query.clone();
    let rows = with_events_log(gate, "context_search", |log| async move {
        let rows = log.search_context_events(&query, limit).await;
        (log, rows)
    })
    .await;
    let Some(rows) = rows else {
        return ContextEventsResponse::cold();
    };
    let (events, omitted_private) = event_views(gate, &rows);
    ContextEventsResponse {
        available: true,
        stale: false,
        events,
        omitted_private,
    }
}

// ---------------------------------------------------------------------------
// context_files
// ---------------------------------------------------------------------------

/// `context_files` request.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct ContextFilesRequest {
    /// Restrict to one resolved project id.
    #[serde(default)]
    pub project: Option<String>,
    /// Max events returned. Default 20, clamped to 100.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Recent file-watcher events (created/modified/deleted/renamed and the
/// storm-collapsed bulk row), oldest first.
pub async fn files(gate: &Gate<'_>, request: &ContextFilesRequest) -> ContextEventsResponse {
    if gate.disabled() || !gate.source_on(ObservedSource::Files) {
        return ContextEventsResponse::off();
    }
    let limit = request
        .limit
        .unwrap_or(FILES_DEFAULT_LIMIT)
        .clamp(1, FILES_MAX_LIMIT) as usize;
    let query = EventQuery {
        // The whole File family, by source rather than by listing types:
        // a type added to the registry later shows up here for free.
        source: Some(EventSource::File),
        project: request.project.clone(),
        limit,
        ..EventQuery::default()
    };
    let rows = with_events_log(gate, "context_files", |log| async move {
        let rows = log.query_context_events(&query).await;
        (log, rows)
    })
    .await;
    let Some(rows) = rows else {
        return ContextEventsResponse::cold();
    };
    let (events, omitted_private) = event_views(gate, &rows);
    ContextEventsResponse {
        available: true,
        stale: false,
        events,
        omitted_private,
    }
}

// ---------------------------------------------------------------------------
// context_git
// ---------------------------------------------------------------------------

/// `context_git` request.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct ContextGitRequest {
    /// A project id to probe on demand. Omit for the active project's
    /// already-published state (no subprocess). Only `configured` and
    /// `confirmed` projects may be probed.
    #[serde(default)]
    pub project: Option<String>,
}

/// `context_git` response.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ContextGitResponse {
    /// False when there is nothing to report or the request was refused.
    pub available: bool,
    /// True when the published snapshot is older than 10 s (never set on
    /// an on-demand probe, which is fresh by construction).
    pub stale: bool,
    /// Why an unavailable answer is unavailable, in one line. Null on
    /// success — this is the field that says "unconfirmed project", not a
    /// tool error.
    pub reason: Option<String>,
    /// Whether a `git` subprocess was actually run for this answer.
    pub probed: bool,
    /// Project id (`context_git` with no argument reports the resolved
    /// project's name, which may differ from a configured id).
    pub project: Option<String>,
    /// Project root, home/username-scrubbed.
    pub root_path: Option<String>,
    /// Current branch; null for detached HEAD and zero-commit repos.
    pub branch: Option<String>,
    /// Tracked files with unstaged working-tree changes.
    pub dirty: u32,
    /// Files with staged (index) changes.
    pub staged: u32,
    /// Untracked files.
    pub untracked: u32,
    /// Commits ahead of upstream (0 when no upstream is configured).
    pub ahead: u32,
    /// Commits behind upstream.
    pub behind: u32,
    /// Unmerged (conflicted) paths.
    pub conflicts: u32,
    /// HEAD commit id. A structured identifier, exempt from scrubbing by
    /// the spec §4.1 privacy contract — an OID is not free text.
    pub last_commit_id: Option<String>,
    /// HEAD commit subject (free text — scrubbed).
    pub last_commit_subject: Option<String>,
}

impl ContextGitResponse {
    fn refused(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            stale: false,
            reason: Some(reason.into()),
            probed: false,
            project: None,
            root_path: None,
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
}

/// Git state: the published state of the active project, or an on-demand
/// bounded probe of a **named, confirmed** project.
///
/// The consent rule (spec §5.2, §4.3) is the whole reason this tool has
/// two paths. Continuum never runs a subprocess against a directory the
/// user has not adopted: a `discovered` row is a *proposal* the discovery
/// heuristic made from a window title, and probing it would turn a guess
/// into filesystem access. Such a request is refused with a `reason`, not
/// an error — the orchestrator is supposed to read the reason and ask the
/// user to confirm the project.
pub async fn git(gate: &Gate<'_>, request: &ContextGitRequest) -> ContextGitResponse {
    if gate.disabled() {
        return ContextGitResponse::refused("context tools are disabled ([context_tools] enabled)");
    }
    if !gate.source_on(ObservedSource::Git) {
        return ContextGitResponse::refused(
            "git observation is switched off ([privacy.toggles] git / pause_all)",
        );
    }
    match request
        .project
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        None => git_from_live_context(gate),
        Some(project) => git_probe_named_project(gate, project).await,
    }
}

/// The no-argument path: whatever the runtime last published for the
/// active project. No subprocess, no filesystem access.
fn git_from_live_context(gate: &Gate<'_>) -> ContextGitResponse {
    let Some((state, stale)) = read_live_context(gate.data_dir, gate.filter) else {
        return ContextGitResponse::refused(
            "the runtime has not published a live-context snapshot yet",
        );
    };
    let Some(project) = state.project else {
        return ContextGitResponse::refused("no project state has been published yet");
    };
    ContextGitResponse {
        available: true,
        stale,
        reason: None,
        probed: false,
        // `cloud_view` already scrubbed the root path and the subject.
        project: project.project_name,
        root_path: project.project_root,
        branch: project.branch,
        dirty: project.dirty,
        staged: project.staged,
        untracked: project.untracked,
        ahead: project.ahead,
        behind: project.behind,
        conflicts: project.conflicts,
        last_commit_id: project.last_commit_id,
        last_commit_subject: project.last_commit_subject,
    }
}

/// The named-project path: consent checks first, then one bounded probe.
async fn git_probe_named_project(gate: &Gate<'_>, project_id: &str) -> ContextGitResponse {
    if !gate.git_context.enabled {
        return ContextGitResponse::refused(
            "the git collector is disabled ([git_context] enabled)",
        );
    }
    let log = match open_events_read_only(gate.db_path).await {
        Ok(log) => log,
        Err(RawLogError::NotYetCreated { .. }) => {
            return ContextGitResponse::refused(
                "the projects database does not exist yet — the runtime has never booted here",
            )
        }
        Err(error) => {
            tracing::warn!(
                layer = "mcp",
                component = "context_tools",
                error = %error,
                "Read-only open of the raw log failed — refusing the named-project git probe"
            );
            return ContextGitResponse::refused("the projects database could not be read");
        }
    };
    let rows = log.list_projects().await;
    log.close().await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                layer = "mcp",
                component = "context_tools",
                error = %error,
                "projects query failed — refusing the named-project git probe"
            );
            return ContextGitResponse::refused("the projects table could not be read");
        }
    };
    let Some(stored) = rows.into_iter().find(|row| row.entry.id == project_id) else {
        return ContextGitResponse::refused(format!(
            "no project {project_id:?} — call context_projects for the known ids"
        ));
    };
    let entry = stored.entry;
    if entry.status == ProjectStatus::Discovered {
        return ContextGitResponse::refused(format!(
            "project {project_id:?} is only a discovery proposal (status \"discovered\"); \
             Continuum never runs commands in a root the user has not confirmed — \
             ask the user to confirm it on the Context page first"
        ));
    }
    if entry.zone == Some(Zone::NeverObserve) {
        return ContextGitResponse::refused(format!(
            "project {project_id:?} is in a never_observe zone"
        ));
    }
    let Some(root) = entry.root_paths.first() else {
        return ContextGitResponse::refused(format!(
            "project {project_id:?} has no root path to probe"
        ));
    };
    let root = std::path::PathBuf::from(root);
    if !root.is_dir() {
        return ContextGitResponse::refused(format!(
            "the root of project {project_id:?} does not exist on this machine"
        ));
    }
    let timeout = Duration::from_secs(gate.git_context.command_timeout_secs.max(1));
    let status = match probe_repo_status(&root, timeout).await {
        Ok(status) => status,
        Err(error) => {
            tracing::warn!(
                layer = "mcp",
                component = "context_tools",
                project = project_id,
                error = %error,
                "On-demand git probe failed"
            );
            return ContextGitResponse::refused(format!(
                "the git probe of project {project_id:?} failed (not a repository, or git is \
                 unavailable)"
            ));
        }
    };
    // A failed `git log` is not a failed probe: a zero-commit repository
    // legitimately has no HEAD.
    let last_commit = probe_last_commit(&root, timeout).await.unwrap_or(None);
    let zone_note = if entry.zone == Some(Zone::LocalOnly) {
        // A local_only project's *content* stays local; the mechanical
        // counts and the branch name are not content, but the commit
        // subject is free text the user marked private.
        None
    } else {
        last_commit
            .as_ref()
            .map(|(_, subject)| gate.filter.scrub_text(subject))
    };
    ContextGitResponse {
        available: true,
        stale: false,
        reason: None,
        probed: true,
        project: Some(entry.id),
        root_path: Some(gate.filter.scrub_path(&root.to_string_lossy())),
        branch: status.branch,
        dirty: status.dirty,
        staged: status.staged,
        untracked: status.untracked,
        ahead: status.ahead,
        behind: status.behind,
        conflicts: status.conflicts,
        last_commit_id: last_commit.map(|(oid, _)| oid),
        last_commit_subject: zone_note,
    }
}

// ---------------------------------------------------------------------------
// context_package
// ---------------------------------------------------------------------------

/// `context_package` request.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct ContextPackageRequest {
    /// Whole-package token budget. Defaults to `[context_package]
    /// token_budget` (1000), clamped to \[200, 8000\].
    #[serde(default)]
    pub token_budget: Option<u32>,
}

/// Per-section freshness (spec §4.9: "per-section `stale` flags"). One
/// package is assembled from four independent sources, and they go cold
/// independently — a stale screen with a fresh event log is a normal state
/// worth telling the caller about.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct PerSectionStale {
    /// `live-context.json` is missing or older than 10 s.
    pub current_moment: bool,
    /// `state.json` is missing or older than 30 s.
    pub session: bool,
    /// The event database could not be read.
    pub events: bool,
    /// The vault / episodic stores could not be opened.
    pub memories: bool,
}

impl PerSectionStale {
    fn any(&self) -> bool {
        self.current_moment || self.session || self.events || self.memories
    }
}

/// The memory half of the package, which the MCP **server** fills from its
/// own lazily-opened vault and episodic stores (the spec §4.9 matrix's
/// "own lazy store opens", following the precedent the memory tools set).
///
/// It is a parameter rather than something this module fetches so the
/// assembler stays testable without a LanceDB index or a fastembed model.
#[derive(Debug, Clone, Default)]
pub struct PackageMemory {
    /// Episodic hits, most relevant first.
    pub memories: Vec<MemoryLine>,
    /// Confirmed vault notes.
    pub vault_notes: Vec<VaultNoteLine>,
    /// True when a store could not be opened or queried.
    pub stale: bool,
}

/// `context_package` response.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ContextPackageResponse {
    /// False only when the tools are switched off. A package with no
    /// sources at all still renders (empty), because "the runtime is not
    /// running" is itself the answer to "what is going on".
    pub available: bool,
    /// True when any contributing source is stale or unreadable.
    pub stale: bool,
    /// The rendered markdown package.
    pub package: String,
    /// Estimated tokens of `package`.
    pub tokens: usize,
    /// The budget actually used, after clamping.
    pub token_budget: usize,
    /// Sections that carry content, in render order.
    pub sections_present: Vec<String>,
    /// Sections this profile does not produce, plus the ones that had no
    /// content this time. `why_woken` and `trigger_frame_moment` are
    /// always here: there is no wake to explain and no trigger frame — the
    /// live-context snapshot replaces the current moment (spec §4.9).
    pub sections_omitted: Vec<String>,
    /// Which sources were stale (see [`PerSectionStale`]).
    pub per_section_stale: PerSectionStale,
    /// Drop rungs the budget forced, in the order they were applied.
    pub dropped: Vec<String>,
}

/// The text `context_package` uses as its memory query: the same compact
/// world render the packager puts in the current moment.
///
/// Exposed separately because the server must fetch memories *before* it
/// can call [`package`], and the query is derived from published state
/// this module owns. `None` when there is nothing published to query with
/// — the server then skips the store opens entirely, which is also what
/// keeps a cold runtime from paying the fastembed load cost.
pub fn package_memory_query(gate: &Gate<'_>) -> Option<String> {
    if gate.disabled() {
        return None;
    }
    let (state, _) = read_live_context(gate.data_dir, gate.filter)?;
    let query = state.compact_for_agents(1_400);
    (!query.trim().is_empty()).then_some(query)
}

/// Assembles and renders the §4.9 **mcp-published** package profile.
pub async fn package(
    gate: &Gate<'_>,
    request: &ContextPackageRequest,
    memory: PackageMemory,
) -> ContextPackageResponse {
    let config = gate.package_config;
    let caps = config.caps();
    // Fixwave 3b (minor): both sources are clamped. Only the request
    // override used to be — a config-supplied `token_budget` bypassed the
    // bounds entirely, so a typo in `[context_package]` could set a budget
    // of 5 (every section dropped) or of millions.
    let token_budget = request
        .token_budget
        .map(|budget| budget as usize)
        .unwrap_or(config.token_budget)
        .clamp(
            PACKAGE_MIN_TOKEN_BUDGET as usize,
            PACKAGE_MAX_TOKEN_BUDGET as usize,
        );

    if gate.disabled() {
        return ContextPackageResponse {
            available: false,
            stale: false,
            package: String::new(),
            tokens: 0,
            token_budget,
            sections_present: Vec::new(),
            sections_omitted: omitted_sections(&[]),
            per_section_stale: PerSectionStale::default(),
            dropped: Vec::new(),
        };
    }

    let mut stale = PerSectionStale {
        memories: memory.stale,
        ..PerSectionStale::default()
    };
    let mut pkg = ContextPackage {
        memories: memory.memories,
        vault_notes: memory.vault_notes,
        ..ContextPackage::default()
    };

    // 1. Current moment — from live-context.json instead of a trigger
    //    frame (the §4.9 omission this profile trades for).
    match read_live_context(gate.data_dir, gate.filter) {
        Some((state, live_stale)) if gate.source_on(ObservedSource::Screen) => {
            stale.current_moment = live_stale;
            pkg.current_moment = Some(moment_from_live(&state, caps.world_compact_chars));
        }
        Some((_, live_stale)) => stale.current_moment = live_stale,
        None => stale.current_moment = true,
    }

    // 2. Session state — from state.json, gated at this egress point
    //    because what is published is raw (Task C1).
    match read_runtime_snapshot(gate.data_dir) {
        Some((snapshot, snapshot_stale)) => {
            stale.session = snapshot_stale;
            if let Some(raw) = snapshot.session_state {
                let view = gate_session_state(gate.filter, &raw);
                pkg.session = Some(SessionSection {
                    project: view.active_project,
                    goal: view.current_goal,
                    task: view.current_task,
                    confidence: view.confidence,
                    open_files: view.open_files,
                    local_only: view.local_only,
                });
            }
        }
        None => stale.session = true,
    }

    // 3. Event-derived sections — the same splitter the wake profile uses,
    //    over rows read through the read-only handle.
    let since = Utc::now() - chrono::Duration::minutes(config.events_window_minutes.max(1));
    let fetch = (caps.just_before + caps.recent_changes + caps.failed_attempts + 1)
        .saturating_mul(PACKAGE_EVENT_OVERFETCH)
        .clamp(1, EVENT_QUERY_MAX_LIMIT);
    let query = EventQuery {
        since: Some(since),
        limit: fetch,
        ..EventQuery::default()
    };
    match with_events_log(gate, "context_package", |log| async move {
        let rows = log.query_context_events(&query).await;
        (log, rows)
    })
    .await
    {
        Some(rows) => {
            // Privacy first, then split: a `local_only` row must not reach
            // the renderer at all, so the caps count survivors. The rows
            // that DO survive are scrubbed here as well — filtering alone
            // would have shipped raw `summary` / `window_title` /
            // `application` text into the rendered package, which is the
            // one §5.1 egress point the other event tools cover via
            // `event_views` (found by the Task C6 safety-redaction bench).
            let allowed: Vec<ContextEventRow> = rows
                .into_iter()
                .filter(|row| keeps_row(gate, row))
                .map(|row| scrub_row(gate, row))
                .collect();
            let sections = split_event_sections(&allowed, Utc::now(), &caps);
            pkg.just_before = sections.just_before;
            pkg.recent_changes = sections.recent_changes;
            pkg.failed_attempts = sections.failed_attempts;
            pkg.last_success = sections.last_success;
        }
        None => stale.events = true,
    }

    let budget = PackageBudget::cloud(token_budget).with_caps(caps);
    let outcome = pkg.render_with_report(&budget);
    // I1: report what the renderer wrote, not what was assembled.
    let sections_present = present_sections(&outcome.sections);
    ContextPackageResponse {
        available: true,
        stale: stale.any(),
        package: outcome.text,
        tokens: outcome.tokens,
        token_budget,
        sections_omitted: omitted_sections(&sections_present),
        sections_present,
        per_section_stale: stale,
        dropped: outcome.dropped.iter().map(drop_step_token).collect(),
    }
}

/// Whether one event row survives the cloud gate — the same three filters
/// [`event_views`] applies, reused so the package and the event tools can
/// never disagree about what is private.
fn keeps_row(gate: &Gate<'_>, row: &ContextEventRow) -> bool {
    let (views, _) = event_views(gate, std::slice::from_ref(row));
    !views.is_empty()
}

/// Applies the same scrubbing [`event_views`] applies to a surviving row,
/// but keeps the row shape [`split_event_sections`] needs.
///
/// Without this, `context_package` would be the only context tool that
/// renders event text straight from the database: `keeps_row` decides
/// *whether* a row may leave the machine, never *in what form*. Persisted
/// text is not automatically egress-safe — a summary written before the
/// user added a `[privacy]` rule, or one a collector failed to scrub,
/// still has to go through the filter at the boundary (spec §5.1).
fn scrub_row(gate: &Gate<'_>, mut row: ContextEventRow) -> ContextEventRow {
    row.application = gate.filter.scrub_path(&row.application);
    row.window_title = gate.filter.scrub_text(&row.window_title);
    row.summary = gate.filter.scrub_text(&row.summary);
    row
}

/// Builds the package's current moment from a **already cloud-gated**
/// live-context snapshot.
fn moment_from_live(state: &LiveWorldState, compact_chars: usize) -> CurrentMoment {
    // The primary monitor is the one the user is looking at; fall back to
    // the first connected display. An excluded monitor's caption is the
    // empty sentinel, which renders as an empty "Screen:" line — correct:
    // there IS a screen and it is deliberately not described.
    let caption = state
        .monitors
        .iter()
        .find(|monitor| monitor.is_primary)
        .or_else(|| state.monitors.first())
        .map(|monitor| monitor.description.clone())
        .unwrap_or_default();
    CurrentMoment {
        caption,
        window_title: state.window.as_ref().map(|window| window.title.clone()),
        app: state
            .window
            .as_ref()
            .map(|window| window.process_name.clone()),
        world_compact: Some(state.compact_for_agents(compact_chars)),
        // The published snapshot carries no transcript — `context_audio`
        // is the tool for that, and re-reading state.json here would give
        // the model the same sentence twice.
        audio: None,
        // Everything above came out of `cloud_view`, so it is already
        // generalized/redacted where it had to be; flagging it local_only
        // again would blank content that was legitimately cleared.
        local_only: false,
    }
}

/// Which sections the renderer **actually wrote**, restricted to this
/// profile's reported vocabulary.
///
/// Fixwave 3b (I1): this used to be computed from the *pre-render*
/// package, while `render_with_report` applies the caps, the cloud gate and
/// the drop ladder to an internal clone. A `local_only` vault note (the
/// server tags sensitive notes that way) is filtered out by the renderer
/// yet was still reported present — so the orchestrator concluded "the
/// vault is in the text" and reasoned over content that had been withheld.
/// [`RenderOutcome::sections`] is the only honest source.
fn present_sections(rendered: &[PackageSection]) -> Vec<String> {
    rendered
        .iter()
        .filter_map(|section| match section {
            PackageSection::CurrentMoment => Some(SECTION_CURRENT_MOMENT),
            PackageSection::Session => Some(SECTION_SESSION),
            PackageSection::JustBefore => Some(SECTION_JUST_BEFORE),
            PackageSection::Memories => Some(SECTION_MEMORIES),
            PackageSection::VaultNotes => Some(SECTION_VAULT_NOTES),
            PackageSection::RecentChanges => Some(SECTION_RECENT_CHANGES),
            PackageSection::FailedAttempts => Some(SECTION_FAILED_ATTEMPTS),
            PackageSection::LastSuccess => Some(SECTION_LAST_SUCCESS),
            // Sections this profile never assembles (§4.9 omissions) or
            // that are not part of the reported vocabulary.
            _ => None,
        })
        .map(str::to_string)
        .collect()
}

/// The profile's permanent omissions plus every candidate section that had
/// nothing to say this time — so `present ∪ omitted` is always the whole
/// section vocabulary and a caller never has to guess which it was.
fn omitted_sections(present: &[String]) -> Vec<String> {
    let candidates = [
        SECTION_CURRENT_MOMENT,
        SECTION_SESSION,
        SECTION_JUST_BEFORE,
        SECTION_MEMORIES,
        SECTION_VAULT_NOTES,
        SECTION_RECENT_CHANGES,
        SECTION_FAILED_ATTEMPTS,
        SECTION_LAST_SUCCESS,
    ];
    let mut omitted: Vec<String> = PROFILE_OMITTED_SECTIONS
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    omitted.extend(
        candidates
            .into_iter()
            .filter(|name| !present.iter().any(|p| p == name))
            .map(str::to_string),
    );
    omitted
}

/// Stable snake_case token for a budget drop rung.
fn drop_step_token(step: &DropStep) -> String {
    match step {
        DropStep::OpenFiles => "open_files",
        DropStep::RecentChanges => "recent_changes",
        DropStep::JustBeforeTail => "just_before_tail",
        DropStep::MemoriesTail => "memories_tail",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use continuum_core::config::{ContextConfig, PrivacyConfig};
    use continuum_core::context::project::ProjectStatus;
    use continuum_core::context::session_state::{
        SessionState, StampedText, PRIVATE_CONTEXT_PHRASE,
    };
    use continuum_core::senses::live_context::{
        write_snapshot, LiveContextHub, MonitorWorldState, WindowWorldState, REDACTED_LOCAL_ONLY,
    };
    use continuum_core::senses::privacy::REDACTED;

    fn test_filter() -> PrivacyFilter {
        PrivacyFilter::from_config(&ContextConfig::default(), &PrivacyConfig::default())
            .with_environment(
                Some("C:\\Users\\testuser".to_string()),
                Some("testuser".to_string()),
            )
    }

    struct Fixture {
        dir: tempfile::TempDir,
        db: std::path::PathBuf,
        filter: PrivacyFilter,
        toggles: ObservationToggles,
        git_context: GitContextConfig,
        package_config: ContextPackageConfig,
        enabled: bool,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("temp dir");
            let db = dir.path().join("raw_log.sqlite");
            Self {
                dir,
                db,
                filter: test_filter(),
                toggles: ObservationToggles::default(),
                git_context: GitContextConfig::default(),
                package_config: ContextPackageConfig::default(),
                enabled: true,
            }
        }

        fn gate(&self) -> Gate<'_> {
            Gate {
                data_dir: self.dir.path(),
                db_path: &self.db,
                filter: &self.filter,
                toggles: &self.toggles,
                git_context: &self.git_context,
                package_config: &self.package_config,
                enabled: self.enabled,
            }
        }

        fn write_state(&self, snapshot: &RuntimeSnapshot) {
            continuum_core::runtime_publish::write_snapshot(
                &self.dir.path().join("state.json"),
                snapshot,
            )
            .expect("publish state.json");
        }

        fn write_live(&self, state: &LiveWorldState) {
            write_snapshot(&self.dir.path().join("live-context.json"), state)
                .expect("publish live-context.json");
        }
    }

    fn fresh_snapshot(session: Option<SessionState>) -> RuntimeSnapshot {
        RuntimeSnapshot {
            last_update: Utc::now().to_rfc3339(),
            session_state: session,
            ..RuntimeSnapshot::default()
        }
    }

    fn live_state() -> LiveWorldState {
        LiveContextHub::default().snapshot()
    }

    fn monitor(id: &str, caption: &str, privacy: PrivacyDisposition) -> MonitorWorldState {
        MonitorWorldState {
            monitor_id: id.into(),
            name: id.into(),
            is_primary: id == "display-1",
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            capture_event_sequence: 1,
            capture_sequence: 1,
            captured_at: Utc::now(),
            change_score: 0.1,
            meaningful_change: true,
            description: caption.into(),
            confidence: 0.8,
            vision_updated_at: Some(Utc::now()),
            privacy,
            target_interval_ms: 200,
            capture_latency_ms: 4,
            dropped_before: 0,
        }
    }

    // -----------------------------------------------------------------
    // context_session
    // -----------------------------------------------------------------

    #[test]
    fn session_is_unavailable_before_the_runtime_publishes() {
        let fixture = Fixture::new();
        let response = session(&fixture.gate());
        assert!(!response.available);
        assert!(!response.stale);
        assert!(response.session.is_none());
    }

    #[test]
    fn session_returns_the_published_state() {
        let fixture = Fixture::new();
        let at = Utc::now();
        fixture.write_state(&fresh_snapshot(Some(SessionState {
            active_project: Some("continuum".into()),
            current_goal: Some("ship the context engine".into()),
            current_task: Some("wire the context tools".into()),
            active_app: Some("Code.exe".into()),
            window_title: Some("context.rs — continuum".into()),
            open_files: vec!["C:\\Users\\testuser\\code\\context.rs".into()],
            last_error: Some(StampedText::new("cargo build failed", at)),
            confidence: 0.8,
            ..SessionState::default()
        })));

        let response = session(&fixture.gate());
        assert!(response.available);
        assert!(!response.stale, "a fresh state.json is not stale");
        let view = response.session.expect("session present");
        assert_eq!(view.project.as_deref(), Some("continuum"));
        assert_eq!(view.goal.as_deref(), Some("ship the context engine"));
        assert_eq!(view.task.as_deref(), Some("wire the context tools"));
        assert_eq!(view.confidence, 0.8);
        assert_eq!(
            view.last_error.expect("last error").text,
            "cargo build failed"
        );
        // Paths are username-scrubbed at the egress point.
        assert_eq!(view.open_files, vec!["~\\code\\context.rs".to_string()]);
    }

    #[test]
    fn session_reports_stale_for_an_old_state_file() {
        let fixture = Fixture::new();
        fixture.write_state(&RuntimeSnapshot {
            last_update: (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339(),
            session_state: Some(SessionState::default()),
            ..RuntimeSnapshot::default()
        });
        let response = session(&fixture.gate());
        assert!(response.available);
        assert!(response.stale, "a 2-minute-old state.json must be stale");
    }

    #[test]
    fn session_generalizes_a_local_only_goal() {
        let fixture = Fixture::new();
        fixture.write_state(&fresh_snapshot(Some(SessionState {
            active_project: Some("continuum".into()),
            current_goal: Some("read the medical results".into()),
            current_task: Some("open the lab portal".into()),
            confidence: 0.9,
            local_only: true,
            ..SessionState::default()
        })));
        let view = session(&fixture.gate()).session.expect("session present");
        assert_eq!(view.goal.as_deref(), Some(PRIVATE_CONTEXT_PHRASE));
        assert_eq!(view.task.as_deref(), Some(PRIVATE_CONTEXT_PHRASE));
        assert!(view.local_only);
        // The mechanical project id is not an inferred field — it stays.
        assert_eq!(view.project.as_deref(), Some("continuum"));
    }

    #[test]
    fn session_scrubs_secrets_out_of_stamped_text() {
        let fixture = Fixture::new();
        fixture.write_state(&fresh_snapshot(Some(SessionState {
            last_error: Some(StampedText::new(
                "push rejected: ghp_AbCd1234EfGh5678IjKl9012MnOp3456QrSt",
                Utc::now(),
            )),
            ..SessionState::default()
        })));
        let view = session(&fixture.gate()).session.expect("session present");
        let text = view.last_error.expect("last error").text;
        assert!(!text.contains("ghp_"), "session leaked a token: {text}");
        assert!(text.contains(REDACTED));
    }

    #[test]
    fn session_is_empty_when_the_family_switch_is_off() {
        let mut fixture = Fixture::new();
        fixture.write_state(&fresh_snapshot(Some(SessionState {
            active_project: Some("continuum".into()),
            ..SessionState::default()
        })));
        fixture.enabled = false;
        let response = session(&fixture.gate());
        assert!(!response.available);
        assert!(response.session.is_none());
    }

    #[test]
    fn session_is_empty_while_observation_is_paused() {
        let mut fixture = Fixture::new();
        fixture.write_state(&fresh_snapshot(Some(SessionState::default())));
        fixture.toggles.pause_all = true;
        assert!(!session(&fixture.gate()).available);
    }

    // -----------------------------------------------------------------
    // context_window
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn window_is_unavailable_before_the_runtime_publishes() {
        let fixture = Fixture::new();
        let response = window(&fixture.gate(), &ContextWindowRequest::default()).await;
        assert!(!response.available);
        assert!(response.active.is_none());
        assert!(response.recent_switches.is_empty());
    }

    #[tokio::test]
    async fn window_returns_the_published_foreground_with_enrichment() {
        let fixture = Fixture::new();
        let mut state = live_state();
        state.window = Some(WindowWorldState {
            process_name: "code.exe".into(),
            title: "context.rs — continuum".into(),
            observed_at: Utc::now(),
            in_call: false,
            privacy: PrivacyDisposition::Visible,
            pid: Some(4242),
            exe_path: Some("C:\\Users\\testuser\\bin\\code.exe".into()),
            monitor_id: Some("display-1".into()),
            active_since_secs: 90,
        });
        fixture.write_live(&state);

        let response = window(&fixture.gate(), &ContextWindowRequest::default()).await;
        assert!(response.available);
        let active = response.active.expect("active window");
        assert_eq!(active.process, "code.exe");
        assert_eq!(active.title, "context.rs — continuum");
        assert_eq!(active.zone, "cloud_allowed");
        assert_eq!(active.pid, Some(4242));
        assert_eq!(active.exe_path.as_deref(), Some("~\\bin\\code.exe"));
        assert_eq!(active.monitor_id.as_deref(), Some("display-1"));
        assert_eq!(active.active_since_secs, 90);
    }

    #[tokio::test]
    async fn window_applies_the_never_observe_sentinel() {
        let fixture = Fixture::new();
        let mut state = live_state();
        state.window = Some(WindowWorldState {
            process_name: "1password.exe".into(),
            title: "Personal Vault".into(),
            observed_at: Utc::now(),
            in_call: true,
            privacy: PrivacyDisposition::Visible,
            pid: Some(9001),
            exe_path: Some("C:\\Program Files\\1Password\\1password.exe".into()),
            monitor_id: Some("display-2".into()),
            active_since_secs: 30,
        });
        fixture.write_live(&state);

        let active = window(&fixture.gate(), &ContextWindowRequest::default())
            .await
            .active
            .expect("active window");
        assert_eq!(active.process, EXCLUDED_PROCESS);
        assert_eq!(active.title, "");
        assert_eq!(active.zone, "never_observe");
        assert!(active.pid.is_none());
        assert!(active.exe_path.is_none());
        assert!(active.monitor_id.is_none());
        assert!(!active.in_call);
    }

    #[tokio::test]
    async fn window_reports_stale_when_the_event_database_is_absent() {
        let fixture = Fixture::new();
        fixture.write_live(&live_state());
        let response = window(&fixture.gate(), &ContextWindowRequest::default()).await;
        assert!(response.available);
        assert!(
            response.stale,
            "an absent context_events database must degrade to stale, not error"
        );
        assert!(response.recent_switches.is_empty());
    }

    #[test]
    fn focus_switch_summaries_parse_and_reject_garbage() {
        assert_eq!(
            parse_focus_switch_summary("code.exe → chrome.exe after 42s"),
            Some(("code.exe".into(), "chrome.exe".into(), 42))
        );
        assert!(parse_focus_switch_summary("code.exe changed").is_none());
        assert!(parse_focus_switch_summary("code.exe → chrome.exe after xs").is_none());
    }

    // -----------------------------------------------------------------
    // context_screen
    // -----------------------------------------------------------------

    #[test]
    fn screen_is_unavailable_before_the_runtime_publishes() {
        let fixture = Fixture::new();
        let response = screen(&fixture.gate());
        assert!(!response.available);
        assert!(response.monitors.is_empty());
        assert!(response.world_compact.is_none());
    }

    #[test]
    fn screen_returns_captions_and_keeps_excluded_monitors_captionless() {
        let fixture = Fixture::new();
        let mut state = live_state();
        state.monitors = vec![
            monitor(
                "display-1",
                "code editor with a test run",
                PrivacyDisposition::Visible,
            ),
            monitor(
                "display-2",
                "banking dashboard",
                PrivacyDisposition::Redacted,
            ),
            monitor("display-3", "vault contents", PrivacyDisposition::Excluded),
        ];
        fixture.write_live(&state);

        let response = screen(&fixture.gate());
        assert!(response.available);
        assert_eq!(response.monitors.len(), 3);
        assert_eq!(response.monitors[0].caption, "code editor with a test run");
        assert_eq!(response.monitors[0].zone, "cloud_allowed");
        assert!(response.monitors[0].primary);
        assert_eq!(response.monitors[1].caption, REDACTED_LOCAL_ONLY);
        assert_eq!(response.monitors[1].zone, "local_only");
        // Sentinel semantics: no caption at all, not even a marker.
        assert_eq!(response.monitors[2].caption, "");
        assert_eq!(response.monitors[2].zone, "never_observe");
        let compact = response.world_compact.expect("compact render");
        assert!(compact.contains("[monitor:display-1]"));
        assert!(!compact.contains("banking dashboard"));
        assert!(!compact.contains("vault contents"));
    }

    #[test]
    fn screen_scrubs_secrets_out_of_a_caption() {
        let fixture = Fixture::new();
        let mut state = live_state();
        state.monitors = vec![monitor(
            "display-1",
            "terminal shows ghp_AbCd1234EfGh5678IjKl9012MnOp3456QrSt",
            PrivacyDisposition::Visible,
        )];
        fixture.write_live(&state);
        let response = screen(&fixture.gate());
        let caption = &response.monitors[0].caption;
        assert!(
            !caption.contains("ghp_"),
            "screen leaked a token: {caption}"
        );
        assert!(caption.contains(REDACTED));
    }

    #[test]
    fn screen_reports_stale_for_an_old_snapshot() {
        let fixture = Fixture::new();
        let mut state = live_state();
        state.generated_at = Utc::now() - chrono::Duration::seconds(60);
        fixture.write_live(&state);
        let response = screen(&fixture.gate());
        assert!(response.available);
        assert!(response.stale);
    }

    #[test]
    fn screen_is_empty_when_the_screen_toggle_is_off() {
        let mut fixture = Fixture::new();
        let mut state = live_state();
        state.monitors = vec![monitor(
            "display-1",
            "secret plans",
            PrivacyDisposition::Visible,
        )];
        fixture.write_live(&state);
        fixture.toggles.screen = false;
        let response = screen(&fixture.gate());
        assert!(!response.available);
        assert!(response.monitors.is_empty());
    }

    // -----------------------------------------------------------------
    // context_audio
    // -----------------------------------------------------------------

    #[test]
    fn audio_is_unavailable_before_the_runtime_publishes() {
        let fixture = Fixture::new();
        let response = audio(&fixture.gate());
        assert!(!response.available);
        assert!(response.transcript.is_none());
    }

    #[test]
    fn audio_returns_the_scrubbed_transcript_and_mute_state() {
        let fixture = Fixture::new();
        fixture.write_state(&RuntimeSnapshot {
            last_update: Utc::now().to_rfc3339(),
            partial_transcript: Some(
                "my key is sk-ant-api03-AbCdEf0123456789AbCdEf0123456789 ok".into(),
            ),
            ambient_mute_active: Some(true),
            ..RuntimeSnapshot::default()
        });
        let response = audio(&fixture.gate());
        assert!(response.available);
        assert!(!response.stale);
        assert!(response.muted);
        let transcript = response.transcript.expect("transcript");
        assert!(!transcript.contains("sk-ant-api03"));
        assert!(transcript.contains(REDACTED));
        assert!(response.at.is_some());
    }

    #[test]
    fn audio_is_empty_when_the_microphone_toggle_is_off() {
        let mut fixture = Fixture::new();
        fixture.write_state(&RuntimeSnapshot {
            last_update: Utc::now().to_rfc3339(),
            partial_transcript: Some("something private".into()),
            ..RuntimeSnapshot::default()
        });
        fixture.toggles.mic = false;
        let response = audio(&fixture.gate());
        assert!(
            !response.available,
            "an honest mic toggle must silence the tool too"
        );
        assert!(response.transcript.is_none());
    }

    #[test]
    fn audio_reads_the_call_flag_from_live_context() {
        let fixture = Fixture::new();
        fixture.write_state(&RuntimeSnapshot {
            last_update: Utc::now().to_rfc3339(),
            ..RuntimeSnapshot::default()
        });
        let mut state = live_state();
        state.window = Some(WindowWorldState {
            process_name: "Teams.exe".into(),
            title: "Weekly sync".into(),
            observed_at: Utc::now(),
            in_call: true,
            privacy: PrivacyDisposition::Visible,
            pid: None,
            exe_path: None,
            monitor_id: None,
            active_since_secs: 5,
        });
        fixture.write_live(&state);
        assert!(audio(&fixture.gate()).in_call);
    }

    // -----------------------------------------------------------------
    // context_projects
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn projects_is_unavailable_without_a_database() {
        let fixture = Fixture::new();
        let response = projects(&fixture.gate()).await;
        assert!(!response.available);
        assert!(response.projects.is_empty());
    }

    #[tokio::test]
    async fn projects_lists_rows_active_first_with_scrubbed_roots() {
        use continuum_core::context::project::ProjectEntry;

        let fixture = Fixture::new();
        let db = fixture.db.clone();
        {
            let log = RawLog::open(&db.to_string_lossy()).await.expect("open");
            log.upsert_project(&ProjectEntry {
                id: "simcharts".into(),
                name: "SimCharts".into(),
                root_paths: vec!["C:\\Users\\testuser\\code\\simcharts".into()],
                repo: None,
                keywords: vec![],
                zone: Some(Zone::LocalOnly),
                status: ProjectStatus::Configured,
            })
            .await
            .expect("upsert simcharts");
            log.upsert_project(&ProjectEntry {
                id: "continuum".into(),
                name: "Continuum".into(),
                root_paths: vec!["C:\\Users\\testuser\\code\\continuum".into()],
                repo: None,
                keywords: vec![],
                zone: None,
                status: ProjectStatus::Confirmed,
            })
            .await
            .expect("upsert continuum");
            log.upsert_project(&ProjectEntry {
                id: "proposal".into(),
                name: "Proposal".into(),
                root_paths: vec![],
                repo: None,
                keywords: vec![],
                zone: None,
                status: ProjectStatus::Discovered,
            })
            .await
            .expect("upsert proposal");
            log.close().await;
        }
        fixture.write_state(&fresh_snapshot(Some(SessionState {
            active_project: Some("continuum".into()),
            ..SessionState::default()
        })));

        let response = projects(&fixture.gate()).await;
        assert!(response.available);
        assert_eq!(response.projects.len(), 3);
        assert_eq!(response.projects[0].id, "continuum");
        assert!(response.projects[0].active);
        assert_eq!(response.projects[0].status, "confirmed");
        assert_eq!(
            response.projects[0].root_paths,
            vec!["~\\code\\continuum".to_string()]
        );
        assert_eq!(response.projects[1].id, "simcharts");
        assert_eq!(response.projects[1].zone.as_deref(), Some("local_only"));
        // Discovery proposals sort last — they never participate in
        // resolution and must not look like real projects.
        assert_eq!(response.projects[2].id, "proposal");
        assert_eq!(response.projects[2].status, "discovered");
    }

    #[tokio::test]
    async fn projects_is_empty_when_the_family_switch_is_off() {
        let mut fixture = Fixture::new();
        fixture.enabled = false;
        let response = projects(&fixture.gate()).await;
        assert!(!response.available);
        assert!(response.projects.is_empty());
    }

    // =================================================================
    // Task C4 — events / git / package
    // =================================================================

    use continuum_core::config::EventsConfig;
    use continuum_core::context::project::ProjectEntry;
    use continuum_core::memory::events::{spawn_event_writer, ContextEvent};
    use continuum_core::senses::live_context::ProjectWorldState;

    /// Builds a cloud-allowed event.
    fn ev(
        source: EventSource,
        event_type: EventType,
        application: &str,
        summary: &str,
        ts: DateTime<Utc>,
    ) -> ContextEvent {
        ContextEvent {
            ts,
            source,
            application: application.into(),
            window_title: format!("{application} — window"),
            project_id: None,
            event_type,
            summary: summary.into(),
            importance: 0.5,
            confidence: 1.0,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: None,
        }
    }

    /// Writes events through the **real** writer task, so the read path is
    /// exercised against rows produced exactly the way the runtime
    /// produces them (registry validation, dedupe, transaction included).
    async fn seed_events(db: &std::path::Path, events: Vec<ContextEvent>) {
        let expected = events.len() as i64;
        let log = RawLog::open(&db.to_string_lossy()).await.expect("open log");
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (sender, _health) = spawn_event_writer(
            log.clone(),
            &EventsConfig::default(),
            None,
            shutdown_rx.clone(),
        );
        for event in events {
            sender.send(event);
        }
        // The writer flushes on a 500 ms tick; poll rather than sleep a
        // fixed amount so the test is neither flaky nor slow.
        for _ in 0..200 {
            if log.context_event_count().await.unwrap_or(0) >= expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            log.context_event_count().await.unwrap_or(0),
            expected,
            "writer did not persist every seeded event"
        );
        let _ = shutdown_tx.send(true);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        log.close().await;
    }

    async fn upsert_project(db: &std::path::Path, entry: ProjectEntry) {
        let log = RawLog::open(&db.to_string_lossy()).await.expect("open log");
        log.upsert_project(&entry).await.expect("upsert project");
        log.close().await;
    }

    // -----------------------------------------------------------------
    // context_timeline
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn timeline_is_stale_without_a_database() {
        let fixture = Fixture::new();
        let response = timeline(&fixture.gate(), &ContextTimelineRequest::default()).await;
        assert!(!response.available);
        assert!(
            response.stale,
            "a missing context_events DB must degrade to stale, not error"
        );
        assert!(response.events.is_empty());
        assert_eq!(response.omitted_private, 0);
    }

    #[tokio::test]
    async fn timeline_returns_events_and_clamps_the_limit() {
        let fixture = Fixture::new();
        let now = Utc::now();
        seed_events(
            &fixture.db,
            vec![
                ev(
                    EventSource::Git,
                    EventType::Commit,
                    "git",
                    "commit: wire the context tools",
                    now - chrono::Duration::seconds(30),
                ),
                ev(
                    EventSource::Screen,
                    EventType::Error,
                    "code.exe",
                    "cargo build failed",
                    now - chrono::Duration::seconds(20),
                ),
                ev(
                    EventSource::File,
                    EventType::FileModified,
                    "",
                    "src/tools/context.rs",
                    now - chrono::Duration::seconds(10),
                ),
            ],
        )
        .await;

        // An absurd limit clamps to the store cap instead of erroring.
        let all = timeline(
            &fixture.gate(),
            &ContextTimelineRequest {
                limit: Some(10_000),
                ..Default::default()
            },
        )
        .await;
        assert!(all.available);
        assert!(!all.stale);
        assert_eq!(all.events.len(), 3);
        // Oldest first, matching every other event reader.
        assert!(all.events[0].ts_last <= all.events[2].ts_last);

        // Zero clamps up to one — the newest row survives.
        let one = timeline(
            &fixture.gate(),
            &ContextTimelineRequest {
                limit: Some(0),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(one.events.len(), 1);
        assert_eq!(one.events[0].event_type, "file_modified");
    }

    #[tokio::test]
    async fn timeline_ignores_unparseable_timestamps_instead_of_erroring() {
        let fixture = Fixture::new();
        seed_events(
            &fixture.db,
            vec![ev(
                EventSource::Git,
                EventType::Commit,
                "git",
                "commit: something",
                Utc::now(),
            )],
        )
        .await;
        let response = timeline(
            &fixture.gate(),
            &ContextTimelineRequest {
                since: Some("yesterday-ish".into()),
                until: Some("".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(response.available);
        assert_eq!(
            response.events.len(),
            1,
            "a malformed timestamp degrades to no filter, never to an error"
        );
    }

    #[tokio::test]
    async fn timeline_filters_by_type_and_narrows_to_nothing_on_an_unknown_one() {
        let fixture = Fixture::new();
        let now = Utc::now();
        seed_events(
            &fixture.db,
            vec![
                ev(
                    EventSource::Screen,
                    EventType::Error,
                    "code.exe",
                    "cargo build failed",
                    now,
                ),
                ev(
                    EventSource::Git,
                    EventType::Commit,
                    "git",
                    "commit: ok",
                    now,
                ),
            ],
        )
        .await;

        let errors = timeline(
            &fixture.gate(),
            &ContextTimelineRequest {
                types: Some(vec!["error".into()]),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(errors.events.len(), 1);
        assert_eq!(errors.events[0].event_type, "error");

        // An unrecognised filter must NARROW, never widen back to "all".
        let bogus = timeline(
            &fixture.gate(),
            &ContextTimelineRequest {
                types: Some(vec!["exploded".into()]),
                ..Default::default()
            },
        )
        .await;
        assert!(bogus.available);
        assert!(bogus.events.is_empty());

        let bogus_source = timeline(
            &fixture.gate(),
            &ContextTimelineRequest {
                source: Some("telepathy".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(bogus_source.events.is_empty());

        let by_source = timeline(
            &fixture.gate(),
            &ContextTimelineRequest {
                source: Some("git".into()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(by_source.events.len(), 1);
        assert_eq!(by_source.events[0].source, "git");
    }

    #[tokio::test]
    async fn timeline_withholds_private_rows_but_reports_the_count() {
        let fixture = Fixture::new();
        let now = Utc::now();
        let mut private = ev(
            EventSource::Screen,
            EventType::Error,
            "chrome.exe",
            "the bank portal rejected the transfer",
            now,
        );
        private.sensitivity = EventSensitivity::LocalOnly;
        seed_events(
            &fixture.db,
            vec![
                ev(
                    EventSource::Git,
                    EventType::Commit,
                    "git",
                    "commit: public work",
                    now,
                ),
                private,
                // Cloud-allowed at write time, but the LIVE zone rules now
                // put this process in never_observe — re-gating must catch
                // it (a rule added later still binds).
                ev(
                    EventSource::Screen,
                    EventType::Routine,
                    "1password.exe",
                    "vault list",
                    now,
                ),
            ],
        )
        .await;

        let response = timeline(&fixture.gate(), &ContextTimelineRequest::default()).await;
        assert_eq!(response.events.len(), 1);
        assert_eq!(response.events[0].summary, "commit: public work");
        assert_eq!(
            response.omitted_private, 2,
            "the caller learns something exists without learning what"
        );
        let json = serde_json::to_string(&response).expect("serialize");
        assert!(!json.contains("bank portal"), "leaked local_only text");
        assert!(!json.contains("1password"), "leaked an excluded process");
    }

    #[tokio::test]
    async fn timeline_does_not_replay_a_source_whose_toggle_is_off() {
        let mut fixture = Fixture::new();
        let now = Utc::now();
        seed_events(
            &fixture.db,
            vec![
                ev(
                    EventSource::Audio,
                    EventType::Decision,
                    "voice",
                    "the user decided to ship on friday",
                    now,
                ),
                ev(
                    EventSource::Git,
                    EventType::Commit,
                    "git",
                    "commit: public work",
                    now,
                ),
            ],
        )
        .await;
        fixture.toggles.mic = false;
        let response = timeline(&fixture.gate(), &ContextTimelineRequest::default()).await;
        assert_eq!(response.events.len(), 1);
        assert_eq!(response.events[0].source, "git");
        assert_eq!(
            response.omitted_private, 0,
            "a switched-off source is not 'private', it is silent"
        );
    }

    #[tokio::test]
    async fn timeline_is_empty_when_the_family_switch_is_off() {
        let mut fixture = Fixture::new();
        fixture.enabled = false;
        let response = timeline(&fixture.gate(), &ContextTimelineRequest::default()).await;
        assert!(!response.available);
        assert!(!response.stale);
        assert!(response.events.is_empty());
    }

    // -----------------------------------------------------------------
    // context_search
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn search_finds_events_and_survives_a_hostile_query() {
        let fixture = Fixture::new();
        let now = Utc::now();
        seed_events(
            &fixture.db,
            vec![
                ev(
                    EventSource::Screen,
                    EventType::Error,
                    "code.exe",
                    "cargo build failed with 3 errors",
                    now,
                ),
                ev(
                    EventSource::Git,
                    EventType::Commit,
                    "git",
                    "commit: unrelated work",
                    now,
                ),
            ],
        )
        .await;

        let hits = search(
            &fixture.gate(),
            &ContextSearchRequest {
                query: "build".into(),
                limit: Some(9_999),
            },
        )
        .await;
        assert!(hits.available);
        assert_eq!(hits.events.len(), 1);
        assert!(hits.events[0].summary.contains("cargo build failed"));

        // FTS operator soup must not become an error. Note the implicit
        // AND: every surviving token must match, so this narrows to the
        // one row containing both words rather than erroring on `"`/`(`.
        let hostile = search(
            &fixture.gate(),
            &ContextSearchRequest {
                query: "\"cargo (build*".into(),
                limit: None,
            },
        )
        .await;
        assert!(hostile.available);
        assert_eq!(hostile.events.len(), 1);

        // A query that normalizes away matches nothing, still not an error.
        let empty = search(
            &fixture.gate(),
            &ContextSearchRequest {
                query: "  ***  ".into(),
                limit: None,
            },
        )
        .await;
        assert!(empty.available);
        assert!(empty.events.is_empty());
    }

    #[tokio::test]
    async fn search_is_stale_without_a_database() {
        let fixture = Fixture::new();
        let response = search(
            &fixture.gate(),
            &ContextSearchRequest {
                query: "anything".into(),
                limit: None,
            },
        )
        .await;
        assert!(!response.available);
        assert!(response.stale);
    }

    // -----------------------------------------------------------------
    // context_files
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn files_returns_only_file_events_and_clamps_the_limit() {
        let fixture = Fixture::new();
        let now = Utc::now();
        seed_events(
            &fixture.db,
            vec![
                ev(
                    EventSource::File,
                    EventType::FileModified,
                    "",
                    "src/tools/context.rs",
                    now,
                ),
                ev(
                    EventSource::File,
                    EventType::FileCreated,
                    "",
                    "src/tools/git.rs",
                    now,
                ),
                ev(
                    EventSource::Git,
                    EventType::Commit,
                    "git",
                    "commit: not a file event",
                    now,
                ),
            ],
        )
        .await;

        let response = files(
            &fixture.gate(),
            &ContextFilesRequest {
                project: None,
                limit: Some(10_000),
            },
        )
        .await;
        assert!(response.available);
        assert_eq!(response.events.len(), 2);
        assert!(response.events.iter().all(|e| e.source == "file"));
    }

    #[tokio::test]
    async fn files_is_silent_when_the_files_toggle_is_off() {
        let mut fixture = Fixture::new();
        seed_events(
            &fixture.db,
            vec![ev(
                EventSource::File,
                EventType::FileModified,
                "",
                "secret-plan.md",
                Utc::now(),
            )],
        )
        .await;
        fixture.toggles.files = false;
        let response = files(&fixture.gate(), &ContextFilesRequest::default()).await;
        assert!(!response.available);
        assert!(response.events.is_empty());
    }

    #[tokio::test]
    async fn files_is_stale_without_a_database() {
        let fixture = Fixture::new();
        let response = files(&fixture.gate(), &ContextFilesRequest::default()).await;
        assert!(!response.available);
        assert!(response.stale);
    }

    // -----------------------------------------------------------------
    // context_git
    // -----------------------------------------------------------------

    fn project_world_state() -> ProjectWorldState {
        ProjectWorldState {
            observed_at: Utc::now(),
            terminal_active: true,
            terminal_process: Some("WindowsTerminal.exe".into()),
            project_root: Some("C:\\Users\\testuser\\code\\continuum".into()),
            project_name: Some("continuum".into()),
            git_head: Some("main".into()),
            branch: Some("main".into()),
            dirty: 3,
            staged: 1,
            untracked: 7,
            ahead: 2,
            behind: 0,
            conflicts: 0,
            last_commit_id: Some("0123456789abcdef0123456789abcdef01234567".into()),
            last_commit_subject: Some("feat(mcp): context tools".into()),
        }
    }

    #[tokio::test]
    async fn git_reads_the_active_project_without_spawning_a_subprocess() {
        let fixture = Fixture::new();
        let mut state = live_state();
        state.project = Some(project_world_state());
        fixture.write_live(&state);

        let response = git(&fixture.gate(), &ContextGitRequest::default()).await;
        assert!(response.available);
        assert!(!response.probed, "the no-argument path never runs git");
        assert!(response.reason.is_none());
        assert_eq!(response.branch.as_deref(), Some("main"));
        assert_eq!(response.dirty, 3);
        assert_eq!(response.staged, 1);
        assert_eq!(response.untracked, 7);
        assert_eq!(response.ahead, 2);
        // Paths are scrubbed; the commit OID is structured and survives.
        assert_eq!(response.root_path.as_deref(), Some("~\\code\\continuum"));
        assert_eq!(
            response.last_commit_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[tokio::test]
    async fn git_is_unavailable_before_the_runtime_publishes() {
        let fixture = Fixture::new();
        let response = git(&fixture.gate(), &ContextGitRequest::default()).await;
        assert!(!response.available);
        assert!(response.reason.is_some());
        assert!(!response.probed);
    }

    #[tokio::test]
    async fn git_refuses_to_probe_an_unconfirmed_project() {
        let fixture = Fixture::new();
        upsert_project(
            &fixture.db,
            ProjectEntry {
                id: "proposal".into(),
                name: "Proposal".into(),
                root_paths: vec![fixture.dir.path().to_string_lossy().to_string()],
                repo: None,
                keywords: vec![],
                zone: None,
                status: ProjectStatus::Discovered,
            },
        )
        .await;

        let response = git(
            &fixture.gate(),
            &ContextGitRequest {
                project: Some("proposal".into()),
            },
        )
        .await;
        assert!(!response.available);
        assert!(
            !response.probed,
            "an unconfirmed root must never be touched — that is the consent rule"
        );
        let reason = response.reason.expect("a refusal states its reason");
        assert!(reason.contains("discovered"), "reason was: {reason}");
        assert!(reason.contains("confirm"), "reason was: {reason}");
    }

    #[tokio::test]
    async fn git_refuses_an_unknown_project_and_a_never_observe_one() {
        let fixture = Fixture::new();
        upsert_project(
            &fixture.db,
            ProjectEntry {
                id: "private-thing".into(),
                name: "Private".into(),
                root_paths: vec![fixture.dir.path().to_string_lossy().to_string()],
                repo: None,
                keywords: vec![],
                zone: Some(Zone::NeverObserve),
                status: ProjectStatus::Confirmed,
            },
        )
        .await;

        let unknown = git(
            &fixture.gate(),
            &ContextGitRequest {
                project: Some("nope".into()),
            },
        )
        .await;
        assert!(!unknown.available);
        assert!(unknown.reason.expect("reason").contains("no project"));

        let excluded = git(
            &fixture.gate(),
            &ContextGitRequest {
                project: Some("private-thing".into()),
            },
        )
        .await;
        assert!(!excluded.available);
        assert!(!excluded.probed);
        assert!(excluded.reason.expect("reason").contains("never_observe"));
    }

    #[tokio::test]
    async fn git_refuses_a_confirmed_project_whose_root_is_gone() {
        let fixture = Fixture::new();
        upsert_project(
            &fixture.db,
            ProjectEntry {
                id: "ghost".into(),
                name: "Ghost".into(),
                root_paths: vec!["C:\\definitely\\not\\here\\ghost".into()],
                repo: None,
                keywords: vec![],
                zone: None,
                status: ProjectStatus::Confirmed,
            },
        )
        .await;
        let response = git(
            &fixture.gate(),
            &ContextGitRequest {
                project: Some("ghost".into()),
            },
        )
        .await;
        assert!(!response.available);
        assert!(!response.probed);
        assert!(response.reason.expect("reason").contains("does not exist"));
    }

    #[tokio::test]
    async fn git_is_refused_when_the_git_toggle_is_off() {
        let mut fixture = Fixture::new();
        let mut state = live_state();
        state.project = Some(project_world_state());
        fixture.write_live(&state);
        fixture.toggles.git = false;
        let response = git(&fixture.gate(), &ContextGitRequest::default()).await;
        assert!(!response.available);
        assert!(response.reason.expect("reason").contains("switched off"));
    }

    /// End-to-end accept path: probe THIS workspace, which is a real git
    /// repository. Skipped when `git` is unavailable so the suite still
    /// passes on a machine without it.
    #[tokio::test]
    async fn git_probes_a_confirmed_project_for_real() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .map(std::path::Path::to_path_buf)
            .expect("workspace root");
        if continuum_core::senses::git_watch::resolve_gitdir(&root).is_none() {
            eprintln!("skipping: workspace is not a git checkout");
            return;
        }
        if continuum_core::senses::git_watch::git_version(std::time::Duration::from_secs(5))
            .await
            .is_none()
        {
            eprintln!("skipping: git is not on PATH");
            return;
        }

        let fixture = Fixture::new();
        upsert_project(
            &fixture.db,
            ProjectEntry {
                id: "continuum".into(),
                name: "Continuum".into(),
                root_paths: vec![root.to_string_lossy().to_string()],
                repo: None,
                keywords: vec![],
                zone: None,
                status: ProjectStatus::Confirmed,
            },
        )
        .await;

        let response = git(
            &fixture.gate(),
            &ContextGitRequest {
                project: Some("continuum".into()),
            },
        )
        .await;
        assert!(response.available, "reason: {:?}", response.reason);
        assert!(response.probed);
        assert!(!response.stale);
        assert!(response.last_commit_id.is_some());
        assert_eq!(response.project.as_deref(), Some("continuum"));
    }

    // -----------------------------------------------------------------
    // context_package
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn package_renders_without_a_runtime() {
        let fixture = Fixture::new();
        let response = package(
            &fixture.gate(),
            &ContextPackageRequest::default(),
            PackageMemory::default(),
        )
        .await;
        assert!(
            response.available,
            "'the runtime is not running' is itself an answer"
        );
        assert!(response.stale);
        assert!(response.per_section_stale.current_moment);
        assert!(response.per_section_stale.session);
        assert!(response.per_section_stale.events);
        assert!(response.package.is_empty());
        assert!(response.sections_present.is_empty());
        // Everything is either a profile omission or an empty candidate.
        for name in PROFILE_OMITTED_SECTIONS {
            assert!(response.sections_omitted.iter().any(|s| s == name));
        }
        assert!(response
            .sections_omitted
            .iter()
            .any(|s| s == SECTION_CURRENT_MOMENT));
    }

    #[tokio::test]
    async fn package_lists_the_sections_the_mcp_profile_fills() {
        let fixture = Fixture::new();
        let now = Utc::now();

        let mut state = live_state();
        state.monitors = vec![monitor(
            "display-1",
            "cargo test output in a terminal",
            PrivacyDisposition::Visible,
        )];
        state.window = Some(WindowWorldState {
            process_name: "code.exe".into(),
            title: "context.rs — continuum".into(),
            observed_at: now,
            in_call: false,
            privacy: PrivacyDisposition::Visible,
            pid: Some(11),
            exe_path: None,
            monitor_id: Some("display-1".into()),
            active_since_secs: 12,
        });
        fixture.write_live(&state);
        fixture.write_state(&fresh_snapshot(Some(SessionState {
            active_project: Some("continuum".into()),
            current_goal: Some("ship the context engine".into()),
            current_task: Some("wire the C4 tools".into()),
            confidence: 0.8,
            ..SessionState::default()
        })));
        seed_events(
            &fixture.db,
            vec![
                ev(
                    EventSource::File,
                    EventType::FileModified,
                    "",
                    "crates/continuum-mcp/src/tools/context.rs",
                    now - chrono::Duration::seconds(60),
                ),
                ev(
                    EventSource::Screen,
                    EventType::Error,
                    "code.exe",
                    "cargo build failed",
                    now - chrono::Duration::seconds(30),
                ),
                ev(
                    EventSource::Screen,
                    EventType::Success,
                    "terminal.exe",
                    "tests green",
                    now - chrono::Duration::seconds(10),
                ),
            ],
        )
        .await;

        let response = package(
            &fixture.gate(),
            &ContextPackageRequest::default(),
            PackageMemory {
                memories: vec![MemoryLine {
                    age: "2h ago".into(),
                    text: "the last context-engine session ended on C3".into(),
                    local_only: false,
                }],
                vault_notes: vec![VaultNoteLine {
                    node_type: "note".into(),
                    title: "private release key rotation".into(),
                    snippet: Some("must stay local".into()),
                    importance: 0.9,
                    local_only: true,
                }],
                stale: false,
            },
        )
        .await;

        assert!(response.available);
        assert!(!response.stale, "everything published is fresh");
        let present = &response.sections_present;
        for expected in [
            SECTION_CURRENT_MOMENT,
            SECTION_SESSION,
            SECTION_JUST_BEFORE,
            SECTION_MEMORIES,
            SECTION_RECENT_CHANGES,
            SECTION_FAILED_ATTEMPTS,
            SECTION_LAST_SUCCESS,
        ] {
            assert!(
                present.iter().any(|s| s == expected),
                "expected {expected} in {present:?}"
            );
        }
        // The two structural omissions of this profile (spec §4.9).
        assert!(response.sections_omitted.iter().any(|s| s == "why_woken"));
        assert!(response
            .sections_omitted
            .iter()
            .any(|s| s == "trigger_frame_moment"));
        // A candidate with nothing in it is reported as omitted too.
        assert!(response
            .sections_omitted
            .iter()
            .any(|s| s == SECTION_VAULT_NOTES));
        assert!(!response.package.contains("private release key rotation"));

        assert!(response.package.contains("## Current moment"));
        assert!(response.package.contains("## Session state"));
        assert!(response.package.contains("cargo build failed"));
        assert!(
            !response.package.contains("## Why you were woken"),
            "the mcp profile has no wake to explain"
        );
    }

    #[tokio::test]
    async fn package_honours_and_clamps_the_token_budget() {
        let fixture = Fixture::new();
        let mut state = live_state();
        state.monitors = vec![monitor(
            "display-1",
            &"a very wordy description of the screen ".repeat(40),
            PrivacyDisposition::Visible,
        )];
        fixture.write_live(&state);
        fixture.write_state(&fresh_snapshot(Some(SessionState {
            active_project: Some("continuum".into()),
            current_task: Some("wire the C4 tools".into()),
            open_files: (0..10).map(|i| format!("crates/file-{i}.rs")).collect(),
            confidence: 0.8,
            ..SessionState::default()
        })));

        let tight = package(
            &fixture.gate(),
            &ContextPackageRequest {
                token_budget: Some(1),
            },
            PackageMemory {
                memories: (0..5)
                    .map(|i| MemoryLine {
                        age: "1h ago".into(),
                        text: format!("memory number {i} with a reasonable amount of text in it"),
                        local_only: false,
                    })
                    .collect(),
                vault_notes: Vec::new(),
                stale: false,
            },
        )
        .await;
        assert_eq!(
            tight.token_budget, PACKAGE_MIN_TOKEN_BUDGET as usize,
            "an absurdly small budget clamps up, it does not produce nonsense"
        );
        assert!(
            !tight.dropped.is_empty(),
            "a 200-token budget must engage the drop ladder"
        );
        assert!(tight.dropped.iter().any(|s| s == "open_files"));

        let huge = package(
            &fixture.gate(),
            &ContextPackageRequest {
                token_budget: Some(u32::MAX),
            },
            PackageMemory::default(),
        )
        .await;
        assert_eq!(huge.token_budget, PACKAGE_MAX_TOKEN_BUDGET as usize);
        assert!(huge.dropped.is_empty());
        assert!(huge.tokens <= huge.token_budget);
    }

    #[tokio::test]
    async fn package_generalizes_a_local_only_session() {
        let fixture = Fixture::new();
        fixture.write_state(&fresh_snapshot(Some(SessionState {
            active_project: Some("continuum".into()),
            current_goal: Some("read the medical results".into()),
            current_task: Some("open the lab portal".into()),
            confidence: 0.9,
            local_only: true,
            ..SessionState::default()
        })));
        let response = package(
            &fixture.gate(),
            &ContextPackageRequest::default(),
            PackageMemory::default(),
        )
        .await;
        assert!(!response.package.contains("medical"));
        assert!(!response.package.contains("lab portal"));
        assert!(response.package.contains(PRIVATE_CONTEXT_PHRASE));
    }

    #[tokio::test]
    async fn package_never_renders_a_private_event() {
        let fixture = Fixture::new();
        let now = Utc::now();
        let mut private = ev(
            EventSource::Screen,
            EventType::Error,
            "chrome.exe",
            "the bank portal rejected the transfer",
            now,
        );
        private.sensitivity = EventSensitivity::LocalOnly;
        seed_events(
            &fixture.db,
            vec![
                private,
                ev(
                    EventSource::Git,
                    EventType::Commit,
                    "git",
                    "commit: public work",
                    now,
                ),
            ],
        )
        .await;
        let response = package(
            &fixture.gate(),
            &ContextPackageRequest::default(),
            PackageMemory::default(),
        )
        .await;
        assert!(response.package.contains("commit: public work"));
        assert!(!response.package.contains("bank portal"));
    }

    #[tokio::test]
    async fn package_scrubs_the_rows_it_renders() {
        // Regression (Task C6, found by continuum-redaction-bench):
        // `keeps_row` decides *whether* a row may leave the machine, not
        // *in what form*. Before `scrub_row`, `context_package` rendered
        // persisted event text verbatim while every other event tool
        // scrubbed it at the boundary — a row written before a scrubber
        // existed (or by a collector that missed one) walked straight out.
        let fixture = Fixture::new();
        let now = Utc::now();
        let mut leaky = ev(
            EventSource::Screen,
            EventType::Error,
            "WindowsTerminal.exe",
            "deploy failed: token ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8 rejected",
            now,
        );
        leaky.window_title = format!("{}\\deploy.ps1 — pwsh", "C:\\Users\\testuser");
        seed_events(&fixture.db, vec![leaky]).await;

        let response = package(
            &fixture.gate(),
            &ContextPackageRequest::default(),
            PackageMemory::default(),
        )
        .await;
        assert!(
            response.package.contains("deploy failed"),
            "the event is still reported: {}",
            response.package
        );
        assert!(
            !response.package.contains("ghp_A1b2C3d4"),
            "the token must be redacted at the egress point: {}",
            response.package
        );
        assert!(
            response.package.contains(REDACTED),
            "the redaction literal is what replaces it: {}",
            response.package
        );
        assert!(
            !response.package.contains("testuser"),
            "the username must be path-scrubbed too: {}",
            response.package
        );
    }

    #[tokio::test]
    async fn package_is_empty_when_the_family_switch_is_off() {
        let mut fixture = Fixture::new();
        fixture.write_state(&fresh_snapshot(Some(SessionState {
            active_project: Some("continuum".into()),
            ..SessionState::default()
        })));
        fixture.enabled = false;
        let response = package(
            &fixture.gate(),
            &ContextPackageRequest::default(),
            PackageMemory::default(),
        )
        .await;
        assert!(!response.available);
        assert!(response.package.is_empty());
        assert!(response.sections_present.is_empty());
    }

    #[test]
    fn package_memory_query_is_none_before_anything_is_published() {
        let fixture = Fixture::new();
        assert!(package_memory_query(&fixture.gate()).is_none());
    }

    #[test]
    fn package_memory_query_is_the_gated_world_render() {
        let fixture = Fixture::new();
        let mut state = live_state();
        state.monitors = vec![
            monitor("display-1", "code editor", PrivacyDisposition::Visible),
            monitor(
                "display-2",
                "banking dashboard",
                PrivacyDisposition::Redacted,
            ),
        ];
        fixture.write_live(&state);
        let query = package_memory_query(&fixture.gate()).expect("a query");
        assert!(query.contains("code editor"));
        assert!(
            !query.contains("banking dashboard"),
            "the memory query is built from the GATED view"
        );
    }
}
