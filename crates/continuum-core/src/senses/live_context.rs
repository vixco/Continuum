//! # Shared live world-state
//!
//! Projects ordered local observations from the senses layer into one compact
//! snapshot. Raw screenshots and raw keyboard/mouse input are deliberately not
//! part of this agent-facing contract.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::senses::privacy::{PrivacyFilter, Zone, EXCLUDED_PROCESS, EXCLUDED_TITLE};

/// Literal substituted for a `local_only` free-text value in a cloud-bound
/// rendering of the world state (spec §4.1 propagation rule: `local_only`
/// content is observed and persisted but stripped from everything
/// cloud-bound). Chosen so that re-scrubbing and re-gating it is a no-op:
/// it matches no scrubber pattern and no default zone rule.
pub const REDACTED_LOCAL_ONLY: &str = "[redacted by local privacy policy]";

/// Schema version for the agent-facing live-context contract.
///
/// Task A2 note: per-monitor privacy zones are held in the in-memory
/// projection only (see [`LiveContextHub::set_monitor_zones`]) and are not
/// part of the serialized [`LiveWorldState`], so they did not bump the
/// version. Publishing zones into the snapshot remains a future additive
/// change requiring another bump.
///
/// Version 2 (Task A5): [`ProjectWorldState`] gained the git-collector
/// fields (`branch`, `dirty`, `staged`, `untracked`, `ahead`, `behind`,
/// `conflicts`, `last_commit_id`, `last_commit_subject`) — additive, all
/// serde-defaulted so v1 documents still parse.
///
/// Version 3 (Task C1): [`LiveWorldState`] gained `session_state` (spec
/// §4.8 publishing) — additive and serde-defaulted, so v1/v2 documents
/// still parse. [`LiveWorldState::compact_for_agents`] renders one extra
/// `[session:current]` line when a session state is present.
///
/// Version 4 (Task C3): [`WindowWorldState`] gained the Task A2 window
/// enrichment already carried on
/// [`crate::senses::types::ContextObservation`] — `pid`, `exe_path`,
/// `monitor_id`, `active_since_secs`. Without them the MCP
/// `context_window` tool (spec §5.2) could only answer with what the
/// *runtime* already published, i.e. process + title, so the enrichment
/// existed but had no consumer outside the frame loop. Additive and
/// serde-defaulted; v1–v3 documents still parse.
pub const LIVE_CONTEXT_SCHEMA_VERSION: u32 = 4;

/// Default number of source events retained in the in-memory projection.
pub const DEFAULT_EVENT_CAPACITY: usize = 256;

/// Origin of a live-context event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveContextSource {
    Monitor,
    Window,
    InputActivity,
    Terminal,
    Project,
    System,
}

/// Privacy treatment applied before an observation enters shared context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyDisposition {
    Visible,
    #[default]
    Redacted,
    Excluded,
}

impl PrivacyDisposition {
    /// Stable snake_case token — the persisted form (raw-log
    /// `context_privacy` column) and the logging form. Matches the serde
    /// representation exactly.
    pub fn as_str(self) -> &'static str {
        match self {
            PrivacyDisposition::Visible => "visible",
            PrivacyDisposition::Redacted => "redacted",
            PrivacyDisposition::Excluded => "excluded",
        }
    }

    /// Parses [`Self::as_str`]. An unknown token yields `None` — callers
    /// treat that as "untagged" and fall back to their own gate, never as
    /// "visible".
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "visible" => Some(PrivacyDisposition::Visible),
            "redacted" => Some(PrivacyDisposition::Redacted),
            "excluded" => Some(PrivacyDisposition::Excluded),
            _ => None,
        }
    }

    /// Numeric strictness rank; higher is stricter.
    fn strictness(self) -> u8 {
        match self {
            PrivacyDisposition::Excluded => 2,
            PrivacyDisposition::Redacted => 1,
            PrivacyDisposition::Visible => 0,
        }
    }
}

/// Returns the stricter of two dispositions (monotonic tightening helper —
/// spec §4.1: privacy may become stricter for a bitmap already in flight,
/// never less strict).
pub fn strictest_disposition(a: PrivacyDisposition, b: PrivacyDisposition) -> PrivacyDisposition {
    if a.strictness() >= b.strictness() {
        a
    } else {
        b
    }
}

/// Monitor identity + desktop-space geometry, exposed so the per-monitor
/// visible-window sweep (ContextWatcher, spec §4.1) can join window rects
/// to monitors without cloning the full projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorGeometry {
    pub monitor_id: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Stable monitor geometry and the latest locally-derived visual context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorWorldState {
    pub monitor_id: String,
    pub name: String,
    pub is_primary: bool,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Global ordered-buffer sequence across all monitor capture workers.
    pub capture_event_sequence: u64,
    /// Per-monitor capture sequence.
    pub capture_sequence: u64,
    pub captured_at: DateTime<Utc>,
    pub change_score: f32,
    pub meaningful_change: bool,
    pub description: String,
    pub confidence: f32,
    pub vision_updated_at: Option<DateTime<Utc>>,
    pub privacy: PrivacyDisposition,
    /// Configured per-monitor capture target used to report missed cadence.
    pub target_interval_ms: u64,
    pub capture_latency_ms: u64,
    pub dropped_before: u64,
}

/// Latest safe foreground-window projection.
///
/// The four enrichment fields (schema v4, Task C3) mirror
/// [`crate::senses::types::ContextObservation`]'s Task A2 fields so a
/// process holding only `live-context.json` — the MCP server, spec §5.2
/// `context_window` — sees the same foreground picture the frame loop
/// does. They are serde-defaulted, so v1–v3 documents still parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowWorldState {
    pub process_name: String,
    pub title: String,
    pub observed_at: DateTime<Utc>,
    pub in_call: bool,
    pub privacy: PrivacyDisposition,
    /// Process id of the foreground window's owning process. `None` when
    /// the lookup failed or the observation is the `never_observe`
    /// sentinel (identity of excluded apps must not leak).
    #[serde(default)]
    pub pid: Option<u32>,
    /// Full executable path, already `scrub_path`-ed at collector emit.
    /// `None` for the sentinel or a failed lookup.
    #[serde(default)]
    pub exe_path: Option<String>,
    /// Hub monitor id (`display-N`) the foreground window sits on.
    /// `None` when the mapping failed or for the sentinel.
    #[serde(default)]
    pub monitor_id: Option<String>,
    /// Whole seconds this (process, title) focus target had been active
    /// **as of `observed_at`**. Resets on every focus switch. Bookkeeping,
    /// not content: it ticks every poll and therefore never bumps the
    /// content version (spec §4.11).
    #[serde(default)]
    pub active_since_secs: u64,
}

/// Coarse activity only; no key values, pointer coordinates, or click targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputActivityWorldState {
    pub observed_at: DateTime<Utc>,
    pub idle_seconds: u64,
    pub active: bool,
}

/// Lightweight local terminal/project projection.
///
/// The identity fields (`project_root`, `project_name`, terminal facts)
/// are produced by the context watcher at 1 Hz; the git-derived fields
/// (`branch` through `last_commit_subject`, plus `git_head`) are **owned
/// by the git collector** (Task A5, spec §4.4) and merged in via
/// [`LiveContextHub::record_git_facts`]. [`LiveContextHub::record_project`]
/// carries git facts forward across context-watcher updates for the same
/// root, and clears them when the root changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectWorldState {
    pub observed_at: DateTime<Utc>,
    pub terminal_active: bool,
    pub terminal_process: Option<String>,
    pub project_root: Option<String>,
    pub project_name: Option<String>,
    /// Legacy head summary: the branch name when on a branch, otherwise
    /// the HEAD commit id (structured field — never scrubbed).
    pub git_head: Option<String>,
    /// Current branch; `None` for detached HEAD and zero-commit repos.
    #[serde(default)]
    pub branch: Option<String>,
    /// Tracked files with unstaged working-tree changes.
    #[serde(default)]
    pub dirty: u32,
    /// Files with staged (index) changes.
    #[serde(default)]
    pub staged: u32,
    /// Untracked files.
    #[serde(default)]
    pub untracked: u32,
    /// Commits ahead of upstream (0 when no upstream).
    #[serde(default)]
    pub ahead: u32,
    /// Commits behind upstream (0 when no upstream).
    #[serde(default)]
    pub behind: u32,
    /// Unmerged (conflicted) paths.
    #[serde(default)]
    pub conflicts: u32,
    /// HEAD commit id (full OID — structured field, exempt from
    /// scrubbing by the spec §4.1 privacy contract).
    #[serde(default)]
    pub last_commit_id: Option<String>,
    /// HEAD commit subject (free text — arrives already scrubbed through
    /// `PrivacyFilter::scrub_text` at collector emit).
    #[serde(default)]
    pub last_commit_subject: Option<String>,
}

/// Git facts for the active project, as merged into
/// [`ProjectWorldState`] by [`LiveContextHub::record_git_facts`] (Task
/// A5, spec §4.4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectGitFacts {
    /// Current branch; `None` for detached HEAD / zero-commit repos.
    pub branch: Option<String>,
    /// Tracked files with unstaged working-tree changes.
    pub dirty: u32,
    /// Files with staged (index) changes.
    pub staged: u32,
    /// Untracked files.
    pub untracked: u32,
    /// Commits ahead of upstream.
    pub ahead: u32,
    /// Commits behind upstream.
    pub behind: u32,
    /// Unmerged (conflicted) paths.
    pub conflicts: u32,
    /// HEAD commit id (full OID, structured — never scrubbed).
    pub last_commit_id: Option<String>,
    /// HEAD commit subject (free text — already scrubbed at emit).
    pub last_commit_subject: Option<String>,
}

/// One ordered, source-attributed change in the live world-state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveContextEvent {
    pub sequence: u64,
    pub observed_at: DateTime<Utc>,
    pub source: LiveContextSource,
    pub source_id: String,
    pub summary: String,
    pub degraded: bool,
    pub dropped_before: u64,
}

/// Health and backpressure counters for the live-context component.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveContextHealth {
    pub capture_events: u64,
    pub vision_updates: u64,
    pub dropped_capture_events: u64,
    pub capture_deadline_misses: u64,
    pub output_events_dropped: u64,
    pub capture_failures: u64,
    pub last_capture_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl LiveContextHealth {
    /// Whether a configured capture component has stalled long enough to restart.
    pub fn should_restart(
        &self,
        now: DateTime<Utc>,
        capture_enabled: bool,
        capture_interval: Duration,
    ) -> bool {
        if !capture_enabled {
            return false;
        }
        let Some(last) = self.last_capture_at else {
            return self.capture_failures >= 3;
        };
        let threshold_ms = capture_interval.as_millis().saturating_mul(10).max(5_000);
        now.signed_duration_since(last).num_milliseconds() > threshold_ms as i64
    }
}

/// Versioned agent-facing projection of the current local world.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveWorldState {
    pub schema_version: u32,
    pub sequence: u64,
    pub generated_at: DateTime<Utc>,
    pub monitors: Vec<MonitorWorldState>,
    pub window: Option<WindowWorldState>,
    pub input_activity: Option<InputActivityWorldState>,
    pub project: Option<ProjectWorldState>,
    pub recent_events: Vec<LiveContextEvent>,
    pub health: LiveContextHealth,
    /// Live session state (Task C1, schema v3): the same
    /// [`crate::context::session_state::SessionState`] the runtime
    /// publishes into `state.json`, mirrored here so a consumer holding
    /// only `live-context.json` (the MCP `context_screen`/`context_package`
    /// profiles, spec §5.2) sees the same picture. Serde-defaulted, so
    /// v1/v2 documents parse. `None` until the runtime records one.
    #[serde(default)]
    pub session_state: Option<crate::context::session_state::SessionState>,
}

impl LiveWorldState {
    /// Compile bounded plain text for models that cannot accept images.
    pub fn compact_for_agents(&self, max_chars: usize) -> String {
        let mut lines = vec![format!(
            "live-context/v{} seq={} generated={}",
            self.schema_version,
            self.sequence,
            self.generated_at.to_rfc3339()
        )];
        for monitor in &self.monitors {
            let description = match monitor.privacy {
                PrivacyDisposition::Visible => monitor.description.replace(['\r', '\n'], " "),
                // Sentinel semantics (spec §4.1): excluded monitors carry
                // no caption at all, not even a redaction marker.
                PrivacyDisposition::Excluded => String::new(),
                PrivacyDisposition::Redacted => REDACTED_LOCAL_ONLY.into(),
            };
            lines.push(format!(
                "[monitor:{}] {}{} event={} capture={} change={:.3} privacy={:?} vision=\"{}\"",
                monitor.monitor_id,
                monitor.name,
                if monitor.is_primary { " primary" } else { "" },
                monitor.capture_event_sequence,
                monitor.capture_sequence,
                monitor.change_score,
                monitor.privacy,
                description
            ));
        }
        if let Some(window) = &self.window {
            lines.push(format!(
                "[window:foreground] process={} title=\"{}\" privacy={:?}",
                window.process_name,
                window.title.replace(['\r', '\n'], " "),
                window.privacy
            ));
        }
        if let Some(activity) = &self.input_activity {
            lines.push(format!(
                "[input:activity] active={} idle_seconds={} (no raw input captured)",
                activity.active, activity.idle_seconds
            ));
        }
        if let Some(project) = &self.project {
            lines.push(format!(
                "[project:current] root={} git_head={} terminal={}",
                project.project_root.as_deref().unwrap_or("unknown"),
                project.git_head.as_deref().unwrap_or("unknown"),
                project.terminal_process.as_deref().unwrap_or("inactive")
            ));
        }
        // Task C1 (spec §4.8 consumers): one compact session line. This
        // blob is a *cloud-bound* rendering (it becomes
        // `ScreenObservation.world_compact`, which only the packager
        // reads — the triage prompt excludes it by construction, Task
        // B4), so the §4.1 propagation rule applies here: `local_only`
        // inferred fields are generalized via `cloud_view()` rather than
        // rendered verbatim.
        if let Some(session) = &self.session_state {
            let view = session.cloud_view();
            let mut parts: Vec<String> = Vec::new();
            if let Some(project) = &view.active_project {
                parts.push(format!("project={project}"));
            }
            if let Some(goal) = &view.current_goal {
                parts.push(format!("goal=\"{}\"", goal.replace(['\r', '\n'], " ")));
            }
            if let Some(task) = &view.current_task {
                parts.push(format!("task=\"{}\"", task.replace(['\r', '\n'], " ")));
            }
            if !parts.is_empty() {
                if view.current_goal.is_some() || view.current_task.is_some() {
                    parts.push(format!("confidence={:.2}", view.confidence));
                }
                lines.push(format!("[session:current] {}", parts.join(" ")));
            }
        }
        if self.health.dropped_capture_events > 0
            || self.health.capture_deadline_misses > 0
            || self.health.output_events_dropped > 0
        {
            lines.push(format!(
                "[system:degradation] capture_dropped={} cadence_missed={} output_dropped={}",
                self.health.dropped_capture_events,
                self.health.capture_deadline_misses,
                self.health.output_events_dropped
            ));
        }
        truncate_utf8(&lines.join("\n"), max_chars)
    }

    /// A **cloud-bound projection** of the world state (spec §5.1 "cloud
    /// gate", §4.1 propagation rule 2).
    ///
    /// The producers already scrub at collector emit, so this is a second,
    /// independent enforcement at the *egress point* — the boundary a
    /// process that only ever reads `live-context.json` (the MCP server)
    /// owns. Everything here is idempotent by [`PrivacyFilter`]'s contract,
    /// so applying it to already-clean content is a no-op.
    ///
    /// What it does, field by field:
    /// - **monitors** — `Excluded` keeps the sentinel (empty caption, spec
    ///   §4.1: not even a redaction marker); `Redacted` collapses to
    ///   [`REDACTED_LOCAL_ONLY`]; `Visible` captions are re-scrubbed and
    ///   re-zoned as free text.
    /// - **window** — the zone is re-resolved from the *live rules* against
    ///   `(process_name, title)` and combined with the recorded disposition
    ///   (strictest wins, so a rule added since the snapshot was written
    ///   still applies). `never_observe` yields the §4.1 sentinel
    ///   ([`EXCLUDED_PROCESS`] / [`EXCLUDED_TITLE`], `in_call = false`);
    ///   `local_only` keeps the process name (not window content) and
    ///   replaces the title.
    /// - **project** — paths through `scrub_path`, the commit *subject*
    ///   through `scrub_text`; `git_head`/`last_commit_id`/`branch` are
    ///   structured fields and pass through untouched by construction.
    /// - **recent_events** — summaries are free text: gated and scrubbed.
    /// - **session_state** — [`crate::context::session_state::SessionState::cloud_view`]
    ///   first (goal/task generalization), then the mechanical fields
    ///   (`active_app`, `window_title`, `open_files`, stamped texts) are
    ///   gated here, because *this* is the egress point that `cloud_view`
    ///   documents as the caller's job.
    /// - **health** — counters are not content; `last_error` is free text.
    pub fn cloud_view(&self, filter: &PrivacyFilter) -> LiveWorldState {
        let mut out = self.clone();

        for monitor in &mut out.monitors {
            monitor.description = match monitor.privacy {
                PrivacyDisposition::Excluded => String::new(),
                PrivacyDisposition::Redacted => REDACTED_LOCAL_ONLY.to_string(),
                PrivacyDisposition::Visible => gate_free_text(filter, &monitor.description),
            };
        }

        if let Some(window) = &mut out.window {
            let zone = crate::senses::privacy::strictest([
                filter.resolve_zone(&window.process_name, &window.title),
                zone_of(window.privacy),
            ]);
            match zone {
                Zone::NeverObserve => {
                    window.process_name = EXCLUDED_PROCESS.to_string();
                    window.title = EXCLUDED_TITLE.to_string();
                    window.in_call = false;
                    // Schema v4 enrichment: pid and exe path ARE the
                    // identity of the excluded app, and the monitor id
                    // says which screen it is on. The sentinel carries
                    // none of it; dwell is zeroed for the same reason
                    // `in_call` is (spec §4.1 sentinel semantics).
                    window.pid = None;
                    window.exe_path = None;
                    window.monitor_id = None;
                    window.active_since_secs = 0;
                }
                Zone::LocalOnly => {
                    window.process_name = filter.scrub_path(&window.process_name);
                    window.title = REDACTED_LOCAL_ONLY.to_string();
                    // The process is not window *content* (same call the
                    // `system_active_window` gate makes), so the exe path
                    // survives path-scrubbed alongside it.
                    window.exe_path = window
                        .exe_path
                        .as_deref()
                        .map(|path| filter.scrub_path(path));
                }
                Zone::CloudAllowed => {
                    window.process_name = filter.scrub_path(&window.process_name);
                    window.title = filter.scrub_text(&window.title);
                    window.exe_path = window
                        .exe_path
                        .as_deref()
                        .map(|path| filter.scrub_path(path));
                }
            }
            window.privacy = zone.into();
        }

        if let Some(project) = &mut out.project {
            project.terminal_process = project
                .terminal_process
                .as_deref()
                .map(|process| filter.scrub_path(process));
            project.project_root = project
                .project_root
                .as_deref()
                .map(|root| filter.scrub_path(root));
            project.project_name = project
                .project_name
                .as_deref()
                .map(|name| filter.scrub_path(name));
            project.last_commit_subject = project
                .last_commit_subject
                .as_deref()
                .map(|subject| gate_free_text(filter, subject));
        }

        for event in &mut out.recent_events {
            event.summary = gate_free_text(filter, &event.summary);
        }

        // Error strings routinely embed the path that failed, so they get
        // both scrubbers: structured path redaction first, then free-text.
        out.health.last_error = out
            .health
            .last_error
            .as_deref()
            .map(|error| gate_free_text(filter, &filter.scrub_path(error)));

        out.session_state = self
            .session_state
            .as_ref()
            .map(|session| gate_session_state(filter, session));

        out
    }
}

/// The **cloud gate for one session state** (spec §4.1 propagation rule 2,
/// §5.1 egress point).
///
/// [`crate::context::session_state::SessionState::cloud_view`] generalizes
/// the *inferred* fields when the state is `local_only`, and explicitly
/// leaves the mechanical fields (`active_app`, `window_title`,
/// `open_files`, the stamped texts) to "the caller's egress point". This
/// function **is** that egress point, so both halves always happen
/// together.
///
/// It is used by [`LiveWorldState::cloud_view`] for the `session_state`
/// carried inside `live-context.json` **and** by the MCP `context_session`
/// tool (spec §5.2), which reads the *raw* state out of `state.json` —
/// the publisher deliberately writes it ungated so every consumer applies
/// its own gate. Sharing one function is what keeps those two egress
/// points from drifting.
pub fn gate_session_state(
    filter: &PrivacyFilter,
    session: &crate::context::session_state::SessionState,
) -> crate::context::session_state::SessionState {
    let mut view = session.cloud_view();
    view.active_app = view.active_app.as_deref().map(|app| filter.scrub_path(app));
    view.window_title = view
        .window_title
        .as_deref()
        .map(|title| gate_free_text(filter, title));
    view.current_goal = view
        .current_goal
        .as_deref()
        .map(|goal| gate_free_text(filter, goal));
    view.current_task = view
        .current_task
        .as_deref()
        .map(|task| gate_free_text(filter, task));
    view.open_files = view
        .open_files
        .iter()
        .map(|file| filter.scrub_path(file))
        .collect();
    for stamped in [
        &mut view.last_error,
        &mut view.last_success,
        &mut view.last_user_command,
    ]
    .into_iter()
    .flatten()
    {
        stamped.text = gate_free_text(filter, &stamped.text);
    }
    view
}

/// The inverse of the [`Zone`] → [`PrivacyDisposition`] mapping, used to
/// fold a *recorded* disposition back into the zone lattice so it can be
/// combined with a freshly resolved zone. `Redacted` maps to
/// [`Zone::LocalOnly`], matching `From<Zone> for PrivacyDisposition`.
fn zone_of(disposition: PrivacyDisposition) -> Zone {
    match disposition {
        PrivacyDisposition::Excluded => Zone::NeverObserve,
        PrivacyDisposition::Redacted => Zone::LocalOnly,
        PrivacyDisposition::Visible => Zone::CloudAllowed,
    }
}

/// Gates one **free-text** value for cloud egress: the zone rules are
/// evaluated against the text itself (title-keyword rules are substring
/// matches, so a `never_observe`/`local_only` keyword surviving inside a
/// caption, summary, or inferred task is caught here), then the secret
/// scrubbers run.
///
/// Process-scoped rules cannot apply to a bare string, so the process name
/// is passed as empty — a rule with a `match_process` criterion simply does
/// not match, which is correct: it is scoped to a window, not to prose.
fn gate_free_text(filter: &PrivacyFilter, text: &str) -> String {
    match filter.resolve_zone("", text) {
        Zone::NeverObserve => String::new(),
        Zone::LocalOnly => REDACTED_LOCAL_ONLY.to_string(),
        Zone::CloudAllowed => filter.scrub_text(text),
    }
}

#[derive(Debug)]
struct Projection {
    connected_monitors: BTreeSet<String>,
    monitors: BTreeMap<String, MonitorWorldState>,
    /// Per-monitor privacy zone from the visible-window sweep (spec §4.1).
    /// A monitor showing any `never_observe`/`local_only` top-level window
    /// inherits that zone for capture/caption purposes. In-memory only —
    /// not part of the serialized snapshot.
    monitor_zones: BTreeMap<String, Zone>,
    window: Option<WindowWorldState>,
    input_activity: Option<InputActivityWorldState>,
    project: Option<ProjectWorldState>,
    /// Latest session state recorded by the runtime (Task C1). Mirrored,
    /// not owned — the `SessionStateHub` remains the source of truth.
    session_state: Option<crate::context::session_state::SessionState>,
    events: VecDeque<LiveContextEvent>,
    health: LiveContextHealth,
}

/// Shared, cheap-to-clone handle used by every live-context producer.
#[derive(Debug, Clone)]
pub struct LiveContextHub {
    inner: Arc<RwLock<Projection>>,
    sequence: Arc<AtomicU64>,
    /// Content-version counter (spec §4.11, Task A8): bumped ONLY by
    /// meaningful content changes — meaningful-change captures, vision
    /// caption updates, privacy/zone changes, window title/process
    /// changes, project state changes. NOT bumped by unchanged-capture
    /// bookkeeping or the 1 Hz no-change context poll. The publisher
    /// keys `live-context.json` writes on it. In-memory only — not
    /// serialized into [`LiveWorldState`], so no schema bump.
    content_version: Arc<AtomicU64>,
    /// Idle flag (spec §4.11): while set, "unchanged" ring events are
    /// suppressed so idle churn cannot flood the bounded event ring.
    /// Health counters and monitor state keep updating regardless.
    idle: Arc<AtomicBool>,
    event_capacity: usize,
}

impl LiveContextHub {
    /// Create an empty projection with a bounded event history.
    pub fn new(event_capacity: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Projection {
                connected_monitors: BTreeSet::new(),
                monitors: BTreeMap::new(),
                monitor_zones: BTreeMap::new(),
                window: None,
                input_activity: None,
                project: None,
                session_state: None,
                events: VecDeque::with_capacity(event_capacity.max(1)),
                health: LiveContextHealth::default(),
            })),
            sequence: Arc::new(AtomicU64::new(0)),
            content_version: Arc::new(AtomicU64::new(0)),
            idle: Arc::new(AtomicBool::new(false)),
            event_capacity: event_capacity.max(1),
        }
    }

    /// Current content version (spec §4.11). The publisher skips its
    /// write when this hasn't moved since the last successful publish.
    pub fn content_version(&self) -> u64 {
        self.content_version.load(Ordering::Acquire)
    }

    /// Marks the hub idle/active (spec §4.11). Set by the runtime's idle
    /// controller; while idle, unchanged-capture and no-change context
    /// ring events are not pushed.
    pub fn set_idle(&self, idle: bool) {
        self.idle.store(idle, Ordering::Release);
    }

    /// Cheap clone of the health counters only (Task A8 health snapshot
    /// registration — avoids cloning the full projection every publish
    /// tick).
    pub fn health(&self) -> LiveContextHealth {
        self.inner.read().health.clone()
    }

    /// Number of monitors currently in the projection — the one field the
    /// runtime's per-frame status update needs that [`Self::health`] does
    /// not carry.
    ///
    /// Exists so the frame arm never calls [`Self::snapshot`] (M2): that
    /// clones every monitor, the whole event ring and the session state
    /// just to read a handful of counters, and it did so while holding the
    /// `runtime_state` mutex the publisher also takes.
    pub fn monitor_count(&self) -> usize {
        self.inner.read().monitors.len()
    }

    fn bump_content_version(&self) {
        self.content_version.fetch_add(1, Ordering::AcqRel);
    }

    /// Publish lightweight capture metadata without waiting for local vision.
    ///
    /// Content-version rule (spec §4.11): bumps only for a meaningful
    /// change or a privacy transition on this monitor — unchanged-capture
    /// bookkeeping (health counters, latest capture metadata) never
    /// bumps, so the publisher stays quiet on a static screen.
    pub fn record_monitor_capture(&self, mut monitor: MonitorWorldState) -> u64 {
        let observed_at = monitor.captured_at;
        let source_id = monitor.monitor_id.clone();
        let dropped_before = monitor.dropped_before;
        let deadline_missed = monitor.capture_latency_ms > monitor.target_interval_ms;
        let mut inner = self.inner.write();
        if !inner.connected_monitors.contains(&source_id) {
            return self.sequence.load(Ordering::Acquire);
        }
        let privacy_changed = inner
            .monitors
            .get(&source_id)
            .map(|previous| previous.privacy != monitor.privacy)
            .unwrap_or(true);
        if monitor.privacy == PrivacyDisposition::Excluded {
            // Sentinel semantics (spec §4.1): a never_observe monitor gets
            // no caption at all — an empty description, not a redaction
            // marker. The zone itself is the only recorded fact.
            monitor.description = String::new();
            monitor.confidence = 1.0;
            monitor.vision_updated_at = None;
        } else if monitor.privacy != PrivacyDisposition::Visible {
            monitor.description = REDACTED_LOCAL_ONLY.into();
            monitor.confidence = 1.0;
            monitor.vision_updated_at = None;
        } else if let Some(previous) = inner.monitors.get(&source_id) {
            if monitor.description.is_empty() {
                monitor.description = previous.description.clone();
                monitor.confidence = previous.confidence;
                monitor.vision_updated_at = previous.vision_updated_at;
            }
        }
        let meaningful = monitor.meaningful_change;
        let summary = if monitor.privacy != PrivacyDisposition::Visible {
            format!("{} {:?}", monitor.name, monitor.privacy)
        } else if meaningful {
            format!("{} changed: {}", monitor.name, monitor.description)
        } else {
            format!("{} unchanged", monitor.name)
        };
        inner.health.capture_events = inner.health.capture_events.saturating_add(1);
        if deadline_missed {
            inner.health.capture_deadline_misses =
                inner.health.capture_deadline_misses.saturating_add(1);
        }
        inner.health.last_capture_at = Some(observed_at);
        inner.monitors.insert(source_id.clone(), monitor);
        if meaningful || privacy_changed {
            self.bump_content_version();
        }
        // Spec §4.11: no "unchanged" ring events during idle — the
        // bounded ring must not fill with idle churn. Degraded unchanged
        // captures (drops, cadence misses) still push so overload
        // evidence is never hidden.
        let degraded = dropped_before > 0 || deadline_missed;
        if self.idle.load(Ordering::Acquire) && !meaningful && !privacy_changed && !degraded {
            return self.sequence.load(Ordering::Acquire);
        }
        self.push_event_locked(
            &mut inner,
            observed_at,
            LiveContextSource::Monitor,
            source_id,
            summary,
            degraded,
            dropped_before,
        )
    }

    /// Apply a selective local-vision result to the latest monitor capture.
    pub fn record_monitor_vision(
        &self,
        monitor_id: &str,
        description: String,
        confidence: f32,
        vision_updated_at: Option<DateTime<Utc>>,
        privacy: PrivacyDisposition,
        vision_updated: bool,
    ) {
        let mut inner = self.inner.write();
        let Some(monitor) = inner.monitors.get_mut(monitor_id) else {
            return;
        };
        let privacy_changed = monitor.privacy != privacy;
        monitor.description = description;
        monitor.confidence = confidence;
        monitor.vision_updated_at = vision_updated_at;
        monitor.privacy = privacy;
        if vision_updated {
            inner.health.vision_updates = inner.health.vision_updates.saturating_add(1);
        }
        if vision_updated || privacy_changed {
            // Content-version rule (spec §4.11): caption updates and
            // privacy transitions are meaningful content.
            self.bump_content_version();
            self.push_event_locked(
                &mut inner,
                Utc::now(),
                LiveContextSource::Monitor,
                monitor_id.to_string(),
                if privacy == PrivacyDisposition::Visible {
                    "local vision summary updated".into()
                } else {
                    "visual context redacted by local privacy policy".into()
                },
                false,
                0,
            );
        }
    }

    /// Read only the current foreground privacy classification.
    pub fn current_privacy(&self) -> PrivacyDisposition {
        self.inner
            .read()
            .window
            .as_ref()
            .map(|window| window.privacy)
            .unwrap_or_default()
    }

    /// Replace the per-monitor privacy zones computed by the visible-window
    /// sweep (spec §4.1). Keys are monitor ids (`display-N`); monitors
    /// absent from the map are treated as [`Zone::CloudAllowed`].
    pub fn set_monitor_zones(&self, zones: BTreeMap<String, Zone>) {
        let mut inner = self.inner.write();
        if inner.monitor_zones != zones {
            // Zone changes are meaningful content (spec §4.11).
            self.bump_content_version();
            inner.monitor_zones = zones;
        }
    }

    /// The sweep-derived zone for one monitor. Unknown monitors default to
    /// [`Zone::CloudAllowed`] — the sweep will tighten them on its next
    /// 1 Hz pass; the foreground disposition still applies in the interim
    /// via [`LiveContextHub::monitor_privacy`].
    pub fn monitor_zone(&self, monitor_id: &str) -> Zone {
        self.inner
            .read()
            .monitor_zones
            .get(monitor_id)
            .copied()
            .unwrap_or(Zone::CloudAllowed)
    }

    /// Effective privacy disposition for a monitor: the **strictest** of
    /// the foreground-window disposition and the monitor's sweep-derived
    /// zone. Folding the foreground in preserves today's behavior (one
    /// sensitive foreground window redacts every monitor) while the sweep
    /// additionally catches sensitive windows on background monitors —
    /// strictly stricter than before, never less strict.
    pub fn monitor_privacy(&self, monitor_id: &str) -> PrivacyDisposition {
        let inner = self.inner.read();
        let foreground = inner
            .window
            .as_ref()
            .map(|window| window.privacy)
            .unwrap_or_default();
        let zone = inner
            .monitor_zones
            .get(monitor_id)
            .copied()
            .unwrap_or(Zone::CloudAllowed);
        strictest_disposition(foreground, zone.into())
    }

    /// Identity + geometry of the currently projected monitors, for the
    /// visible-window sweep's rect∩monitor join.
    pub fn monitor_geometries(&self) -> Vec<MonitorGeometry> {
        self.inner
            .read()
            .monitors
            .values()
            .map(|monitor| MonitorGeometry {
                monitor_id: monitor.monitor_id.clone(),
                x: monitor.x,
                y: monitor.y,
                width: monitor.width,
                height: monitor.height,
            })
            .collect()
    }

    /// Reconcile hot-plug topology and evict disconnected monitor state.
    pub fn set_connected_monitors(&self, monitor_ids: impl IntoIterator<Item = String>) {
        let connected: BTreeSet<String> = monitor_ids.into_iter().collect();
        let mut inner = self.inner.write();
        let removed: Vec<String> = inner
            .connected_monitors
            .difference(&connected)
            .cloned()
            .collect();
        if inner.connected_monitors != connected {
            // Topology changes are meaningful content (spec §4.11).
            self.bump_content_version();
        }
        inner.connected_monitors = connected;
        for monitor_id in removed {
            if inner.monitors.remove(&monitor_id).is_some() {
                self.push_event_locked(
                    &mut inner,
                    Utc::now(),
                    LiveContextSource::System,
                    monitor_id,
                    "monitor disconnected".into(),
                    false,
                    0,
                );
            }
        }
    }

    /// Replace the foreground-window and coarse input-activity projections.
    ///
    /// **Privacy contract (spec §4.1):** payloads must arrive already
    /// scrubbed at collector emit — `title` through
    /// [`crate::senses::privacy::PrivacyFilter::scrub_text`] (or replaced
    /// by the redaction/sentinel literal), `process_name` through
    /// `scrub_path`. An [`PrivacyDisposition::Excluded`] window must be the
    /// sentinel observation (`process="[excluded]"`, empty title).
    pub fn record_context(
        &self,
        window: WindowWorldState,
        input_activity: InputActivityWorldState,
    ) -> u64 {
        debug_assert!(
            window.privacy != PrivacyDisposition::Excluded
                || (window.process_name == crate::senses::privacy::EXCLUDED_PROCESS
                    && window.title.is_empty()),
            "excluded window observations must be the §4.1 sentinel, got process={:?} title={:?}",
            window.process_name,
            window.title,
        );
        let summary = format!(
            "foreground={} active={} title={}",
            window.process_name, input_activity.active, window.title
        );
        let observed_at = window.observed_at;
        let mut inner = self.inner.write();
        // Content-version rule (spec §4.11): the 1 Hz no-change poll must
        // NOT bump — only a window process/title/privacy/call transition,
        // an identity/placement change (pid, exe path, monitor — v4), or
        // an activity flip counts as content. `idle_seconds` ticking up,
        // `active_since_secs` ticking up, and `observed_at` advancing are
        // bookkeeping, not content, so a static desktop publishes nothing.
        let content_changed = match (&inner.window, &inner.input_activity) {
            (Some(previous_window), Some(previous_activity)) => {
                previous_window.process_name != window.process_name
                    || previous_window.title != window.title
                    || previous_window.privacy != window.privacy
                    || previous_window.in_call != window.in_call
                    || previous_window.pid != window.pid
                    || previous_window.exe_path != window.exe_path
                    || previous_window.monitor_id != window.monitor_id
                    || previous_activity.active != input_activity.active
            }
            _ => true,
        };
        inner.window = Some(window);
        inner.input_activity = Some(input_activity);
        if content_changed {
            self.bump_content_version();
        } else if self.idle.load(Ordering::Acquire) {
            // Spec §4.11: no no-change ring events during idle.
            return self.sequence.load(Ordering::Acquire);
        }
        self.push_event_locked(
            &mut inner,
            observed_at,
            LiveContextSource::Window,
            "foreground".into(),
            summary,
            false,
            0,
        )
    }

    /// Replace the lightweight terminal/project projection.
    ///
    /// **Privacy contract (spec §4.1):** `project_root` and `project_name`
    /// must arrive already path-scrubbed
    /// ([`crate::senses::privacy::PrivacyFilter::scrub_path`]); `git_head`
    /// is a structured field and passes through unscrubbed by construction.
    ///
    /// Git-derived fields are owned by the git collector (Task A5): when
    /// the incoming projection targets the **same root** as the previous
    /// one, the previous git facts are carried forward (the context
    /// watcher never fills them); a root change clears them so stale git
    /// state from another project can never be projected.
    pub fn record_project(&self, mut project: ProjectWorldState) -> u64 {
        let source = if project.terminal_active {
            LiveContextSource::Terminal
        } else {
            LiveContextSource::Project
        };
        let observed_at = project.observed_at;
        let mut inner = self.inner.write();
        if let Some(previous) = &inner.project {
            if previous.project_root == project.project_root {
                project.git_head = previous.git_head.clone();
                project.branch = previous.branch.clone();
                project.dirty = previous.dirty;
                project.staged = previous.staged;
                project.untracked = previous.untracked;
                project.ahead = previous.ahead;
                project.behind = previous.behind;
                project.conflicts = previous.conflicts;
                project.last_commit_id = previous.last_commit_id.clone();
                project.last_commit_subject = previous.last_commit_subject.clone();
            }
        }
        let summary = format!(
            "project={} head={} terminal={}",
            project.project_name.as_deref().unwrap_or("unknown"),
            project.git_head.as_deref().unwrap_or("unknown"),
            project.terminal_process.as_deref().unwrap_or("inactive")
        );
        // Content-version rule (spec §4.11): project state changes are
        // content; a same-root republish that carried every field forward
        // unchanged (`observed_at` excluded — bookkeeping) is not.
        let content_changed = inner
            .project
            .as_ref()
            .map(|previous| !project_content_eq(previous, &project))
            .unwrap_or(true);
        inner.project = Some(project);
        if content_changed {
            self.bump_content_version();
        } else if self.idle.load(Ordering::Acquire) {
            // Spec §4.11: no no-change ring events during idle.
            return self.sequence.load(Ordering::Acquire);
        }
        self.push_event_locked(
            &mut inner,
            observed_at,
            source,
            "current-project".into(),
            summary,
            false,
            0,
        )
    }

    /// Merge git-collector facts into the current project projection
    /// (Task A5, spec §4.4). Touches only the git-derived fields; the
    /// identity fields stay the context watcher's. A no-op when no
    /// project is projected yet — the 1 Hz context poll will project one
    /// within a second and the collector republishes on its next change.
    ///
    /// **Privacy contract (spec §4.1):** `last_commit_subject` must
    /// arrive already scrubbed; `branch` and `last_commit_id` are
    /// structured fields, exempt by construction.
    pub fn record_git_facts(&self, observed_at: DateTime<Utc>, facts: ProjectGitFacts) -> u64 {
        let mut inner = self.inner.write();
        let Some(project) = inner.project.as_mut() else {
            return self.sequence.load(Ordering::Acquire);
        };
        // Content-version rule (spec §4.11): git state changes are
        // content; the collector emits on changes only, but a defensive
        // diff keeps an identical merge from churning the publisher.
        let content_changed = project.branch != facts.branch
            || project.dirty != facts.dirty
            || project.staged != facts.staged
            || project.untracked != facts.untracked
            || project.ahead != facts.ahead
            || project.behind != facts.behind
            || project.conflicts != facts.conflicts
            || project.last_commit_id != facts.last_commit_id
            || project.last_commit_subject != facts.last_commit_subject;
        let summary = format!(
            "git branch={} dirty={} staged={} untracked={} ahead={} behind={} conflicts={}",
            facts.branch.as_deref().unwrap_or("(detached)"),
            facts.dirty,
            facts.staged,
            facts.untracked,
            facts.ahead,
            facts.behind,
            facts.conflicts
        );
        project.git_head = facts
            .branch
            .clone()
            .or_else(|| facts.last_commit_id.clone());
        project.branch = facts.branch;
        project.dirty = facts.dirty;
        project.staged = facts.staged;
        project.untracked = facts.untracked;
        project.ahead = facts.ahead;
        project.behind = facts.behind;
        project.conflicts = facts.conflicts;
        project.last_commit_id = facts.last_commit_id;
        project.last_commit_subject = facts.last_commit_subject;
        if content_changed {
            self.bump_content_version();
        }
        self.push_event_locked(
            &mut inner,
            observed_at,
            LiveContextSource::Project,
            "current-project-git".into(),
            summary,
            false,
            0,
        )
    }

    /// Record explicit bounded-buffer degradation without stopping capture.
    pub fn record_capture_drop(&self, count: u64) {
        if count == 0 {
            return;
        }
        let mut inner = self.inner.write();
        inner.health.dropped_capture_events =
            inner.health.dropped_capture_events.saturating_add(count);
        self.push_event_locked(
            &mut inner,
            Utc::now(),
            LiveContextSource::System,
            "capture-buffer".into(),
            format!("bounded capture buffer dropped {count} oldest event(s)"),
            true,
            count,
        );
    }

    /// Record a downstream summary-channel drop.
    pub fn record_output_drop(&self, count: u64) {
        let mut inner = self.inner.write();
        inner.health.output_events_dropped =
            inner.health.output_events_dropped.saturating_add(count);
    }

    /// Record a capture failure while leaving the watcher alive.
    pub fn record_capture_failure(&self, error: impl Into<String>) {
        let error = error.into();
        let mut inner = self.inner.write();
        inner.health.capture_failures = inner.health.capture_failures.saturating_add(1);
        inner.health.last_error = Some(error.clone());
        self.push_event_locked(
            &mut inner,
            Utc::now(),
            LiveContextSource::System,
            "capture".into(),
            error,
            true,
            0,
        );
    }

    /// Mirror the runtime's live session state into the projection (Task
    /// C1, spec §4.8 publishing). Follows the hub's `record_*` contract:
    /// the content version is bumped **only** when something other than
    /// `updated` actually changed, so a session state that merely
    /// re-stamps itself causes no `live-context.json` write.
    ///
    /// Returns the current sequence (unchanged — a session-state mirror
    /// is not a source event and does not push into the event ring).
    pub fn record_session_state(&self, state: crate::context::session_state::SessionState) -> u64 {
        {
            let mut inner = self.inner.write();
            let changed = match &inner.session_state {
                Some(previous) => !session_content_eq(previous, &state),
                None => true,
            };
            inner.session_state = Some(state);
            if !changed {
                return self.sequence.load(Ordering::Acquire);
            }
        }
        self.bump_content_version();
        self.sequence.load(Ordering::Acquire)
    }

    /// Clone the complete bounded projection for serialization or agents.
    pub fn snapshot(&self) -> LiveWorldState {
        let inner = self.inner.read();
        LiveWorldState {
            schema_version: LIVE_CONTEXT_SCHEMA_VERSION,
            sequence: self.sequence.load(Ordering::Acquire),
            generated_at: Utc::now(),
            monitors: inner.monitors.values().cloned().collect(),
            window: inner.window.clone(),
            input_activity: inner.input_activity.clone(),
            project: inner.project.clone(),
            recent_events: inner.events.iter().cloned().collect(),
            health: inner.health.clone(),
            session_state: inner.session_state.clone(),
        }
    }

    // Keeping the serialized event fields explicit here makes every producer
    // pass source, ordering, and degradation metadata through one choke point.
    #[allow(clippy::too_many_arguments)]
    fn push_event_locked(
        &self,
        inner: &mut Projection,
        observed_at: DateTime<Utc>,
        source: LiveContextSource,
        source_id: String,
        summary: String,
        degraded: bool,
        dropped_before: u64,
    ) -> u64 {
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel) + 1;
        if inner.events.len() == self.event_capacity {
            inner.events.pop_front();
        }
        inner.events.push_back(LiveContextEvent {
            sequence,
            observed_at,
            source,
            source_id,
            summary: truncate_utf8(&summary, 240),
            degraded,
            dropped_before,
        });
        sequence
    }
}

impl Default for LiveContextHub {
    fn default() -> Self {
        Self::new(DEFAULT_EVENT_CAPACITY)
    }
}

/// Content equality for [`ProjectWorldState`] (spec §4.11): every field
/// except `observed_at`, which is poll bookkeeping rather than content.
fn project_content_eq(a: &ProjectWorldState, b: &ProjectWorldState) -> bool {
    a.terminal_active == b.terminal_active
        && a.terminal_process == b.terminal_process
        && a.project_root == b.project_root
        && a.project_name == b.project_name
        && a.git_head == b.git_head
        && a.branch == b.branch
        && a.dirty == b.dirty
        && a.staged == b.staged
        && a.untracked == b.untracked
        && a.ahead == b.ahead
        && a.behind == b.behind
        && a.conflicts == b.conflicts
        && a.last_commit_id == b.last_commit_id
        && a.last_commit_subject == b.last_commit_subject
}

/// Content equality for [`crate::context::session_state::SessionState`]
/// (Task C1): every field except `updated`, which is the hub's own
/// bookkeeping stamp rather than content — the same rule
/// `SessionStateHub::mutate` applies to its content version.
fn session_content_eq(
    a: &crate::context::session_state::SessionState,
    b: &crate::context::session_state::SessionState,
) -> bool {
    let mut probe = b.clone();
    probe.updated = a.updated;
    *a == probe
}

/// Atomically persist an agent-facing world-state snapshot.
pub fn write_snapshot(path: &Path, snapshot: &LiveWorldState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(snapshot)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Publish without blocking producers, so slow disk I/O cannot pause capture.
///
/// Writes are keyed on the hub's content-version counter (spec §4.11):
/// a tick whose version matches the last successfully published one is
/// skipped entirely, so a static screen — and idle mode in particular —
/// causes zero disk churn. The version is read *before* the snapshot so
/// a change racing the clone is republished on the next tick rather
/// than missed.
pub fn spawn_publisher(
    hub: LiveContextHub,
    path: std::path::PathBuf,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(100)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut published_version: Option<u64> = None;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let version = hub.content_version();
                    if published_version == Some(version) {
                        continue;
                    }
                    let snapshot = hub.snapshot();
                    let path = path.clone();
                    let result = tokio::task::spawn_blocking(move || write_snapshot(&path, &snapshot)).await;
                    match result {
                        Ok(Ok(())) => {
                            published_version = Some(version);
                        }
                        Ok(Err(error)) => tracing::warn!(layer = "senses", component = "live_context", error = %error, "Failed to publish live world-state"),
                        Err(error) => tracing::warn!(layer = "senses", component = "live_context", error = %error, "Live world-state publisher task failed"),
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });
}

fn truncate_utf8(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 1 {
        return "…".chars().take(max_chars).collect();
    }
    let mut out: String = value.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: &str, capture_sequence: u64) -> MonitorWorldState {
        MonitorWorldState {
            monitor_id: id.into(),
            name: id.into(),
            is_primary: id == "display-1",
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            capture_event_sequence: capture_sequence,
            capture_sequence,
            captured_at: Utc::now(),
            change_score: 0.2,
            meaningful_change: true,
            description: "code editor".into(),
            confidence: 0.8,
            vision_updated_at: Some(Utc::now()),
            privacy: PrivacyDisposition::Visible,
            target_interval_ms: 200,
            capture_latency_ms: 4,
            dropped_before: 0,
        }
    }

    #[test]
    fn events_are_globally_ordered_and_bounded() {
        let hub = LiveContextHub::new(2);
        hub.set_connected_monitors(["display-1".into(), "display-2".into()]);
        hub.record_monitor_capture(monitor("display-1", 1));
        hub.record_monitor_capture(monitor("display-2", 1));
        hub.record_capture_drop(1);
        let snapshot = hub.snapshot();
        let sequences: Vec<u64> = snapshot.recent_events.iter().map(|e| e.sequence).collect();
        assert_eq!(sequences.len(), 2);
        assert!(sequences[0] < sequences[1]);
        assert_eq!(snapshot.monitors.len(), 2);
        assert_eq!(snapshot.health.dropped_capture_events, 1);
    }

    /// M2: the cheap per-frame accessors must agree with the full
    /// projection clone they replace.
    #[test]
    fn cheap_counters_match_the_full_snapshot() {
        let hub = LiveContextHub::new(4);
        hub.set_connected_monitors(["display-1".into(), "display-2".into()]);
        hub.record_monitor_capture(monitor("display-1", 1));
        hub.record_monitor_capture(monitor("display-2", 1));
        hub.record_capture_drop(2);
        let snapshot = hub.snapshot();
        assert_eq!(hub.monitor_count(), snapshot.monitors.len());
        let health = hub.health();
        assert_eq!(health.capture_events, snapshot.health.capture_events);
        assert_eq!(
            health.dropped_capture_events,
            snapshot.health.dropped_capture_events
        );
        assert_eq!(health.last_capture_at, snapshot.health.last_capture_at);
    }

    #[test]
    fn compact_context_is_source_attributed_and_bounded() {
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into()]);
        hub.record_monitor_capture(monitor("display-1", 1));
        let text = hub.snapshot().compact_for_agents(180);
        assert!(text.contains("[monitor:display-1]"));
        assert!(text.chars().count() <= 180);
    }

    /// Task C1: the mirrored session state reaches the snapshot, survives
    /// a JSON round-trip, and bumps the content version only on a real
    /// content change (`updated` alone is bookkeeping).
    #[test]
    fn session_state_mirror_round_trips_and_versions_on_content() {
        use crate::context::session_state::SessionState;

        let hub = LiveContextHub::default();
        assert!(hub.snapshot().session_state.is_none());

        let mut state = SessionState {
            active_project: Some("continuum".into()),
            current_task: Some("publish session state".into()),
            confidence: 0.8,
            ..SessionState::default()
        };
        hub.record_session_state(state.clone());
        let v1 = hub.content_version();
        assert_eq!(
            hub.snapshot().session_state.as_ref().map(|s| s.confidence),
            Some(0.8)
        );

        // Same content, new `updated` stamp → no version bump, no write.
        state.updated += chrono::Duration::seconds(30);
        hub.record_session_state(state.clone());
        assert_eq!(hub.content_version(), v1);

        // Real content change → bump.
        state.current_task = Some("write the tests".into());
        hub.record_session_state(state.clone());
        assert_eq!(hub.content_version(), v1 + 1);

        let snapshot = hub.snapshot();
        assert_eq!(snapshot.schema_version, LIVE_CONTEXT_SCHEMA_VERSION);
        let json = serde_json::to_string(&snapshot).unwrap();
        let parsed: LiveWorldState = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.session_state.and_then(|s| s.current_task).as_deref(),
            Some("write the tests")
        );
    }

    /// Task C1: a v2 document (written before `session_state` existed)
    /// still parses — the field is serde-defaulted.
    #[test]
    fn legacy_v2_document_parses_without_session_state() {
        let hub = LiveContextHub::default();
        let mut value = serde_json::to_value(hub.snapshot()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("session_state")
            .expect("field present in v3 output");
        value["schema_version"] = serde_json::json!(2);
        let parsed: LiveWorldState = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.schema_version, 2);
        assert!(parsed.session_state.is_none());
    }

    /// Task C3: the v4 window enrichment is additive — a v3 document
    /// (written before `pid`/`exe_path`/`monitor_id`/`active_since_secs`
    /// existed on the window projection) still parses, defaulting them.
    #[test]
    fn legacy_v3_window_without_enrichment_parses() {
        let hub = LiveContextHub::default();
        hub.record_context(window_state("code.exe", "main.rs"), activity(0));
        let mut value = serde_json::to_value(hub.snapshot()).unwrap();
        let window = value["window"].as_object_mut().expect("window object");
        for field in ["pid", "exe_path", "monitor_id", "active_since_secs"] {
            window.remove(field).expect("field present in v4 output");
        }
        value["schema_version"] = serde_json::json!(3);
        let parsed: LiveWorldState = serde_json::from_value(value).unwrap();
        let parsed_window = parsed.window.expect("window survives");
        assert_eq!(parsed_window.process_name, "code.exe");
        assert!(parsed_window.pid.is_none());
        assert!(parsed_window.exe_path.is_none());
        assert!(parsed_window.monitor_id.is_none());
        assert_eq!(parsed_window.active_since_secs, 0);
    }

    /// Task C3: `active_since_secs` is bookkeeping, not content — a poll
    /// that only advances the dwell counter must not bump the content
    /// version (spec §4.11), or a focused window would republish
    /// `live-context.json` every single second. A monitor change is
    /// content and does bump.
    #[test]
    fn window_dwell_ticks_do_not_bump_the_content_version() {
        let hub = LiveContextHub::default();
        hub.record_context(window_state("code.exe", "main.rs"), activity(0));
        let baseline = hub.content_version();

        let mut ticked = window_state("code.exe", "main.rs");
        ticked.active_since_secs = 900;
        hub.record_context(ticked, activity(1));
        assert_eq!(
            hub.content_version(),
            baseline,
            "a dwell tick is bookkeeping, not content"
        );

        let mut moved = window_state("code.exe", "main.rs");
        moved.monitor_id = Some("display-2".into());
        hub.record_context(moved, activity(1));
        assert_eq!(
            hub.content_version(),
            baseline + 1,
            "moving the window to another monitor is content"
        );
    }

    /// Task C1: the compact render carries project/goal/task, and the
    /// §4.1 cloud gate generalizes `local_only` inferred fields.
    #[test]
    fn compact_render_includes_a_cloud_gated_session_line() {
        use crate::context::session_state::{SessionState, PRIVATE_CONTEXT_PHRASE};

        let hub = LiveContextHub::default();
        hub.record_session_state(SessionState {
            active_project: Some("continuum".into()),
            current_goal: Some("ship the context engine".into()),
            current_task: Some("publish session state".into()),
            confidence: 0.75,
            ..SessionState::default()
        });
        let text = hub.snapshot().compact_for_agents(4_000);
        assert!(
            text.contains("[session:current] project=continuum"),
            "{text}"
        );
        assert!(text.contains("goal=\"ship the context engine\""), "{text}");
        assert!(text.contains("task=\"publish session state\""), "{text}");
        assert!(text.contains("confidence=0.75"), "{text}");

        hub.record_session_state(SessionState {
            active_project: Some("continuum".into()),
            current_goal: Some("ship the context engine".into()),
            current_task: Some("publish session state".into()),
            confidence: 0.75,
            local_only: true,
            ..SessionState::default()
        });
        let gated = hub.snapshot().compact_for_agents(4_000);
        assert!(gated.contains(PRIVATE_CONTEXT_PHRASE), "{gated}");
        assert!(!gated.contains("publish session state"), "{gated}");
        // The mechanical project id is not an inferred field — it stays.
        assert!(gated.contains("project=continuum"), "{gated}");
    }

    /// Task C1: an empty session state adds no line at all (no noise in
    /// the packager's budget before the first inference).
    #[test]
    fn compact_render_omits_an_empty_session_line() {
        use crate::context::session_state::SessionState;

        let hub = LiveContextHub::default();
        hub.record_session_state(SessionState::default());
        assert!(!hub
            .snapshot()
            .compact_for_agents(4_000)
            .contains("[session:"));
    }

    #[test]
    fn disconnected_monitor_leaves_current_projection() {
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into()]);
        hub.record_monitor_capture(monitor("display-1", 1));
        hub.set_connected_monitors(Vec::new());
        let snapshot = hub.snapshot();
        assert!(snapshot.monitors.is_empty());
        assert!(snapshot
            .recent_events
            .last()
            .is_some_and(|event| event.summary == "monitor disconnected"));
    }

    #[test]
    fn slow_capture_reports_a_cadence_miss() {
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into()]);
        let mut slow = monitor("display-1", 1);
        slow.capture_latency_ms = 250;
        hub.record_monitor_capture(slow);
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.health.capture_deadline_misses, 1);
        assert!(snapshot
            .recent_events
            .last()
            .is_some_and(|event| event.degraded));
    }

    #[test]
    fn health_restart_only_after_stall_or_repeated_boot_failures() {
        let mut health = LiveContextHealth::default();
        assert!(!health.should_restart(Utc::now(), false, Duration::from_millis(200)));
        health.capture_failures = 3;
        assert!(health.should_restart(Utc::now(), true, Duration::from_millis(200)));
        health.last_capture_at = Some(Utc::now());
        assert!(!health.should_restart(Utc::now(), true, Duration::from_millis(200)));
    }

    #[test]
    fn strictest_disposition_orders_excluded_over_redacted_over_visible() {
        use PrivacyDisposition::*;
        assert_eq!(strictest_disposition(Visible, Visible), Visible);
        assert_eq!(strictest_disposition(Visible, Redacted), Redacted);
        assert_eq!(strictest_disposition(Redacted, Visible), Redacted);
        assert_eq!(strictest_disposition(Redacted, Excluded), Excluded);
        assert_eq!(strictest_disposition(Excluded, Visible), Excluded);
    }

    #[test]
    fn monitor_privacy_combines_foreground_and_sweep_zone() {
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into(), "display-2".into()]);
        hub.record_monitor_capture(monitor("display-1", 1));
        hub.record_context(
            WindowWorldState {
                process_name: "code.exe".into(),
                title: "main.rs".into(),
                observed_at: Utc::now(),
                in_call: false,
                privacy: PrivacyDisposition::Visible,
                pid: None,
                exe_path: None,
                monitor_id: None,
                active_since_secs: 0,
            },
            InputActivityWorldState {
                observed_at: Utc::now(),
                idle_seconds: 0,
                active: true,
            },
        );
        // No sweep zones yet: everything follows the foreground (Visible).
        assert_eq!(
            hub.monitor_privacy("display-1"),
            PrivacyDisposition::Visible
        );
        // A never_observe window on display-2 tightens ONLY that monitor.
        hub.set_monitor_zones(BTreeMap::from([("display-2".into(), Zone::NeverObserve)]));
        assert_eq!(
            hub.monitor_privacy("display-1"),
            PrivacyDisposition::Visible
        );
        assert_eq!(
            hub.monitor_privacy("display-2"),
            PrivacyDisposition::Excluded
        );
        assert_eq!(hub.monitor_zone("display-2"), Zone::NeverObserve);
        assert_eq!(hub.monitor_zone("display-1"), Zone::CloudAllowed);
        // A redacted foreground window tightens every monitor (monotonic:
        // the sweep never relaxes the legacy foreground rule).
        hub.record_context(
            WindowWorldState {
                process_name: "chrome.exe".into(),
                title: "[redacted sensitive window]".into(),
                observed_at: Utc::now(),
                in_call: false,
                privacy: PrivacyDisposition::Redacted,
                pid: None,
                exe_path: None,
                monitor_id: None,
                active_since_secs: 0,
            },
            InputActivityWorldState {
                observed_at: Utc::now(),
                idle_seconds: 0,
                active: true,
            },
        );
        assert_eq!(
            hub.monitor_privacy("display-1"),
            PrivacyDisposition::Redacted
        );
        assert_eq!(
            hub.monitor_privacy("display-2"),
            PrivacyDisposition::Excluded
        );
    }

    #[test]
    fn excluded_monitor_capture_has_no_caption() {
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into()]);
        let mut excluded = monitor("display-1", 1);
        excluded.privacy = PrivacyDisposition::Excluded;
        hub.record_monitor_capture(excluded);
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.monitors[0].description, "");
        assert!(snapshot.monitors[0].vision_updated_at.is_none());
        let compact = snapshot.compact_for_agents(4_000);
        assert!(
            compact.contains("vision=\"\""),
            "excluded monitor must render an empty caption: {compact}"
        );
        assert!(!compact.contains("code editor"));
    }

    #[test]
    fn monitor_geometries_reflect_projected_monitors() {
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into()]);
        hub.record_monitor_capture(monitor("display-1", 1));
        let geometries = hub.monitor_geometries();
        assert_eq!(geometries.len(), 1);
        assert_eq!(geometries[0].monitor_id, "display-1");
        assert_eq!(geometries[0].width, 1920);
        assert_eq!(geometries[0].height, 1080);
    }

    fn project_state(root: Option<&str>) -> ProjectWorldState {
        ProjectWorldState {
            observed_at: Utc::now(),
            terminal_active: false,
            terminal_process: None,
            project_root: root.map(str::to_string),
            project_name: root.map(|_| "proj".to_string()),
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

    #[test]
    fn git_facts_merge_and_carry_forward_for_same_root() {
        // Task A5 (spec §4.4): the git collector owns the git fields; the
        // 1 Hz context poll must not wipe them, and a root change must.
        let hub = LiveContextHub::default();
        // No project projected yet: merging is a no-op, not a panic.
        hub.record_git_facts(Utc::now(), ProjectGitFacts::default());
        assert!(hub.snapshot().project.is_none());

        hub.record_project(project_state(Some("C:\\repos\\proj")));
        hub.record_git_facts(
            Utc::now(),
            ProjectGitFacts {
                branch: Some("feat/x".into()),
                dirty: 2,
                staged: 1,
                last_commit_id: Some("abc1234def".into()),
                last_commit_subject: Some("fix things".into()),
                ..ProjectGitFacts::default()
            },
        );
        let project = hub.snapshot().project.expect("project projected");
        assert_eq!(project.branch.as_deref(), Some("feat/x"));
        assert_eq!(project.git_head.as_deref(), Some("feat/x"));
        assert_eq!(project.dirty, 2);
        assert_eq!(project.last_commit_subject.as_deref(), Some("fix things"));

        // Context-watcher republish for the SAME root: git facts carried.
        hub.record_project(project_state(Some("C:\\repos\\proj")));
        let project = hub.snapshot().project.expect("project projected");
        assert_eq!(project.branch.as_deref(), Some("feat/x"));
        assert_eq!(project.staged, 1);
        assert_eq!(project.last_commit_id.as_deref(), Some("abc1234def"));

        // Root change: stale git facts from another project are cleared.
        hub.record_project(project_state(Some("C:\\repos\\other")));
        let project = hub.snapshot().project.expect("project projected");
        assert_eq!(project.branch, None);
        assert_eq!(project.git_head, None);
        assert_eq!(project.dirty, 0);
        assert_eq!(project.last_commit_id, None);
    }

    #[test]
    fn detached_git_facts_fall_back_to_commit_id_for_git_head() {
        let hub = LiveContextHub::default();
        hub.record_project(project_state(Some("C:\\repos\\proj")));
        hub.record_git_facts(
            Utc::now(),
            ProjectGitFacts {
                branch: None,
                last_commit_id: Some("deadbeef1234".into()),
                ..ProjectGitFacts::default()
            },
        );
        let project = hub.snapshot().project.expect("project projected");
        assert_eq!(project.git_head.as_deref(), Some("deadbeef1234"));
        assert_eq!(project.branch, None);
    }

    fn unchanged_monitor(id: &str, capture_sequence: u64) -> MonitorWorldState {
        MonitorWorldState {
            meaningful_change: false,
            change_score: 0.0,
            description: String::new(),
            ..monitor(id, capture_sequence)
        }
    }

    fn window_state(process: &str, title: &str) -> WindowWorldState {
        WindowWorldState {
            process_name: process.into(),
            title: title.into(),
            observed_at: Utc::now(),
            in_call: false,
            privacy: PrivacyDisposition::Visible,
            pid: Some(4242),
            exe_path: Some(format!("C:\\apps\\{process}")),
            monitor_id: Some("display-1".into()),
            active_since_secs: 7,
        }
    }

    fn activity(idle_seconds: u64) -> InputActivityWorldState {
        InputActivityWorldState {
            observed_at: Utc::now(),
            idle_seconds,
            active: idle_seconds < 2,
        }
    }

    /// Spec §4.11 content-version bump matrix (Task A8): meaningful
    /// capture bumps, unchanged-capture bookkeeping doesn't, the 1 Hz
    /// no-change context poll doesn't, a title change does.
    #[test]
    fn content_version_bumps_only_on_meaningful_changes() {
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into()]);
        let v0 = hub.content_version();
        assert!(v0 > 0, "initial topology change is content");

        // Meaningful capture bumps.
        hub.record_monitor_capture(monitor("display-1", 1));
        let v1 = hub.content_version();
        assert!(v1 > v0, "meaningful capture must bump");

        // Unchanged-capture bookkeeping does NOT bump.
        hub.record_monitor_capture(unchanged_monitor("display-1", 2));
        assert_eq!(hub.content_version(), v1, "unchanged capture must not bump");

        // First context poll is content (no previous), repeat identical
        // polls are not — idle_seconds ticking is bookkeeping.
        hub.record_context(window_state("code.exe", "main.rs"), activity(0));
        let v2 = hub.content_version();
        assert!(v2 > v1);
        hub.record_context(window_state("code.exe", "main.rs"), activity(1));
        assert_eq!(
            hub.content_version(),
            v2,
            "identical 1 Hz context poll must not bump"
        );

        // A title change is content.
        hub.record_context(window_state("code.exe", "lib.rs"), activity(1));
        let v3 = hub.content_version();
        assert!(v3 > v2, "title change must bump");

        // Vision caption update bumps.
        hub.record_monitor_vision(
            "display-1",
            "terminal output".into(),
            0.9,
            Some(Utc::now()),
            PrivacyDisposition::Visible,
            true,
        );
        let v4 = hub.content_version();
        assert!(v4 > v3, "vision caption update must bump");

        // Identical zone replacement doesn't bump; a zone change does.
        hub.set_monitor_zones(BTreeMap::new());
        assert_eq!(hub.content_version(), v4);
        hub.set_monitor_zones(BTreeMap::from([("display-1".into(), Zone::LocalOnly)]));
        let v5 = hub.content_version();
        assert!(v5 > v4, "zone change must bump");

        // Same-content project republish doesn't bump; a change does.
        hub.record_project(project_state(Some("C:\\repos\\proj")));
        let v6 = hub.content_version();
        assert!(v6 > v5, "first project projection is content");
        hub.record_project(project_state(Some("C:\\repos\\proj")));
        assert_eq!(
            hub.content_version(),
            v6,
            "same-root same-content republish must not bump"
        );
        hub.record_project(project_state(Some("C:\\repos\\other")));
        assert!(hub.content_version() > v6, "project switch must bump");

        // Git facts: change bumps, identical merge doesn't.
        let facts = ProjectGitFacts {
            branch: Some("main".into()),
            dirty: 1,
            ..ProjectGitFacts::default()
        };
        hub.record_git_facts(Utc::now(), facts.clone());
        let v7 = hub.content_version();
        hub.record_git_facts(Utc::now(), facts);
        assert_eq!(
            hub.content_version(),
            v7,
            "identical git merge must not bump"
        );
        hub.record_git_facts(
            Utc::now(),
            ProjectGitFacts {
                branch: Some("main".into()),
                dirty: 2,
                ..ProjectGitFacts::default()
            },
        );
        assert!(hub.content_version() > v7, "dirty-count change must bump");
    }

    /// Spec §4.11: while idle, unchanged-capture and no-change context
    /// polls push no ring events (the bounded ring must not fill with
    /// idle churn); meaningful changes still push.
    #[test]
    fn idle_suppresses_unchanged_ring_events() {
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into()]);
        hub.record_monitor_capture(monitor("display-1", 1));
        hub.record_context(window_state("code.exe", "main.rs"), activity(500));
        hub.set_idle(true);

        let before = hub.snapshot().recent_events.len();
        hub.record_monitor_capture(unchanged_monitor("display-1", 2));
        hub.record_context(window_state("code.exe", "main.rs"), activity(501));
        let snapshot = hub.snapshot();
        assert_eq!(
            snapshot.recent_events.len(),
            before,
            "idle unchanged captures/polls must push no ring events"
        );
        // Bookkeeping still updated: latest capture metadata + counters.
        assert_eq!(snapshot.health.capture_events, 2);
        assert_eq!(snapshot.monitors[0].capture_sequence, 2);
        assert_eq!(
            snapshot.input_activity.as_ref().map(|a| a.idle_seconds),
            Some(501)
        );

        // Meaningful changes still push while idle (unattended-error
        // detection stays alive — spec §4.11).
        hub.record_monitor_capture(monitor("display-1", 3));
        assert_eq!(hub.snapshot().recent_events.len(), before + 1);

        hub.set_idle(false);
        hub.record_monitor_capture(unchanged_monitor("display-1", 4));
        assert_eq!(
            hub.snapshot().recent_events.len(),
            before + 2,
            "active-mode unchanged captures keep pushing (existing behavior)"
        );
    }

    /// Spec §4.11: the publisher writes only when the content version
    /// moved since its last successful write.
    #[tokio::test]
    async fn publisher_skips_writes_when_content_is_unchanged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("live-context.json");
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into()]);
        hub.record_monitor_capture(monitor("display-1", 1));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        spawn_publisher(
            hub.clone(),
            path.clone(),
            Duration::from_millis(50),
            shutdown_rx,
        );

        // First publish.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !path.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(path.exists(), "first tick must publish");
        let first = std::fs::read(&path).expect("read first publish");

        // No content change → no rewrite (generated_at would differ if a
        // write happened, so byte equality proves the skip).
        tokio::time::sleep(Duration::from_millis(300)).await;
        let unchanged = std::fs::read(&path).expect("read after quiet period");
        assert_eq!(
            first, unchanged,
            "no-change ticks must not rewrite the file"
        );

        // A meaningful change → republished.
        hub.record_monitor_capture(monitor("display-1", 2));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut republished = std::fs::read(&path).expect("read after change");
        while republished == first && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            republished = std::fs::read(&path).expect("read after change");
        }
        assert_ne!(republished, first, "content change must republish");
        let _ = shutdown_tx.send(true);
    }

    #[test]
    fn snapshot_publication_replaces_previous_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("live-context.json");
        let hub = LiveContextHub::default();
        write_snapshot(&path, &hub.snapshot()).expect("first snapshot");
        hub.set_connected_monitors(["display-2".into()]);
        hub.record_monitor_capture(monitor("display-2", 1));
        write_snapshot(&path, &hub.snapshot()).expect("replacement snapshot");
        let published: LiveWorldState =
            serde_json::from_slice(&std::fs::read(&path).expect("read replacement snapshot"))
                .expect("decode replacement snapshot");
        assert_eq!(published.monitors.len(), 1);
        assert!(!path.with_extension("json.tmp").exists());
    }

    // --- Cloud gate (spec §5.1 / §4.1 propagation) ---

    fn cloud_gate_filter() -> PrivacyFilter {
        PrivacyFilter::from_config(
            &crate::config::ContextConfig::default(),
            &crate::config::PrivacyConfig::default(),
        )
        .with_environment(
            Some("C:\\Users\\testuser".to_string()),
            Some("testuser".to_string()),
        )
    }

    /// A world state carrying one secret in every free-text field the
    /// gate is responsible for.
    fn leaky_state() -> LiveWorldState {
        let now = Utc::now();
        let mut visible = monitor("display-1", 1);
        visible.description = "terminal shows ghp_AbCd1234EfGh5678IjKl9012MnOp3456QrSt".into();
        let mut redacted = monitor("display-2", 1);
        redacted.privacy = PrivacyDisposition::Redacted;
        redacted.description = "banking dashboard, balance visible".into();
        let mut excluded = monitor("display-3", 1);
        excluded.privacy = PrivacyDisposition::Excluded;
        excluded.description = "vault contents".into();
        LiveWorldState {
            schema_version: LIVE_CONTEXT_SCHEMA_VERSION,
            sequence: 7,
            generated_at: now,
            monitors: vec![visible, redacted, excluded],
            window: Some(WindowWorldState {
                process_name: "code.exe".into(),
                title: "deploy.sh — sk-ant-api03-AbCdEf0123456789AbCdEf0123456789".into(),
                observed_at: now,
                in_call: false,
                privacy: PrivacyDisposition::Visible,
                pid: Some(1234),
                exe_path: Some("C:\\Users\\testuser\\bin\\code.exe".into()),
                monitor_id: Some("display-1".into()),
                active_since_secs: 42,
            }),
            input_activity: None,
            project: Some(ProjectWorldState {
                observed_at: now,
                terminal_active: true,
                terminal_process: Some("pwsh.exe".into()),
                project_root: Some("C:\\Users\\testuser\\code\\continuum".into()),
                project_name: Some("continuum".into()),
                git_head: Some("main".into()),
                branch: Some("main".into()),
                dirty: 1,
                staged: 0,
                untracked: 0,
                ahead: 0,
                behind: 0,
                conflicts: 0,
                // Structured field: must survive verbatim.
                last_commit_id: Some("3f785ea1b9c2d4e6a8b0c2d4e6f8a0b2c4d6e8f0".into()),
                last_commit_subject: Some("fix: stop logging AKIAIOSFODNN7EXAMPLE".into()),
            }),
            recent_events: vec![LiveContextEvent {
                sequence: 1,
                observed_at: now,
                source: LiveContextSource::Window,
                source_id: "foreground".into(),
                summary: "foreground=code.exe active=true title=Bearer abcdefghij1234567890XYZ"
                    .into(),
                degraded: false,
                dropped_before: 0,
            }],
            health: LiveContextHealth {
                last_error: Some("capture failed for C:\\Users\\testuser\\tmp".into()),
                ..LiveContextHealth::default()
            },
            session_state: None,
        }
    }

    #[test]
    fn cloud_view_scrubs_every_free_text_field() {
        let filter = cloud_gate_filter();
        let gated = leaky_state().cloud_view(&filter);
        let json = serde_json::to_string(&gated).expect("serialize gated state");
        for secret in [
            "ghp_AbCd1234EfGh5678IjKl9012MnOp3456QrSt",
            "sk-ant-api03-AbCdEf0123456789AbCdEf0123456789",
            "AKIAIOSFODNN7EXAMPLE",
            "abcdefghij1234567890XYZ",
            "banking dashboard",
            "vault contents",
        ] {
            assert!(!json.contains(secret), "secret {secret:?} survived: {json}");
        }
        // Excluded monitor keeps the sentinel: no caption, not even a marker.
        assert_eq!(gated.monitors[2].description, "");
        // Redacted monitor collapses to the local-only literal.
        assert_eq!(gated.monitors[1].description, REDACTED_LOCAL_ONLY);
        // Structured git fields survive by construction.
        let project = gated.project.as_ref().expect("project");
        assert_eq!(
            project.last_commit_id.as_deref(),
            Some("3f785ea1b9c2d4e6a8b0c2d4e6f8a0b2c4d6e8f0")
        );
        assert_eq!(project.branch.as_deref(), Some("main"));
        // Home directory redacted in paths, structure preserved.
        assert_eq!(
            project.project_root.as_deref(),
            Some("~\\code\\continuum"),
            "project root must be path-scrubbed"
        );
    }

    #[test]
    fn cloud_view_applies_the_sentinel_to_a_never_observe_window() {
        let filter = cloud_gate_filter();
        let mut state = leaky_state();
        // 1password.exe is a legacy sensitive process → never_observe.
        if let Some(window) = &mut state.window {
            window.process_name = "1password.exe".into();
            window.title = "Personal Vault — my seed phrase".into();
            window.in_call = true;
            window.privacy = PrivacyDisposition::Visible; // stale disposition
        }
        let gated = state.cloud_view(&filter);
        let window = gated.window.as_ref().expect("window");
        assert_eq!(window.process_name, EXCLUDED_PROCESS);
        assert_eq!(window.title, EXCLUDED_TITLE);
        assert!(!window.in_call);
        assert_eq!(window.privacy, PrivacyDisposition::Excluded);
    }

    #[test]
    fn cloud_view_strips_local_only_window_titles_but_keeps_the_process() {
        let filter = cloud_gate_filter();
        let mut state = leaky_state();
        if let Some(window) = &mut state.window {
            window.process_name = "msedge.exe".into();
            window.title = "Docs - InPrivate".into();
            window.privacy = PrivacyDisposition::Visible;
        }
        let gated = state.cloud_view(&filter);
        let window = gated.window.as_ref().expect("window");
        assert_eq!(window.process_name, "msedge.exe");
        assert_eq!(window.title, REDACTED_LOCAL_ONLY);
        assert_eq!(window.privacy, PrivacyDisposition::Redacted);
    }

    #[test]
    fn cloud_view_never_relaxes_a_recorded_disposition() {
        // The recorded disposition is folded into the lattice: a window the
        // producer marked Redacted stays redacted even when no live rule
        // matches it any more.
        let filter = cloud_gate_filter();
        let mut state = leaky_state();
        if let Some(window) = &mut state.window {
            window.process_name = "chrome.exe".into();
            window.title = "Nothing sensitive here".into();
            window.privacy = PrivacyDisposition::Redacted;
        }
        let gated = state.cloud_view(&filter);
        let window = gated.window.as_ref().expect("window");
        assert_eq!(window.title, REDACTED_LOCAL_ONLY);
        assert_eq!(window.privacy, PrivacyDisposition::Redacted);
    }

    #[test]
    fn cloud_view_generalizes_local_only_session_state_and_scrubs_the_rest() {
        use crate::context::session_state::{SessionState, StampedText, PRIVATE_CONTEXT_PHRASE};

        let filter = cloud_gate_filter();
        let mut state = leaky_state();
        state.session_state = Some(SessionState {
            active_project: Some("continuum".into()),
            current_goal: Some("ship the context engine".into()),
            current_task: Some("wire the privacy retrofit".into()),
            active_app: Some("C:\\Users\\testuser\\bin\\code.exe".into()),
            window_title: Some("token ghp_AbCd1234EfGh5678IjKl9012MnOp3456QrSt".into()),
            open_files: vec!["C:\\Users\\testuser\\code\\main.rs".into()],
            last_error: Some(StampedText::new(
                "build failed: AKIAIOSFODNN7EXAMPLE",
                Utc::now(),
            )),
            last_success: None,
            last_user_command: None,
            confidence: 0.8,
            local_only: true,
            ..SessionState::default()
        });
        let gated = state.cloud_view(&filter);
        let session = gated.session_state.as_ref().expect("session state");
        assert_eq!(
            session.current_goal.as_deref(),
            Some(PRIVATE_CONTEXT_PHRASE)
        );
        assert_eq!(
            session.current_task.as_deref(),
            Some(PRIVATE_CONTEXT_PHRASE)
        );
        assert_eq!(session.active_app.as_deref(), Some("~\\bin\\code.exe"));
        assert_eq!(session.open_files, vec!["~\\code\\main.rs".to_string()]);
        let json = serde_json::to_string(session).expect("serialize session");
        assert!(!json.contains("ghp_"), "session leaked a token: {json}");
        assert!(
            !json.contains("AKIAIOSFODNN7EXAMPLE"),
            "session leaked a key: {json}"
        );
    }

    #[test]
    fn cloud_view_is_idempotent() {
        let filter = cloud_gate_filter();
        let once = leaky_state().cloud_view(&filter);
        let twice = once.cloud_view(&filter);
        assert_eq!(
            serde_json::to_string(&once).expect("once"),
            serde_json::to_string(&twice).expect("twice"),
            "the cloud gate must be idempotent"
        );
    }

    #[test]
    fn compact_of_a_gated_state_carries_no_secrets() {
        let filter = cloud_gate_filter();
        let compact = leaky_state().cloud_view(&filter).compact_for_agents(4_000);
        for secret in [
            "ghp_AbCd1234",
            "sk-ant-api03",
            "AKIAIOSFODNN7EXAMPLE",
            "banking dashboard",
            "vault contents",
        ] {
            assert!(
                !compact.contains(secret),
                "compact leaked {secret:?}: {compact}"
            );
        }
    }
}
