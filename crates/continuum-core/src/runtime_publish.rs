//! # Runtime state publisher
//!
//! The `continuum` binary and the desktop dashboard run as separate processes
//! (because the runtime pulls in llama-cpp-sys-2, which has Windows build
//! quirks we don't want dragged into the Tauri build). Their shared
//! surface is this tiny JSON blob: the runtime writes it every few
//! seconds, the dashboard's `runtime_bridge` reads it.
//!
//! Keep this struct **small**. Every field the runtime writes here must
//! be cheap to compute on a hot loop. Bulk data (logs, memory) goes
//! through dedicated channels.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::hardware::{HardwareSpecs, ResolvedResourcePlan};
use crate::operational_state::{ComponentDiagnostic, OperationalEvent, OperationalState};
use crate::runtime_control::RuntimeServiceSnapshot;

/// The shared shape the runtime writes and the dashboard reads. This is
/// the single source of truth for the state.json contract — both the
/// `continuum` binary and the `continuum-desktop` bridge serialise against this
/// struct. Any new runtime telemetry field goes here, not in a parallel
/// definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    #[serde(default)]
    pub triage_model_loaded: bool,
    #[serde(default)]
    pub vision_model_loaded: bool,
    #[serde(default)]
    pub tts_loaded: bool,
    #[serde(default)]
    pub stt_loaded: bool,
    #[serde(default)]
    pub orchestrator_ready: bool,
    #[serde(default)]
    pub voice_mode: Option<String>,
    #[serde(default)]
    pub partial_transcript: Option<String>,
    /// Master playback gain currently applied by the running voice process.
    /// `None` keeps older runtime snapshots backwards-compatible.
    #[serde(default)]
    pub voice_volume: Option<f32>,
    /// Number of TTS utterances waiting for synthesis/playback, including the
    /// utterance currently being processed.
    #[serde(default)]
    pub tts_queue_len: Option<usize>,
    /// Whether call detection is actively suppressing voice output.
    #[serde(default)]
    pub ambient_mute_active: Option<bool>,
    /// Foreground process that caused ambient mute to activate.
    #[serde(default)]
    pub detected_call_app: Option<String>,
    /// Wake-word setting applied by this runtime process at boot.
    #[serde(default)]
    pub wake_word_enabled: Option<bool>,
    /// Active voice front-end mode (`"pipeline"` / `"moshi"`). `None` on
    /// snapshots from runtimes that predate the Moshi front-end.
    #[serde(default)]
    pub voice_frontend_mode: Option<String>,
    /// Whether the Moshi S2S backend is loaded+connected. `None` on older
    /// snapshots; `Some(false)` means Moshi mode is selected but the backend
    /// isn't up yet (binary missing, CUDA unavailable, still connecting).
    #[serde(default)]
    pub moshi_loaded: Option<bool>,
    #[serde(default)]
    pub frame_count: u64,
    #[serde(default)]
    pub monitor_count: usize,
    #[serde(default)]
    pub capture_event_count: u64,
    #[serde(default)]
    pub dropped_capture_event_count: u64,
    #[serde(default)]
    pub last_capture_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub wake_count: u64,
    #[serde(default)]
    pub last_update: String,
    /// Detected host hardware (probed once at boot). `None` until the runtime
    /// has run `probe_hardware`. Read by the dashboard's resource panel.
    #[serde(default)]
    pub hardware_specs: Option<HardwareSpecs>,
    /// Resolved adaptive resource plan (computed once at boot from
    /// `hardware_specs` + `[resources]` config). `None` until the runtime
    /// has resolved it. Read by the dashboard's resource panel.
    #[serde(default)]
    pub resource_plan: Option<ResolvedResourcePlan>,
    /// Memory-vault curator (Plan B) health — last pass time, consecutive
    /// failures, and pending/written counts. `None` only for snapshots
    /// written before this field existed (`#[serde(default)]` keeps old
    /// `state.json` files parsing); the runtime always publishes `Some`
    /// once it starts, using `enabled: false` and zeroed counters when the
    /// curator hasn't spawned (no triage model loaded). Read by the
    /// dashboard's Curator row.
    #[serde(default)]
    pub curator: Option<CuratorSnapshot>,
    /// Whether `[privacy.toggles].pause_all` fully paused observation at
    /// boot (Task A8, closing the A2 seam). `None` only for snapshots
    /// written before this field existed; the runtime always publishes
    /// `Some`. The dashboard bridge mirrors it into
    /// `SystemState.paused`.
    #[serde(default)]
    pub paused: Option<bool>,
    /// Context-engine component health (Task A8, spec §7): the runtime
    /// process has no `HealthRegistry` — this snapshot section IS its
    /// health registration, refreshed every publish tick from the shared
    /// health handles of each collector. `None` only for snapshots
    /// written before this field existed. Read by the repair agent
    /// straight from `state.json`.
    #[serde(default)]
    pub context_engine: Option<ContextEngineSnapshot>,
    /// Live session state (Task C1, spec §4.8 consumers): the runtime's
    /// answer to "what is the user doing right now?", republished every
    /// tick from the `SessionStateHub`.
    ///
    /// **The JSON key is a contract.** Three consumers read it straight
    /// out of `state.json` and all three expect exactly `session_state`
    /// at the snapshot root:
    /// 1. boot rehydration —
    ///    [`crate::context::session_state::read_persisted_state`] (Task B5),
    /// 2. the desktop chat profile's session section (Task B8),
    /// 3. the `context_session` MCP tool (Plan C, spec §5.2).
    ///
    /// The **raw** state is published, not
    /// [`crate::context::session_state::SessionState::cloud_view`]: each
    /// consumer applies the cloud gate at its own egress point, and a
    /// local consumer is allowed to see the real text (spec §4.1).
    /// `None` only for snapshots written before this field existed.
    #[serde(default)]
    pub session_state: Option<crate::context::session_state::SessionState>,
    /// Context-page list data (Task C5, spec §4.13): projects, override
    /// rules, pins, the recent-events strip, the live toggle values and
    /// the ranked continuation candidates.
    ///
    /// The dashboard cannot read the raw-log database (it links this crate
    /// without the `runtime` feature), so this is the *only* path those
    /// lists take to the Context page. `None` means the runtime is not
    /// running or predates the field — both render as the page's empty
    /// state.
    #[serde(default)]
    pub context_page: Option<ContextPageSnapshot>,
    /// Bounded, deduplicated health/watcher/repair transition history. The
    /// runtime never publishes raw logs or paths here.
    #[serde(default)]
    pub operational_events: Vec<OperationalEvent>,
}

/// Per-component health summary inside [`ContextEngineSnapshot`]. The
/// shape is deliberately uniform (spec §7): degraded-permanent states
/// report `healthy: true, enabled: false` with the reason in `detail`,
/// and `should_restart` is the only signal that ever asks for a restart.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComponentHealthSummary {
    /// `true` unless the component is in a genuinely broken state.
    /// Disabled-with-reason is healthy (spec §7).
    #[serde(default)]
    pub healthy: bool,
    /// Whether the component is actively running (vs parked
    /// disabled-with-reason).
    #[serde(default)]
    pub enabled: bool,
    /// `true` only when a restart could actually fix the component
    /// (stalls, dead channels) — never for deliberate disabled states.
    #[serde(default)]
    pub should_restart: bool,
    /// Human/agent-readable detail: disabled reason, freshness, queue
    /// depths, per-root unavailability.
    #[serde(default)]
    pub detail: Option<String>,
    /// Typed lifecycle state. This disambiguates idle from disabled while the
    /// legacy booleans remain for older dashboard builds.
    #[serde(default)]
    pub state: OperationalState,
    /// Public-safe root-cause diagnosis and repair policy.
    #[serde(default)]
    pub diagnostic: Option<ComponentDiagnostic>,
}

/// Context-engine health section of [`RuntimeSnapshot`] (Task A8, spec
/// §7). Built by the `continuum` binary's publish closure from each
/// component's shared health handle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextEngineSnapshot {
    /// Whether the idle controller currently has idle cadences applied
    /// (spec §4.11).
    #[serde(default)]
    pub idle: bool,
    /// 1 Hz window/context poller (`ContextWatchHealth` freshness —
    /// `senses::context`).
    #[serde(default)]
    pub context_watcher: Option<ComponentHealthSummary>,
    /// Live-context hub + capture workers
    /// ([`crate::senses::live_context::LiveContextHealth::should_restart`]).
    #[serde(default)]
    pub live_context: Option<ComponentHealthSummary>,
    /// Git collector (disabled-with-reason when git is absent or the
    /// source is toggled off; never restarts).
    #[serde(default)]
    pub git_watcher: Option<ComponentHealthSummary>,
    /// File watcher (disabled-with-reason by default; restarts only on
    /// notify channel death).
    #[serde(default)]
    pub file_watcher: Option<ComponentHealthSummary>,
    /// Background-process lifecycle/resource collector (opt-in; never reads
    /// command lines, environment variables, or process memory).
    #[serde(default)]
    pub process_watcher: Option<ComponentHealthSummary>,
    /// Context-events writer task (queue depth + last flush; restarts
    /// only when the task died unexpectedly).
    #[serde(default)]
    pub events_writer: Option<ComponentHealthSummary>,
    /// Off-loop triage evaluation
    /// ([`crate::triage::coalesce::TriageBusyHandle`]): `enabled` follows
    /// whether a triage model loaded, and `should_restart` fires when a
    /// single evaluation has been "in flight" long past any plausible
    /// model latency — the signature of an evaluation task that died
    /// without releasing the coalescer, which silently parks every later
    /// frame.
    #[serde(default)]
    pub triage: Option<ComponentHealthSummary>,
}

/// One Projects-table row as the Context page renders it (Task C5,
/// spec §4.13).
///
/// The dashboard process links `continuum-core` with
/// `default-features = false` and therefore cannot open the raw-log
/// database at all — the whole Context page's list data travels through
/// this section of `state.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectSummaryView {
    /// Slug id.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// `configured` | `discovered` | `confirmed`. A `discovered` row is an
    /// unconfirmed candidate: never resolved, never collected from.
    pub status: String,
    /// Root directories, path-scrubbed.
    #[serde(default)]
    pub root_paths: Vec<String>,
    /// RFC 3339 timestamp of the last frame attributed to this project.
    #[serde(default)]
    pub last_active: Option<String>,
    /// Frames attributed to this project.
    #[serde(default)]
    pub frames_count: i64,
    /// Whether this is the resolver's current post-hysteresis project.
    #[serde(default)]
    pub active: bool,
}

/// A persisted tier-0 resolver override rule, for the page's overrides
/// list (spec §4.3/§4.13).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OverrideRuleView {
    /// Process the rule matches on.
    #[serde(default)]
    pub match_process: Option<String>,
    /// Title fragment the rule matches on.
    #[serde(default)]
    pub match_title_substring: Option<String>,
    /// `force_project` | `exclude_project`.
    pub action: String,
    /// The project forced or excluded.
    pub project_id: String,
}

/// A persisted session pin (spec §4.13).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionPinView {
    /// `project` | `goal` | `task`.
    pub field: String,
    /// The pinned value.
    #[serde(default)]
    pub value: Option<String>,
}

/// One deduped `context_events` row for the recent-events strip.
///
/// The runtime applies the §4.1 cloud gate before publishing: rows tagged
/// `local_only` never reach this list. The dashboard is a local process,
/// but `state.json` is a file on disk that backup and support flows copy,
/// so the same egress discipline applies.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextEventView {
    /// Rowid — the Forget action's `event_id`.
    pub id: i64,
    /// RFC 3339 `ts_last` (the collapse anchor).
    pub ts: String,
    /// Collector family token.
    pub source: String,
    /// Event-type token from the closed §4.6 registry.
    pub event_type: String,
    /// Application the event is about.
    #[serde(default)]
    pub application: String,
    /// Display summary (the first occurrence's text).
    #[serde(default)]
    pub summary: String,
    /// Occurrences collapsed into this row.
    #[serde(default)]
    pub count: i64,
    /// Resolved project.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Pointer into the raw log — the Forget cascade key.
    #[serde(default)]
    pub raw_reference: Option<String>,
}

/// Live values of the honest observation toggles (spec §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationTogglesView {
    /// Microphone / audio observation.
    pub mic: bool,
    /// Screen capture + vision.
    pub screen: bool,
    /// File watcher.
    pub files: bool,
    /// Git collector.
    pub git: bool,
    /// Master pause.
    pub pause_all: bool,
}

impl Default for ObservationTogglesView {
    fn default() -> Self {
        Self {
            mic: true,
            screen: true,
            files: true,
            git: true,
            pause_all: false,
        }
    }
}

impl From<&crate::config::ObservationToggles> for ObservationTogglesView {
    fn from(t: &crate::config::ObservationToggles) -> Self {
        Self {
            mic: t.mic,
            screen: t.screen,
            files: t.files,
            git: t.git,
            pause_all: t.pause_all,
        }
    }
}

/// A ranked continuation candidate (spec §4.12) for the Context page.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContinuationCandidateView {
    /// Stable candidate-kind token.
    pub kind: String,
    /// Human label for the kind.
    pub label: String,
    /// The candidate text.
    pub text: String,
    /// Ranked score in \[0, 1\].
    pub confidence: f32,
}

/// Everything the Context page needs that is not already in
/// `session_state` or `context_engine` (Task C5, spec §4.13).
///
/// Refreshed on its own slower ticker in the runtime (the DB reads behind
/// it are far too expensive for the 2 s publish tick) and cloned into every
/// snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextPageSnapshot {
    /// Projects table: configured, discovered candidates and confirmed
    /// rows alike, active first.
    #[serde(default)]
    pub projects: Vec<ProjectSummaryView>,
    /// Persisted override rules.
    #[serde(default)]
    pub rules: Vec<OverrideRuleView>,
    /// Persisted session pins.
    #[serde(default)]
    pub pins: Vec<SessionPinView>,
    /// Recent deduped context events, newest first, privacy-gated.
    #[serde(default)]
    pub recent_events: Vec<ContextEventView>,
    /// Live toggle values (what the switches must show).
    #[serde(default)]
    pub toggles: ObservationTogglesView,
    /// Live optional-service requests. Privacy toggles remain a separate,
    /// stricter gate and can never be weakened by this control.
    #[serde(default)]
    pub services: RuntimeServiceSnapshot,
    /// Ranked continuation candidates.
    #[serde(default)]
    pub continuation: Vec<ContinuationCandidateView>,
}

/// Curator (Plan B memory-vault) health surfaced to the dashboard. Mirrors
/// [`crate::curator::CuratorStatus`] plus an `enabled` flag derived from
/// config — see `build_curator_snapshot` in the `continuum` binary, which
/// fills this in on every publish tick from the curator's
/// [`crate::curator::SharedCuratorStatus`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CuratorSnapshot {
    /// RFC3339 timestamp of the most recent curator pass, successful or
    /// not. `None` until the first pass completes.
    #[serde(default)]
    pub last_pass_at: Option<String>,
    /// Consecutive failed passes. The dashboard shows a warning badge once
    /// this crosses the same threshold (3) the repair agent escalates on.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Lifetime count of candidate/confirmed notes the curator has written.
    #[serde(default)]
    pub candidates_written_total: u64,
    /// Current count of notes awaiting human review.
    #[serde(default)]
    pub pending_count: u64,
    /// Whether the curator pipeline is actually running: both
    /// `[memory.curator] enabled = true` in config and a triage model
    /// loaded at boot. `false` (with zeroed counters above) means the
    /// dashboard should render "Curator: off".
    #[serde(default)]
    pub enabled: bool,
}

pub fn write_snapshot(path: &Path, snapshot: &RuntimeSnapshot) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(snapshot)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Spawn a background ticker that polls the provided snapshot provider
/// every `interval_secs` and writes the result to `path`. Exits cleanly
/// on shutdown.
pub fn spawn_publisher<F>(
    path: std::path::PathBuf,
    interval_secs: u64,
    mut shutdown: watch::Receiver<bool>,
    snapshot_fn: F,
) where
    F: Fn() -> RuntimeSnapshot + Send + 'static,
{
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let snap = snapshot_fn();
                    if let Err(e) = write_snapshot(&path, &snap) {
                        tracing::trace!(
                            layer = "system",
                            component = "runtime_publish",
                            error = %e,
                            "snapshot write failed"
                        );
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_snapshot_creates_json_at_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        let snap = RuntimeSnapshot {
            triage_model_loaded: true,
            voice_mode: Some("listening".into()),
            voice_volume: Some(0.65),
            tts_queue_len: Some(2),
            last_update: "2026-04-14T10:00:00Z".into(),
            ..RuntimeSnapshot::default()
        };
        write_snapshot(&path, &snap).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"triage_model_loaded\": true"));
        assert!(contents.contains("\"voice_mode\": \"listening\""));
        assert!(contents.contains("\"voice_volume\": 0.65"));
        assert!(contents.contains("\"tts_queue_len\": 2"));
    }

    #[test]
    fn older_snapshot_without_voice_telemetry_still_deserializes() {
        let snapshot: RuntimeSnapshot =
            serde_json::from_str(r#"{"voice_mode":"idle","last_update":"2026-08-03T00:00:00Z"}"#)
                .unwrap();

        assert_eq!(snapshot.voice_mode.as_deref(), Some("idle"));
        assert_eq!(snapshot.voice_volume, None);
        assert_eq!(snapshot.tts_queue_len, None);
        assert_eq!(snapshot.ambient_mute_active, None);
    }

    #[test]
    fn write_snapshot_is_atomic() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        // First write.
        write_snapshot(
            &path,
            &RuntimeSnapshot {
                frame_count: 1,
                ..RuntimeSnapshot::default()
            },
        )
        .unwrap();
        // Second write — the old .tmp should not linger.
        write_snapshot(
            &path,
            &RuntimeSnapshot {
                frame_count: 2,
                ..RuntimeSnapshot::default()
            },
        )
        .unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }

    /// Task 11: a `state.json` written before the `curator` field existed
    /// must still parse — `#[serde(default)]` on both the field and every
    /// `CuratorSnapshot` member is what makes that true.
    #[test]
    fn snapshot_deserializes_without_curator_field() {
        let json = r#"{
            "triage_model_loaded": true,
            "vision_model_loaded": false,
            "tts_loaded": false,
            "stt_loaded": false,
            "orchestrator_ready": true,
            "frame_count": 12,
            "wake_count": 1,
            "last_update": "2026-04-14T10:00:00Z"
        }"#;
        let snap: RuntimeSnapshot = serde_json::from_str(json).unwrap();
        assert!(snap.curator.is_none());
    }

    /// Round trip with the field present: serialize, reparse, and confirm
    /// every `CuratorSnapshot` member survives — this is the shape the
    /// `continuum` binary's publisher actually writes once a curator status
    /// is available.
    #[test]
    fn snapshot_roundtrip_with_curator_field() {
        let snap = RuntimeSnapshot {
            curator: Some(CuratorSnapshot {
                last_pass_at: Some("2026-04-14T10:05:00+00:00".to_string()),
                consecutive_failures: 2,
                candidates_written_total: 7,
                pending_count: 3,
                enabled: true,
            }),
            last_update: "2026-04-14T10:05:02Z".into(),
            ..RuntimeSnapshot::default()
        };
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: RuntimeSnapshot = serde_json::from_str(&json).unwrap();
        let curator = parsed.curator.expect("curator field should round-trip");
        assert_eq!(
            curator.last_pass_at.as_deref(),
            Some("2026-04-14T10:05:00+00:00")
        );
        assert_eq!(curator.consecutive_failures, 2);
        assert_eq!(curator.candidates_written_total, 7);
        assert_eq!(curator.pending_count, 3);
        assert!(curator.enabled);
    }

    /// Task A8: `paused` + `context_engine` round-trip, and a pre-A8
    /// `state.json` without them still parses (`#[serde(default)]`).
    #[test]
    fn snapshot_roundtrip_with_context_engine_health() {
        let snap = RuntimeSnapshot {
            paused: Some(false),
            context_engine: Some(ContextEngineSnapshot {
                idle: true,
                context_watcher: Some(ComponentHealthSummary {
                    healthy: true,
                    enabled: true,
                    should_restart: false,
                    detail: Some("last poll 1s ago".into()),
                    ..ComponentHealthSummary::default()
                }),
                live_context: Some(ComponentHealthSummary {
                    healthy: true,
                    enabled: true,
                    should_restart: false,
                    detail: None,
                    ..ComponentHealthSummary::default()
                }),
                git_watcher: Some(ComponentHealthSummary {
                    healthy: true,
                    enabled: false,
                    should_restart: false,
                    detail: Some("disabled by [git_context].enabled".into()),
                    ..ComponentHealthSummary::default()
                }),
                file_watcher: Some(ComponentHealthSummary {
                    healthy: true,
                    enabled: false,
                    should_restart: false,
                    detail: Some("disabled by [file_watcher].enabled".into()),
                    ..ComponentHealthSummary::default()
                }),
                process_watcher: Some(ComponentHealthSummary {
                    healthy: true,
                    enabled: false,
                    should_restart: false,
                    detail: Some("disabled by [process_watcher].enabled".into()),
                    ..ComponentHealthSummary::default()
                }),
                events_writer: Some(ComponentHealthSummary {
                    healthy: true,
                    enabled: true,
                    should_restart: false,
                    detail: Some("queue_depth=0".into()),
                    ..ComponentHealthSummary::default()
                }),
                triage: Some(ComponentHealthSummary {
                    healthy: true,
                    enabled: true,
                    should_restart: false,
                    detail: Some("idle".into()),
                    ..ComponentHealthSummary::default()
                }),
            }),
            ..RuntimeSnapshot::default()
        };
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: RuntimeSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.paused, Some(false));
        let engine = parsed.context_engine.expect("context_engine round-trips");
        assert!(engine.idle);
        let git = engine.git_watcher.expect("git summary");
        assert!(git.healthy && !git.enabled && !git.should_restart);
        assert_eq!(
            git.detail.as_deref(),
            Some("disabled by [git_context].enabled")
        );
        assert!(engine.events_writer.expect("writer summary").enabled);
        let triage = engine.triage.expect("triage summary");
        assert!(triage.healthy && !triage.should_restart);

        // Pre-A8 snapshot parses with both fields absent.
        let old: RuntimeSnapshot =
            serde_json::from_str(r#"{"frame_count": 3, "last_update": "x"}"#).unwrap();
        assert!(old.paused.is_none());
        assert!(old.context_engine.is_none());
    }

    /// Task C1: `session_state` round-trips through the published shape,
    /// and — the actual contract — the B5 rehydration reader parses the
    /// exact bytes [`write_snapshot`] produces.
    /// Task C5: the Context-page section survives the JSON round-trip the
    /// dashboard actually performs, and a legacy snapshot without it still
    /// parses (additive-schema contract).
    #[test]
    fn snapshot_roundtrip_with_context_page() {
        let page = ContextPageSnapshot {
            projects: vec![ProjectSummaryView {
                id: "continuum".into(),
                name: "Continuum".into(),
                status: "confirmed".into(),
                root_paths: vec!["~/code/continuum".into()],
                last_active: Some("2026-08-05T10:00:00+00:00".into()),
                frames_count: 3,
                active: true,
            }],
            rules: vec![OverrideRuleView {
                match_process: Some("Code.exe".into()),
                match_title_substring: None,
                action: "exclude_project".into(),
                project_id: "other".into(),
            }],
            pins: vec![SessionPinView {
                field: "task".into(),
                value: Some("ship C5".into()),
            }],
            recent_events: vec![ContextEventView {
                id: 12,
                ts: "2026-08-05T10:00:00+00:00".into(),
                source: "git".into(),
                event_type: "commit".into(),
                application: "git".into(),
                summary: "feat(core): ship it".into(),
                count: 1,
                project_id: Some("continuum".into()),
                raw_reference: Some("deadbeef".into()),
            }],
            toggles: ObservationTogglesView {
                files: false,
                ..ObservationTogglesView::default()
            },
            continuation: vec![ContinuationCandidateView {
                kind: "open_task".into(),
                label: "open task from the last session".into(),
                text: "finish the page".into(),
                confidence: 0.64,
            }],
        };
        let snap = RuntimeSnapshot {
            context_page: Some(page.clone()),
            ..RuntimeSnapshot::default()
        };
        let json = serde_json::to_string(&snap).unwrap();
        // The key is a contract: the desktop bridge reads exactly this.
        assert!(json.contains("\"context_page\""));
        let parsed: RuntimeSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.context_page.as_ref(), Some(&page));

        // A pre-C5 document still parses, with the section absent.
        let legacy: RuntimeSnapshot =
            serde_json::from_str(r#"{"frame_count":3,"last_update":"x"}"#).unwrap();
        assert!(legacy.context_page.is_none());
        assert_eq!(legacy.frame_count, 3);
    }

    #[test]
    fn snapshot_roundtrip_with_session_state() {
        use crate::context::session_state::{read_persisted_state, SessionState, StampedText};
        use chrono::TimeZone;

        let at = chrono::Utc.with_ymd_and_hms(2026, 8, 5, 9, 30, 0).unwrap();
        let session = SessionState {
            active_project: Some("continuum".into()),
            current_goal: Some("ship the context engine".into()),
            current_task: Some("publish session state".into()),
            active_app: Some("Code.exe".into()),
            window_title: Some("runtime_publish.rs — continuum".into()),
            open_files: vec!["runtime_publish.rs".into(), "state.rs".into()],
            last_error: Some(StampedText::new("cargo build failed", at)),
            last_success: Some(StampedText::new("tests green", at)),
            last_user_command: Some(StampedText::new("ga door", at)),
            confidence: 0.72,
            local_only: true,
            inferred_at: Some(at),
            pinned: vec!["project".into()],
            user_confirmed: vec!["task".into()],
            since: at,
            updated: at,
        };
        let snap = RuntimeSnapshot {
            session_state: Some(session.clone()),
            ..RuntimeSnapshot::default()
        };

        let json = serde_json::to_string(&snap).unwrap();
        let parsed: RuntimeSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.session_state.as_ref(), Some(&session));

        // The key the three consumers look for lives at the snapshot root.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("session_state").is_some());

        // End-to-end: the B5 rehydration reader against the real file the
        // publisher writes.
        let tmp = TempDir::new().unwrap();
        write_snapshot(&tmp.path().join("state.json"), &snap).unwrap();
        let read = read_persisted_state(tmp.path()).expect("rehydration reader parses");
        assert_eq!(read, session);
    }

    /// Task C1: a pre-C1 `state.json` (no `session_state`) still loads, and
    /// the rehydration reader simply finds nothing.
    #[test]
    fn legacy_snapshot_without_session_state_still_loads() {
        use crate::context::session_state::read_persisted_state;

        let json = r#"{
            "triage_model_loaded": true,
            "frame_count": 12,
            "wake_count": 1,
            "last_update": "2026-04-14T10:00:00Z"
        }"#;
        let snap: RuntimeSnapshot = serde_json::from_str(json).unwrap();
        assert!(snap.session_state.is_none());
        assert!(snap.context_engine.is_none());

        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("state.json"), json).unwrap();
        assert!(read_persisted_state(tmp.path()).is_none());
    }

    /// The "curator never spawned" shape (Task 11 brief): `Some` with
    /// `enabled: false` and zeroed counters, not `None` — the dashboard
    /// tells "off" apart from "old state.json" this way.
    #[test]
    fn snapshot_roundtrip_curator_disabled_is_some_with_zeros() {
        let snap = RuntimeSnapshot {
            curator: Some(CuratorSnapshot::default()),
            ..RuntimeSnapshot::default()
        };
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: RuntimeSnapshot = serde_json::from_str(&json).unwrap();
        let curator = parsed.curator.expect("curator field should round-trip");
        assert!(!curator.enabled);
        assert_eq!(curator.consecutive_failures, 0);
        assert_eq!(curator.candidates_written_total, 0);
        assert_eq!(curator.pending_count, 0);
        assert!(curator.last_pass_at.is_none());
    }
}
