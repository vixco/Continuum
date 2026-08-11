//! # Git collector (context engine spec §4.4)
//!
//! Watches the **active, confirmed** project's repository — the project
//! resolver's post-hysteresis output read through [`CurrentProjectHandle`],
//! never the daemon's own working directory and never an unconfirmed
//! discovery candidate. Reports branch, staged/dirty/untracked counts,
//! ahead/behind, conflicts, and the last commit (id + subject) into the
//! shared [`ProjectWorldState`] projection, and emits **change-only**
//! [`ContextEvent`]s (`commit`, `branch_switch`, `conflict`,
//! `dirty_change`) onto the events channel via its [`EventSender`]
//! (Task A6) — pre-stamped with the project id, non-blocking, log-only
//! when no sender was injected.
//!
//! ## Probe shape
//!
//! One consolidated `git status --porcelain=v2 --branch` per poll (branch,
//! head OID, ahead/behind, all counts in a single subprocess), plus
//! `git log -1 --format=%H%x1f%s` **only when ref-derived facts may have
//! changed** — gated on the mtimes of the resolved gitdir's `HEAD`,
//! `logs/HEAD`, and `packed-refs` (spec §4.4: the mtime gate applies only
//! to ref-derived facts; working-tree dirtiness has no mtime signal and
//! always polls). Subprocesses run with `CREATE_NO_WINDOW`, a hard
//! `[git_context] command_timeout_secs` timeout, and `kill_on_drop`.
//!
//! ## Privacy (spec §4.1)
//!
//! The commit **subject is free text** and passes through
//! [`PrivacyFilter::scrub_text`] at collector emit; the commit **OID,
//! branch name, and counts are structured fields**, exempt by
//! construction. The project's zone (spec §4.3 project zones) gates the
//! collector: `never_observe` → nothing runs; `local_only` → collected,
//! but every event is tagged [`EventSensitivity::LocalOnly`]. The
//! `[privacy.toggles].git` honest toggle means no `git` subprocess is ever
//! spawned when off.
//!
//! ## Failure modes (spec §4.4)
//!
//! `git --version` is probed once at task start — absent → the collector
//! parks in a *disabled-with-reason* state: `is_healthy() = true`,
//! `enabled = false`, the reason surfaced in [`GitWatchHealth`],
//! `should_restart() = false` (re-probed only when a config change
//! restarts the senses). Probe errors/timeouts log a rate-limited warning
//! and keep the last known state — never restart-thrash. `.git`-as-file
//! worktrees resolve the `gitdir:` pointer; zero-commit and detached-HEAD
//! repos report `branch: None` without error.
//!
//! ## Layer
//!
//! Layer 1 — Senses. Structured polling, no AI involvement.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::config::{ContextConfig, GitContextConfig, ObservationToggles, PrivacyConfig};
use crate::context::project::{CurrentProject, CurrentProjectHandle, ProjectStatus};
use crate::memory::events::{
    ContextEvent, EventSender, EventSensitivity, EventSource, EventType, COLLECTOR_EVENT_IMPORTANCE,
};
use crate::senses::live_context::{LiveContextHub, ProjectGitFacts};
use crate::senses::privacy::{
    emit_system_event, source_enabled, ObservedSource, PrivacyFilter, Zone,
};
use crate::senses::toggles::ToggleControl;

/// `CREATE_NO_WINDOW` — spawn `git` without flashing a console window
/// (spec §4.4).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Minimum seconds between two probe-failure warnings for the same watcher
/// (probe errors keep the last state; the log must not thrash either).
const WARN_INTERVAL_SECS: u64 = 60;

/// Scheduler tick — how often the watcher re-reads the project handle to
/// notice a post-hysteresis switch. Probes themselves are paced by
/// `poll_secs` / `min_spawn_interval_secs`.
const SCHEDULER_TICK_MS: u64 = 1_000;

// ---------------------------------------------------------------------------
// Porcelain v2 parsing (pure)
// ---------------------------------------------------------------------------

/// Parsed output of one `git status --porcelain=v2 --branch` probe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitStatusSnapshot {
    /// Current branch name. `None` for detached HEAD **and** zero-commit
    /// repos (spec §4.4: empty/detached → `branch: null`, no error).
    pub branch: Option<String>,
    /// HEAD commit OID (structured field — never scrubbed). `None` for
    /// zero-commit repos (`branch.oid (initial)`).
    pub head_oid: Option<String>,
    /// Commits ahead of upstream (0 when no upstream is configured).
    pub ahead: u32,
    /// Commits behind upstream.
    pub behind: u32,
    /// Entries with staged (index) changes.
    pub staged: u32,
    /// Tracked entries with unstaged working-tree changes.
    pub dirty: u32,
    /// Untracked files.
    pub untracked: u32,
    /// Unmerged (conflicted) paths.
    pub conflicts: u32,
}

impl GitStatusSnapshot {
    /// The boolean the `dirty_change` event hysteresis runs on: any
    /// staged, unstaged, or conflicted **tracked** change. Untracked
    /// files are deliberately excluded — they appear and vanish too often
    /// to be a clean/dirty signal (the file watcher covers them).
    pub fn is_dirty(&self) -> bool {
        self.staged > 0 || self.dirty > 0 || self.conflicts > 0
    }
}

/// Parses `git status --porcelain=v2 --branch` output (pure — the entire
/// probe result shape is testable without git).
///
/// Recognized lines (format documented in `git-status(1)`):
/// - `# branch.oid <oid>|(initial)` → [`GitStatusSnapshot::head_oid`]
/// - `# branch.head <name>|(detached)` → [`GitStatusSnapshot::branch`]
/// - `# branch.ab +<ahead> -<behind>` → ahead/behind
/// - `1 XY …` / `2 XY …` (changed/renamed): `X != '.'` counts staged,
///   `Y != '.'` counts dirty
/// - `u …` → conflicts, `? …` → untracked, `! …` (ignored) skipped
pub fn parse_porcelain_v2(output: &str) -> GitStatusSnapshot {
    let mut snapshot = GitStatusSnapshot::default();
    for line in output.lines() {
        if let Some(oid) = line.strip_prefix("# branch.oid ") {
            snapshot.head_oid = match oid.trim() {
                "" | "(initial)" => None,
                oid => Some(oid.to_string()),
            };
        } else if let Some(head) = line.strip_prefix("# branch.head ") {
            snapshot.branch = match head.trim() {
                "" | "(detached)" => None,
                head => Some(head.to_string()),
            };
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for token in ab.split_whitespace() {
                if let Some(ahead) = token.strip_prefix('+') {
                    snapshot.ahead = ahead.parse().unwrap_or(0);
                } else if let Some(behind) = token.strip_prefix('-') {
                    snapshot.behind = behind.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let bytes = line.as_bytes();
            if bytes.len() >= 4 {
                if bytes[2] != b'.' {
                    snapshot.staged += 1;
                }
                if bytes[3] != b'.' {
                    snapshot.dirty += 1;
                }
            }
        } else if line.starts_with("u ") {
            snapshot.conflicts += 1;
        } else if line.starts_with("? ") {
            snapshot.untracked += 1;
        }
        // "! " (ignored) and unknown lines: skipped.
    }
    // Zero-commit rule (spec §4.4): an unborn branch has no commits yet —
    // report branch: None rather than the unborn ref name.
    if snapshot.head_oid.is_none() {
        snapshot.branch = None;
    }
    snapshot
}

// ---------------------------------------------------------------------------
// Gitdir resolution + ref mtime gate (pure / fs-only)
// ---------------------------------------------------------------------------

/// Resolves a project root's git directory, following the `.git`-as-file
/// `gitdir: <path>` pointer used by worktrees and submodules (spec §4.4).
/// Returns `None` when the root is not a git repository.
pub fn resolve_gitdir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let marker = std::fs::read_to_string(&dot_git).ok()?;
    let pointer = marker.trim().strip_prefix("gitdir:")?.trim();
    let candidate = PathBuf::from(pointer);
    Some(if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    })
}

/// Mtimes of the ref-bearing files inside a gitdir. Any change here means
/// ref-derived facts (HEAD, branch, last commit) may have moved, so the
/// `git log -1` probe re-runs; unchanged mtimes skip it (spec §4.4 gate
/// split — dirtiness is *not* gated on this).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefState {
    /// `<gitdir>/HEAD` — changes on checkout / detach.
    pub head: Option<SystemTime>,
    /// `<gitdir>/logs/HEAD` — the reflog; appended on every commit,
    /// checkout, reset.
    pub logs_head: Option<SystemTime>,
    /// `<commondir>/packed-refs` — changes on pack/fetch.
    pub packed_refs: Option<SystemTime>,
}

/// Reads the current [`RefState`] for a resolved gitdir. Worktree gitdirs
/// keep `packed-refs` in the shared common dir (`<gitdir>/commondir`
/// pointer); missing files simply report `None`.
pub fn read_ref_state(gitdir: &Path) -> RefState {
    fn mtime(path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    }
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|pointer| {
            let path = PathBuf::from(pointer.trim());
            if path.is_absolute() {
                path
            } else {
                gitdir.join(path)
            }
        })
        .unwrap_or_else(|| gitdir.to_path_buf());
    RefState {
        head: mtime(&gitdir.join("HEAD")),
        logs_head: mtime(&gitdir.join("logs").join("HEAD")),
        packed_refs: mtime(&common.join("packed-refs")),
    }
}

/// The gate-split decision (pure, spec §4.4): whether this poll needs the
/// `git log -1` subject probe. Ref-derived facts re-probe when the ref
/// mtimes changed **or** the status probe already shows a different HEAD
/// OID (belt-and-braces for filesystems with coarse mtimes); the very
/// first probe always runs it. Working-tree dirtiness never consults this
/// gate — the status probe runs every poll unconditionally.
pub fn needs_log_probe(
    previous_refs: Option<&RefState>,
    refs: &RefState,
    previous_oid: Option<&str>,
    oid: Option<&str>,
) -> bool {
    let Some(previous_refs) = previous_refs else {
        return true;
    };
    previous_refs != refs || previous_oid != oid
}

/// Probe pacing (pure): a probe runs when `poll` has elapsed since the
/// last one, or immediately after a post-hysteresis project switch — but
/// never more often than `min_spawn` (spec §4.4 min-spawn-interval knob).
/// The first probe (no `last`) always runs.
pub fn should_probe(
    now: Instant,
    last: Option<Instant>,
    poll: Duration,
    min_spawn: Duration,
    project_changed: bool,
) -> bool {
    let Some(last) = last else {
        return true;
    };
    let elapsed = now.saturating_duration_since(last);
    elapsed >= poll || (project_changed && elapsed >= min_spawn)
}

// ---------------------------------------------------------------------------
// Watch-target gating (pure)
// ---------------------------------------------------------------------------

/// What the watcher would probe this tick, derived from the resolver's
/// current project (spec §4.4 gating): the project must be **confirmed**
/// (configured entries count as confirmed — both are participating
/// statuses; discovery candidates never reach the handle *and* are
/// re-checked here), must have a root path, and must not be zoned
/// `never_observe`. A `local_only` zone collects but tags every event
/// [`EventSensitivity::LocalOnly`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTarget {
    /// Resolved project id (stamped onto every emitted event).
    pub project_id: String,
    /// Repository root to probe (raw, unscrubbed — paths never leave the
    /// collector; only structured git facts and scrubbed subjects do).
    pub root: PathBuf,
    /// Zone-derived sensitivity for this project's events.
    pub sensitivity: EventSensitivity,
}

/// Derives the [`WatchTarget`] from the current project, or `None` when
/// nothing may be probed.
pub fn watch_target(current: Option<&CurrentProject>) -> Option<WatchTarget> {
    let project = current?;
    if !matches!(
        project.status,
        ProjectStatus::Configured | ProjectStatus::Confirmed
    ) {
        return None;
    }
    if project.zone == Some(Zone::NeverObserve) {
        return None;
    }
    let root = project.root_path.clone()?;
    let sensitivity = if project.zone == Some(Zone::LocalOnly) {
        EventSensitivity::LocalOnly
    } else {
        EventSensitivity::CloudAllowed
    };
    Some(WatchTarget {
        project_id: project.id.clone(),
        root,
        sensitivity,
    })
}

// ---------------------------------------------------------------------------
// Change-only event derivation (pure)
// ---------------------------------------------------------------------------

/// Short display form of a commit OID (7 chars, the git default).
fn short_oid(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

/// Branch display form for event summaries: `(detached)` for `None`
/// (which also covers zero-commit repos, whose branch is normalized to
/// `None` — see [`parse_porcelain_v2`]).
fn branch_label(branch: Option<&str>) -> &str {
    branch.unwrap_or("(detached)")
}

/// Derives the change-only events between two consecutive fact sets for
/// the **same project** (spec §4.4). The first probe of a project has no
/// `previous` and emits nothing — this function is only called with a
/// baseline. Rules (stable summary templates — consumers parse them):
///
/// - `branch_switch` — branch changed between two born states:
///   `"branch <old> → <new>"` (`(detached)` endpoints included).
/// - `commit` — HEAD OID changed and no branch switch was emitted (a
///   checkout moves the OID too; one event per cause):
///   `"commit <shortoid> <subject>"`.
/// - `conflict` — unmerged paths appeared (0 → N):
///   `"conflict: <n> unmerged"`. Resolution (N → 0) emits nothing.
/// - `dirty_change` — the [`GitStatusSnapshot::is_dirty`]-equivalent
///   boolean crossed (hysteresis: count changes while already dirty are
///   NOT events): `"worktree dirty (<staged> staged, <dirty> modified)"`
///   / `"worktree clean"`.
pub fn git_events(
    project_id: &str,
    sensitivity: EventSensitivity,
    previous: &ProjectGitFacts,
    current: &ProjectGitFacts,
    ts: DateTime<Utc>,
) -> Vec<ContextEvent> {
    let event = |event_type: EventType, summary: String| ContextEvent {
        ts,
        source: EventSource::Git,
        application: "git".to_string(),
        window_title: String::new(),
        project_id: Some(project_id.to_string()),
        event_type,
        summary,
        importance: COLLECTOR_EVENT_IMPORTANCE,
        confidence: 1.0,
        sensitivity,
        raw_reference: None,
    };
    let mut events = Vec::new();

    // Branch switch: both endpoints must be born (an unborn → first-commit
    // transition is a commit, not a switch).
    let branch_switched = previous.branch != current.branch
        && previous.last_commit_id.is_some()
        && current.last_commit_id.is_some();
    if branch_switched {
        events.push(event(
            EventType::BranchSwitch,
            format!(
                "branch {} → {}",
                branch_label(previous.branch.as_deref()),
                branch_label(current.branch.as_deref())
            ),
        ));
    } else if previous.last_commit_id != current.last_commit_id {
        if let Some(oid) = current.last_commit_id.as_deref() {
            let subject = current.last_commit_subject.as_deref().unwrap_or("");
            events.push(event(
                EventType::Commit,
                format!("commit {} {}", short_oid(oid), subject)
                    .trim_end()
                    .to_string(),
            ));
        }
    }

    if previous.conflicts == 0 && current.conflicts > 0 {
        events.push(event(
            EventType::Conflict,
            format!("conflict: {} unmerged", current.conflicts),
        ));
    }

    let was_dirty = previous.staged > 0 || previous.dirty > 0 || previous.conflicts > 0;
    let is_dirty = current.staged > 0 || current.dirty > 0 || current.conflicts > 0;
    if was_dirty != is_dirty {
        events.push(event(
            EventType::DirtyChange,
            if is_dirty {
                format!(
                    "worktree dirty ({} staged, {} modified)",
                    current.staged, current.dirty
                )
            } else {
                "worktree clean".to_string()
            },
        ));
    }

    events
}

/// Assembles the publishable fact set from a status snapshot and the
/// (possibly cached) last-commit subject. The **subject is scrubbed
/// here** — the single free-text field this collector emits; OID, branch,
/// and counts are structured and exempt (spec §4.1 privacy contract).
pub fn assemble_facts(
    privacy: &PrivacyFilter,
    snapshot: &GitStatusSnapshot,
    subject: Option<&str>,
) -> ProjectGitFacts {
    ProjectGitFacts {
        branch: snapshot.branch.clone(),
        dirty: snapshot.dirty,
        staged: snapshot.staged,
        untracked: snapshot.untracked,
        ahead: snapshot.ahead,
        behind: snapshot.behind,
        conflicts: snapshot.conflicts,
        last_commit_id: snapshot.head_oid.clone(),
        last_commit_subject: snapshot
            .head_oid
            .is_some()
            .then(|| subject.map(|subject| privacy.scrub_text(subject)))
            .flatten(),
    }
}

// ---------------------------------------------------------------------------
// Subprocess probes (exported — Task C4's `context_git` named-project
// probe reuses these)
// ---------------------------------------------------------------------------

/// Runs one `git` subprocess: `CREATE_NO_WINDOW`, hard timeout,
/// `kill_on_drop` (a timed-out child is killed when the future drops).
async fn run_git(args: &[&str], timeout: Duration) -> Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(output) => output.context("failed to spawn git"),
        Err(_) => bail!("git probe timed out after {}s", timeout.as_secs()),
    }
}

/// Probes whether a `git` executable is on PATH, returning its version
/// line. Called once at watcher start (spec §4.4: absent →
/// disabled-with-reason; re-probed only on a config-change restart).
pub async fn git_version(timeout: Duration) -> Option<String> {
    let output = run_git(&["--version"], timeout).await.ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The consolidated status probe (spec §4.4): one
/// `git status --porcelain=v2 --branch` giving branch, HEAD OID,
/// ahead/behind, and every count in a single subprocess.
pub async fn probe_repo_status(root: &Path, timeout: Duration) -> Result<GitStatusSnapshot> {
    let root_arg = root.to_string_lossy();
    let output = run_git(
        &["-C", &root_arg, "status", "--porcelain=v2", "--branch"],
        timeout,
    )
    .await?;
    if !output.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(parse_porcelain_v2(&String::from_utf8_lossy(&output.stdout)))
}

/// The last-commit probe: `git log -1 --format=%H%x1f%s` → `(oid, raw
/// subject)`. Returns `Ok(None)` for zero-commit repos (git exits
/// non-zero on an unborn HEAD — that is not an error, spec §4.4). The
/// subject is **raw** here; callers scrub via [`assemble_facts`].
pub async fn probe_last_commit(root: &Path, timeout: Duration) -> Result<Option<(String, String)>> {
    let root_arg = root.to_string_lossy();
    let output = run_git(
        &["-C", &root_arg, "log", "-1", "--format=%H%x1f%s"],
        timeout,
    )
    .await?;
    if !output.status.success() {
        // Unborn HEAD ("does not have any commits yet") — a normal state.
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    Ok(line
        .split_once('\u{1f}')
        .map(|(oid, subject)| (oid.trim().to_string(), subject.trim().to_string())))
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// Health snapshot of the git collector, readable by the repair agent and
/// the Context page (spec §7: degraded-permanent states report
/// healthy + disabled with a reason — never restart-thrash).
#[derive(Debug, Clone, Default, Serialize)]
pub struct GitWatchHealth {
    /// Whether the collector is actively polling. `false` + a reason is
    /// the *disabled-with-reason* state (git absent, toggle off, config
    /// off) — still healthy.
    pub enabled: bool,
    /// Why the collector is disabled, when it is.
    pub disabled_reason: Option<String>,
    /// When the last successful status probe completed.
    pub last_probe_at: Option<DateTime<Utc>>,
    /// Total successful status probes.
    pub probes: u64,
    /// Consecutive failed probes (reset on success). Failures keep the
    /// last known state and never restart the watcher.
    pub consecutive_failures: u32,
    /// Total change-only events emitted into the buffer.
    pub events_emitted: u64,
}

/// Shared handle onto the collector's health snapshot.
pub type SharedGitWatchHealth = Arc<RwLock<GitWatchHealth>>;

// ---------------------------------------------------------------------------
// GitWatcher
// ---------------------------------------------------------------------------

/// Per-repo probe baseline: the last published facts and ref mtimes for
/// one project id. Diffing only ever happens against a baseline of the
/// **same** project — a switch re-baselines (first probe of a project
/// emits no events).
#[derive(Debug, Clone)]
struct RepoWatchState {
    project_id: String,
    facts: ProjectGitFacts,
    refs: RefState,
}

/// The runtime-gated git collector task (spec §4.4).
///
/// # Layer
///
/// Layer 1 — Senses. Structured subprocess polling, no AI involvement.
///
/// # Self-healing
///
/// [`GitWatcher::health`] exposes the [`GitWatchHealth`] snapshot;
/// [`GitWatcher::is_healthy`] is `true` even when disabled-with-reason
/// (spec §7), and [`GitWatcher::should_restart`] is always `false` —
/// probe failures keep the last state, and a missing git binary is only
/// re-probed when a config change restarts the senses.
pub struct GitWatcher {
    config: GitContextConfig,
    /// The privacy choke point (spec §4.1) — scrubs commit subjects.
    privacy: Arc<PrivacyFilter>,
    /// Honest per-source toggles; `[privacy.toggles].git` gates every
    /// subprocess spawn.
    toggles: ObservationToggles,
    /// Live toggle control (Task C5): re-read every poll cycle so a
    /// Context-page switch stops the `git` subprocesses without a restart.
    toggle_control: Option<ToggleControl>,
    /// Shared read handle onto the resolver's current project (Task A4).
    /// `None` (tests, tools) watches nothing.
    project: Option<CurrentProjectHandle>,
    /// Events-channel producer handle (Task A6): change-only git events
    /// go here, non-blocking, already project-stamped. Log-only until the
    /// runtime injects the real sender via
    /// [`GitWatcher::with_event_sender`].
    events: EventSender,
    health: SharedGitWatchHealth,
}

impl GitWatcher {
    /// Creates a git watcher with the given `[git_context]` config.
    ///
    /// Standalone construction (tests, tools) synthesizes a default
    /// privacy filter; the runtime shares its boot-time filter via
    /// [`GitWatcher::with_privacy`].
    pub fn new(config: GitContextConfig) -> Self {
        debug!(
            layer = "senses",
            component = "git_watch",
            enabled = config.enabled,
            poll_secs = config.poll_secs,
            "GitWatcher created"
        );
        Self {
            config,
            privacy: Arc::new(PrivacyFilter::from_config(
                &ContextConfig::default(),
                &PrivacyConfig::default(),
            )),
            toggles: ObservationToggles::default(),
            toggle_control: None,
            project: None,
            events: EventSender::log_only(),
            health: SharedGitWatchHealth::default(),
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

    /// Attaches the project resolver's shared current-project handle
    /// (Task A4 seam) — the only source of "which repo to watch".
    pub fn with_project_handle(mut self, handle: CurrentProjectHandle) -> Self {
        self.project = Some(handle);
        self
    }

    /// Attaches the events-channel producer handle (Task A6, spec §3).
    /// Called once at senses spawn in the runtime binary; without it the
    /// collector's change events are log-only.
    pub fn with_event_sender(mut self, sender: EventSender) -> Self {
        self.events = sender;
        self
    }

    /// Current health snapshot (cheap clone).
    pub fn health(&self) -> GitWatchHealth {
        self.health.read().clone()
    }

    /// Shared handle onto the health snapshot, for callers that outlive
    /// the moved watcher (health loop, tests).
    pub fn health_handle(&self) -> SharedGitWatchHealth {
        self.health.clone()
    }

    /// Attach a health handle created externally — used by the runtime
    /// supervisor respawn path so a respawned watcher keeps writing to the
    /// same shared health Arc the publisher already holds (health survives
    /// restarts instead of orphaning on the dead instance).
    pub fn with_health(mut self, health: SharedGitWatchHealth) -> Self {
        self.health = health;
        self
    }

    /// Always `true`: probe failures keep the last state and
    /// disabled-with-reason is a healthy, deliberate state (spec §7). The
    /// details live in [`GitWatcher::health`].
    pub fn is_healthy(&self) -> bool {
        true
    }

    /// Always `false` (spec §4.4): restarting cannot fix a missing git
    /// binary, and transient probe failures never thrash the watcher. A
    /// config-change restart of the senses re-probes everything.
    pub fn should_restart(&self) -> bool {
        false
    }

    fn disable(&self, reason: &str) {
        let mut health = self.health.write();
        health.enabled = false;
        health.disabled_reason = Some(reason.to_string());
    }

    /// Runs the collector until the shutdown signal fires. Disabled states
    /// (config off, toggle off, git absent) park here — still responding
    /// to shutdown — so the health snapshot stays observable.
    pub async fn run(
        &self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        live_context: Option<LiveContextHub>,
    ) {
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
                component = "git_watch",
                "git collector disabled by [git_context].enabled"
            );
            self.disable("disabled by [git_context].enabled");
            wait_for_shutdown(&mut shutdown).await;
            return;
        }
        // Honest toggle (spec §4.1, the A2 seam): no git subprocess is
        // ever spawned when the git source is off.
        if !source_enabled(&self.live_toggles(), ObservedSource::Git) {
            emit_system_event(
                "toggle_change",
                "git observation disabled by [privacy.toggles]; git collector will not run",
            );
            self.disable("disabled by [privacy.toggles]");
            wait_for_shutdown(&mut shutdown).await;
            return;
        }

        let timeout = Duration::from_secs(self.config.command_timeout_secs.max(1));
        // Version probe, once (spec §4.4 failure mode): absent → park in
        // disabled-with-reason; re-probed only on a config-change restart.
        match git_version(timeout).await {
            Some(version) => {
                debug!(
                    layer = "senses",
                    component = "git_watch",
                    version = %version,
                    "git executable found"
                );
                let mut health = self.health.write();
                health.enabled = true;
                health.disabled_reason = None;
            }
            None => {
                warn!(
                    layer = "senses",
                    component = "git_watch",
                    "git executable not found on PATH; git collector disabled"
                );
                emit_system_event(
                    "source_unavailable",
                    "git executable not found on PATH; git collector disabled",
                );
                self.disable("git executable not found on PATH");
                wait_for_shutdown(&mut shutdown).await;
                return;
            }
        }

        let poll = Duration::from_secs(self.config.poll_secs.max(1));
        let min_spawn = Duration::from_secs(self.config.min_spawn_interval_secs);
        let mut ticker = tokio::time::interval(Duration::from_millis(SCHEDULER_TICK_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        debug!(
            layer = "senses",
            component = "git_watch",
            poll_secs = self.config.poll_secs,
            "git collector loop starting"
        );

        let mut last_probe: Option<Instant> = None;
        let mut last_warn: Option<Instant> = None;
        let mut state: Option<RepoWatchState> = None;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // Honest toggle, live (spec §4.1, Task C5): no `git`
                    // subprocess is spawned while the source is off, and
                    // the baseline is dropped so the return re-baselines
                    // instead of diffing against stale facts.
                    if !source_enabled(&self.live_toggles(), ObservedSource::Git) {
                        if state.is_some() {
                            state = None;
                            self.disable("disabled by [privacy.toggles]");
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
                    let current = self
                        .project
                        .as_ref()
                        .and_then(|handle| handle.read().clone());
                    let Some(target) = watch_target(current.as_ref()) else {
                        // Nothing watchable: drop the baseline so a later
                        // return to the project re-baselines instead of
                        // diffing against stale facts.
                        state = None;
                        continue;
                    };
                    let project_changed = state
                        .as_ref()
                        .map(|s| s.project_id != target.project_id)
                        .unwrap_or(true);
                    let now = Instant::now();
                    if !should_probe(now, last_probe, poll, min_spawn, project_changed) {
                        continue;
                    }
                    last_probe = Some(now);
                    state = self
                        .probe_cycle(&target, state.take(), &live_context, timeout, &mut last_warn)
                        .await;
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        debug!(
                            layer = "senses",
                            component = "git_watch",
                            "Shutdown signal received, stopping git collector"
                        );
                        return;
                    }
                }
            }
        }
    }

    /// One probe cycle: consolidated status probe, mtime-gated log probe,
    /// change-only event derivation, hub publish. Returns the new
    /// baseline (`None` only when the probe failed with no prior state —
    /// probe failures otherwise keep the previous baseline).
    async fn probe_cycle(
        &self,
        target: &WatchTarget,
        previous: Option<RepoWatchState>,
        live_context: &Option<LiveContextHub>,
        timeout: Duration,
        last_warn: &mut Option<Instant>,
    ) -> Option<RepoWatchState> {
        // A baseline from a different project must never be diffed against.
        let previous = previous.filter(|state| state.project_id == target.project_id);

        let snapshot = match probe_repo_status(&target.root, timeout).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                {
                    let mut health = self.health.write();
                    health.consecutive_failures = health.consecutive_failures.saturating_add(1);
                }
                // Rate-limited warn (spec §4.4): keep the last state, do
                // not thrash the log.
                let now = Instant::now();
                let should_log = last_warn
                    .map(|at| {
                        now.saturating_duration_since(at) >= Duration::from_secs(WARN_INTERVAL_SECS)
                    })
                    .unwrap_or(true);
                if should_log {
                    *last_warn = Some(now);
                    warn!(
                        layer = "senses",
                        component = "git_watch",
                        project = %target.project_id,
                        error = %error,
                        "git status probe failed; keeping last known state"
                    );
                }
                return previous;
            }
        };

        // Ref-mtime gate (spec §4.4): only ref-derived facts skip work —
        // the status probe above always ran.
        let refs = resolve_gitdir(&target.root)
            .map(|gitdir| read_ref_state(&gitdir))
            .unwrap_or_default();
        let need_log = needs_log_probe(
            previous.as_ref().map(|state| &state.refs),
            &refs,
            previous
                .as_ref()
                .and_then(|state| state.facts.last_commit_id.as_deref()),
            snapshot.head_oid.as_deref(),
        );
        let subject = if need_log {
            match probe_last_commit(&target.root, timeout).await {
                Ok(commit) => commit.map(|(_, subject)| subject),
                Err(error) => {
                    debug!(
                        layer = "senses",
                        component = "git_watch",
                        project = %target.project_id,
                        error = %error,
                        "git log probe failed; keeping cached commit subject"
                    );
                    previous
                        .as_ref()
                        .and_then(|state| state.facts.last_commit_subject.clone())
                }
            }
        } else {
            previous
                .as_ref()
                .and_then(|state| state.facts.last_commit_subject.clone())
        };
        // Note: an already-scrubbed cached subject passes through
        // scrub_text again unchanged (scrubbing is idempotent — A1).
        let facts = assemble_facts(&self.privacy, &snapshot, subject.as_deref());

        {
            let mut health = self.health.write();
            health.probes = health.probes.saturating_add(1);
            health.consecutive_failures = 0;
            health.last_probe_at = Some(Utc::now());
        }

        // Change-only events (spec §4.4): the first probe of a project is
        // the baseline and emits nothing.
        if let Some(previous_state) = &previous {
            let events = git_events(
                &target.project_id,
                target.sensitivity,
                &previous_state.facts,
                &facts,
                Utc::now(),
            );
            if !events.is_empty() {
                let mut health = self.health.write();
                health.events_emitted = health.events_emitted.saturating_add(events.len() as u64);
            }
            // Events channel (Task A6): git events are pre-stamped with
            // their project id — the collector knows its project — so the
            // writer's fill-missing stamping skips them.
            for event in events {
                self.events.send(event);
            }
        }

        // Publish into the shared projection on first probe or change.
        let facts_changed = previous
            .as_ref()
            .map(|state| state.facts != facts)
            .unwrap_or(true);
        if facts_changed {
            if let Some(hub) = live_context {
                hub.record_git_facts(Utc::now(), facts.clone());
            }
        }

        Some(RepoWatchState {
            project_id: target.project_id.clone(),
            facts,
            refs,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn filter() -> PrivacyFilter {
        PrivacyFilter::from_config(&ContextConfig::default(), &PrivacyConfig::default())
            .with_environment(None, None)
    }

    fn facts(
        branch: Option<&str>,
        oid: Option<&str>,
        staged: u32,
        dirty: u32,
        conflicts: u32,
    ) -> ProjectGitFacts {
        ProjectGitFacts {
            branch: branch.map(str::to_string),
            dirty,
            staged,
            untracked: 0,
            ahead: 0,
            behind: 0,
            conflicts,
            last_commit_id: oid.map(str::to_string),
            last_commit_subject: oid.map(|_| "initial".to_string()),
        }
    }

    fn current(id: &str, status: ProjectStatus, zone: Option<Zone>) -> CurrentProject {
        CurrentProject {
            id: id.to_string(),
            name: id.to_string(),
            root_path: Some(PathBuf::from("C:\\repos").join(id)),
            confidence: 0.9,
            source_tier: 1,
            zone,
            status,
        }
    }

    // --- Porcelain v2 parsing matrix ---

    #[test]
    fn porcelain_v2_full_matrix() {
        let output = concat!(
            "# branch.oid 4c5b3f1a9d8e7c6b5a4f3e2d1c0b9a8f7e6d5c4b\n",
            "# branch.head feat/x\n",
            "# branch.upstream origin/feat/x\n",
            "# branch.ab +2 -1\n",
            "1 .M N... 100644 100644 100644 aaa bbb src/unstaged.rs\n",
            "1 M. N... 100644 100644 100644 aaa bbb src/staged.rs\n",
            "1 MM N... 100644 100644 100644 aaa bbb src/both.rs\n",
            "2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\told.rs\n",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflicted.rs\n",
            "? untracked.txt\n",
            "! ignored.txt\n",
        );
        let snapshot = parse_porcelain_v2(output);
        assert_eq!(snapshot.branch.as_deref(), Some("feat/x"));
        assert_eq!(
            snapshot.head_oid.as_deref(),
            Some("4c5b3f1a9d8e7c6b5a4f3e2d1c0b9a8f7e6d5c4b")
        );
        assert_eq!(snapshot.ahead, 2);
        assert_eq!(snapshot.behind, 1);
        assert_eq!(snapshot.staged, 3, "M., MM, R. carry staged changes");
        assert_eq!(snapshot.dirty, 2, ".M, MM carry unstaged changes");
        assert_eq!(snapshot.untracked, 1, "ignored entries are not untracked");
        assert_eq!(snapshot.conflicts, 1);
        assert!(snapshot.is_dirty());
    }

    #[test]
    fn porcelain_v2_detached_head_has_no_branch() {
        let output = concat!(
            "# branch.oid 4c5b3f1a9d8e7c6b5a4f3e2d1c0b9a8f7e6d5c4b\n",
            "# branch.head (detached)\n",
        );
        let snapshot = parse_porcelain_v2(output);
        assert_eq!(snapshot.branch, None);
        assert!(snapshot.head_oid.is_some());
        assert!(!snapshot.is_dirty());
    }

    #[test]
    fn porcelain_v2_zero_commit_repo_normalizes_branch_to_none() {
        // Spec §4.4: empty repos report branch: null, no error — even
        // though porcelain reports the unborn branch name.
        let output = concat!("# branch.oid (initial)\n", "# branch.head main\n");
        let snapshot = parse_porcelain_v2(output);
        assert_eq!(snapshot.head_oid, None);
        assert_eq!(
            snapshot.branch, None,
            "unborn branch must normalize to None"
        );
    }

    #[test]
    fn porcelain_v2_clean_repo_without_upstream() {
        let output = concat!(
            "# branch.oid aaaabbbbccccddddeeeeffff0000111122223333\n",
            "# branch.head main\n",
        );
        let snapshot = parse_porcelain_v2(output);
        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.ahead, 0);
        assert_eq!(snapshot.behind, 0);
        assert!(!snapshot.is_dirty());
    }

    #[test]
    fn untracked_files_do_not_make_the_tree_dirty() {
        let output = concat!(
            "# branch.oid aaaabbbbccccddddeeeeffff0000111122223333\n",
            "# branch.head main\n",
            "? notes.txt\n",
        );
        let snapshot = parse_porcelain_v2(output);
        assert_eq!(snapshot.untracked, 1);
        assert!(
            !snapshot.is_dirty(),
            "untracked files are excluded from the dirty boolean"
        );
    }

    // --- Gate split (spec §4.4) ---

    #[test]
    fn log_probe_gate_splits_ref_facts_from_dirtiness() {
        let refs = RefState {
            head: Some(SystemTime::UNIX_EPOCH),
            logs_head: Some(SystemTime::UNIX_EPOCH),
            packed_refs: None,
        };
        // First probe always runs the log probe.
        assert!(needs_log_probe(None, &refs, None, Some("aaa")));
        // Unchanged refs + same OID: skip (dirtiness alone never
        // triggers the log probe — the status probe already ran).
        assert!(!needs_log_probe(
            Some(&refs),
            &refs,
            Some("aaa"),
            Some("aaa")
        ));
        // Ref mtime moved: probe.
        let moved = RefState {
            logs_head: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(5)),
            ..refs.clone()
        };
        assert!(needs_log_probe(
            Some(&refs),
            &moved,
            Some("aaa"),
            Some("aaa")
        ));
        // OID mismatch with identical mtimes (coarse filesystem): probe.
        assert!(needs_log_probe(
            Some(&refs),
            &refs,
            Some("aaa"),
            Some("bbb")
        ));
    }

    #[test]
    fn probe_pacing_honors_poll_and_min_spawn() {
        let poll = Duration::from_secs(30);
        let min_spawn = Duration::from_secs(5);
        let t0 = Instant::now();
        // First probe: always.
        assert!(should_probe(t0, None, poll, min_spawn, false));
        // 10 s later, same project: below poll cadence → no probe.
        assert!(!should_probe(
            t0 + Duration::from_secs(10),
            Some(t0),
            poll,
            min_spawn,
            false
        ));
        // 10 s later, project switched: past min_spawn → immediate probe.
        assert!(should_probe(
            t0 + Duration::from_secs(10),
            Some(t0),
            poll,
            min_spawn,
            true
        ));
        // 3 s later, project switched: min_spawn still guards the spawn.
        assert!(!should_probe(
            t0 + Duration::from_secs(3),
            Some(t0),
            poll,
            min_spawn,
            true
        ));
        // Past the poll interval: probe regardless.
        assert!(should_probe(
            t0 + Duration::from_secs(31),
            Some(t0),
            poll,
            min_spawn,
            false
        ));
    }

    // --- Gitdir resolution (worktrees) ---

    #[test]
    fn gitdir_file_pointer_resolves_relative_and_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("wt");
        std::fs::create_dir_all(&root).unwrap();
        // Relative pointer.
        std::fs::write(root.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();
        assert_eq!(
            resolve_gitdir(&root),
            Some(root.join("../repo/.git/worktrees/wt"))
        );
        // Absolute pointer.
        let absolute = tmp.path().join("elsewhere").join(".git");
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", absolute.display()),
        )
        .unwrap();
        assert_eq!(resolve_gitdir(&root), Some(absolute));
        // Plain directory.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(plain.join(".git")).unwrap();
        assert_eq!(resolve_gitdir(&plain), Some(plain.join(".git")));
        // Not a repo at all.
        let bare_dir = tmp.path().join("norepo");
        std::fs::create_dir_all(&bare_dir).unwrap();
        assert_eq!(resolve_gitdir(&bare_dir), None);
    }

    // --- Watch-target gating (zones + status, spec §4.4) ---

    #[test]
    fn watch_target_requires_participating_status_root_and_zone() {
        // No project at all.
        assert_eq!(watch_target(None), None);
        // Confirmed + configured participate.
        let confirmed = current("proj", ProjectStatus::Confirmed, None);
        let target = watch_target(Some(&confirmed)).expect("confirmed watches");
        assert_eq!(target.project_id, "proj");
        assert_eq!(target.sensitivity, EventSensitivity::CloudAllowed);
        assert!(watch_target(Some(&current("p2", ProjectStatus::Configured, None))).is_some());
        // Discovered candidates are never collected from (defense in
        // depth — they cannot win resolution either).
        assert_eq!(
            watch_target(Some(&current("p3", ProjectStatus::Discovered, None))),
            None
        );
        // never_observe zone: nothing runs.
        assert_eq!(
            watch_target(Some(&current(
                "p4",
                ProjectStatus::Confirmed,
                Some(Zone::NeverObserve)
            ))),
            None
        );
        // local_only zone: collected, but events tagged LocalOnly.
        let local = watch_target(Some(&current(
            "p5",
            ProjectStatus::Confirmed,
            Some(Zone::LocalOnly),
        )))
        .expect("local_only collects");
        assert_eq!(local.sensitivity, EventSensitivity::LocalOnly);
        // Missing root: nothing to probe.
        let mut rootless = current("p6", ProjectStatus::Confirmed, None);
        rootless.root_path = None;
        assert_eq!(watch_target(Some(&rootless)), None);
    }

    // --- Change-only event derivation ---

    #[test]
    fn commit_event_uses_stable_template_and_short_oid() {
        let previous = facts(Some("main"), Some("aaaa111122223333"), 0, 0, 0);
        let mut next = facts(Some("main"), Some("bbbb444455556666"), 0, 0, 0);
        next.last_commit_subject = Some("fix the frobnicator".to_string());
        let events = git_events(
            "proj",
            EventSensitivity::CloudAllowed,
            &previous,
            &next,
            Utc::now(),
        );
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.event_type, EventType::Commit);
        assert!(event.event_type.valid_for(event.source));
        assert_eq!(event.source, EventSource::Git);
        assert_eq!(event.summary, "commit bbbb444 fix the frobnicator");
        assert_eq!(event.project_id.as_deref(), Some("proj"));
        assert_eq!(event.application, "git");
        assert_eq!(event.confidence, 1.0);
    }

    #[test]
    fn branch_switch_suppresses_the_commit_event() {
        // A checkout moves the HEAD OID too — one event per cause.
        let previous = facts(Some("main"), Some("aaaa111122223333"), 0, 0, 0);
        let next = facts(Some("feat/x"), Some("bbbb444455556666"), 0, 0, 0);
        let events = git_events(
            "proj",
            EventSensitivity::CloudAllowed,
            &previous,
            &next,
            Utc::now(),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::BranchSwitch);
        assert_eq!(events[0].summary, "branch main → feat/x");
    }

    #[test]
    fn detached_checkout_reports_detached_endpoint() {
        let previous = facts(Some("main"), Some("aaaa111122223333"), 0, 0, 0);
        let next = facts(None, Some("aaaa111122223333"), 0, 0, 0);
        let events = git_events(
            "proj",
            EventSensitivity::CloudAllowed,
            &previous,
            &next,
            Utc::now(),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::BranchSwitch);
        assert_eq!(events[0].summary, "branch main → (detached)");
    }

    #[test]
    fn first_commit_in_empty_repo_is_a_commit_not_a_switch() {
        // Unborn (branch None, oid None) → first commit: the branch
        // "changed" (None → main) but the transition is a commit.
        let previous = facts(None, None, 0, 0, 0);
        let mut next = facts(Some("main"), Some("cccc777788889999"), 0, 0, 0);
        next.last_commit_subject = Some("initial commit".to_string());
        let events = git_events(
            "proj",
            EventSensitivity::CloudAllowed,
            &previous,
            &next,
            Utc::now(),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Commit);
        assert_eq!(events[0].summary, "commit cccc777 initial commit");
    }

    #[test]
    fn conflict_fires_only_on_zero_to_nonzero() {
        let clean = facts(Some("main"), Some("aaa1111"), 0, 0, 0);
        let conflicted = facts(Some("main"), Some("aaa1111"), 0, 0, 3);
        let events = git_events(
            "proj",
            EventSensitivity::CloudAllowed,
            &clean,
            &conflicted,
            Utc::now(),
        );
        let conflict_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == EventType::Conflict)
            .collect();
        assert_eq!(conflict_events.len(), 1);
        assert_eq!(conflict_events[0].summary, "conflict: 3 unmerged");
        // More conflicts while already conflicted: no new conflict event.
        let worse = facts(Some("main"), Some("aaa1111"), 0, 0, 5);
        let events = git_events(
            "proj",
            EventSensitivity::CloudAllowed,
            &conflicted,
            &worse,
            Utc::now(),
        );
        assert!(events.iter().all(|e| e.event_type != EventType::Conflict));
        // Resolution (N → 0) emits no conflict event either.
        let events = git_events(
            "proj",
            EventSensitivity::CloudAllowed,
            &conflicted,
            &clean,
            Utc::now(),
        );
        assert!(events.iter().all(|e| e.event_type != EventType::Conflict));
    }

    #[test]
    fn dirty_change_is_boolean_hysteresis_not_count_tracking() {
        let clean = facts(Some("main"), Some("aaa1111"), 0, 0, 0);
        let dirty1 = facts(Some("main"), Some("aaa1111"), 1, 2, 0);
        let dirty2 = facts(Some("main"), Some("aaa1111"), 1, 7, 0);
        // clean → dirty: one event.
        let events = git_events(
            "proj",
            EventSensitivity::CloudAllowed,
            &clean,
            &dirty1,
            Utc::now(),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::DirtyChange);
        assert_eq!(events[0].summary, "worktree dirty (1 staged, 2 modified)");
        // dirty → dirtier: count changed, boolean did not — NO event.
        let events = git_events(
            "proj",
            EventSensitivity::CloudAllowed,
            &dirty1,
            &dirty2,
            Utc::now(),
        );
        assert!(events.is_empty());
        // dirty → clean: one event.
        let events = git_events(
            "proj",
            EventSensitivity::CloudAllowed,
            &dirty2,
            &clean,
            Utc::now(),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "worktree clean");
    }

    #[test]
    fn local_only_zone_tags_every_event_local_only() {
        let previous = facts(Some("main"), Some("aaa1111"), 0, 0, 0);
        let next = facts(Some("feat/x"), Some("bbb2222"), 1, 1, 0);
        let events = git_events(
            "proj",
            EventSensitivity::LocalOnly,
            &previous,
            &next,
            Utc::now(),
        );
        assert!(!events.is_empty());
        assert!(events
            .iter()
            .all(|e| e.sensitivity == EventSensitivity::LocalOnly));
    }

    // --- Privacy: subject scrubbed, OID exempt (spec §4.1) ---

    #[test]
    fn commit_subject_is_scrubbed_and_oid_survives() {
        let privacy = filter();
        let oid = "4c5b3f1a9d8e7c6b5a4f3e2d1c0b9a8f7e6d5c4b";
        let snapshot = GitStatusSnapshot {
            branch: Some("main".to_string()),
            head_oid: Some(oid.to_string()),
            ..GitStatusSnapshot::default()
        };
        let facts = assemble_facts(
            &privacy,
            &snapshot,
            Some("add key sk-live1234567890abcdef to config"),
        );
        // Free-text subject: scrubbed.
        let subject = facts.last_commit_subject.expect("subject present");
        assert!(
            !subject.contains("sk-live"),
            "secret must be scrubbed: {subject}"
        );
        assert!(subject.contains("[REDACTED]"));
        // Structured OID: exempt by construction — byte-identical. This
        // matters because the A1 entropy rule *would* redact a bare
        // 40-hex OID in free text (documented, intended); the structured
        // path is the only reason the id survives.
        assert_eq!(facts.last_commit_id.as_deref(), Some(oid));
        assert_ne!(
            privacy.scrub_text(oid),
            oid,
            "free-text scrub redacts bare OIDs — the structured exemption is load-bearing"
        );
        // The 7-char short OID in event summaries is below the entropy
        // threshold and survives free text.
        assert_eq!(privacy.scrub_text(short_oid(oid)), short_oid(oid));
    }

    #[test]
    fn assemble_facts_without_commit_has_no_subject() {
        let privacy = filter();
        let snapshot = GitStatusSnapshot::default(); // zero-commit repo
        let facts = assemble_facts(&privacy, &snapshot, Some("stale cached subject"));
        assert_eq!(facts.last_commit_id, None);
        assert_eq!(
            facts.last_commit_subject, None,
            "no commit → no subject, even with a stale cache"
        );
    }

    // --- Disabled-with-reason state machine (spec §4.4 failure modes) ---

    #[tokio::test]
    async fn toggle_off_parks_disabled_without_spawning() {
        let toggles = ObservationToggles {
            git: false,
            ..ObservationToggles::default()
        };
        let watcher =
            GitWatcher::new(GitContextConfig::default()).with_privacy(Arc::new(filter()), toggles);
        let health = watcher.health_handle();
        assert!(watcher.is_healthy());
        assert!(!watcher.should_restart());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { watcher.run(shutdown_rx, None).await });
        // The disabled state is entered synchronously before the park.
        tokio::time::sleep(Duration::from_millis(50)).await;
        {
            let snapshot = health.read().clone();
            assert!(!snapshot.enabled);
            assert_eq!(
                snapshot.disabled_reason.as_deref(),
                Some("disabled by [privacy.toggles]")
            );
            assert_eq!(snapshot.probes, 0, "no git subprocess may have run");
        }
        shutdown_tx.send(true).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("watcher must park and respond to shutdown")
            .expect("no panic");
    }

    #[tokio::test]
    async fn config_disabled_parks_with_reason() {
        let watcher = GitWatcher::new(GitContextConfig {
            enabled: false,
            ..GitContextConfig::default()
        });
        let health = watcher.health_handle();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { watcher.run(shutdown_rx, None).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            health.read().disabled_reason.as_deref(),
            Some("disabled by [git_context].enabled")
        );
        shutdown_tx.send(true).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("shutdown")
            .expect("no panic");
    }

    // --- Integration against a real repo (git is available on dev/CI
    // machines; each test builds a tiny temp repo) ---

    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn git_available_sync() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn probe_cycle_against_real_repo() {
        if !git_available_sync() {
            eprintln!("git not available; skipping integration test");
            return;
        }
        let timeout = Duration::from_secs(10);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(git(root, &["init", "-q"]));

        // Zero-commit repo: branch None, oid None, no error (spec §4.4).
        let snapshot = probe_repo_status(root, timeout).await.expect("status");
        assert_eq!(snapshot.branch, None);
        assert_eq!(snapshot.head_oid, None);
        assert_eq!(
            probe_last_commit(root, timeout).await.expect("log"),
            None,
            "unborn HEAD is a normal state, not an error"
        );

        // Untracked file: counted, not dirty.
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        let snapshot = probe_repo_status(root, timeout).await.expect("status");
        assert_eq!(snapshot.untracked, 1);
        assert!(!snapshot.is_dirty());

        // Stage + commit: branch born, OID present, subject readable.
        assert!(git(root, &["add", "a.txt"]));
        assert!(git(
            root,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "first commit"
            ]
        ));
        let snapshot = probe_repo_status(root, timeout).await.expect("status");
        assert!(snapshot.branch.is_some(), "born branch must be reported");
        let oid = snapshot.head_oid.clone().expect("oid after commit");
        let (log_oid, subject) = probe_last_commit(root, timeout)
            .await
            .expect("log")
            .expect("commit present");
        assert_eq!(log_oid, oid, "status OID and log OID must agree");
        assert_eq!(subject, "first commit");
        assert!(!snapshot.is_dirty());

        // Modify the tracked file: dirty without any ref change.
        std::fs::write(root.join("a.txt"), "changed").unwrap();
        let snapshot = probe_repo_status(root, timeout).await.expect("status");
        assert_eq!(snapshot.dirty, 1);
        assert!(snapshot.is_dirty());
    }

    #[tokio::test]
    async fn ref_state_changes_after_commit_and_not_after_edit() {
        if !git_available_sync() {
            eprintln!("git not available; skipping integration test");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(git(root, &["init", "-q"]));
        std::fs::write(root.join("a.txt"), "one").unwrap();
        assert!(git(root, &["add", "a.txt"]));
        assert!(git(
            root,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "one"
            ]
        ));
        let gitdir = resolve_gitdir(root).expect("gitdir");
        let before = read_ref_state(&gitdir);
        assert!(before.head.is_some());

        // Editing a working-tree file changes NO ref mtimes — this is the
        // gate split: dirtiness must be picked up by the always-running
        // status probe, never by the ref gate.
        std::fs::write(root.join("a.txt"), "two").unwrap();
        assert_eq!(read_ref_state(&gitdir), before);

        // A commit moves the reflog (and branch ref).
        assert!(git(root, &["add", "a.txt"]));
        assert!(git(
            root,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "two"
            ]
        ));
        let after = read_ref_state(&gitdir);
        assert_ne!(after, before, "commit must move ref mtimes");
    }
}
