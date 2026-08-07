//! # File watcher (context engine spec §4.5) — opt-in, default OFF
//!
//! `notify`-based watcher over the root paths of **every confirmed or
//! configured project** whose zone allows observation — never unconfirmed
//! discovery candidates, never `never_observe` roots. Unlike the git
//! collector (active project only), file activity in background projects
//! is cheap to observe via `notify` and wanted by the context engine, so
//! all participating roots are watched at once.
//!
//! ## Pipeline (per raw notify event)
//!
//! ignore-glob filter → per-path debounce (`debounce_ms`, last kind wins
//! except created+modified stays created) → rename From/To pairing within
//! the debounce window → per-project storm gate (> `storm_threshold`
//! pending events in one flush collapse into ONE `files_bulk_change`) →
//! [`PrivacyFilter::scrub_path`] on every displayed path →
//! [`ContextEvent`] onto the events channel via [`EventSender`]
//! (non-blocking, Task A6).
//!
//! `.git` is ignored entirely — git facts are the git collector's job
//! (spec §4.4); duplicating `.git/HEAD` / `.git/index` signals here would
//! double-count every commit.
//!
//! ## Privacy (spec §4.1)
//!
//! Paths are structured fields: they pass through
//! [`PrivacyFilter::scrub_path`] (home → `~`, username components
//! redacted), never the free-text secret scrubbers. Event summaries show
//! the path **relative to the project root** when possible, so the root
//! prefix never leaves the collector. The project's zone gates the root:
//! `never_observe` → not watched at all; `local_only` → watched, every
//! event tagged [`EventSensitivity::LocalOnly`]. The
//! `[privacy.toggles].files` honest toggle means no `notify` watch is
//! ever armed when off, and `[file_watcher].enabled` is **`false` by
//! default** — file watching is opt-in.
//!
//! ## Failure modes (spec §4.5)
//!
//! Per-root state machine: a root that errors or vanishes is marked
//! *unavailable* (ONE `source_unavailable` system event + health note),
//! retried on the `rearm_secs` backoff; other roots are unaffected. A
//! reappearing root is rewatched and announces itself with one
//! `files_bulk_change` resync event. notify overflow/rescan notices emit
//! a structured health log + one resync event per root instead of
//! replaying the world. [`FileWatcher::should_restart`] is `true` ONLY
//! when the notify event channel itself died (all watchers gone
//! unexpectedly) — root-level failures never restart-thrash.
//!
//! The confirmed-project set is re-read from the injected
//! [`ProjectsProvider`] every `rearm_secs`, so projects confirmed at
//! runtime (Plan C intents) get watched without a restart.
//!
//! ## Layer
//!
//! Layer 1 — Senses. Structured filesystem observation, no AI involvement.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher as _};
use parking_lot::RwLock;
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::config::{ContextConfig, FileWatcherConfig, ObservationToggles, PrivacyConfig};
use crate::context::project::{ProjectEntry, ProjectStatus};
use crate::memory::events::{
    ContextEvent, EventSender, EventSensitivity, EventSource, EventType, COLLECTOR_EVENT_IMPORTANCE,
};
use crate::senses::privacy::{
    emit_system_event, source_enabled, ObservedSource, PrivacyFilter, Zone,
};
use crate::senses::toggles::ToggleControl;

/// Capacity of the raw notify → task bridge channel. Overflow sets a
/// resync flag instead of blocking notify's callback thread.
const RAW_CHANNEL_CAP: usize = 4096;

/// Shared slot holding the most recent non-ignored file-event path (raw,
/// in-process only — the Task A4 seam: the frame loop feeds it into
/// `FrameInput.recent_file_path` to activate tier-3 git-root resolution).
pub type RecentFileHandle = Arc<RwLock<Option<PathBuf>>>;

/// Async provider of the current project set (the Projects table read
/// injected from the runtime binary, mirroring how the git watcher gets
/// its `CurrentProjectHandle`). `None` means "the read failed — keep the
/// current watch set" so a transient DB error never tears down watches.
pub type ProjectsProvider =
    Arc<dyn Fn() -> BoxFuture<'static, Option<Vec<ProjectEntry>>> + Send + Sync>;

// ---------------------------------------------------------------------------
// Watched roots (pure)
// ---------------------------------------------------------------------------

/// One project root the watcher arms, derived from the project set
/// (spec §4.5 gating): confirmed/configured only, `never_observe` skipped,
/// `local_only` tagging every event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedRoot {
    /// Project id stamped onto every emitted event.
    pub project_id: String,
    /// Root directory (raw, unscrubbed — paths never leave the collector
    /// unscrubbed; summaries are root-relative or scrubbed).
    pub root: PathBuf,
    /// Zone-derived sensitivity for this root's events.
    pub sensitivity: EventSensitivity,
}

/// Derives the watchable roots from a project set: one [`WatchedRoot`]
/// per root path of every confirmed/configured project whose zone is not
/// `never_observe`. Discovery candidates are never collected from.
pub fn watched_roots(projects: &[ProjectEntry]) -> Vec<WatchedRoot> {
    let mut roots = Vec::new();
    for project in projects {
        if !matches!(
            project.status,
            ProjectStatus::Configured | ProjectStatus::Confirmed
        ) {
            continue;
        }
        if project.zone == Some(Zone::NeverObserve) {
            continue;
        }
        let sensitivity = if project.zone == Some(Zone::LocalOnly) {
            EventSensitivity::LocalOnly
        } else {
            EventSensitivity::CloudAllowed
        };
        for root in &project.root_paths {
            if root.trim().is_empty() {
                continue;
            }
            roots.push(WatchedRoot {
                project_id: project.id.clone(),
                root: PathBuf::from(root),
                sensitivity,
            });
        }
    }
    roots
}

/// Lowercased forward-slash form for prefix comparison (Windows paths are
/// case-insensitive and arrive with either separator).
fn normalize_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

/// Index of the deepest watched root containing `path` (longest-prefix
/// match), or `None` when the path is outside every root.
pub fn best_root_index(roots: &[WatchedRoot], path: &Path) -> Option<usize> {
    let p = normalize_path(path);
    let mut best: Option<(usize, usize)> = None;
    for (i, root) in roots.iter().enumerate() {
        let rn = normalize_path(&root.root);
        if rn.is_empty() {
            continue;
        }
        if p == rn || p.starts_with(&format!("{rn}/")) {
            let better = best.map(|(_, len)| rn.len() > len).unwrap_or(true);
            if better {
                best = Some((i, rn.len()));
            }
        }
    }
    best.map(|(i, _)| i)
}

// ---------------------------------------------------------------------------
// Ignore globs (pure)
// ---------------------------------------------------------------------------

/// Matcher over `[file_watcher].ignore_globs`: a path is ignored when ANY
/// of its components matches ANY pattern. Patterns without wildcards are
/// case-insensitive literal component names (`node_modules`, `.git`);
/// patterns with `*`/`?` match component names glob-style (`*.tmp`).
#[derive(Debug, Clone)]
pub struct IgnoreMatcher {
    patterns: Vec<String>,
}

/// Case-sensitive glob match supporting `*` (any run) and `?` (any one
/// char); callers pass pre-lowercased inputs.
fn glob_match(pattern: &[char], name: &[char]) -> bool {
    match pattern.first() {
        None => name.is_empty(),
        Some('*') => {
            glob_match(&pattern[1..], name) || (!name.is_empty() && glob_match(pattern, &name[1..]))
        }
        Some('?') => !name.is_empty() && glob_match(&pattern[1..], &name[1..]),
        Some(c) => name.first() == Some(c) && glob_match(&pattern[1..], &name[1..]),
    }
}

impl IgnoreMatcher {
    /// Builds a matcher from the config glob list (empty/whitespace
    /// entries dropped, everything lowercased).
    pub fn new(globs: &[String]) -> Self {
        Self {
            patterns: globs
                .iter()
                .map(|g| g.trim().to_lowercase())
                .filter(|g| !g.is_empty())
                .collect(),
        }
    }

    /// Whether any component of `path` (ideally root-relative) matches an
    /// ignore pattern.
    pub fn is_ignored(&self, path: &Path) -> bool {
        path.components().any(|component| {
            let std::path::Component::Normal(name) = component else {
                return false;
            };
            let name: Vec<char> = name.to_string_lossy().to_lowercase().chars().collect();
            self.patterns
                .iter()
                .any(|p| glob_match(&p.chars().collect::<Vec<_>>(), &name))
        })
    }
}

// ---------------------------------------------------------------------------
// Debounce + rename pairing (pure)
// ---------------------------------------------------------------------------

/// A raw per-path change kind after notify-kind mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// The path appeared.
    Created,
    /// The path's content/metadata changed.
    Modified,
    /// The path vanished.
    Deleted,
}

/// One debounced, pairing-resolved file change ready for event emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    /// A file was created.
    Created(PathBuf),
    /// A file was modified.
    Modified(PathBuf),
    /// A file was deleted.
    Deleted(PathBuf),
    /// A rename pair (`From`/`To` halves matched within the window).
    Renamed {
        /// Old path.
        from: PathBuf,
        /// New path.
        to: PathBuf,
    },
}

#[derive(Debug, Clone)]
enum PendingKind {
    Plain(ChangeKind),
    Renamed { from: PathBuf },
}

/// Per-root debounce buffer (spec §4.5): coalesces rapid same-path events
/// (last kind wins, except created+modified stays created and a rename
/// followed by a modify stays a rename), pairs rename `From`/`To` halves
/// within the window, and ages unpaired `From` halves out to deletes /
/// unpaired `To` halves to creates.
#[derive(Debug)]
pub struct DebounceBuffer {
    window: Duration,
    pending: HashMap<PathBuf, (PendingKind, Instant)>,
    rename_from: VecDeque<(PathBuf, Instant)>,
}

impl DebounceBuffer {
    /// Creates an empty buffer with the given debounce window.
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: HashMap::new(),
            rename_from: VecDeque::new(),
        }
    }

    /// Records a plain change, coalescing with any pending change on the
    /// same path.
    pub fn record(&mut self, path: PathBuf, kind: ChangeKind, now: Instant) {
        match self.pending.entry(path) {
            Entry::Occupied(mut occupied) => {
                let (pending, last) = occupied.get_mut();
                *last = now;
                // Coalesce rules: created+modified stays created (the net
                // effect is "a new file exists"); renamed+modified stays
                // renamed (the rename is the interesting signal). All
                // other combinations: last kind wins.
                let keep = matches!(
                    (&*pending, kind),
                    (
                        PendingKind::Plain(ChangeKind::Created),
                        ChangeKind::Modified
                    ) | (PendingKind::Renamed { .. }, ChangeKind::Modified)
                );
                if !keep {
                    *pending = PendingKind::Plain(kind);
                }
            }
            Entry::Vacant(vacant) => {
                vacant.insert((PendingKind::Plain(kind), now));
            }
        }
    }

    /// Records the `From` half of a rename. Any pending change on the old
    /// path is dropped — the rename supersedes it.
    pub fn record_rename_from(&mut self, path: PathBuf, now: Instant) {
        self.pending.remove(&path);
        self.rename_from.push_back((path, now));
    }

    /// Records the `To` half of a rename: pairs with the oldest unexpired
    /// `From` half (FIFO), or degrades to a create when none is pending.
    pub fn record_rename_to(&mut self, to: PathBuf, now: Instant) {
        let index = self
            .rename_from
            .iter()
            .position(|(_, t)| now.duration_since(*t) < self.window);
        match index.and_then(|i| self.rename_from.remove(i)) {
            Some((from, _)) => {
                self.pending
                    .insert(to, (PendingKind::Renamed { from }, now));
            }
            None => self.record(to, ChangeKind::Created, now),
        }
    }

    /// Records an already-paired rename (notify `RenameMode::Both`).
    pub fn record_rename_pair(&mut self, from: PathBuf, to: PathBuf, now: Instant) {
        self.pending.remove(&from);
        self.pending
            .insert(to, (PendingKind::Renamed { from }, now));
    }

    /// Drains every entry whose debounce window has elapsed. Unpaired
    /// `From` halves older than the window age out as deletes (the file
    /// left the watched tree).
    pub fn flush(&mut self, now: Instant) -> Vec<FileChange> {
        let mut out = Vec::new();
        while let Some((_, t)) = self.rename_from.front() {
            if now.duration_since(*t) >= self.window {
                if let Some((path, _)) = self.rename_from.pop_front() {
                    out.push(FileChange::Deleted(path));
                }
            } else {
                break;
            }
        }
        let ready: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(_, (_, t))| now.duration_since(*t) >= self.window)
            .map(|(path, _)| path.clone())
            .collect();
        for path in ready {
            if let Some((kind, _)) = self.pending.remove(&path) {
                out.push(match kind {
                    PendingKind::Plain(ChangeKind::Created) => FileChange::Created(path),
                    PendingKind::Plain(ChangeKind::Modified) => FileChange::Modified(path),
                    PendingKind::Plain(ChangeKind::Deleted) => FileChange::Deleted(path),
                    PendingKind::Renamed { from } => FileChange::Renamed { from, to: path },
                });
            }
        }
        out
    }

    /// Pending entries (both plain and unpaired rename halves) — tests.
    pub fn pending_len(&self) -> usize {
        self.pending.len() + self.rename_from.len()
    }

    /// Drops everything pending (resync supersedes individual replay).
    pub fn clear(&mut self) {
        self.pending.clear();
        self.rename_from.clear();
    }
}

// ---------------------------------------------------------------------------
// Storm gate (pure)
// ---------------------------------------------------------------------------

/// Outcome of one per-project flush after the storm gate (spec §4.5).
#[derive(Debug, PartialEq, Eq)]
pub enum FlushOutcome {
    /// At or below the threshold: emit each change individually.
    Individual(Vec<FileChange>),
    /// Storm: emit ONE `files_bulk_change` carrying the count.
    Bulk(usize),
}

/// Applies the storm rule: strictly more than `threshold` changes in one
/// flush for one project collapse into a single bulk event (branch
/// switches touch thousands of tracked files; debounce alone is
/// per-path).
pub fn storm_gate(changes: Vec<FileChange>, threshold: usize) -> FlushOutcome {
    if !changes.is_empty() && changes.len() > threshold {
        FlushOutcome::Bulk(changes.len())
    } else {
        FlushOutcome::Individual(changes)
    }
}

// ---------------------------------------------------------------------------
// Event construction (pure)
// ---------------------------------------------------------------------------

/// Display form of a changed path: relative to the root when possible
/// (the root prefix never leaves the collector), otherwise the full path;
/// either way passed through [`PrivacyFilter::scrub_path`].
pub fn display_path(privacy: &PrivacyFilter, root: &Path, path: &Path) -> String {
    let shown = path
        .strip_prefix(root)
        .ok()
        .filter(|rel| !rel.as_os_str().is_empty())
        .unwrap_or(path);
    privacy.scrub_path(&shown.to_string_lossy())
}

fn file_event(
    root: &WatchedRoot,
    event_type: EventType,
    summary: String,
    ts: DateTime<Utc>,
) -> ContextEvent {
    ContextEvent {
        ts,
        source: EventSource::File,
        application: String::new(),
        window_title: String::new(),
        project_id: Some(root.project_id.clone()),
        event_type,
        summary,
        importance: COLLECTOR_EVENT_IMPORTANCE,
        confidence: 1.0,
        sensitivity: root.sensitivity,
        raw_reference: None,
    }
}

/// Builds the [`ContextEvent`] for one debounced change. Summaries are
/// the scrubbed (root-relative) path; renames render `"old → new"`.
pub fn change_event(
    privacy: &PrivacyFilter,
    root: &WatchedRoot,
    change: &FileChange,
    ts: DateTime<Utc>,
) -> ContextEvent {
    let display = |path: &Path| display_path(privacy, &root.root, path);
    let (event_type, summary) = match change {
        FileChange::Created(path) => (EventType::FileCreated, display(path)),
        FileChange::Modified(path) => (EventType::FileModified, display(path)),
        FileChange::Deleted(path) => (EventType::FileDeleted, display(path)),
        FileChange::Renamed { from, to } => (
            EventType::FileRenamed,
            format!("{} → {}", display(from), display(to)),
        ),
    };
    file_event(root, event_type, summary, ts)
}

/// The storm-collapsed `files_bulk_change` event (stable template:
/// `"N files changed in <project>"`).
pub fn bulk_change_event(root: &WatchedRoot, count: usize, ts: DateTime<Utc>) -> ContextEvent {
    file_event(
        root,
        EventType::FilesBulkChange,
        format!("{count} files changed in {}", root.project_id),
        ts,
    )
}

/// The resync `files_bulk_change` event, emitted instead of replaying the
/// world after a notify overflow/rescan or a reappeared root (stable
/// template: `"file resync in <project>"`).
pub fn resync_event(root: &WatchedRoot, ts: DateTime<Utc>) -> ContextEvent {
    file_event(
        root,
        EventType::FilesBulkChange,
        format!("file resync in {}", root.project_id),
        ts,
    )
}

// ---------------------------------------------------------------------------
// Per-root state machine (pure)
// ---------------------------------------------------------------------------

/// Availability of one watched root (spec §4.5 failure modes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootState {
    /// The notify watch is armed and delivering.
    Active,
    /// The root errored or vanished; retried on the rearm backoff.
    Unavailable {
        /// Short, path-free reason (detailed errors go to the log).
        reason: String,
    },
}

/// Transition tracker for one root: transitions report whether they are
/// *new* so the caller emits exactly ONE `source_unavailable` event (and
/// one resync on recovery) — never one per retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootTracker {
    state: RootState,
}

impl RootTracker {
    /// A root that armed successfully.
    pub fn active() -> Self {
        Self {
            state: RootState::Active,
        }
    }

    /// A root that failed to arm.
    pub fn unavailable(reason: &str) -> Self {
        Self {
            state: RootState::Unavailable {
                reason: reason.to_string(),
            },
        }
    }

    /// Marks the root unavailable; `true` only on the active→unavailable
    /// transition (repeat failures update the reason silently).
    pub fn mark_unavailable(&mut self, reason: &str) -> bool {
        let was_active = matches!(self.state, RootState::Active);
        self.state = RootState::Unavailable {
            reason: reason.to_string(),
        };
        was_active
    }

    /// Marks the root active; `true` only on the unavailable→active
    /// recovery transition.
    pub fn mark_active(&mut self) -> bool {
        let was_unavailable = !matches!(self.state, RootState::Active);
        self.state = RootState::Active;
        was_unavailable
    }

    /// Whether the root is currently watched.
    pub fn is_active(&self) -> bool {
        matches!(self.state, RootState::Active)
    }

    /// The unavailability reason, when unavailable.
    pub fn reason(&self) -> Option<&str> {
        match &self.state {
            RootState::Active => None,
            RootState::Unavailable { reason } => Some(reason),
        }
    }
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// Health snapshot of the file watcher, readable by the repair agent and
/// the Context page (spec §7: disabled-with-reason and per-root
/// unavailability are healthy, deliberate states — never restart-thrash).
#[derive(Debug, Clone, Default, Serialize)]
pub struct FileWatchHealth {
    /// Whether the watcher is armed. `false` + a reason is the
    /// *disabled-with-reason* state (config off — the default —, toggle
    /// off, notify init failure).
    pub enabled: bool,
    /// Why the watcher is disabled, when it is.
    pub disabled_reason: Option<String>,
    /// Roots currently armed and delivering.
    pub roots_active: usize,
    /// `"<project>: <reason>"` for every unavailable root.
    pub roots_unavailable: Vec<String>,
    /// Total file events emitted onto the channel.
    pub events_emitted: u64,
    /// When the last debounce flush ran.
    pub last_flush_at: Option<DateTime<Utc>>,
    /// The notify event channel died unexpectedly — the ONLY state in
    /// which [`FileWatcher::should_restart`] is `true`.
    pub channel_dead: bool,
}

/// Shared handle onto the watcher's health snapshot.
pub type SharedFileWatchHealth = Arc<RwLock<FileWatchHealth>>;

// ---------------------------------------------------------------------------
// FileWatcher
// ---------------------------------------------------------------------------

/// One armed (or pending) root: gating info, availability, and its
/// debounce buffer.
struct RootWatch {
    info: WatchedRoot,
    tracker: RootTracker,
    buffer: DebounceBuffer,
    resync_pending: bool,
}

fn root_key(info: &WatchedRoot) -> (String, String) {
    (info.project_id.clone(), normalize_path(&info.root))
}

/// Internal routing action for one raw notify path.
enum RawAction {
    Plain(ChangeKind),
    RenameFrom,
    RenameTo,
}

/// The runtime-gated file watcher task (spec §4.5). Opt-in, default OFF.
///
/// # Layer
///
/// Layer 1 — Senses. Structured filesystem observation, no AI involvement.
///
/// # Self-healing
///
/// [`FileWatcher::health`] exposes the [`FileWatchHealth`] snapshot;
/// [`FileWatcher::is_healthy`] is `true` even when disabled-with-reason or
/// when individual roots are unavailable (spec §7), and
/// [`FileWatcher::should_restart`] is `true` ONLY when the notify event
/// channel itself died.
pub struct FileWatcher {
    config: FileWatcherConfig,
    /// The privacy choke point (spec §4.1) — scrubs displayed paths.
    privacy: Arc<PrivacyFilter>,
    /// Honest per-source toggles; `[privacy.toggles].files` gates every
    /// notify watch.
    toggles: ObservationToggles,
    /// Live toggle control (Task C5): re-read on every flush tick so a
    /// Context-page switch drops the notify watches without a restart.
    toggle_control: Option<ToggleControl>,
    /// Async project-set provider (Projects table read injected from the
    /// runtime binary). `None` (tests, tools) parks disabled.
    projects: Option<ProjectsProvider>,
    /// Events-channel producer handle (Task A6): file events go here,
    /// non-blocking, pre-stamped with their root's project id.
    events: EventSender,
    /// Most recent non-ignored file-event path (Task A4 tier-3 seam).
    recent_file: RecentFileHandle,
    health: SharedFileWatchHealth,
}

impl FileWatcher {
    /// Creates a file watcher with the given `[file_watcher]` config.
    ///
    /// Standalone construction (tests, tools) synthesizes a default
    /// privacy filter; the runtime shares its boot-time filter via
    /// [`FileWatcher::with_privacy`].
    pub fn new(config: FileWatcherConfig) -> Self {
        debug!(
            layer = "senses",
            component = "file_watch",
            enabled = config.enabled,
            debounce_ms = config.debounce_ms,
            storm_threshold = config.storm_threshold,
            "FileWatcher created"
        );
        Self {
            config,
            privacy: Arc::new(PrivacyFilter::from_config(
                &ContextConfig::default(),
                &PrivacyConfig::default(),
            )),
            toggles: ObservationToggles::default(),
            toggle_control: None,
            projects: None,
            events: EventSender::log_only(),
            recent_file: RecentFileHandle::default(),
            health: SharedFileWatchHealth::default(),
        }
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
    /// Without it the watcher honours the boot-time [`ObservationToggles`]
    /// copy it was given; with it, every loop iteration re-reads the
    /// current value, so a Context-page switch takes effect without a
    /// restart.
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

    /// Attaches the project-set provider (the Projects table read from
    /// the runtime binary) — the only source of "which roots to watch".
    pub fn with_projects_provider(mut self, provider: ProjectsProvider) -> Self {
        self.projects = Some(provider);
        self
    }

    /// Attaches the events-channel producer handle (Task A6, spec §3).
    pub fn with_event_sender(mut self, sender: EventSender) -> Self {
        self.events = sender;
        self
    }

    /// Shares an externally owned recent-file slot (the runtime binary
    /// reads it into `FrameInput.recent_file_path` each frame).
    pub fn with_recent_file(mut self, handle: RecentFileHandle) -> Self {
        self.recent_file = handle;
        self
    }

    /// The recent-file slot this watcher updates (Task A4 tier-3 seam).
    pub fn recent_file_handle(&self) -> RecentFileHandle {
        self.recent_file.clone()
    }

    /// Current health snapshot (cheap clone).
    pub fn health(&self) -> FileWatchHealth {
        self.health.read().clone()
    }

    /// Shared handle onto the health snapshot, for callers that outlive
    /// the moved watcher (health loop, tests).
    pub fn health_handle(&self) -> SharedFileWatchHealth {
        self.health.clone()
    }

    /// Always `true`: disabled-with-reason and per-root unavailability are
    /// healthy, deliberate states (spec §7). Details in
    /// [`FileWatcher::health`].
    pub fn is_healthy(&self) -> bool {
        true
    }

    /// `true` ONLY when the notify event channel itself died (all
    /// watchers gone unexpectedly) — per-root failures rearm on backoff
    /// and never restart the watcher (spec §4.5).
    pub fn should_restart(&self) -> bool {
        self.health.read().channel_dead
    }

    fn disable(&self, reason: &str) {
        let mut health = self.health.write();
        health.enabled = false;
        health.disabled_reason = Some(reason.to_string());
    }

    fn send_event(&self, event: ContextEvent) {
        self.events.send(event);
        self.health.write().events_emitted += 1;
    }

    fn publish_root_health(&self, roots: &[RootWatch]) {
        let mut health = self.health.write();
        health.roots_active = roots.iter().filter(|r| r.tracker.is_active()).count();
        health.roots_unavailable = roots
            .iter()
            .filter_map(|r| {
                r.tracker
                    .reason()
                    .map(|reason| format!("{}: {reason}", r.info.project_id))
            })
            .collect();
    }

    /// Runs the watcher until the shutdown signal fires. Disabled states
    /// (config off — the default —, toggle off, notify init failure) park
    /// here — still responding to shutdown — so the health snapshot stays
    /// observable.
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        /// Parks a disabled watcher until shutdown, keeping the health
        /// snapshot observable.
        async fn wait_for_shutdown(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
            while !*shutdown.borrow() {
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
        }

        if !self.config.enabled {
            info!(
                layer = "senses",
                component = "file_watch",
                "file watcher disabled by [file_watcher].enabled (opt-in, default OFF)"
            );
            self.disable("disabled by [file_watcher].enabled");
            wait_for_shutdown(&mut shutdown).await;
            return;
        }
        // Honest toggle (spec §4.1, the A2 seam): no notify watch is ever
        // armed when the files source is off.
        if !source_enabled(&self.live_toggles(), ObservedSource::Files) {
            emit_system_event(
                "toggle_change",
                "files observation disabled by [privacy.toggles]; file watcher will not run",
            );
            self.disable("disabled by [privacy.toggles]");
            wait_for_shutdown(&mut shutdown).await;
            return;
        }
        let Some(provider) = self.projects.clone() else {
            self.disable("no projects provider attached");
            wait_for_shutdown(&mut shutdown).await;
            return;
        };

        // Raw notify → task bridge. The callback runs on notify's thread:
        // never block it — overflow flips the resync flag instead.
        let (raw_tx, mut raw_rx) =
            tokio::sync::mpsc::channel::<notify::Result<notify::Event>>(RAW_CHANNEL_CAP);
        let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let overflow_flag = overflowed.clone();
        let mut watcher = match notify::recommended_watcher(move |result| {
            if raw_tx.try_send(result).is_err() {
                overflow_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                warn!(
                    layer = "senses",
                    component = "file_watch",
                    error = %error,
                    "notify backend failed to initialize; file watcher disabled"
                );
                emit_system_event(
                    "source_unavailable",
                    "file watcher backend failed to initialize; file watching disabled",
                );
                self.disable("notify backend failed to initialize");
                wait_for_shutdown(&mut shutdown).await;
                return;
            }
        };
        {
            let mut health = self.health.write();
            health.enabled = true;
            health.disabled_reason = None;
        }

        let window = Duration::from_millis(self.config.debounce_ms.max(1));
        let ignore = IgnoreMatcher::new(&self.config.ignore_globs);
        let mut roots: Vec<RootWatch> = Vec::new();

        if let Some(projects) = provider().await {
            self.sync_roots(&mut watcher, &mut roots, watched_roots(&projects), window);
        }
        info!(
            layer = "senses",
            component = "file_watch",
            roots = roots.len(),
            debounce_ms = self.config.debounce_ms,
            "file watcher armed"
        );

        let mut flush_tick = tokio::time::interval(Duration::from_millis(
            (self.config.debounce_ms / 2).clamp(50, 1000),
        ));
        flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut rearm_tick =
            tokio::time::interval(Duration::from_secs(self.config.rearm_secs.max(1)));
        rearm_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first interval tick fires immediately; consume it so the
        // rearm path does not double-run the sync done above.
        rearm_tick.tick().await;

        loop {
            tokio::select! {
                received = raw_rx.recv() => match received {
                    Some(Ok(event)) => self.handle_notify_event(&ignore, &mut roots, event),
                    Some(Err(error)) => self.handle_notify_error(&mut watcher, &mut roots, &error),
                    None => {
                        // All senders gone while we still hold the watcher:
                        // the notify channel itself is dead. The ONLY
                        // should_restart() state (spec §4.5).
                        warn!(
                            layer = "senses",
                            component = "file_watch",
                            "notify event channel died unexpectedly; file watcher requires restart"
                        );
                        emit_system_event(
                            "source_unavailable",
                            "file watcher event channel died; file watching stopped until restart",
                        );
                        {
                            let mut health = self.health.write();
                            health.channel_dead = true;
                            health.enabled = false;
                            health.disabled_reason = Some("notify event channel died".to_string());
                        }
                        wait_for_shutdown(&mut shutdown).await;
                        return;
                    }
                },
                _ = flush_tick.tick() => {
                    // Honest toggle, live (spec §4.1, Task C5): switching
                    // the files source off drops every notify watch, so no
                    // filesystem event is even observed; switching it back
                    // on re-arms from the provider on the next tick.
                    let files_enabled = source_enabled(&self.live_toggles(), ObservedSource::Files);
                    if !files_enabled {
                        if !roots.is_empty() {
                            self.sync_roots(&mut watcher, &mut roots, Vec::new(), window);
                            self.disable("disabled by [privacy.toggles]");
                        }
                        continue;
                    }
                    if roots.is_empty() && self.health.read().disabled_reason.is_some() {
                        {
                            let mut health = self.health.write();
                            health.enabled = true;
                            health.disabled_reason = None;
                        }
                        if let Some(projects) = provider().await {
                            self.sync_roots(
                                &mut watcher,
                                &mut roots,
                                watched_roots(&projects),
                                window,
                            );
                        }
                    }
                    if overflowed.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        warn!(
                            layer = "senses",
                            component = "file_watch",
                            "raw event bridge overflowed; scheduling resync for every root"
                        );
                        for root in roots.iter_mut() {
                            root.resync_pending = true;
                        }
                    }
                    self.flush_roots(&mut roots);
                }
                _ = rearm_tick.tick() => {
                    if let Some(projects) = provider().await {
                        self.sync_roots(&mut watcher, &mut roots, watched_roots(&projects), window);
                    }
                    self.rearm(&mut watcher, &mut roots);
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        debug!(
                            layer = "senses",
                            component = "file_watch",
                            "Shutdown signal received, stopping file watcher"
                        );
                        return;
                    }
                }
            }
        }
    }

    /// Reconciles the armed watch set against the desired root list:
    /// removed roots are unwatched, new roots armed (or marked
    /// unavailable), surviving roots keep their state and buffers but
    /// pick up zone changes.
    fn sync_roots(
        &self,
        watcher: &mut notify::RecommendedWatcher,
        roots: &mut Vec<RootWatch>,
        desired: Vec<WatchedRoot>,
        window: Duration,
    ) {
        let desired_keys: HashSet<_> = desired.iter().map(root_key).collect();
        let kept: Vec<RootWatch> = roots
            .drain(..)
            .filter_map(|root| {
                if desired_keys.contains(&root_key(&root.info)) {
                    Some(root)
                } else {
                    if root.tracker.is_active() {
                        let _ = watcher.unwatch(&root.info.root);
                    }
                    debug!(
                        layer = "senses",
                        component = "file_watch",
                        project = %root.info.project_id,
                        "root left the confirmed project set; unwatched"
                    );
                    None
                }
            })
            .collect();
        *roots = kept;

        for info in desired {
            if let Some(existing) = roots
                .iter_mut()
                .find(|r| root_key(&r.info) == root_key(&info))
            {
                // Zone changes (Plan C edits) apply without rearming.
                existing.info.sensitivity = info.sensitivity;
                continue;
            }
            let tracker = match arm_root(watcher, &info.root) {
                Ok(()) => {
                    debug!(
                        layer = "senses",
                        component = "file_watch",
                        project = %info.project_id,
                        "root armed"
                    );
                    RootTracker::active()
                }
                Err(reason) => {
                    warn!(
                        layer = "senses",
                        component = "file_watch",
                        project = %info.project_id,
                        reason = %reason,
                        "root unavailable at arm"
                    );
                    emit_system_event(
                        "source_unavailable",
                        &format!(
                            "file watcher root unavailable ({}): {reason}",
                            info.project_id
                        ),
                    );
                    RootTracker::unavailable(&reason)
                }
            };
            roots.push(RootWatch {
                info,
                tracker,
                buffer: DebounceBuffer::new(window),
                resync_pending: false,
            });
        }
        self.publish_root_health(roots);
    }

    /// Rearm pass (every `rearm_secs`): retries unavailable roots, and
    /// demotes active roots whose directory vanished. Recovered roots
    /// announce one resync event instead of replaying missed changes.
    fn rearm(&self, watcher: &mut notify::RecommendedWatcher, roots: &mut [RootWatch]) {
        let ts = Utc::now();
        for root in roots.iter_mut() {
            if root.tracker.is_active() {
                if !root.info.root.is_dir() {
                    let _ = watcher.unwatch(&root.info.root);
                    root.buffer.clear();
                    if root.tracker.mark_unavailable("root path missing") {
                        warn!(
                            layer = "senses",
                            component = "file_watch",
                            project = %root.info.project_id,
                            "watched root vanished; marked unavailable"
                        );
                        emit_system_event(
                            "source_unavailable",
                            &format!(
                                "file watcher root unavailable ({}): root path missing",
                                root.info.project_id
                            ),
                        );
                    }
                }
                continue;
            }
            match arm_root(watcher, &root.info.root) {
                Ok(()) => {
                    if root.tracker.mark_active() {
                        info!(
                            layer = "senses",
                            component = "file_watch",
                            project = %root.info.project_id,
                            "root reappeared; rewatched with resync"
                        );
                        self.send_event(resync_event(&root.info, ts));
                    }
                }
                Err(reason) => {
                    // Still unavailable: update the reason silently — the
                    // transition event fired once already.
                    let _ = root.tracker.mark_unavailable(&reason);
                }
            }
        }
        self.publish_root_health(roots);
    }

    /// Maps one raw notify event into per-root debounce records.
    fn handle_notify_event(
        &self,
        ignore: &IgnoreMatcher,
        roots: &mut [RootWatch],
        event: notify::Event,
    ) {
        if event.need_rescan() {
            // Overflow/rescan notice (spec §4.5): structured health log +
            // ignore-aware resync of every root instead of replaying the
            // world.
            warn!(
                layer = "senses",
                component = "file_watch",
                "notify rescan notice; scheduling resync for every root"
            );
            for root in roots.iter_mut() {
                root.resync_pending = true;
            }
            return;
        }
        let now = Instant::now();
        match event.kind {
            EventKind::Create(_) => {
                for path in &event.paths {
                    self.route_path(
                        ignore,
                        roots,
                        path,
                        RawAction::Plain(ChangeKind::Created),
                        now,
                    );
                }
            }
            EventKind::Remove(_) => {
                for path in &event.paths {
                    self.route_path(
                        ignore,
                        roots,
                        path,
                        RawAction::Plain(ChangeKind::Deleted),
                        now,
                    );
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                if let Some(path) = event.paths.first() {
                    self.route_path(ignore, roots, path, RawAction::RenameFrom, now);
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                if let Some(path) = event.paths.first() {
                    self.route_path(ignore, roots, path, RawAction::RenameTo, now);
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                if event.paths.len() >= 2 {
                    self.handle_rename_pair(ignore, roots, &event.paths[0], &event.paths[1], now);
                } else if let Some(path) = event.paths.first() {
                    self.route_path(
                        ignore,
                        roots,
                        path,
                        RawAction::Plain(ChangeKind::Modified),
                        now,
                    );
                }
            }
            EventKind::Modify(_) | EventKind::Any => {
                for path in &event.paths {
                    self.route_path(
                        ignore,
                        roots,
                        path,
                        RawAction::Plain(ChangeKind::Modified),
                        now,
                    );
                }
            }
            EventKind::Access(_) | EventKind::Other => {}
        }
    }

    /// Routes one path to its (deepest) root's buffer, applying the
    /// ignore filter and updating the recent-file slot.
    fn route_path(
        &self,
        ignore: &IgnoreMatcher,
        roots: &mut [RootWatch],
        path: &Path,
        action: RawAction,
        now: Instant,
    ) {
        let Some(index) = best_root_index_watches(roots, path) else {
            return;
        };
        let root = &mut roots[index];
        if !root.tracker.is_active() {
            return;
        }
        let relative = path.strip_prefix(&root.info.root).unwrap_or(path);
        if ignore.is_ignored(relative) {
            return;
        }
        *self.recent_file.write() = Some(path.to_path_buf());
        match action {
            RawAction::Plain(kind) => root.buffer.record(path.to_path_buf(), kind, now),
            RawAction::RenameFrom => root.buffer.record_rename_from(path.to_path_buf(), now),
            RawAction::RenameTo => root.buffer.record_rename_to(path.to_path_buf(), now),
        }
    }

    /// Routes an already-paired rename. When only one endpoint survives
    /// the ignore filter, the rename degrades to that endpoint's plain
    /// change (moved into an ignored dir → delete; out of one → create).
    fn handle_rename_pair(
        &self,
        ignore: &IgnoreMatcher,
        roots: &mut [RootWatch],
        from: &Path,
        to: &Path,
        now: Instant,
    ) {
        let ignored = |path: &Path| {
            best_root_index_watches(roots, path)
                .map(|i| {
                    let relative = path.strip_prefix(&roots[i].info.root).unwrap_or(path);
                    ignore.is_ignored(relative)
                })
                .unwrap_or(true)
        };
        let from_ok = !ignored(from);
        let to_ok = !ignored(to);
        match (from_ok, to_ok) {
            (true, true) => {
                if let Some(index) = best_root_index_watches(roots, to) {
                    let root = &mut roots[index];
                    if root.tracker.is_active() {
                        *self.recent_file.write() = Some(to.to_path_buf());
                        root.buffer
                            .record_rename_pair(from.to_path_buf(), to.to_path_buf(), now);
                    }
                }
            }
            (true, false) => self.route_path(
                ignore,
                roots,
                from,
                RawAction::Plain(ChangeKind::Deleted),
                now,
            ),
            (false, true) => self.route_path(
                ignore,
                roots,
                to,
                RawAction::Plain(ChangeKind::Created),
                now,
            ),
            (false, false) => {}
        }
    }

    /// Handles a notify error: path-scoped errors mark the affected roots
    /// unavailable (rearm backoff recovers them); global errors are
    /// logged and keep the last state.
    fn handle_notify_error(
        &self,
        watcher: &mut notify::RecommendedWatcher,
        roots: &mut [RootWatch],
        error: &notify::Error,
    ) {
        warn!(
            layer = "senses",
            component = "file_watch",
            error = %error,
            "notify reported an error"
        );
        if error.paths.is_empty() {
            return;
        }
        for path in &error.paths {
            let Some(index) = best_root_index_watches(roots, path) else {
                continue;
            };
            let root = &mut roots[index];
            let _ = watcher.unwatch(&root.info.root);
            root.buffer.clear();
            if root.tracker.mark_unavailable("watch error") {
                emit_system_event(
                    "source_unavailable",
                    &format!(
                        "file watcher root unavailable ({}): watch error",
                        root.info.project_id
                    ),
                );
            }
        }
        self.publish_root_health(roots);
    }

    /// One debounce flush across every root: resyncs supersede pending
    /// changes; ready changes pass the storm gate and go onto the events
    /// channel.
    fn flush_roots(&self, roots: &mut [RootWatch]) {
        let now = Instant::now();
        let ts = Utc::now();
        let mut flushed_any = false;
        for root in roots.iter_mut() {
            if root.resync_pending {
                root.resync_pending = false;
                root.buffer.clear();
                if root.tracker.is_active() {
                    self.send_event(resync_event(&root.info, ts));
                    flushed_any = true;
                }
                continue;
            }
            let changes = root.buffer.flush(now);
            if changes.is_empty() {
                continue;
            }
            flushed_any = true;
            match storm_gate(changes, self.config.storm_threshold) {
                FlushOutcome::Individual(changes) => {
                    for change in &changes {
                        self.send_event(change_event(&self.privacy, &root.info, change, ts));
                    }
                }
                FlushOutcome::Bulk(count) => {
                    debug!(
                        layer = "senses",
                        component = "file_watch",
                        project = %root.info.project_id,
                        count = count,
                        "file event storm collapsed into files_bulk_change"
                    );
                    self.send_event(bulk_change_event(&root.info, count, ts));
                }
            }
        }
        if flushed_any {
            self.health.write().last_flush_at = Some(ts);
        }
    }
}

/// [`best_root_index`] over the runtime watch list.
fn best_root_index_watches(roots: &[RootWatch], path: &Path) -> Option<usize> {
    let p = normalize_path(path);
    let mut best: Option<(usize, usize)> = None;
    for (i, watch) in roots.iter().enumerate() {
        let rn = normalize_path(&watch.info.root);
        if rn.is_empty() {
            continue;
        }
        if p == rn || p.starts_with(&format!("{rn}/")) {
            let better = best.map(|(_, len)| rn.len() > len).unwrap_or(true);
            if better {
                best = Some((i, rn.len()));
            }
        }
    }
    best.map(|(i, _)| i)
}

/// Arms one root, returning a short, path-free reason on failure
/// (detailed errors go to the log — event summaries must not embed raw
/// OS error strings containing user paths).
fn arm_root(watcher: &mut notify::RecommendedWatcher, root: &Path) -> Result<(), String> {
    if !root.is_dir() {
        return Err("root path missing".to_string());
    }
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|error| {
            warn!(
                layer = "senses",
                component = "file_watch",
                error = %error,
                "notify watch call failed"
            );
            "watch error".to_string()
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_file_watcher_ignore_globs;

    fn filter() -> PrivacyFilter {
        PrivacyFilter::from_config(&ContextConfig::default(), &PrivacyConfig::default())
            .with_environment(None, Some("testuser".to_string()))
    }

    fn root(project: &str, path: &str, sensitivity: EventSensitivity) -> WatchedRoot {
        WatchedRoot {
            project_id: project.to_string(),
            root: PathBuf::from(path),
            sensitivity,
        }
    }

    fn entry(id: &str, roots: &[&str], status: ProjectStatus, zone: Option<Zone>) -> ProjectEntry {
        ProjectEntry {
            id: id.to_string(),
            name: id.to_string(),
            root_paths: roots.iter().map(|s| s.to_string()).collect(),
            repo: None,
            keywords: Vec::new(),
            zone,
            status,
        }
    }

    fn window() -> Duration {
        Duration::from_millis(1000)
    }

    // --- Ignore globs ---

    #[test]
    fn ignore_matcher_defaults_skip_dep_trees_git_and_tmp() {
        let matcher = IgnoreMatcher::new(&default_file_watcher_ignore_globs());
        // Dependency/build trees, at any depth.
        assert!(matcher.is_ignored(Path::new("node_modules/react/index.js")));
        assert!(matcher.is_ignored(Path::new("web/node_modules/x/y.js")));
        assert!(matcher.is_ignored(Path::new("target/debug/foo.exe")));
        assert!(matcher.is_ignored(Path::new(".next/cache/x")));
        assert!(matcher.is_ignored(Path::new("dist/bundle.js")));
        assert!(matcher.is_ignored(Path::new("build/out.o")));
        assert!(matcher.is_ignored(Path::new("pkg/__pycache__/mod.pyc")));
        // .git is skipped ENTIRELY — including HEAD/index (the git
        // collector owns git facts; duplicating them doubles signals).
        assert!(matcher.is_ignored(Path::new(".git/HEAD")));
        assert!(matcher.is_ignored(Path::new(".git/index")));
        assert!(matcher.is_ignored(Path::new(".git/objects/ab/cdef")));
        // Wildcards match file names.
        assert!(matcher.is_ignored(Path::new("src/scratch.tmp")));
        // Case-insensitive (Windows).
        assert!(matcher.is_ignored(Path::new("NODE_MODULES/x.js")));
        assert!(matcher.is_ignored(Path::new("notes.TMP")));
        // Real source files pass.
        assert!(!matcher.is_ignored(Path::new("src/main.rs")));
        assert!(!matcher.is_ignored(Path::new("docs/targets.md")));
        assert!(!matcher.is_ignored(Path::new("gitignore.txt")));
        // Component equality, not substring: "distance" is not "dist".
        assert!(!matcher.is_ignored(Path::new("distance/x.rs")));
    }

    #[test]
    fn ignore_matcher_custom_globs() {
        let matcher = IgnoreMatcher::new(&["*.log".to_string(), "vendor".to_string()]);
        assert!(matcher.is_ignored(Path::new("out/app.log")));
        assert!(matcher.is_ignored(Path::new("vendor/lib.rs")));
        assert!(!matcher.is_ignored(Path::new("node_modules/x.js")));
        assert!(!matcher.is_ignored(Path::new("catalog.rs")));
        // `?` matches exactly one character.
        let q = IgnoreMatcher::new(&["cache?".to_string()]);
        assert!(q.is_ignored(Path::new("cache1/x")));
        assert!(!q.is_ignored(Path::new("cache/x")));
        assert!(!q.is_ignored(Path::new("cache12/x")));
    }

    // --- Debounce coalescing matrix ---

    #[test]
    fn debounce_coalesces_same_path_last_kind_wins_with_exceptions() {
        let t0 = Instant::now();
        let flush_at = t0 + window() + Duration::from_millis(1);
        let path = PathBuf::from("D:/proj/a.rs");

        // Created + Modified → Created.
        let mut buf = DebounceBuffer::new(window());
        buf.record(path.clone(), ChangeKind::Created, t0);
        buf.record(path.clone(), ChangeKind::Modified, t0);
        assert_eq!(buf.flush(flush_at), vec![FileChange::Created(path.clone())]);

        // Modified + Modified → one Modified.
        let mut buf = DebounceBuffer::new(window());
        buf.record(path.clone(), ChangeKind::Modified, t0);
        buf.record(path.clone(), ChangeKind::Modified, t0);
        assert_eq!(
            buf.flush(flush_at),
            vec![FileChange::Modified(path.clone())]
        );

        // Modified + Deleted → Deleted (last wins).
        let mut buf = DebounceBuffer::new(window());
        buf.record(path.clone(), ChangeKind::Modified, t0);
        buf.record(path.clone(), ChangeKind::Deleted, t0);
        assert_eq!(buf.flush(flush_at), vec![FileChange::Deleted(path.clone())]);

        // Deleted + Created → Created (last wins; e.g. atomic save).
        let mut buf = DebounceBuffer::new(window());
        buf.record(path.clone(), ChangeKind::Deleted, t0);
        buf.record(path.clone(), ChangeKind::Created, t0);
        assert_eq!(buf.flush(flush_at), vec![FileChange::Created(path.clone())]);

        // Renamed + Modified → Renamed (the rename is the signal).
        let mut buf = DebounceBuffer::new(window());
        let old = PathBuf::from("D:/proj/old.rs");
        buf.record_rename_pair(old.clone(), path.clone(), t0);
        buf.record(path.clone(), ChangeKind::Modified, t0);
        assert_eq!(
            buf.flush(flush_at),
            vec![FileChange::Renamed {
                from: old,
                to: path.clone()
            }]
        );
    }

    #[test]
    fn debounce_holds_events_until_window_elapses() {
        let t0 = Instant::now();
        let mut buf = DebounceBuffer::new(window());
        buf.record(PathBuf::from("a.rs"), ChangeKind::Modified, t0);
        // Inside the window: nothing flushes.
        assert!(buf.flush(t0 + Duration::from_millis(500)).is_empty());
        assert_eq!(buf.pending_len(), 1);
        // A fresh event on the same path restarts the clock.
        buf.record(
            PathBuf::from("a.rs"),
            ChangeKind::Modified,
            t0 + Duration::from_millis(800),
        );
        assert!(buf.flush(t0 + Duration::from_millis(1100)).is_empty());
        // After the (restarted) window: one event.
        let out = buf.flush(t0 + Duration::from_millis(1900));
        assert_eq!(out, vec![FileChange::Modified(PathBuf::from("a.rs"))]);
        assert_eq!(buf.pending_len(), 0);
    }

    // --- Rename pairing ---

    #[test]
    fn rename_from_and_to_pair_within_window() {
        let t0 = Instant::now();
        let mut buf = DebounceBuffer::new(window());
        buf.record_rename_from(PathBuf::from("old.rs"), t0);
        buf.record_rename_to(PathBuf::from("new.rs"), t0 + Duration::from_millis(10));
        let out = buf.flush(t0 + window() + Duration::from_millis(20));
        assert_eq!(
            out,
            vec![FileChange::Renamed {
                from: PathBuf::from("old.rs"),
                to: PathBuf::from("new.rs"),
            }]
        );
    }

    #[test]
    fn rename_from_supersedes_pending_change_on_old_path() {
        let t0 = Instant::now();
        let mut buf = DebounceBuffer::new(window());
        buf.record(PathBuf::from("old.rs"), ChangeKind::Modified, t0);
        buf.record_rename_from(PathBuf::from("old.rs"), t0);
        buf.record_rename_to(PathBuf::from("new.rs"), t0);
        let out = buf.flush(t0 + window());
        assert_eq!(
            out,
            vec![FileChange::Renamed {
                from: PathBuf::from("old.rs"),
                to: PathBuf::from("new.rs"),
            }],
            "the old path's modify must not survive the rename"
        );
    }

    #[test]
    fn unpaired_rename_halves_age_out_to_delete_and_create() {
        let t0 = Instant::now();
        // Lone From (moved out of the watched tree) → Deleted.
        let mut buf = DebounceBuffer::new(window());
        buf.record_rename_from(PathBuf::from("gone.rs"), t0);
        assert!(buf.flush(t0 + Duration::from_millis(500)).is_empty());
        assert_eq!(
            buf.flush(t0 + window()),
            vec![FileChange::Deleted(PathBuf::from("gone.rs"))]
        );
        // Lone To (moved into the watched tree) → Created.
        let mut buf = DebounceBuffer::new(window());
        buf.record_rename_to(PathBuf::from("arrived.rs"), t0);
        assert_eq!(
            buf.flush(t0 + window()),
            vec![FileChange::Created(PathBuf::from("arrived.rs"))]
        );
        // An EXPIRED From half must not pair with a late To.
        let mut buf = DebounceBuffer::new(window());
        buf.record_rename_from(PathBuf::from("stale.rs"), t0);
        buf.record_rename_to(
            PathBuf::from("late.rs"),
            t0 + window() + Duration::from_millis(1),
        );
        let mut out = buf.flush(t0 + window() * 3);
        out.sort_by_key(|c| format!("{c:?}"));
        assert_eq!(
            out,
            vec![
                FileChange::Created(PathBuf::from("late.rs")),
                FileChange::Deleted(PathBuf::from("stale.rs")),
            ]
        );
    }

    // --- Storm gate ---

    #[test]
    fn storm_gate_thresholds() {
        let changes = |n: usize| -> Vec<FileChange> {
            (0..n)
                .map(|i| FileChange::Modified(PathBuf::from(format!("f{i}.rs"))))
                .collect()
        };
        // 49 and exactly 50: individual (rule is strictly greater-than).
        assert!(matches!(
            storm_gate(changes(49), 50),
            FlushOutcome::Individual(list) if list.len() == 49
        ));
        assert!(matches!(
            storm_gate(changes(50), 50),
            FlushOutcome::Individual(list) if list.len() == 50
        ));
        // 51: ONE bulk event carrying the count.
        assert_eq!(storm_gate(changes(51), 50), FlushOutcome::Bulk(51));
        // Empty flush stays individual-empty.
        assert!(matches!(
            storm_gate(Vec::new(), 0),
            FlushOutcome::Individual(list) if list.is_empty()
        ));
    }

    // --- Watched roots (status/zone gating) ---

    #[test]
    fn watched_roots_filters_status_and_zone() {
        let projects = vec![
            entry("conf", &["D:/repos/conf"], ProjectStatus::Configured, None),
            entry(
                "confirmed",
                &["D:/repos/a", "E:/repos/b"],
                ProjectStatus::Confirmed,
                None,
            ),
            entry(
                "candidate",
                &["D:/repos/cand"],
                ProjectStatus::Discovered,
                None,
            ),
            entry(
                "secret",
                &["D:/repos/secret"],
                ProjectStatus::Confirmed,
                Some(Zone::NeverObserve),
            ),
            entry(
                "private",
                &["D:/repos/private"],
                ProjectStatus::Confirmed,
                Some(Zone::LocalOnly),
            ),
            entry("rootless", &[], ProjectStatus::Confirmed, None),
            entry("blank", &["  "], ProjectStatus::Confirmed, None),
        ];
        let roots = watched_roots(&projects);
        let ids: Vec<&str> = roots.iter().map(|r| r.project_id.as_str()).collect();
        // Configured + confirmed participate; one entry PER root path.
        assert_eq!(ids, vec!["conf", "confirmed", "confirmed", "private"]);
        // never_observe and discovery candidates are absent entirely.
        assert!(!ids.contains(&"candidate"));
        assert!(!ids.contains(&"secret"));
        // local_only roots tag their events LocalOnly.
        let private = roots.iter().find(|r| r.project_id == "private").unwrap();
        assert_eq!(private.sensitivity, EventSensitivity::LocalOnly);
        let conf = roots.iter().find(|r| r.project_id == "conf").unwrap();
        assert_eq!(conf.sensitivity, EventSensitivity::CloudAllowed);
    }

    #[test]
    fn best_root_index_prefers_deepest_root() {
        let roots = vec![
            root("outer", "D:/repos", EventSensitivity::CloudAllowed),
            root("inner", "D:/repos/app", EventSensitivity::CloudAllowed),
        ];
        assert_eq!(
            best_root_index(&roots, Path::new("D:\\repos\\app\\src\\x.rs")),
            Some(1),
            "longest prefix wins, separators/case normalized"
        );
        assert_eq!(
            best_root_index(&roots, Path::new("D:/repos/other/y.rs")),
            Some(0)
        );
        assert_eq!(
            best_root_index(&roots, Path::new("E:/elsewhere/z.rs")),
            None
        );
        // Prefix must respect component boundaries: "D:/repos-x" is not
        // under "D:/repos".
        assert_eq!(best_root_index(&roots, Path::new("D:/repos-x/z.rs")), None);
    }

    // --- Event construction: paths scrubbed, zone tagging, templates ---

    #[test]
    fn change_events_use_relative_paths_and_stable_templates() {
        let privacy = filter();
        let ts = Utc::now();
        let watched = root("proj", "D:/repos/proj", EventSensitivity::CloudAllowed);
        let event = change_event(
            &privacy,
            &watched,
            &FileChange::Created(PathBuf::from("D:/repos/proj/src/main.rs")),
            ts,
        );
        assert_eq!(event.source, EventSource::File);
        assert_eq!(event.event_type, EventType::FileCreated);
        assert!(event.event_type.valid_for(event.source));
        // Separator style follows the input path (real notify paths are
        // OS-native); the load-bearing part is the root prefix is gone.
        assert_eq!(event.summary, "src/main.rs");
        assert_eq!(event.project_id.as_deref(), Some("proj"));
        assert_eq!(event.application, "");
        assert_eq!(event.confidence, 1.0);
        assert_eq!(event.importance, COLLECTOR_EVENT_IMPORTANCE);
        assert_eq!(event.sensitivity, EventSensitivity::CloudAllowed);

        let renamed = change_event(
            &privacy,
            &watched,
            &FileChange::Renamed {
                from: PathBuf::from("D:/repos/proj/old.rs"),
                to: PathBuf::from("D:/repos/proj/new.rs"),
            },
            ts,
        );
        assert_eq!(renamed.event_type, EventType::FileRenamed);
        assert_eq!(renamed.summary, "old.rs → new.rs");

        let deleted = change_event(
            &privacy,
            &watched,
            &FileChange::Deleted(PathBuf::from("D:/repos/proj/a.rs")),
            ts,
        );
        assert_eq!(deleted.event_type, EventType::FileDeleted);
        let modified = change_event(
            &privacy,
            &watched,
            &FileChange::Modified(PathBuf::from("D:/repos/proj/a.rs")),
            ts,
        );
        assert_eq!(modified.event_type, EventType::FileModified);
    }

    #[test]
    fn paths_outside_the_root_are_scrubbed_absolute() {
        let privacy = filter();
        let watched = root("proj", "C:/work/proj", EventSensitivity::CloudAllowed);
        let event = change_event(
            &privacy,
            &watched,
            &FileChange::Modified(PathBuf::from("D:\\backup\\testuser\\file.txt")),
            Utc::now(),
        );
        assert!(
            !event.summary.contains("testuser"),
            "username must be redacted: {}",
            event.summary
        );
        assert!(event.summary.contains("[REDACTED]"));
    }

    #[test]
    fn local_only_zone_tags_every_event_local_only() {
        let privacy = filter();
        let watched = root("private", "D:/repos/private", EventSensitivity::LocalOnly);
        let ts = Utc::now();
        for event in [
            change_event(
                &privacy,
                &watched,
                &FileChange::Created(PathBuf::from("D:/repos/private/x.rs")),
                ts,
            ),
            bulk_change_event(&watched, 99, ts),
            resync_event(&watched, ts),
        ] {
            assert_eq!(event.sensitivity, EventSensitivity::LocalOnly);
        }
    }

    #[test]
    fn bulk_and_resync_events_use_stable_templates() {
        let watched = root("proj", "D:/repos/proj", EventSensitivity::CloudAllowed);
        let ts = Utc::now();
        let bulk = bulk_change_event(&watched, 137, ts);
        assert_eq!(bulk.event_type, EventType::FilesBulkChange);
        assert!(bulk.event_type.valid_for(bulk.source));
        assert_eq!(bulk.summary, "137 files changed in proj");
        let resync = resync_event(&watched, ts);
        assert_eq!(resync.event_type, EventType::FilesBulkChange);
        assert_eq!(resync.summary, "file resync in proj");
    }

    // --- Root state machine ---

    #[test]
    fn root_tracker_transitions_fire_once() {
        // error → unavailable (ONE transition) → rearm → active.
        let mut tracker = RootTracker::active();
        assert!(tracker.is_active());
        assert!(
            tracker.mark_unavailable("watch error"),
            "first failure transitions"
        );
        assert!(!tracker.is_active());
        assert_eq!(tracker.reason(), Some("watch error"));
        assert!(
            !tracker.mark_unavailable("root path missing"),
            "repeat failures must not re-fire the transition"
        );
        assert_eq!(
            tracker.reason(),
            Some("root path missing"),
            "reason updates"
        );
        assert!(tracker.mark_active(), "recovery transitions");
        assert!(tracker.is_active());
        assert!(!tracker.mark_active(), "already active is not a recovery");
        // Missing at start: born unavailable, first success recovers.
        let mut missing = RootTracker::unavailable("root path missing");
        assert!(!missing.is_active());
        assert!(missing.mark_active());
    }

    #[test]
    fn should_restart_only_on_channel_death() {
        let watcher = FileWatcher::new(FileWatcherConfig::default());
        assert!(watcher.is_healthy());
        assert!(!watcher.should_restart(), "fresh watcher never restarts");
        // Disabled-with-reason: healthy, no restart.
        watcher.disable("disabled by [file_watcher].enabled");
        assert!(watcher.is_healthy());
        assert!(!watcher.should_restart());
        // Unavailable roots: no restart.
        let health = watcher.health_handle();
        health.write().roots_unavailable = vec!["proj: root path missing".to_string()];
        assert!(!watcher.should_restart());
        // Channel death: the ONLY restart state.
        health.write().channel_dead = true;
        assert!(watcher.should_restart());
    }

    // --- Disabled-with-reason park states (mirror git_watch) ---

    #[tokio::test]
    async fn config_disabled_parks_with_reason() {
        // Default config: enabled=false — the watcher is opt-in.
        let watcher = FileWatcher::new(FileWatcherConfig::default());
        let health = watcher.health_handle();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { watcher.run(shutdown_rx).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            health.read().disabled_reason.as_deref(),
            Some("disabled by [file_watcher].enabled")
        );
        assert!(!health.read().enabled);
        shutdown_tx.send(true).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("watcher must park and respond to shutdown")
            .expect("no panic");
    }

    #[tokio::test]
    async fn toggle_off_parks_without_arming() {
        let toggles = ObservationToggles {
            files: false,
            ..ObservationToggles::default()
        };
        let config = FileWatcherConfig {
            enabled: true,
            ..FileWatcherConfig::default()
        };
        let watcher = FileWatcher::new(config).with_privacy(Arc::new(filter()), toggles);
        let health = watcher.health_handle();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { watcher.run(shutdown_rx).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            health.read().disabled_reason.as_deref(),
            Some("disabled by [privacy.toggles]")
        );
        shutdown_tx.send(true).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("shutdown")
            .expect("no panic");
    }

    // --- Integration against real notify (tempdir; generous timeouts,
    // poll-wait — keep it fast and non-flaky) ---

    fn provider_for(projects: Vec<ProjectEntry>) -> ProjectsProvider {
        Arc::new(move || {
            let projects = projects.clone();
            let fut: BoxFuture<'static, Option<Vec<ProjectEntry>>> =
                Box::pin(async move { Some(projects) });
            fut
        })
    }

    async fn wait_until(mut check: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if check() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        check()
    }

    async fn next_event_of(
        rx: &mut tokio::sync::mpsc::Receiver<ContextEvent>,
        event_type: EventType,
        timeout: Duration,
    ) -> Option<ContextEvent> {
        let deadline = Instant::now() + timeout;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(event)) if event.event_type == event_type => return Some(event),
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        None
    }

    #[tokio::test]
    async fn create_modify_rename_flow_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let root_dir = tmp.path().join("proj");
        std::fs::create_dir_all(&root_dir).unwrap();
        let project = entry(
            "proj",
            &[&root_dir.to_string_lossy()],
            ProjectStatus::Confirmed,
            None,
        );
        let config = FileWatcherConfig {
            enabled: true,
            debounce_ms: 200,
            ..FileWatcherConfig::default()
        };
        let (sender, mut rx) = EventSender::bounded(256);
        let watcher = FileWatcher::new(config)
            .with_privacy(Arc::new(filter()), ObservationToggles::default())
            .with_projects_provider(provider_for(vec![project]))
            .with_event_sender(sender);
        let health = watcher.health_handle();
        let recent = watcher.recent_file_handle();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { watcher.run(shutdown_rx).await });

        assert!(
            wait_until(|| health.read().roots_active == 1, Duration::from_secs(10)).await,
            "root must arm"
        );

        // Create.
        std::fs::write(root_dir.join("hello.txt"), "hi").unwrap();
        let created = next_event_of(&mut rx, EventType::FileCreated, Duration::from_secs(10))
            .await
            .expect("file_created event");
        assert!(
            created.summary.contains("hello.txt"),
            "summary: {}",
            created.summary
        );
        assert_eq!(created.project_id.as_deref(), Some("proj"));
        assert_eq!(created.sensitivity, EventSensitivity::CloudAllowed);
        // The recent-file slot (Task A4 tier-3 seam) tracked the path.
        assert!(
            wait_until(
                || {
                    recent
                        .read()
                        .as_ref()
                        .map(|p| {
                            p.to_string_lossy().contains("hello.txt")
                                || p.to_string_lossy().contains("proj")
                        })
                        .unwrap_or(false)
                },
                Duration::from_secs(2)
            )
            .await,
            "recent-file handle must update"
        );

        // Modify (after the create's debounce window closed).
        std::fs::write(root_dir.join("hello.txt"), "hi again").unwrap();
        let modified = next_event_of(&mut rx, EventType::FileModified, Duration::from_secs(10))
            .await
            .expect("file_modified event");
        assert!(modified.summary.contains("hello.txt"));

        // Rename.
        std::fs::rename(root_dir.join("hello.txt"), root_dir.join("world.txt")).unwrap();
        let renamed = next_event_of(&mut rx, EventType::FileRenamed, Duration::from_secs(10))
            .await
            .expect("file_renamed event");
        assert!(
            renamed.summary.contains("hello.txt") && renamed.summary.contains("world.txt"),
            "rename summary must show both endpoints: {}",
            renamed.summary
        );
        assert!(renamed.summary.contains(" → "));

        shutdown_tx.send(true).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("watcher must stop on shutdown")
            .expect("no panic");
    }

    #[tokio::test]
    async fn missing_root_is_unavailable_then_rearms_when_created() {
        let tmp = tempfile::tempdir().unwrap();
        let ghost = tmp.path().join("ghost");
        let project = entry(
            "ghost",
            &[&ghost.to_string_lossy()],
            ProjectStatus::Confirmed,
            None,
        );
        let config = FileWatcherConfig {
            enabled: true,
            debounce_ms: 100,
            rearm_secs: 1,
            ..FileWatcherConfig::default()
        };
        let (sender, mut rx) = EventSender::bounded(64);
        let watcher = FileWatcher::new(config)
            .with_privacy(Arc::new(filter()), ObservationToggles::default())
            .with_projects_provider(provider_for(vec![project]))
            .with_event_sender(sender);
        let health = watcher.health_handle();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { watcher.run(shutdown_rx).await });

        // Missing at start → unavailable, other state healthy, no restart.
        assert!(
            wait_until(
                || health.read().roots_unavailable.len() == 1,
                Duration::from_secs(10)
            )
            .await,
            "missing root must report unavailable"
        );
        assert!(health.read().roots_unavailable[0].contains("root path missing"));
        assert!(!health.read().channel_dead);

        // Root appears → rearm on backoff → active + one resync event.
        std::fs::create_dir_all(&ghost).unwrap();
        assert!(
            wait_until(|| health.read().roots_active == 1, Duration::from_secs(10)).await,
            "root must rearm after it appears"
        );
        let resync = next_event_of(&mut rx, EventType::FilesBulkChange, Duration::from_secs(10))
            .await
            .expect("resync files_bulk_change event");
        assert_eq!(resync.summary, "file resync in ghost");

        shutdown_tx.send(true).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("shutdown")
            .expect("no panic");
    }
}
