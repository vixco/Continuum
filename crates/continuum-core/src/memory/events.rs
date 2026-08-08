//! # Context events — types, transport, and writer task (spec §4.6)
//!
//! The typed event vocabulary for the `context_events` table plus the
//! event transport (spec §3): one bounded `mpsc::Sender<ContextEvent>`
//! cloned into every collector, and a dedicated **events-writer task**
//! that owns the receiver, applies dedupe, and batch-inserts into the
//! raw-log DB. Collectors never block on SQLite; the frame loop never
//! touches the events DB inline. Producers that overflow the queue
//! increment a per-source dropped counter which the writer coalesces
//! into `events_dropped` rows.
//!
//! Table DDL, the FTS5 mirror, and row-level SQL live in
//! [`crate::memory::raw_log`] (spec §4.6: all DDL for the raw-log DB in
//! one place, runtime single-writer). The dedupe algorithm, the writer
//! loop, and the channel plumbing live here.
//!
//! ## Stability policy (additive-only)
//!
//! [`EventSource`], [`EventType`], and [`EventSensitivity`] are **closed
//! registries** with the same stability policy as published MCP schemas:
//! variants may be *added* (with a `valid_for` entry), but existing variants
//! and their serde names must never change or disappear — persisted rows and
//! downstream consumers key on the snake_case strings.
//!
//! ## Dedupe (spec §4.6, normative)
//!
//! - Template sources (window/git/system/voice, plus the file storm
//!   templates): `dedupe_key = hash(source, event_type, project_id,
//!   normalized_summary)`; normalization = lowercase → strip quoted
//!   strings, paths, hex runs, digits → collapse whitespace → first 12
//!   tokens.
//! - Per-path file events (`file_modified|created|deleted|renamed`):
//!   `dedupe_key = hash(source, event_type, project_id, summary)` — the
//!   **raw** summary, because that summary *is* the path and
//!   normalization would erase it (see [`file_event_keys_on_raw_path`]).
//! - Classified screen/audio events: **summary is NOT in the key** —
//!   `dedupe_key = hash(source, event_type, project_id, application)`.
//!   LLM summaries are never byte-stable; "build failed ×14" must
//!   collapse regardless of summary variance (first summary kept).
//! - Collapse window anchors on `ts_last` (ongoing repetition keeps
//!   collapsing); caps `count_cap` / `span_cap_hours` start a fresh row.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::config::EventsConfig;
use crate::context::project::CurrentProjectHandle;
use crate::memory::raw_log::{self, DedupeCandidate, RawLog};
use crate::senses::privacy::Zone;

/// Importance assigned to deterministic collector events (focus switches,
/// system events) — low, they are routine bookkeeping, not signals.
pub const COLLECTOR_EVENT_IMPORTANCE: f32 = 0.2;

/// In-memory LRU capacity of open collapse keys (spec §4.6): hot keys
/// skip the `(dedupe_key, ts_last)` index SELECT entirely.
pub const DEDUPE_LRU_CAPACITY: usize = 256;

/// The writer flushes a pending batch at least this often.
const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_millis(500);

/// A batch this large flushes immediately instead of waiting for the tick.
const BATCH_FLUSH_THRESHOLD: usize = 32;

/// Upper bound on one drained batch (bounds transaction size under storms).
const MAX_BATCH: usize = 256;

/// How often the writer runs `[events]` retention rotation (also runs on
/// the first flush tick after start).
const ROTATE_INTERVAL: Duration = Duration::from_secs(3600);

/// The collector family an event originated from (spec §4.6, closed
/// registry — additive-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    /// Foreground-window poll (ContextWatcher).
    Window,
    /// Git collector (Task A5).
    Git,
    /// File watcher (Task A7).
    File,
    /// Screen classification riding triage (spec §4.7).
    Screen,
    /// Audio classification riding triage (spec §4.7).
    Audio,
    /// Runtime/system events (toggles, idle, wakes).
    System,
    /// Voice-command pipeline.
    Voice,
    /// Background-process lifecycle and sustained resource pressure.
    Process,
}

/// Every [`EventSource`] variant, for per-source bookkeeping (dropped
/// counters, registry tests). Keep in sync when the registry grows.
pub const ALL_EVENT_SOURCES: [EventSource; 8] = [
    EventSource::Window,
    EventSource::Git,
    EventSource::File,
    EventSource::Screen,
    EventSource::Audio,
    EventSource::System,
    EventSource::Voice,
    EventSource::Process,
];

/// The per-source event vocabulary as one flat enum (spec §4.6, closed
/// registry — additive-only). Use [`EventType::valid_for`] to check that a
/// type is legal for a source before emitting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    // -- window --
    /// Foreground focus moved to a different (process, title) pair.
    FocusSwitch,
    /// The resolved project changed (Task A4).
    ProjectSwitch,
    // -- git --
    /// A new commit appeared on the watched branch.
    Commit,
    /// HEAD moved to a different branch.
    BranchSwitch,
    /// Merge/rebase conflict markers detected.
    Conflict,
    /// Working-tree dirtiness changed.
    DirtyChange,
    // -- file --
    /// A watched file was modified.
    FileModified,
    /// A watched file was created.
    FileCreated,
    /// A watched file was deleted.
    FileDeleted,
    /// A watched file was renamed.
    FileRenamed,
    /// Storm-collapsed "N files changed in <project>" event.
    FilesBulkChange,
    // -- process --
    /// A configured or resource-significant process appeared.
    ProcessStarted,
    /// A previously observed significant process disappeared. Generic OS
    /// polling cannot prove whether this was a clean exit or a crash.
    ProcessStopped,
    /// CPU or resident memory stayed above the configured threshold.
    ResourcePressure,
    // -- screen/audio classification (spec §4.7 enum) --
    /// An error was observed (build failure, stack trace, error dialog).
    Error,
    /// A success was observed (tests green, deploy finished).
    Success,
    /// The user made or stated a decision.
    Decision,
    /// The user expressed a preference.
    Preference,
    /// Progress on an ongoing task.
    TaskProgress,
    /// Communication activity (chat, mail, call).
    Communication,
    /// Routine activity with no particular signal.
    Routine,
    /// Anything that fits no other classification bucket.
    Other,
    // -- system --
    /// The user went idle.
    IdleStart,
    /// The user returned from idle.
    IdleEnd,
    /// The orchestrator was woken.
    Wake,
    /// A wake finished (with its outcome).
    WakeResult,
    /// A voice command was handled.
    VoiceCommand,
    /// An observation toggle changed (spec §4.1 honest toggles).
    ToggleChange,
    /// A collector source became unavailable.
    SourceUnavailable,
    /// Events were dropped (queue overflow, backpressure).
    EventsDropped,
}

/// Every [`EventType`] variant, for registry-stability tests. Keep in
/// sync when the registry grows.
pub const ALL_EVENT_TYPES: [EventType; 30] = [
    EventType::FocusSwitch,
    EventType::ProjectSwitch,
    EventType::Commit,
    EventType::BranchSwitch,
    EventType::Conflict,
    EventType::DirtyChange,
    EventType::FileModified,
    EventType::FileCreated,
    EventType::FileDeleted,
    EventType::FileRenamed,
    EventType::FilesBulkChange,
    EventType::ProcessStarted,
    EventType::ProcessStopped,
    EventType::ResourcePressure,
    EventType::Error,
    EventType::Success,
    EventType::Decision,
    EventType::Preference,
    EventType::TaskProgress,
    EventType::Communication,
    EventType::Routine,
    EventType::Other,
    EventType::IdleStart,
    EventType::IdleEnd,
    EventType::Wake,
    EventType::WakeResult,
    EventType::VoiceCommand,
    EventType::ToggleChange,
    EventType::SourceUnavailable,
    EventType::EventsDropped,
];

impl EventType {
    /// Whether this event type is legal for the given source, per the
    /// spec §4.6 registry table. `voice_command` is valid for both
    /// `system` (the table's row) and the dedicated `voice` source.
    pub fn valid_for(self, source: EventSource) -> bool {
        use EventType::*;
        match self {
            FocusSwitch | ProjectSwitch => source == EventSource::Window,
            Commit | BranchSwitch | Conflict | DirtyChange => source == EventSource::Git,
            FileModified | FileCreated | FileDeleted | FileRenamed | FilesBulkChange => {
                source == EventSource::File
            }
            ProcessStarted | ProcessStopped | ResourcePressure => source == EventSource::Process,
            Error | Success | Decision | Preference | TaskProgress | Communication | Routine
            | Other => matches!(source, EventSource::Screen | EventSource::Audio),
            VoiceCommand => matches!(source, EventSource::System | EventSource::Voice),
            IdleStart | IdleEnd | Wake | WakeResult | ToggleChange | SourceUnavailable
            | EventsDropped => source == EventSource::System,
        }
    }
}

/// Event sensitivity, inherited from the strictest zone of the event's
/// inputs (spec §4.1 propagation rule). `never_observe` rows cannot exist —
/// excluded windows only ever appear as the synthetic `[excluded]` bucket
/// endpoint, tagged `local_only`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSensitivity {
    /// Persisted and visible to local models; stripped from everything
    /// cloud-bound.
    LocalOnly,
    /// Eligible for cloud-bound context (already scrubbed).
    CloudAllowed,
}

/// One context event, destined for the `context_events` table.
///
/// Every free-text field (`summary`, `window_title`) must already be
/// privacy-scrubbed at construction — events are built from collector
/// output *after* the §4.1 choke point, never from raw observations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextEvent {
    /// When the event occurred.
    pub ts: DateTime<Utc>,
    /// Originating collector family.
    pub source: EventSource,
    /// Application (process name) the event is about; may be the literal
    /// `[excluded]` bucket or empty for system events.
    pub application: String,
    /// Window title at event time (already scrubbed/redacted upstream).
    pub window_title: String,
    /// Resolved project id. Producers that know their project (git
    /// collector, project_switch) stamp it at emit; producers below the
    /// resolver send `None` and either stamp from their own
    /// [`CurrentProjectHandle`] (ContextWatcher) or leave it to the
    /// events-writer, which stamps from the handle it owns at flush time.
    pub project_id: Option<String>,
    /// What happened — must satisfy `event_type.valid_for(source)`.
    pub event_type: EventType,
    /// One-line human-readable description (free text, scrubbed).
    pub summary: String,
    /// Importance in \[0.0, 1.0\] (deterministic collectors use
    /// [`COLLECTOR_EVENT_IMPORTANCE`]).
    pub importance: f32,
    /// Confidence in \[0.0, 1.0\] (1.0 for deterministic collectors).
    pub confidence: f32,
    /// Zone-derived sensitivity tag the cloud gate enforces.
    pub sensitivity: EventSensitivity,
    /// Optional pointer into the raw log (frame id etc.).
    pub raw_reference: Option<String>,
}

// ---------------------------------------------------------------------------
// Registry token helpers
// ---------------------------------------------------------------------------

/// Serializes a registry enum to its stable snake_case token (the string
/// persisted in `context_events` and hashed into dedupe keys).
///
/// `pub` since Task C4: the MCP `context_timeline` / `context_search` /
/// `context_files` tools render `source` / `event_type` as these exact
/// tokens, and a second hand-written mapping in another crate would drift
/// from the persisted one the first time the registry grows.
pub fn event_enum_token<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Parses a stable snake_case token back into a registry enum; `None`
/// for strings outside the registry.
///
/// `pub` since Task C4: `context_timeline`'s `types` / `source` filter
/// arguments arrive as strings from the model and must be parsed against
/// the **closed** registry — an unknown token has to be rejected here, not
/// interpolated into SQL.
pub fn parse_event_enum<T: serde::de::DeserializeOwned>(token: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(token.to_string())).ok()
}

/// Parses a system-event kind string (e.g. `"toggle_change"`) into its
/// [`EventType`], accepting only types valid for [`EventSource::System`].
pub fn system_event_type(kind: &str) -> Option<EventType> {
    let event_type: EventType = parse_event_enum(kind)?;
    event_type
        .valid_for(EventSource::System)
        .then_some(event_type)
}

// ---------------------------------------------------------------------------
// Event constructors
// ---------------------------------------------------------------------------

/// Builds the `project_switch` [`ContextEvent`] for a post-hysteresis
/// resolver flip (Task A4, spec §4.3/§4.6). `application`/`window_title`
/// are the current frame's sanitized fields; `project_id` is the *new*
/// project. Summary template (stable): `"project <from> → <to>"` with
/// `(none)` for the first adoption.
///
/// `to_zone` is the **destination project's** privacy zone (spec §4.3
/// project zones). Fixwave 3b (minor): this used to be hardcoded
/// `cloud_allowed`, unlike the git and file collectors which both derive
/// their sensitivity from the project zone. The name of a `local_only`
/// project is exactly the identity §4.1 says must not leave the machine,
/// and the switch event announces it.
pub fn project_switch_event(
    from: Option<&str>,
    to: &str,
    application: &str,
    window_title: &str,
    to_zone: Option<Zone>,
    ts: DateTime<Utc>,
) -> ContextEvent {
    ContextEvent {
        ts,
        source: EventSource::Window,
        application: application.to_string(),
        window_title: window_title.to_string(),
        project_id: Some(to.to_string()),
        event_type: EventType::ProjectSwitch,
        summary: format!("project {} → {}", from.unwrap_or("(none)"), to),
        importance: COLLECTOR_EVENT_IMPORTANCE,
        confidence: 1.0,
        sensitivity: zone_sensitivity(to_zone),
        raw_reference: None,
    }
}

/// The [`EventSensitivity`] a project zone implies (spec §4.1 strictest
/// zone wins). `never_observe` never reaches an event builder — those
/// projects are not collected from at all — so it maps to the strict side.
pub fn zone_sensitivity(zone: Option<Zone>) -> EventSensitivity {
    match zone {
        Some(Zone::LocalOnly) | Some(Zone::NeverObserve) => EventSensitivity::LocalOnly,
        _ => EventSensitivity::CloudAllowed,
    }
}

/// Builds a `system` [`ContextEvent`] from an already-validated kind.
fn system_context_event(event_type: EventType, detail: &str) -> ContextEvent {
    ContextEvent {
        ts: Utc::now(),
        source: EventSource::System,
        application: String::new(),
        window_title: String::new(),
        project_id: None,
        event_type,
        summary: detail.to_string(),
        importance: COLLECTOR_EVENT_IMPORTANCE,
        confidence: 1.0,
        sensitivity: EventSensitivity::CloudAllowed,
        raw_reference: None,
    }
}

/// Builds the overflow-coalesce row content (spec §3): "N events dropped
/// from <source>", emitted by the writer when it drains the per-source
/// dropped counters. Digits are stripped by normalization, so repeated
/// coalesce rows for one source collapse across flushes.
fn events_dropped_event(source: EventSource, dropped: u64, ts: DateTime<Utc>) -> ContextEvent {
    ContextEvent {
        ts,
        source: EventSource::System,
        application: String::new(),
        window_title: String::new(),
        project_id: None,
        event_type: EventType::EventsDropped,
        summary: format!(
            "{} events dropped from {}",
            dropped,
            event_enum_token(&source)
        ),
        importance: COLLECTOR_EVENT_IMPORTANCE,
        confidence: 1.0,
        sensitivity: EventSensitivity::CloudAllowed,
        raw_reference: None,
    }
}

/// Whether a source's events may be project-stamped by the transport
/// (fixwave 3a, I3).
///
/// Only *handle-less* producers qualify — the ones whose `None` means
/// "I never looked", not "I looked and there is none":
///
/// - `window` / `system` / `voice` build events below the resolver and
///   deliberately leave the stamping to the transport.
/// - `git` / `file` collectors always carry their own watched root's
///   project.
/// - `screen` / `audio` went through `triage::consume::resolve_project`,
///   which drops a classifier-named project that the Projects table does
///   not know to the resolver's value *at frame time* (spec §4.6). A
///   `None` there is a decision. Re-stamping it later with whatever the
///   resolver happens to hold — possibly minutes after the frame, from a
///   different window — invented an attribution the classifier had
///   explicitly refused.
pub fn source_defers_project_to_transport(source: EventSource) -> bool {
    matches!(
        source,
        EventSource::Window | EventSource::System | EventSource::Voice
    )
}

/// Stamps the resolver's current project onto an event whose producer ran
/// below the resolver (`project_id` still `None`). Events that already
/// carry a project (git collector, project_switch), and events whose
/// source resolved its project deliberately
/// ([`source_defers_project_to_transport`]), are left untouched.
///
/// Fixwave 3b (minor): stamping a project also **folds that project's
/// zone into the event's sensitivity** (spec §4.1: strictest zone wins,
/// never downgrades). `focus_switch` in particular derived its sensitivity
/// from the two window dispositions alone, so a switch inside a
/// `local_only` *project* whose windows were individually `visible` was
/// tagged `cloud_allowed` — unlike the git and file collectors, which
/// have always read the project zone.
pub fn fill_project_id_from_handle(
    event: &mut ContextEvent,
    handle: Option<&CurrentProjectHandle>,
) {
    if event.project_id.is_some() || !source_defers_project_to_transport(event.source) {
        return;
    }
    if let Some(handle) = handle {
        if let Some(current) = handle.read().as_ref() {
            event.project_id = Some(current.id.clone());
            if zone_sensitivity(current.zone) == EventSensitivity::LocalOnly {
                event.sensitivity = EventSensitivity::LocalOnly;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Dedupe keys (spec §4.6, normative)
// ---------------------------------------------------------------------------

static QUOTED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""[^"]*"|'[^']*'"#).expect("static quoted-string regex"));
static PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\S*[\\/]\S*").expect("static path regex"));
static HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-f]{7,}\b").expect("static hex-run regex"));
static DIGIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+").expect("static digit regex"));

/// Normalizes a summary for template-source dedupe keys (spec §4.6):
/// lowercase → strip quoted strings → strip path-like tokens → strip hex
/// runs (≥ 7 hex chars, e.g. git OIDs) → strip digit runs → collapse
/// whitespace → first 12 tokens. Every strip replaces with a space so
/// neighboring tokens never merge; the result is deterministic, not
/// linguistically pretty.
pub fn normalize_summary(summary: &str) -> String {
    let lower = summary.to_lowercase();
    let no_quotes = QUOTED_RE.replace_all(&lower, " ");
    let no_paths = PATH_RE.replace_all(&no_quotes, " ");
    let no_hex = HEX_RE.replace_all(&no_paths, " ");
    let no_digits = DIGIT_RE.replace_all(&no_hex, " ");
    no_digits
        .split_whitespace()
        .take(12)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether an event's dedupe discriminator must be the **raw** summary
/// instead of [`normalize_summary`] (fixwave 3a, C2).
///
/// A per-path file event's entire summary *is* a root-relative path
/// (`senses/file_watch.rs`), and normalization strips path-like tokens —
/// so `"src/main.rs"` normalized to `""` and every file in every
/// subdirectory of a project collapsed onto one row keyed
/// `hash("file", "file_modified", project, "")`. Touching 40 files then
/// produced a single `"src/first_file.rs ×40"` row, which is what "Recent
/// changes", the Context page strip and `context_search` all reported.
/// `file_renamed` was worse: `"a/b.rs → c/d.rs"` normalized to `"→"`.
///
/// The storm-collapse templates (`files_bulk_change`: `"N files changed in
/// <project>"`, `"file resync in <project>"`) are prose, not paths, so
/// they keep normalization — digits are stripped there, which is exactly
/// what lets repeated bulk rows collapse across flushes.
fn file_event_keys_on_raw_path(event: &ContextEvent) -> bool {
    event.source == EventSource::File && event.event_type != EventType::FilesBulkChange
}

/// 64-bit FNV-1a — deliberately hand-rolled: dedupe keys persist in the
/// DB, so the hash must be stable across releases (std's `DefaultHasher`
/// makes no such guarantee).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Computes the spec §4.6 dedupe key for an event.
///
/// - Template sources (window/git/system/voice):
///   `hash(source, event_type, project_id, normalized_summary)`.
/// - Classified screen/audio: `hash(source, event_type, project_id,
///   application)` — the summary is **not** keyed (LLM summaries are
///   never byte-stable).
/// - Per-path file events: `hash(source, event_type, project_id,
///   summary)` — the **raw** summary, see [`file_event_keys_on_raw_path`].
/// - Process lifecycle/pressure events: `hash(source, event_type, project_id,
///   raw_reference)` — the collector's stable process identity keeps
///   concurrent processes with the same executable name distinct.
pub fn dedupe_key(event: &ContextEvent) -> String {
    let discriminator = match event.source {
        EventSource::Screen | EventSource::Audio => event.application.clone(),
        EventSource::Process => event
            .raw_reference
            .clone()
            .unwrap_or_else(|| event.application.clone()),
        _ if file_event_keys_on_raw_path(event) => event.summary.clone(),
        _ => normalize_summary(&event.summary),
    };
    let material = format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        event_enum_token(&event.source),
        event_enum_token(&event.event_type),
        event.project_id.as_deref().unwrap_or(""),
        discriminator
    );
    format!("{:016x}", fnv1a64(material.as_bytes()))
}

// ---------------------------------------------------------------------------
// Transport: EventSender + dropped counters
// ---------------------------------------------------------------------------

/// Per-source dropped-event counters (spec §3 overflow coalesce).
/// Producers increment on `try_send` failure — they never block or await;
/// the writer drains the counters into `events_dropped` rows each flush.
#[derive(Debug, Default)]
pub struct DropCounters {
    counts: [AtomicU64; ALL_EVENT_SOURCES.len()],
}

fn source_index(source: EventSource) -> usize {
    match source {
        EventSource::Window => 0,
        EventSource::Git => 1,
        EventSource::File => 2,
        EventSource::Screen => 3,
        EventSource::Audio => 4,
        EventSource::System => 5,
        EventSource::Voice => 6,
        EventSource::Process => 7,
    }
}

impl DropCounters {
    /// Records one dropped event from `source`.
    fn record(&self, source: EventSource) {
        self.counts[source_index(source)].fetch_add(1, Ordering::Relaxed);
    }

    /// Drains every non-zero counter, returning `(source, dropped)` pairs
    /// and resetting them to zero.
    fn drain(&self) -> Vec<(EventSource, u64)> {
        ALL_EVENT_SOURCES
            .iter()
            .filter_map(|&source| {
                let n = self.counts[source_index(source)].swap(0, Ordering::Relaxed);
                (n > 0).then_some((source, n))
            })
            .collect()
    }

    /// Total dropped since the last drain (all sources).
    pub fn total(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }
}

/// Cloneable, non-blocking producer handle onto the events channel
/// (spec §3): one is cloned into every collector. `send` never blocks and
/// never awaits — a full or closed queue increments the per-source
/// dropped counter instead.
#[derive(Clone)]
pub struct EventSender {
    tx: Option<mpsc::Sender<ContextEvent>>,
    dropped: Arc<DropCounters>,
    depth: Arc<AtomicI64>,
    observer: Option<EventObserver>,
    /// The resolver's current-project handle, used to stamp handle-less
    /// producers' events **at emit time** (fixwave 3a, I3).
    ///
    /// The stamp used to happen in the writer's flush, which is up to a
    /// batch interval later and — crucially — *after* the B5 observer tap,
    /// so every in-memory consumer saw `project_id: None` for window,
    /// system and voice events. Filling here fixes both: the observer sees
    /// the same value the DB row gets, and the project is the one that was
    /// current when the event happened rather than when it was flushed.
    project: Option<CurrentProjectHandle>,
}

/// A synchronous, non-blocking tap on every registry-valid event handed to
/// [`EventSender::send`] (Task B5). Runs on the producer's thread *before*
/// the queue, so an in-memory consumer sees an event even when the DB queue
/// is full and drops it — session state must never go blind because the
/// writer fell behind.
///
/// Implementations must be fast and lock-scoped. They must never block,
/// await, or re-enter the events channel.
pub type EventObserver = Arc<dyn Fn(&ContextEvent) + Send + Sync>;

impl std::fmt::Debug for EventSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSender")
            .field("connected", &self.tx.is_some())
            .field("observed", &self.observer.is_some())
            .finish()
    }
}

impl EventSender {
    /// A sink that only logs events (perception bin, standalone tools,
    /// tests): no DB, no channel, nothing dropped.
    pub fn log_only() -> Self {
        Self {
            tx: None,
            dropped: Arc::new(DropCounters::default()),
            depth: Arc::new(AtomicI64::new(0)),
            observer: None,
            project: None,
        }
    }

    /// Attaches an in-memory [`EventObserver`]. Apply this to the sender
    /// returned by [`spawn_event_writer`] **before** cloning it into
    /// collectors — clones carry the observer, the original does not gain
    /// one retroactively.
    pub fn with_observer(mut self, observer: EventObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Creates a bounded channel pair. The writer task normally owns the
    /// receiver ([`spawn_event_writer`]); tests use this directly.
    pub(crate) fn bounded(queue_cap: usize) -> (Self, mpsc::Receiver<ContextEvent>) {
        let (tx, rx) = mpsc::channel(queue_cap.max(1));
        (
            Self {
                tx: Some(tx),
                dropped: Arc::new(DropCounters::default()),
                depth: Arc::new(AtomicI64::new(0)),
                observer: None,
                project: None,
            },
            rx,
        )
    }

    /// Attaches the resolver's current-project handle so handle-less
    /// producers are stamped at emit time (fixwave 3a, I3). Applied by
    /// [`spawn_event_writer`] before the sender is cloned into collectors.
    pub(crate) fn with_project(mut self, project: Option<CurrentProjectHandle>) -> Self {
        self.project = project;
        self
    }

    /// Sends an event without blocking. Registry violations
    /// (`!event_type.valid_for(source)`) are logged and dropped — never
    /// persisted. Queue overflow or a closed channel increments the
    /// per-source dropped counter (coalesced by the writer).
    pub fn send(&self, mut event: ContextEvent) {
        if !event.event_type.valid_for(event.source) {
            tracing::warn!(
                layer = "memory",
                component = "events",
                source = %event_enum_token(&event.source),
                event_type = %event_enum_token(&event.event_type),
                "event type is not valid for its source (spec §4.6 registry); dropping"
            );
            return;
        }
        // Fixwave 3a (I3): stamp the project BEFORE the observer tap, so
        // the in-memory consumer and the persisted row agree — and so the
        // stamp is the project current at emit time, not at flush time.
        fill_project_id_from_handle(&mut event, self.project.as_ref());
        // Task B5: the in-memory tap runs before the queue, so a full
        // queue costs a persisted row but never a session-state update.
        if let Some(observe) = &self.observer {
            observe(&event);
        }
        let Some(tx) = &self.tx else {
            tracing::debug!(
                layer = "memory",
                component = "events",
                source = %event_enum_token(&event.source),
                event_type = %event_enum_token(&event.event_type),
                summary = %event.summary,
                "context event (log-only sink)"
            );
            return;
        };
        match tx.try_send(event) {
            Ok(()) => {
                self.depth.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Full(event))
            | Err(mpsc::error::TrySendError::Closed(event)) => {
                self.dropped.record(event.source);
            }
        }
    }

    /// Total events dropped (queue full/closed) since the last writer
    /// drain — the health feed behind `events_dropped` rows.
    pub fn dropped_total(&self) -> u64 {
        self.dropped.total()
    }
}

/// The process-global sender slot used by producers with no injection
/// path (`senses::privacy::emit_system_event`'s scattered call sites).
/// `None` until [`install_global_sender`] runs — system events before
/// that (or in the perception bin) fall back to log-only.
static GLOBAL_SENDER: LazyLock<RwLock<Option<EventSender>>> = LazyLock::new(|| RwLock::new(None));

/// Installs the events channel as the process-global sender for producers
/// without an injection path. Called once at runtime boot, right after
/// [`spawn_event_writer`].
pub fn install_global_sender(sender: EventSender) {
    *GLOBAL_SENDER.write() = Some(sender);
}

/// Builds a `system` [`ContextEvent`] from a kind string and sends it via
/// the process-global sender (log-only until one is installed). Called by
/// `senses::privacy::emit_system_event` — the A2 seam, rewired to the
/// events channel in Task A6 without touching any call site. Unknown
/// kinds are logged and dropped rather than inventing registry entries.
pub fn send_system_event(kind: &str, detail: &str) {
    let Some(event_type) = system_event_type(kind) else {
        tracing::warn!(
            layer = "memory",
            component = "events",
            kind = kind,
            "system event kind is not in the §4.6 registry; dropping"
        );
        return;
    };
    let event = system_context_event(event_type, detail);
    match GLOBAL_SENDER.read().as_ref() {
        Some(sender) => sender.send(event),
        None => tracing::debug!(
            layer = "memory",
            component = "events",
            kind = kind,
            detail = detail,
            "system event before events channel install (log-only)"
        ),
    }
}

// ---------------------------------------------------------------------------
// Writer task
// ---------------------------------------------------------------------------

/// Fixed-capacity LRU of open collapse keys → dedupe bookkeeping, so hot
/// keys skip the SELECT. Only the writer task touches it (single-writer),
/// so a plain map + logical clock is enough.
#[derive(Debug)]
struct LruCache {
    map: HashMap<String, (u64, DedupeCandidate)>,
    tick: u64,
    capacity: usize,
}

impl LruCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            tick: 0,
            capacity: capacity.max(1),
        }
    }

    fn get(&mut self, key: &str) -> Option<DedupeCandidate> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|entry| {
            entry.0 = tick;
            entry.1
        })
    }

    fn put(&mut self, key: String, candidate: DedupeCandidate) {
        self.tick += 1;
        self.map.insert(key, (self.tick, candidate));
        if self.map.len() > self.capacity {
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (tick, _))| *tick)
                .map(|(key, _)| key.clone())
            {
                self.map.remove(&oldest);
            }
        }
    }
}

/// Shared writer status the health handle reads.
#[derive(Debug)]
struct WriterStatus {
    last_flush_unix_ms: AtomicI64,
    alive: AtomicBool,
    stopped_cleanly: AtomicBool,
    rows_written: AtomicU64,
    rows_collapsed: AtomicU64,
}

impl Default for WriterStatus {
    fn default() -> Self {
        Self {
            last_flush_unix_ms: AtomicI64::new(0),
            alive: AtomicBool::new(true),
            stopped_cleanly: AtomicBool::new(false),
            rows_written: AtomicU64::new(0),
            rows_collapsed: AtomicU64::new(0),
        }
    }
}

/// Health handle onto the events-writer task (spec §7): queue depth and
/// last-flush timestamp for the health snapshot, and a `should_restart`
/// that is `true` only when the writer died unexpectedly (channel closed
/// or panic path — never on clean shutdown).
#[derive(Debug, Clone)]
pub struct EventWriterHandle {
    depth: Arc<AtomicI64>,
    status: Arc<WriterStatus>,
    /// The writer task itself, so shutdown can *wait* for its final flush
    /// instead of racing it. Taken by the first [`Self::wait_for_stop`]
    /// call; every later call falls back to the liveness flag.
    task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl EventWriterHandle {
    /// Waits (up to `timeout`) for the writer task to observe the shutdown
    /// signal, flush its pending batch and exit.
    ///
    /// **Call this before closing the raw-log pool.** The writer's final
    /// flush needs a pooled connection; closing the pool first makes the
    /// flush fail and silently drop up to `MAX_BATCH` events on every
    /// clean shutdown.
    ///
    /// Returns `true` when the task is confirmed stopped.
    pub async fn wait_for_stop(&self, timeout: Duration) -> bool {
        let task = self.task.lock().await.take();
        let Some(task) = task else {
            // Someone already awaited it (or this is a clone racing the
            // first waiter) — report the observable state.
            return !self.status.alive.load(Ordering::Relaxed);
        };
        match tokio::time::timeout(timeout, task).await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                tracing::error!(
                    layer = "memory",
                    component = "events",
                    error = %error,
                    "events writer task ended abnormally; its pending batch is lost"
                );
                false
            }
            Err(_) => {
                tracing::warn!(
                    layer = "memory",
                    component = "events",
                    timeout_ms = timeout.as_millis() as u64,
                    "events writer did not stop in time; shutting down without its final flush"
                );
                false
            }
        }
    }

    /// Events currently queued between producers and the writer.
    pub fn queue_depth(&self) -> u64 {
        self.depth.load(Ordering::Relaxed).max(0) as u64
    }

    /// When the writer last completed a flush (successful batch or empty
    /// tick); `None` before the first flush.
    pub fn last_flush_at(&self) -> Option<DateTime<Utc>> {
        let ms = self.status.last_flush_unix_ms.load(Ordering::Relaxed);
        (ms > 0).then(|| DateTime::from_timestamp_millis(ms))?
    }

    /// `true` while the writer task is running.
    pub fn is_healthy(&self) -> bool {
        self.status.alive.load(Ordering::Relaxed)
    }

    /// `true` only when the writer died without a shutdown signal
    /// (channel closed unexpectedly). Clean shutdown never restarts.
    pub fn should_restart(&self) -> bool {
        !self.status.alive.load(Ordering::Relaxed)
            && !self.status.stopped_cleanly.load(Ordering::Relaxed)
    }

    /// Fresh rows inserted since start (tests, health detail).
    pub fn rows_written(&self) -> u64 {
        self.status.rows_written.load(Ordering::Relaxed)
    }

    /// Collapse bumps applied since start (tests, health detail).
    pub fn rows_collapsed(&self) -> u64 {
        self.status.rows_collapsed.load(Ordering::Relaxed)
    }
}

/// The dedupe + batch-write core of the events-writer task. Owned by
/// [`spawn_event_writer`]'s loop; tests drive it directly.
struct EventWriter {
    raw_log: RawLog,
    collapse_window: chrono::Duration,
    count_cap: i64,
    span_cap: chrono::Duration,
    lru: LruCache,
    project: Option<CurrentProjectHandle>,
    dropped: Arc<DropCounters>,
    status: Arc<WriterStatus>,
}

impl EventWriter {
    fn new(
        raw_log: RawLog,
        config: &EventsConfig,
        project: Option<CurrentProjectHandle>,
        dropped: Arc<DropCounters>,
        status: Arc<WriterStatus>,
    ) -> Self {
        Self {
            raw_log,
            collapse_window: chrono::Duration::minutes(config.collapse_window_minutes.max(1) as i64),
            count_cap: i64::from(config.count_cap.max(1)),
            span_cap: chrono::Duration::hours(config.span_cap_hours.max(1) as i64),
            lru: LruCache::new(DEDUPE_LRU_CAPACITY),
            project,
            dropped,
            status,
        }
    }

    /// Flushes one batch: drains the dropped counters into
    /// `events_dropped` events, stamps missing project ids from the
    /// writer's [`CurrentProjectHandle`] (a backstop — since fixwave 3a
    /// the stamp normally happens at [`EventSender::send`]; this still
    /// covers the writer's own `events_dropped` rows and any event that
    /// reached the channel through a sender with no handle), and applies
    /// dedupe + inserts in
    /// ONE `BEGIN IMMEDIATE` transaction (the vault-index `finish()`
    /// pattern). A rolled-back batch may leave LRU counts slightly ahead
    /// of the DB; the `rows_affected == 0` fallback in
    /// [`Self::write_one`] heals dangling rowids, and counts self-correct
    /// on the next successful collapse.
    async fn flush(&mut self, mut batch: Vec<ContextEvent>) -> anyhow::Result<()> {
        for (source, dropped) in self.dropped.drain() {
            tracing::warn!(
                layer = "memory",
                component = "events",
                source = %event_enum_token(&source),
                dropped = dropped,
                "events dropped on queue overflow; coalescing into events_dropped row"
            );
            batch.push(events_dropped_event(source, dropped, Utc::now()));
        }
        if batch.is_empty() {
            self.mark_flush();
            return Ok(());
        }
        if let Some(handle) = self.project.clone() {
            for event in &mut batch {
                fill_project_id_from_handle(event, Some(&handle));
            }
        }
        let mut conn = self.raw_log.immediate_conn().await?;
        let mut result = Ok(());
        for event in &batch {
            if let Err(error) = self.write_one(&mut conn, event).await {
                result = Err(error);
                break;
            }
        }
        RawLog::finish_write(conn, result).await?;
        self.mark_flush();
        Ok(())
    }

    /// Dedupes and writes one event inside the batch transaction.
    async fn write_one(
        &mut self,
        conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
        event: &ContextEvent,
    ) -> anyhow::Result<()> {
        let key = dedupe_key(event);
        let candidate = match self.lru.get(&key) {
            Some(candidate) => Some(candidate),
            None => raw_log::latest_context_event_for_key(&mut *conn, &key).await?,
        };
        // Collapse window anchors on ts_last; caps start a fresh row
        // (spec §4.6).
        let open = candidate.filter(|row| {
            event.ts.signed_duration_since(row.ts_last) <= self.collapse_window
                && row.count < self.count_cap
                && event.ts.signed_duration_since(row.ts_first) <= self.span_cap
        });
        if let Some(row) = open {
            let collapsed =
                raw_log::collapse_context_event(&mut *conn, row.id, row.count + 1, event.ts)
                    .await?;
            if collapsed {
                self.lru.put(
                    key,
                    DedupeCandidate {
                        id: row.id,
                        ts_first: row.ts_first,
                        ts_last: event.ts,
                        count: row.count + 1,
                    },
                );
                self.status.rows_collapsed.fetch_add(1, Ordering::Relaxed);
                Self::mark_source_frame(conn, event).await?;
                return Ok(());
            }
            // Stale LRU entry (row rotated away) — fall through to insert.
        }
        let id = raw_log::insert_context_event(&mut *conn, event, &key).await?;
        self.lru.put(
            key,
            DedupeCandidate {
                id,
                ts_first: event.ts,
                ts_last: event.ts,
                count: 1,
            },
        );
        self.status.rows_written.fetch_add(1, Ordering::Relaxed);
        Self::mark_source_frame(conn, event).await?;
        Ok(())
    }

    /// Records that this event's source frame has become a context event
    /// (fixwave 3a, C3), so the raw-frame distillation fallback stops
    /// re-recording the same moment.
    ///
    /// Two deliberate conditions:
    ///
    /// - The reference must parse as a **UUID**. Only screen/audio
    ///   classification puts a frame id in `raw_reference`; a git sha or a
    ///   file path there is not a frame pointer.
    /// - The summary must be **non-blank** (spec §4.11 / fixwave 3a I2).
    ///   The distiller's primary query excludes blank-summary rows
    ///   permanently, so letting such an event suppress its frame would
    ///   erase the moment from memory entirely rather than compress it.
    async fn mark_source_frame(
        conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
        event: &ContextEvent,
    ) -> anyhow::Result<()> {
        if event.summary.trim().is_empty() {
            return Ok(());
        }
        let Some(reference) = event.raw_reference.as_deref() else {
            return Ok(());
        };
        let Ok(frame_id) = uuid::Uuid::parse_str(reference) else {
            return Ok(());
        };
        raw_log::mark_frame_context_event(&mut *conn, &frame_id, event.ts).await
    }

    fn mark_flush(&self) {
        self.status
            .last_flush_unix_ms
            .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
    }
}

/// Spawns the dedicated events-writer task (spec §3): creates the bounded
/// channel (`[events].queue_cap`), returns the producer handle to clone
/// into collectors and the health handle for the snapshot. The task
/// drains the channel, batches (≥ [`BATCH_FLUSH_THRESHOLD`] events or a
/// 500 ms tick), dedupes, and writes each batch in one transaction; it
/// also runs `[events]` retention rotation hourly.
///
/// `project` is the resolver's shared current-project handle: events
/// arriving with `project_id: None` (producers without their own handle,
/// e.g. system events) are stamped at flush time.
pub fn spawn_event_writer(
    raw_log: RawLog,
    config: &EventsConfig,
    project: Option<CurrentProjectHandle>,
    shutdown: watch::Receiver<bool>,
) -> (EventSender, EventWriterHandle) {
    spawn_event_writer_with_interval(raw_log, config, project, shutdown, DEFAULT_FLUSH_INTERVAL)
}

/// [`spawn_event_writer`] with an explicit flush interval (tests).
fn spawn_event_writer_with_interval(
    raw_log: RawLog,
    config: &EventsConfig,
    project: Option<CurrentProjectHandle>,
    shutdown: watch::Receiver<bool>,
    flush_interval: Duration,
) -> (EventSender, EventWriterHandle) {
    let (sender, rx) = EventSender::bounded(config.queue_cap);
    let sender = sender.with_project(project.clone());
    let status = Arc::new(WriterStatus::default());
    let writer = EventWriter::new(
        raw_log,
        config,
        project,
        sender.dropped.clone(),
        status.clone(),
    );
    let retention_days = config.retention_days;
    let depth = sender.depth.clone();
    let task = tokio::spawn(writer_loop(
        writer,
        rx,
        shutdown,
        flush_interval,
        retention_days,
        depth,
    ));
    let handle = EventWriterHandle {
        depth: sender.depth.clone(),
        status,
        task: Arc::new(tokio::sync::Mutex::new(Some(task))),
    };
    (sender, handle)
}

/// The writer task body: recv + try_recv drain, tick-or-threshold batch
/// flushes, hourly retention rotation, never-fail-the-loop error
/// handling. Exits on shutdown signal (clean) or channel close
/// (unexpected — every sender dropped while the runtime lives).
async fn writer_loop(
    mut writer: EventWriter,
    mut rx: mpsc::Receiver<ContextEvent>,
    mut shutdown: watch::Receiver<bool>,
    flush_interval: Duration,
    retention_days: u32,
    depth: Arc<AtomicI64>,
) {
    tracing::info!(
        layer = "memory",
        component = "events",
        flush_interval_ms = flush_interval.as_millis() as u64,
        retention_days = retention_days,
        "events writer task started"
    );
    let mut ticker = tokio::time::interval(flush_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pending: Vec<ContextEvent> = Vec::new();
    let mut last_rotate: Option<Instant> = None;
    let clean = loop {
        tokio::select! {
            _ = ticker.tick() => {
                flush_pending(&mut writer, &mut pending).await;
                let rotate_due = last_rotate
                    .map(|at| at.elapsed() >= ROTATE_INTERVAL)
                    .unwrap_or(true);
                if rotate_due {
                    last_rotate = Some(Instant::now());
                    if let Err(error) = writer.raw_log.rotate_events(retention_days).await {
                        tracing::warn!(
                            layer = "memory",
                            component = "events",
                            error = %error,
                            "context_events retention rotation failed; retrying next interval"
                        );
                    }
                }
            }
            received = rx.recv() => {
                match received {
                    Some(event) => {
                        depth.fetch_sub(1, Ordering::Relaxed);
                        pending.push(event);
                        while pending.len() < MAX_BATCH {
                            match rx.try_recv() {
                                Ok(event) => {
                                    depth.fetch_sub(1, Ordering::Relaxed);
                                    pending.push(event);
                                }
                                Err(_) => break,
                            }
                        }
                        if pending.len() >= BATCH_FLUSH_THRESHOLD {
                            flush_pending(&mut writer, &mut pending).await;
                        }
                    }
                    None => {
                        flush_pending(&mut writer, &mut pending).await;
                        break false;
                    }
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    flush_pending(&mut writer, &mut pending).await;
                    break true;
                }
            }
        }
    };
    writer
        .status
        .stopped_cleanly
        .store(clean, Ordering::Relaxed);
    writer.status.alive.store(false, Ordering::Relaxed);
    if clean {
        tracing::info!(
            layer = "memory",
            component = "events",
            "events writer task stopped (shutdown)"
        );
    } else {
        tracing::error!(
            layer = "memory",
            component = "events",
            "events writer channel closed unexpectedly; writer task stopped"
        );
    }
}

/// Flushes the pending batch, never failing the loop: a failed batch is
/// logged and dropped (the transaction rolled back as one unit).
async fn flush_pending(writer: &mut EventWriter, pending: &mut Vec<ContextEvent>) {
    let batch = std::mem::take(pending);
    let batch_len = batch.len();
    if let Err(error) = writer.flush(batch).await {
        tracing::warn!(
            layer = "memory",
            component = "events",
            error = %error,
            batch_len = batch_len,
            "events batch flush failed; batch dropped"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::project::{CurrentProject, ProjectStatus};

    fn event(summary: &str) -> ContextEvent {
        ContextEvent {
            ts: Utc::now(),
            source: EventSource::Window,
            application: "Code.exe".into(),
            window_title: "main.rs".into(),
            project_id: None,
            event_type: EventType::FocusSwitch,
            summary: summary.into(),
            importance: COLLECTOR_EVENT_IMPORTANCE,
            confidence: 1.0,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: None,
        }
    }

    fn template_event(summary: &str, ts: DateTime<Utc>) -> ContextEvent {
        ContextEvent {
            ts,
            source: EventSource::System,
            application: String::new(),
            window_title: String::new(),
            project_id: None,
            event_type: EventType::ToggleChange,
            summary: summary.into(),
            importance: COLLECTOR_EVENT_IMPORTANCE,
            confidence: 1.0,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: None,
        }
    }

    fn screen_event(application: &str, summary: &str, ts: DateTime<Utc>) -> ContextEvent {
        ContextEvent {
            ts,
            source: EventSource::Screen,
            application: application.into(),
            window_title: "editor".into(),
            project_id: Some("continuum".into()),
            event_type: EventType::Error,
            summary: summary.into(),
            importance: 0.7,
            confidence: 0.8,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: None,
        }
    }

    fn current_project(id: &str) -> CurrentProject {
        CurrentProject {
            id: id.to_string(),
            name: id.to_string(),
            root_path: None,
            confidence: 0.9,
            source_tier: 1,
            zone: None,
            status: ProjectStatus::Confirmed,
        }
    }

    async fn open_log(dir: &tempfile::TempDir) -> RawLog {
        let path = dir.path().join("events-test.db");
        RawLog::open(&path.to_string_lossy()).await.unwrap()
    }

    fn writer_for(
        raw_log: RawLog,
        config: &EventsConfig,
        project: Option<CurrentProjectHandle>,
    ) -> (EventWriter, Arc<DropCounters>) {
        let dropped = Arc::new(DropCounters::default());
        let writer = EventWriter::new(
            raw_log,
            config,
            project,
            dropped.clone(),
            Arc::new(WriterStatus::default()),
        );
        (writer, dropped)
    }

    // --- registry stability -------------------------------------------------

    #[test]
    fn registries_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&EventSource::Window).unwrap(),
            "\"window\""
        );
        assert_eq!(
            serde_json::to_string(&EventType::FocusSwitch).unwrap(),
            "\"focus_switch\""
        );
        assert_eq!(
            serde_json::to_string(&EventSensitivity::LocalOnly).unwrap(),
            "\"local_only\""
        );
        let t: EventType = serde_json::from_str("\"events_dropped\"").unwrap();
        assert_eq!(t, EventType::EventsDropped);
    }

    #[test]
    fn registry_round_trips_every_type_and_source() {
        // The persisted tokens are frozen (additive-only registry): this
        // list is the contract. A variant rename breaks this test — and
        // would break every persisted row, so don't.
        let expected_types: [(EventType, &str); 30] = [
            (EventType::FocusSwitch, "focus_switch"),
            (EventType::ProjectSwitch, "project_switch"),
            (EventType::Commit, "commit"),
            (EventType::BranchSwitch, "branch_switch"),
            (EventType::Conflict, "conflict"),
            (EventType::DirtyChange, "dirty_change"),
            (EventType::FileModified, "file_modified"),
            (EventType::FileCreated, "file_created"),
            (EventType::FileDeleted, "file_deleted"),
            (EventType::FileRenamed, "file_renamed"),
            (EventType::FilesBulkChange, "files_bulk_change"),
            (EventType::ProcessStarted, "process_started"),
            (EventType::ProcessStopped, "process_stopped"),
            (EventType::ResourcePressure, "resource_pressure"),
            (EventType::Error, "error"),
            (EventType::Success, "success"),
            (EventType::Decision, "decision"),
            (EventType::Preference, "preference"),
            (EventType::TaskProgress, "task_progress"),
            (EventType::Communication, "communication"),
            (EventType::Routine, "routine"),
            (EventType::Other, "other"),
            (EventType::IdleStart, "idle_start"),
            (EventType::IdleEnd, "idle_end"),
            (EventType::Wake, "wake"),
            (EventType::WakeResult, "wake_result"),
            (EventType::VoiceCommand, "voice_command"),
            (EventType::ToggleChange, "toggle_change"),
            (EventType::SourceUnavailable, "source_unavailable"),
            (EventType::EventsDropped, "events_dropped"),
        ];
        assert_eq!(expected_types.len(), ALL_EVENT_TYPES.len());
        for (event_type, token) in expected_types {
            assert_eq!(event_enum_token(&event_type), token);
            assert_eq!(parse_event_enum::<EventType>(token), Some(event_type));
            assert!(
                ALL_EVENT_SOURCES.iter().any(|&s| event_type.valid_for(s)),
                "{token} has no valid source"
            );
            assert!(ALL_EVENT_TYPES.contains(&event_type));
        }
        let expected_sources: [(EventSource, &str); 8] = [
            (EventSource::Window, "window"),
            (EventSource::Git, "git"),
            (EventSource::File, "file"),
            (EventSource::Screen, "screen"),
            (EventSource::Audio, "audio"),
            (EventSource::System, "system"),
            (EventSource::Voice, "voice"),
            (EventSource::Process, "process"),
        ];
        for (source, token) in expected_sources {
            assert_eq!(event_enum_token(&source), token);
            assert_eq!(parse_event_enum::<EventSource>(token), Some(source));
        }
        for (sensitivity, token) in [
            (EventSensitivity::LocalOnly, "local_only"),
            (EventSensitivity::CloudAllowed, "cloud_allowed"),
        ] {
            assert_eq!(event_enum_token(&sensitivity), token);
            assert_eq!(
                parse_event_enum::<EventSensitivity>(token),
                Some(sensitivity)
            );
        }
    }

    #[test]
    fn context_event_roundtrips_through_serde() {
        let original = event("Code.exe → chrome.exe after 12s");
        let json = serde_json::to_string(&original).unwrap();
        let back: ContextEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
        assert!(json.contains("\"focus_switch\""));
        assert!(json.contains("\"cloud_allowed\""));
    }

    #[test]
    fn valid_for_matches_the_spec_table() {
        use EventSource::*;
        use EventType::*;
        // window
        assert!(FocusSwitch.valid_for(Window));
        assert!(ProjectSwitch.valid_for(Window));
        assert!(!FocusSwitch.valid_for(Git));
        // git
        for t in [Commit, BranchSwitch, Conflict, DirtyChange] {
            assert!(t.valid_for(Git));
            assert!(!t.valid_for(Window));
        }
        // file
        for t in [
            FileModified,
            FileCreated,
            FileDeleted,
            FileRenamed,
            FilesBulkChange,
        ] {
            assert!(t.valid_for(File));
            assert!(!t.valid_for(System));
        }
        // screen/audio classification
        for t in [
            Error,
            Success,
            Decision,
            Preference,
            TaskProgress,
            Communication,
            Routine,
            Other,
        ] {
            assert!(t.valid_for(Screen));
            assert!(t.valid_for(Audio));
            assert!(!t.valid_for(Window));
            assert!(!t.valid_for(System));
        }
        // system
        for t in [
            IdleStart,
            IdleEnd,
            Wake,
            WakeResult,
            VoiceCommand,
            ToggleChange,
            SourceUnavailable,
            EventsDropped,
        ] {
            assert!(t.valid_for(System));
        }
        assert!(!ToggleChange.valid_for(Voice));
        // voice_command is additionally valid for the voice source.
        assert!(VoiceCommand.valid_for(Voice));
        assert!(!FocusSwitch.valid_for(Voice));
    }

    #[test]
    fn project_switch_event_uses_the_stable_template() {
        let ts = Utc::now();
        let event = project_switch_event(
            Some("continuum"),
            "simcharts",
            "Code.exe",
            "chart.tsx - simcharts - Code",
            None,
            ts,
        );
        assert_eq!(event.summary, "project continuum → simcharts");
        assert_eq!(event.source, EventSource::Window);
        assert_eq!(event.event_type, EventType::ProjectSwitch);
        assert!(event.event_type.valid_for(event.source));
        assert_eq!(event.project_id.as_deref(), Some("simcharts"));
        assert_eq!(event.confidence, 1.0);
        assert_eq!(event.sensitivity, EventSensitivity::CloudAllowed);
        // First adoption renders (none).
        let first = project_switch_event(None, "continuum", "Code.exe", "x", None, ts);
        assert_eq!(first.summary, "project (none) → continuum");
    }

    /// Fixwave 3b (minor): the destination project's zone decides the
    /// event's sensitivity, like it does for git and file events. The name
    /// of a `local_only` project is exactly the identity §4.1 protects.
    #[test]
    fn project_switch_event_folds_in_the_destination_project_zone() {
        let ts = Utc::now();
        let private = project_switch_event(
            Some("continuum"),
            "taxes",
            "Code.exe",
            "x",
            Some(Zone::LocalOnly),
            ts,
        );
        assert_eq!(private.sensitivity, EventSensitivity::LocalOnly);
        let open = project_switch_event(
            Some("taxes"),
            "continuum",
            "Code.exe",
            "x",
            Some(Zone::CloudAllowed),
            ts,
        );
        assert_eq!(open.sensitivity, EventSensitivity::CloudAllowed);
    }

    /// The transport stamp folds the project zone in too, which is what
    /// covers `focus_switch` (built below the resolver, so it can only see
    /// the two window dispositions).
    #[test]
    fn stamping_a_local_only_project_downgrades_a_cloud_allowed_event() {
        let handle: CurrentProjectHandle = Arc::new(RwLock::new(Some(CurrentProject {
            id: "taxes".to_string(),
            name: "Taxes".to_string(),
            root_path: None,
            confidence: 0.9,
            source_tier: 1,
            zone: Some(Zone::LocalOnly),
            status: ProjectStatus::Confirmed,
        })));

        let mut event = event("visible → visible after 30s");
        event.source = EventSource::Window;
        event.event_type = EventType::FocusSwitch;
        event.sensitivity = EventSensitivity::CloudAllowed;
        fill_project_id_from_handle(&mut event, Some(&handle));
        assert_eq!(event.project_id.as_deref(), Some("taxes"));
        assert_eq!(event.sensitivity, EventSensitivity::LocalOnly);
    }

    #[test]
    fn system_event_type_accepts_registry_kinds_only() {
        assert_eq!(
            system_event_type("toggle_change"),
            Some(EventType::ToggleChange)
        );
        assert_eq!(system_event_type("idle_start"), Some(EventType::IdleStart));
        assert_eq!(
            system_event_type("events_dropped"),
            Some(EventType::EventsDropped)
        );
        // Valid EventType but not a system type.
        assert_eq!(system_event_type("focus_switch"), None);
        // Not in the registry at all.
        assert_eq!(system_event_type("made_up_kind"), None);
    }

    // --- normalization ------------------------------------------------------

    #[test]
    fn normalize_lowercases_and_collapses_whitespace() {
        assert_eq!(
            normalize_summary("Build   FAILED\t badly"),
            "build failed badly"
        );
    }

    #[test]
    fn normalize_strips_digits_hex_paths_and_quotes() {
        // Digits.
        assert_eq!(
            normalize_summary("error 404 occurred 12 times"),
            "error occurred times"
        );
        // Hex runs (git OIDs) — short hex-ish words survive.
        assert_eq!(
            normalize_summary("commit deadbeef1234567890 pushed to abc"),
            "commit pushed to abc"
        );
        // Paths, both flavors.
        assert_eq!(
            normalize_summary(r"saved D:\dev\continuum\main.rs quickly"),
            "saved quickly"
        );
        assert_eq!(normalize_summary("opened src/lib.rs now"), "opened now");
        // Quoted strings.
        assert_eq!(
            normalize_summary(r#"deleted "temp file" and 'old notes' done"#),
            "deleted and done"
        );
    }

    #[test]
    fn normalize_truncates_to_twelve_tokens() {
        let long = "a b c d e f g h i j k l m n o p";
        assert_eq!(normalize_summary(long), "a b c d e f g h i j k l");
        // And is idempotent.
        assert_eq!(
            normalize_summary(&normalize_summary(long)),
            normalize_summary(long)
        );
    }

    // --- dedupe keys --------------------------------------------------------

    #[test]
    fn template_key_uses_normalized_summary_not_application() {
        let ts = Utc::now();
        let a = template_event("toggle mic changed 3 times", ts);
        let b = template_event("Toggle MIC changed 99 times", ts);
        assert_eq!(dedupe_key(&a), dedupe_key(&b), "digits/case must not split");

        // Different application, same template → same key (application is
        // not keyed for template sources).
        let mut c = event("Code.exe → chrome.exe after 12s");
        let mut d = event("Code.exe → chrome.exe after 99s");
        c.application = "chrome.exe".into();
        d.application = "msedge.exe".into();
        assert_eq!(dedupe_key(&c), dedupe_key(&d));

        // Different normalized summaries split.
        let e = template_event("mic muted", ts);
        assert_ne!(dedupe_key(&a), dedupe_key(&e));

        // project_id and event_type are keyed.
        let mut f = template_event("toggle mic changed 3 times", ts);
        f.project_id = Some("continuum".into());
        assert_ne!(dedupe_key(&a), dedupe_key(&f));
        let mut g = template_event("toggle mic changed 3 times", ts);
        g.event_type = EventType::SourceUnavailable;
        assert_ne!(dedupe_key(&a), dedupe_key(&g));
    }

    #[test]
    fn classified_key_uses_application_not_summary() {
        let ts = Utc::now();
        let a = screen_event("Code.exe", "build failed with 3 errors", ts);
        let b = screen_event(
            "Code.exe",
            "compilation broke again, totally different words",
            ts,
        );
        assert_eq!(
            dedupe_key(&a),
            dedupe_key(&b),
            "screen/audio summaries must not be keyed"
        );
        let c = screen_event("chrome.exe", "build failed with 3 errors", ts);
        assert_ne!(dedupe_key(&a), dedupe_key(&c), "application is keyed");
        // Source is keyed: an identical audio event gets its own key.
        let mut d = a.clone();
        d.source = EventSource::Audio;
        assert_ne!(dedupe_key(&a), dedupe_key(&d));
    }

    #[test]
    fn process_key_uses_stable_process_identity() {
        let ts = Utc::now();
        let mut first = template_event("runtime process python stopped (pid 10)", ts);
        first.source = EventSource::Process;
        first.event_type = EventType::ProcessStopped;
        first.application = "python".into();
        first.raw_reference = Some("pid:10:started:100".into());

        let mut second = first.clone();
        second.summary = "runtime process python stopped (pid 11)".into();
        second.raw_reference = Some("pid:11:started:101".into());
        assert_ne!(dedupe_key(&first), dedupe_key(&second));

        let mut repeat = first.clone();
        repeat.summary = "python sustained resource pressure: 91% CPU".into();
        assert_eq!(dedupe_key(&first), dedupe_key(&repeat));
    }

    // --- fixwave 3a C2: per-path file events must not share one key -------

    fn file_event(event_type: EventType, summary: &str) -> ContextEvent {
        ContextEvent {
            ts: Utc::now(),
            source: EventSource::File,
            application: String::new(),
            window_title: String::new(),
            project_id: Some("continuum".into()),
            event_type,
            summary: summary.into(),
            importance: COLLECTOR_EVENT_IMPORTANCE,
            confidence: 1.0,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: None,
        }
    }

    #[test]
    fn forty_distinct_paths_produce_forty_distinct_keys() {
        // The C2 bug: `normalize_summary` strips path-like tokens, and a
        // file event's whole summary IS a root-relative path — so every
        // file in every subdirectory hashed to
        // `hash("file", "file_modified", project, "")` and 40 touched
        // files became one row reading "src/first_file.rs ×40".
        let paths: Vec<String> = (0..40)
            .map(|i| format!("src/module{i}/file{i}.rs"))
            .collect();
        let keys: std::collections::HashSet<String> = paths
            .iter()
            .map(|p| dedupe_key(&file_event(EventType::FileModified, p)))
            .collect();
        assert_eq!(
            keys.len(),
            40,
            "40 distinct paths collapsed onto {} row(s)",
            keys.len()
        );

        // The same path twice still collapses — dedupe is not disabled,
        // only its discriminator corrected.
        assert_eq!(
            dedupe_key(&file_event(EventType::FileModified, &paths[0])),
            dedupe_key(&file_event(EventType::FileModified, &paths[0]))
        );

        // A bare filename at the root has no path separator at all, and
        // must not merge with a nested one.
        assert_ne!(
            dedupe_key(&file_event(EventType::FileModified, "Cargo.toml")),
            dedupe_key(&file_event(EventType::FileModified, "crates/Cargo.toml"))
        );
    }

    #[test]
    fn rename_pairs_are_distinct_and_keyed_by_type_and_project() {
        // `"a/b.rs → c/d.rs"` normalized to the bare arrow, so every
        // rename in a project shared one row.
        let first = file_event(EventType::FileRenamed, "src/a.rs → src/b.rs");
        let second = file_event(EventType::FileRenamed, "docs/c.md → docs/d.md");
        assert_ne!(dedupe_key(&first), dedupe_key(&second));

        // event_type still splits: modifying `src/a.rs` is not deleting it.
        assert_ne!(
            dedupe_key(&file_event(EventType::FileModified, "src/a.rs")),
            dedupe_key(&file_event(EventType::FileDeleted, "src/a.rs"))
        );
        // project_id still splits.
        let mut other_project = file_event(EventType::FileModified, "src/a.rs");
        other_project.project_id = Some("simcharts".into());
        assert_ne!(
            dedupe_key(&file_event(EventType::FileModified, "src/a.rs")),
            dedupe_key(&other_project)
        );
    }

    #[test]
    fn bulk_change_templates_keep_normalized_collapse() {
        // The storm-collapse summary is prose with a count, not a path:
        // it must keep normalizing so repeated bulk rows still collapse
        // across flushes instead of minting a row per count.
        let a = file_event(EventType::FilesBulkChange, "40 files changed in continuum");
        let b = file_event(
            EventType::FilesBulkChange,
            "1200 files changed in continuum",
        );
        assert_eq!(
            dedupe_key(&a),
            dedupe_key(&b),
            "bulk-change counts must not split the row"
        );
        let resync = file_event(EventType::FilesBulkChange, "file resync in continuum");
        assert_ne!(dedupe_key(&a), dedupe_key(&resync));
    }

    // --- project stamping ---------------------------------------------------

    #[test]
    fn fill_project_id_stamps_only_missing_ids() {
        let handle: CurrentProjectHandle =
            Arc::new(RwLock::new(Some(current_project("continuum"))));
        let mut unstamped = event("switch");
        fill_project_id_from_handle(&mut unstamped, Some(&handle));
        assert_eq!(unstamped.project_id.as_deref(), Some("continuum"));

        let mut stamped = event("switch");
        stamped.project_id = Some("simcharts".into());
        fill_project_id_from_handle(&mut stamped, Some(&handle));
        assert_eq!(
            stamped.project_id.as_deref(),
            Some("simcharts"),
            "existing project ids must never be overwritten"
        );

        let empty: CurrentProjectHandle = Arc::new(RwLock::new(None));
        let mut no_project = event("switch");
        fill_project_id_from_handle(&mut no_project, Some(&empty));
        assert_eq!(no_project.project_id, None);
        fill_project_id_from_handle(&mut no_project, None);
        assert_eq!(no_project.project_id, None);
    }

    // --- fixwave 3a I3: a deliberate None is not re-stamped ---------------

    #[test]
    fn a_classifier_nulled_project_survives_the_writer_fill() {
        // `triage::consume::resolve_project` already applied the resolver's
        // value at frame time and landed on `None`. Re-stamping that later
        // with whatever the resolver holds at flush time invents an
        // attribution the classifier refused — and attributes a frame from
        // 20 minutes ago to the window that happens to be focused now.
        let handle: CurrentProjectHandle =
            Arc::new(RwLock::new(Some(current_project("continuum"))));
        for source in [EventSource::Screen, EventSource::Audio] {
            let mut classified = screen_event("Code.exe", "build failed", Utc::now());
            classified.source = source;
            classified.project_id = None;
            fill_project_id_from_handle(&mut classified, Some(&handle));
            assert_eq!(
                classified.project_id, None,
                "{source:?} events resolve their own project"
            );
        }

        // Git/file collectors always carry their watched root's project;
        // an unset one there is a bug in the collector, not a gap the
        // transport should paper over.
        let mut file = file_event(EventType::FileModified, "src/a.rs");
        file.project_id = None;
        fill_project_id_from_handle(&mut file, Some(&handle));
        assert_eq!(file.project_id, None);

        // Handle-less producers are still stamped.
        for source in [EventSource::Window, EventSource::System, EventSource::Voice] {
            assert!(source_defers_project_to_transport(source), "{source:?}");
        }
        for source in [
            EventSource::Screen,
            EventSource::Audio,
            EventSource::Git,
            EventSource::File,
        ] {
            assert!(!source_defers_project_to_transport(source), "{source:?}");
        }
    }

    #[test]
    fn the_observer_tap_sees_the_stamped_project() {
        // B5 seam: the observer used to run before the writer's flush-time
        // fill, so every in-memory consumer saw `project_id: None` for
        // handle-less producers.
        let handle: CurrentProjectHandle =
            Arc::new(RwLock::new(Some(current_project("continuum"))));
        let seen: Arc<parking_lot::Mutex<Vec<Option<String>>>> = Arc::default();
        let sink = seen.clone();
        let (sender, mut rx) = EventSender::bounded(8);
        let sender = sender.with_project(Some(handle)).with_observer(Arc::new(
            move |event: &ContextEvent| {
                sink.lock().push(event.project_id.clone());
            },
        ));

        sender.send(event("focus switch"));
        assert_eq!(
            seen.lock().as_slice(),
            &[Some("continuum".to_string())],
            "the tap must see the post-fill project"
        );
        // And the queued event carries the same value the tap saw.
        let queued = rx.try_recv().expect("event was queued");
        assert_eq!(queued.project_id.as_deref(), Some("continuum"));
    }

    // --- writer dedupe matrix ----------------------------------------------

    #[tokio::test]
    async fn writer_collapses_within_window_and_keeps_first_summary() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let (mut writer, _) = writer_for(log.clone(), &EventsConfig::default(), None);

        let t0 = Utc::now();
        writer
            .flush(vec![template_event("toggle mic changed 3 times", t0)])
            .await
            .unwrap();
        writer
            .flush(vec![template_event(
                "toggle mic changed 99 times",
                t0 + chrono::Duration::minutes(9),
            )])
            .await
            .unwrap();

        let rows = log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 1, "second event must collapse into the first");
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].summary, "toggle mic changed 3 times");
        assert_eq!(rows[0].ts_first, t0);
        assert_eq!(rows[0].ts_last, t0 + chrono::Duration::minutes(9));
        log.close().await;
    }

    #[tokio::test]
    async fn writer_starts_fresh_row_outside_window() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let (mut writer, _) = writer_for(log.clone(), &EventsConfig::default(), None);

        let t0 = Utc::now();
        writer
            .flush(vec![template_event("toggle mic changed", t0)])
            .await
            .unwrap();
        writer
            .flush(vec![template_event(
                "toggle mic changed",
                t0 + chrono::Duration::minutes(11),
            )])
            .await
            .unwrap();

        let rows = log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 2, "an 11-minute gap must start a fresh row");
        assert!(rows.iter().all(|r| r.count == 1));
        log.close().await;
    }

    #[tokio::test]
    async fn writer_anchor_rolls_with_ts_last() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let (mut writer, _) = writer_for(log.clone(), &EventsConfig::default(), None);

        // t0, +9 m, +18 m: each within 10 m of the previous ts_last, so
        // all three collapse even though +18 m is outside the window from
        // ts_first. A fourth at +29 m (11 m after ts_last) starts fresh.
        let t0 = Utc::now();
        for minutes in [0, 9, 18] {
            writer
                .flush(vec![template_event(
                    "toggle mic changed",
                    t0 + chrono::Duration::minutes(minutes),
                )])
                .await
                .unwrap();
        }
        let rows = log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 3);

        writer
            .flush(vec![template_event(
                "toggle mic changed",
                t0 + chrono::Duration::minutes(29),
            )])
            .await
            .unwrap();
        let rows = log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 2);
        log.close().await;
    }

    #[tokio::test]
    async fn writer_count_cap_starts_fresh_row() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let config = EventsConfig {
            count_cap: 2,
            ..EventsConfig::default()
        };
        let (mut writer, _) = writer_for(log.clone(), &config, None);

        let t0 = Utc::now();
        for minutes in [0, 1, 2] {
            writer
                .flush(vec![template_event(
                    "toggle mic changed",
                    t0 + chrono::Duration::minutes(minutes),
                )])
                .await
                .unwrap();
        }
        let rows = log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 2, "count_cap must start a fresh row");
        let mut counts: Vec<i64> = rows.iter().map(|r| r.count).collect();
        counts.sort_unstable();
        assert_eq!(counts, [1, 2]);
        log.close().await;
    }

    #[tokio::test]
    async fn writer_span_cap_starts_fresh_row() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let config = EventsConfig {
            collapse_window_minutes: 60,
            span_cap_hours: 1,
            ..EventsConfig::default()
        };
        let (mut writer, _) = writer_for(log.clone(), &config, None);

        // 0 m and 50 m collapse (span 50 m ≤ 60 m); 100 m is 50 m after
        // ts_last (inside the window) but span 100 m > 60 m → fresh row.
        let t0 = Utc::now();
        for minutes in [0, 50, 100] {
            writer
                .flush(vec![template_event(
                    "toggle mic changed",
                    t0 + chrono::Duration::minutes(minutes),
                )])
                .await
                .unwrap();
        }
        let rows = log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 2, "span_cap must start a fresh row");
        let mut counts: Vec<i64> = rows.iter().map(|r| r.count).collect();
        counts.sort_unstable();
        assert_eq!(counts, [1, 2]);
        log.close().await;
    }

    #[tokio::test]
    async fn writer_collapses_classified_events_despite_summary_variance() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let (mut writer, _) = writer_for(log.clone(), &EventsConfig::default(), None);

        let t0 = Utc::now();
        writer
            .flush(vec![
                screen_event("Code.exe", "build failed with 3 errors", t0),
                screen_event(
                    "Code.exe",
                    "compile broke, entirely different words",
                    t0 + chrono::Duration::minutes(1),
                ),
            ])
            .await
            .unwrap();

        let rows = log.list_context_events().await.unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the flagship build-failed case must collapse"
        );
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].summary, "build failed with 3 errors");

        // Control: template events with different normalized summaries do
        // NOT collapse.
        writer
            .flush(vec![
                template_event("mic muted", t0),
                template_event("screen paused", t0),
            ])
            .await
            .unwrap();
        assert_eq!(log.list_context_events().await.unwrap().len(), 3);
        log.close().await;
    }

    #[tokio::test]
    async fn writer_dedupes_within_a_single_batch() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let (mut writer, _) = writer_for(log.clone(), &EventsConfig::default(), None);

        let t0 = Utc::now();
        writer
            .flush(vec![
                template_event("toggle mic changed 1 times", t0),
                template_event(
                    "toggle mic changed 2 times",
                    t0 + chrono::Duration::seconds(5),
                ),
                template_event(
                    "toggle mic changed 3 times",
                    t0 + chrono::Duration::seconds(9),
                ),
            ])
            .await
            .unwrap();
        let rows = log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 3);
        log.close().await;
    }

    #[tokio::test]
    async fn writer_stamps_missing_project_ids_at_flush() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let handle: CurrentProjectHandle =
            Arc::new(RwLock::new(Some(current_project("continuum"))));
        let (mut writer, _) = writer_for(log.clone(), &EventsConfig::default(), Some(handle));

        let t0 = Utc::now();
        let mut pre_stamped = template_event("git thing", t0);
        pre_stamped.project_id = Some("simcharts".into());
        writer
            .flush(vec![template_event("toggle mic changed", t0), pre_stamped])
            .await
            .unwrap();

        let rows = log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 2);
        let stamped = rows.iter().find(|r| r.summary.contains("toggle")).unwrap();
        assert_eq!(stamped.project_id.as_deref(), Some("continuum"));
        let kept = rows.iter().find(|r| r.summary.contains("git")).unwrap();
        assert_eq!(
            kept.project_id.as_deref(),
            Some("simcharts"),
            "pre-stamped events (git collector) keep their project"
        );
        log.close().await;
    }

    // --- overflow coalesce --------------------------------------------------

    #[tokio::test]
    async fn overflow_coalesces_into_one_events_dropped_row_per_source() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;

        // A tiny queue with no consumer: 2 fit, 3 drop.
        let (sender, _rx) = EventSender::bounded(2);
        for i in 0..5 {
            sender.send(event(&format!("switch {i}")));
        }
        assert_eq!(sender.dropped_total(), 3);

        let dropped = sender.dropped.clone();
        let mut writer = EventWriter::new(
            log.clone(),
            &EventsConfig::default(),
            None,
            dropped,
            Arc::new(WriterStatus::default()),
        );
        writer.flush(Vec::new()).await.unwrap();
        assert_eq!(sender.dropped_total(), 0, "flush drains the counters");

        let rows = log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, EventType::EventsDropped);
        assert_eq!(rows[0].source, EventSource::System);
        assert_eq!(rows[0].summary, "3 events dropped from window");

        // No new drops → no new row on the next flush.
        writer.flush(Vec::new()).await.unwrap();
        assert_eq!(log.list_context_events().await.unwrap().len(), 1);

        // Further drops collapse into the same row (digits normalized out).
        for i in 0..3 {
            sender.send(event(&format!("switch again {i}")));
        }
        writer.flush(Vec::new()).await.unwrap();
        let rows = log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 1, "repeat coalesce rows collapse via dedupe");
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].summary, "3 events dropped from window");
        log.close().await;
    }

    #[test]
    fn sender_rejects_registry_violations() {
        let (sender, mut rx) = EventSender::bounded(4);
        let mut bad = event("switch");
        bad.event_type = EventType::Commit; // commit is git-only
        sender.send(bad);
        assert!(
            rx.try_recv().is_err(),
            "invalid combos never enter the queue"
        );
        assert_eq!(sender.dropped_total(), 0, "violations are not 'drops'");
        sender.send(event("ok"));
        assert!(rx.try_recv().is_ok());
    }

    // --- observer tap (Task B5) ---------------------------------------------

    /// The session-state tap must see every registry-valid event, must
    /// still see it when the queue is full (the row is lost, the state
    /// update is not), and must NOT see registry violations — those are
    /// dropped before anything downstream exists.
    #[test]
    fn observer_sees_valid_events_including_ones_the_queue_drops() {
        use std::sync::Mutex;

        // Capacity 1: the second send has nowhere to go.
        let (sender, _rx) = EventSender::bounded(1);
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sender = sender.with_observer({
            let seen = seen.clone();
            Arc::new(move |ev: &ContextEvent| {
                seen.lock().unwrap().push(ev.summary.clone());
            })
        });

        sender.send(event("first"));
        sender.send(event("second (queue full)"));
        assert_eq!(sender.dropped_total(), 1, "the second event was dropped");

        let mut bad = event("registry violation");
        bad.event_type = EventType::Commit; // commit is git-only
        sender.send(bad);

        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec!["first".to_string(), "second (queue full)".to_string()],
            "the tap sees dropped events but never registry violations"
        );
    }

    /// Clones made after `with_observer` carry the tap — the wiring in
    /// `bin/continuum.rs` depends on this (the observer is attached once,
    /// then the sender is cloned into every collector).
    #[test]
    fn observer_survives_cloning_the_sender() {
        use std::sync::atomic::AtomicUsize;

        let (sender, mut rx) = EventSender::bounded(8);
        let count = Arc::new(AtomicUsize::new(0));
        let sender = sender.with_observer({
            let count = count.clone();
            Arc::new(move |_: &ContextEvent| {
                count.fetch_add(1, Ordering::Relaxed);
            })
        });
        let collector = sender.clone();
        collector.send(event("from a clone"));
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert!(rx.try_recv().is_ok());

        // A log-only sink with no observer stays silent and safe.
        EventSender::log_only().send(event("nowhere"));
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    // --- FTS consistency ----------------------------------------------------

    #[tokio::test]
    async fn fts_stays_consistent_across_collapse_updates() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let (mut writer, _) = writer_for(log.clone(), &EventsConfig::default(), None);

        let t0 = Utc::now();
        writer
            .flush(vec![screen_event("Code.exe", "widget build failed", t0)])
            .await
            .unwrap();
        let hits = log.search_context_events("failed", 10).await.unwrap();
        assert_eq!(hits.len(), 1);

        // A collapse bump (count/ts_last only) must not rewrite or
        // duplicate the FTS row: the kept display text stays searchable,
        // the new occurrence's variant summary is NOT indexed.
        writer
            .flush(vec![screen_event(
                "Code.exe",
                "compilation exploded",
                t0 + chrono::Duration::minutes(2),
            )])
            .await
            .unwrap();
        let hits = log.search_context_events("failed", 10).await.unwrap();
        assert_eq!(hits.len(), 1, "collapse must not duplicate FTS rows");
        assert_eq!(hits[0].count, 2);
        assert!(log
            .search_context_events("exploded", 10)
            .await
            .unwrap()
            .is_empty());
        log.close().await;
    }

    // --- global sender / system events -------------------------------------

    #[tokio::test]
    async fn send_system_event_routes_through_installed_global_sender() {
        let (sender, mut rx) = EventSender::bounded(16);
        install_global_sender(sender);
        let marker = format!("a6-global-sender-test-{}", uuid::Uuid::new_v4());
        send_system_event("toggle_change", &marker);
        // Unknown kinds never enter the channel.
        send_system_event("not_a_kind", &marker);

        let mut found = None;
        while let Ok(event) = rx.try_recv() {
            if event.summary == marker {
                found = Some(event);
            }
        }
        let event = found.expect("system event must reach the channel");
        assert_eq!(event.source, EventSource::System);
        assert_eq!(event.event_type, EventType::ToggleChange);
        assert!(event.event_type.valid_for(event.source));
    }

    // --- writer task integration -------------------------------------------

    #[tokio::test]
    async fn spawned_writer_flushes_batches_and_reports_health() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (sender, handle) = spawn_event_writer_with_interval(
            log.clone(),
            &EventsConfig::default(),
            None,
            shutdown_rx,
            Duration::from_millis(50),
        );
        assert!(handle.is_healthy());

        let t0 = Utc::now();
        sender.send(template_event("toggle mic changed 1 times", t0));
        sender.send(template_event("toggle mic changed 2 times", t0));
        sender.send(template_event("screen paused", t0));

        // Wait for at least one flush tick.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let rows = log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 2, "duplicates collapse, distinct stay");
        assert_eq!(handle.queue_depth(), 0);
        assert!(handle.last_flush_at().is_some());
        assert_eq!(handle.rows_written(), 2);
        assert_eq!(handle.rows_collapsed(), 1);

        // Clean shutdown: not healthy anymore, but no restart request.
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!handle.is_healthy());
        assert!(!handle.should_restart(), "clean shutdown never restarts");
        log.close().await;
    }

    /// Shutdown ordering (I5): the runtime must await the writer's final
    /// flush *before* closing the raw-log pool. With a long flush interval
    /// nothing has been persisted when the shutdown signal fires, so this
    /// only passes if `wait_for_stop` genuinely waits for the flush.
    #[tokio::test]
    async fn pending_batch_survives_shutdown_when_awaited_before_pool_close() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (sender, handle) = spawn_event_writer_with_interval(
            log.clone(),
            &EventsConfig::default(),
            None,
            shutdown_rx,
            // Long enough that no periodic flush can happen during the test.
            Duration::from_secs(30),
        );

        // Let the interval's immediate first tick pass on an empty batch,
        // so the next flush is 30 s away and only shutdown can trigger it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let t0 = Utc::now();
        sender.send(template_event("toggle mic changed 1 times", t0));
        sender.send(template_event("screen paused", t0));
        // Let the writer drain the channel into its pending batch without
        // flushing it (threshold not reached, tick far away).
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            log.context_event_count().await.unwrap(),
            0,
            "nothing flushed yet — the batch is still in memory"
        );

        shutdown_tx.send(true).unwrap();
        assert!(
            handle.wait_for_stop(Duration::from_secs(5)).await,
            "the writer must stop within the timeout"
        );
        // This is the ordering the runtime uses: close the pool only after
        // the writer has stopped.
        assert_eq!(
            log.context_event_count().await.unwrap(),
            2,
            "the final flush must land before the pool closes"
        );
        assert!(!handle.is_healthy());
        assert!(!handle.should_restart(), "clean shutdown never restarts");
        // A second call is harmless and reports the stopped state.
        assert!(handle.wait_for_stop(Duration::from_millis(50)).await);
        log.close().await;
    }

    #[tokio::test]
    async fn writer_death_by_channel_close_requests_restart() {
        let dir = tempfile::tempdir().unwrap();
        let log = open_log(&dir).await;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (sender, handle) = spawn_event_writer_with_interval(
            log.clone(),
            &EventsConfig::default(),
            None,
            shutdown_rx,
            Duration::from_millis(50),
        );
        sender.send(template_event("toggle mic changed", Utc::now()));
        // Dropping every sender closes the channel while the runtime is
        // still "alive" — the unexpected-death path.
        drop(sender);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!handle.is_healthy());
        assert!(handle.should_restart());
        // The pending event was still flushed on the way out.
        assert_eq!(log.context_event_count().await.unwrap(), 1);
        log.close().await;
    }
}
