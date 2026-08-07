//! # Project resolver (context engine spec §4.3)
//!
//! The **single source of truth for "current project"**. The frame loop owns
//! one [`ProjectResolver`] and calls [`ProjectResolver::observe`] once per
//! perception frame; the post-hysteresis output feeds the curator's
//! `ActivitySignal`, semantic-fact prefixes, skills matching, and (via the
//! shared [`CurrentProjectHandle`]) the `ProjectWorldState` projection.
//!
//! Resolution runs the spec §4.3 strength order per frame:
//!
//! 0. **User override rules** (`{match_process, match_title_substring,
//!    action: force_project | exclude_project}`) — persisted corrections
//!    from the Context page (Plan C seam).
//! 1. **Title-path match** — a configured/confirmed `root_path` visible in
//!    the window title.
//! 2. **Editor title patterns** — `"file — folder — app"` shapes (em/en
//!    dash and `" - "` hyphen variants; VS Code and JetBrains layouts).
//! 3. **Git root of the most recent file event** — the caller passes the
//!    path (Task A7 seam: the file watcher supplies it; `None` until then).
//! 4. **Keyword match** — legacy config-driven substring matching.
//!
//! The winner only becomes the *public* current project after hysteresis: a
//! **different** project must win continuously for `switch_min_secs`
//! (default 20) before the current flips. Exclusion rules remove a project
//! from candidacy before ranking. **Discovered-but-unconfirmed candidates
//! never participate in resolution** — they exist only as proposal rows
//! (spec §4.3 "unconfirmed candidates are never collected from").
//!
//! ## Auto-discovery (non-circular)
//!
//! Absolute paths observed in window titles accumulate dwell per folder;
//! once a folder has been visible ≥ `discover_min_secs` (default 30) it is
//! validated (directory exists + [`find_git_root`]) and proposed exactly
//! once as a candidate with a slugified id (`-2`/`-3` suffix on collision).
//! The runtime persists proposals to the Projects table (raw-log DB); the
//! Context page confirms them (Plan C, Task C5 seam).
//!
//! ## Gating
//!
//! This module is **ungated** (pure logic + `std::fs` probes) so all three
//! processes link it. Projects-table persistence lives in
//! `memory::raw_log` behind the `runtime` feature; the perception bin runs
//! the resolver purely in-memory.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::config::{ProjectConfigEntry, ProjectsConfig};
use crate::senses::privacy::Zone;

/// Maximum length of a project id slug (spec §4.3: `[a-z0-9-]{1,64}`).
pub const PROJECT_SLUG_MAX_LEN: usize = 64;

/// Maximum seconds credited between two sightings of the same discovery
/// folder — keeps a folder that reappears after hours from instantly
/// crossing `discover_min_secs`.
const DISCOVERY_MAX_GAP_SECS: i64 = 10;

/// Hard cap on remembered "already proposed or rejected" discovery paths.
///
/// The set exists only to stop the same folder being re-validated on every
/// frame; it is not durable state. Without a cap it grows for the process's
/// whole lifetime — one entry per distinct path ever seen in a window
/// title, which on a browsing-heavy day is unbounded. Oldest entries are
/// dropped first; re-learning an evicted path costs one `is_dir` check.
const DISCOVERY_RESOLVED_CAP: usize = 2048;

/// Multiplier on `discover_min_secs` after which an un-promoted
/// accumulation entry is forgotten. A folder seen once and never again
/// must not keep its partial dwell (or its memory) forever.
const DISCOVERY_ACCUM_STALE_FACTOR: i64 = 2;

/// Floor for the accumulation eviction window, so a tiny (or zero)
/// `discover_min_secs` cannot evict entries the same second they appear.
const DISCOVERY_ACCUM_MIN_STALE_SECS: i64 = 60;

// ---------------------------------------------------------------------------
// Slugs
// ---------------------------------------------------------------------------

/// Returns `true` when `id` is a valid project id slug: lowercase
/// `[a-z0-9-]`, 1–64 chars, no dots (spec §4.3 — ids feed semantic-fact
/// prefixes, dedupe keys, and vault relations).
pub fn is_valid_project_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= PROJECT_SLUG_MAX_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Slugifies a free-form name into the project id format: lowercase,
/// non-alphanumeric runs collapse to single `-`, trimmed, max 64 chars.
/// Returns `"project"` when nothing survives (never an empty slug).
pub fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_dash = true; // suppress leading dashes
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug.truncate(PROJECT_SLUG_MAX_LEN);
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "project".to_string()
    } else {
        slug
    }
}

/// Slugifies `name` and appends `-2`, `-3`, … until the id is not in
/// `existing` (spec §4.3 discovery collision suffix). The suffix always
/// fits within [`PROJECT_SLUG_MAX_LEN`].
pub fn unique_slug(name: &str, existing: &HashSet<String>) -> String {
    let base = slugify(name);
    if !existing.contains(&base) {
        return base;
    }
    for n in 2u32.. {
        let suffix = format!("-{n}");
        let mut candidate = base.clone();
        candidate.truncate(PROJECT_SLUG_MAX_LEN - suffix.len());
        while candidate.ends_with('-') {
            candidate.pop();
        }
        candidate.push_str(&suffix);
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 suffix space exhausted");
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Lifecycle status of a project row (spec §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    /// Seeded from `[[projects.known]]` config (config wins by id at boot).
    Configured,
    /// Auto-discovery proposal — **never participates in resolution** and
    /// is never collected from until confirmed.
    Discovered,
    /// A discovered candidate the user confirmed (Context page / intent).
    Confirmed,
}

impl ProjectStatus {
    /// Stable snake_case string for DB storage.
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectStatus::Configured => "configured",
            ProjectStatus::Discovered => "discovered",
            ProjectStatus::Confirmed => "confirmed",
        }
    }

    /// Parses the DB string form; unknown strings map to `Discovered`
    /// (the least-privileged status — never silently promotes a row into
    /// resolution/collection).
    pub fn parse(s: &str) -> Self {
        match s {
            "configured" => ProjectStatus::Configured,
            "confirmed" => ProjectStatus::Confirmed,
            _ => ProjectStatus::Discovered,
        }
    }
}

/// One project known to the resolver (config mirror or discovery row).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// Slug id (`[a-z0-9-]{1,64}`).
    pub id: String,
    /// Human-readable name (defaults to the id).
    pub name: String,
    /// Absolute root directories of this project.
    pub root_paths: Vec<String>,
    /// Optional remote repo URL (informational).
    pub repo: Option<String>,
    /// Title keywords for tier-4 matching (case-insensitive substring).
    pub keywords: Vec<String>,
    /// Optional per-project privacy zone (spec §4.3 project zones) applied
    /// to the project's git/file events and title matches by the
    /// collectors and cloud gate (Tasks A5/A7 seam).
    pub zone: Option<Zone>,
    /// Lifecycle status; `Discovered` rows never resolve.
    pub status: ProjectStatus,
}

impl ProjectEntry {
    /// Whether this project may win resolution (spec §4.3: discovered
    /// candidates do **not** participate — proposals only).
    pub fn participates(&self) -> bool {
        self.status != ProjectStatus::Discovered
    }

    /// Builds a resolver entry from a `[[projects.known]]` config entry.
    /// Invalid ids are slugified (with a warning) rather than dropped —
    /// user config must keep working while converging on the id format.
    pub fn from_config(entry: &ProjectConfigEntry) -> Self {
        let id = if is_valid_project_id(&entry.id) {
            entry.id.clone()
        } else {
            let fixed = slugify(&entry.id);
            tracing::warn!(
                layer = "context",
                component = "project",
                configured_id = %entry.id,
                slug = %fixed,
                "[[projects.known]] id is not a valid slug ([a-z0-9-]{{1,64}}); using slugified form"
            );
            fixed
        };
        let name = if entry.name.trim().is_empty() {
            id.clone()
        } else {
            entry.name.clone()
        };
        Self {
            id,
            name,
            root_paths: entry.root_paths.clone(),
            repo: entry.repo.clone(),
            keywords: entry.keywords.clone(),
            zone: entry.zone,
            status: ProjectStatus::Configured,
        }
    }
}

/// What an override rule does when it matches (spec §4.3 tier 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    /// The matched frame belongs to `project_id`, full stop.
    ForceProject,
    /// The matched frame is *not* `project_id` — remove it from candidacy
    /// before ranking.
    ExcludeProject,
}

impl RuleAction {
    /// Stable snake_case string for DB storage.
    pub fn as_str(self) -> &'static str {
        match self {
            RuleAction::ForceProject => "force_project",
            RuleAction::ExcludeProject => "exclude_project",
        }
    }

    /// Parses the DB string form.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "force_project" => Some(RuleAction::ForceProject),
            "exclude_project" => Some(RuleAction::ExcludeProject),
            _ => None,
        }
    }
}

/// A persisted user override rule (tier 0, spec §4.3): matching semantics
/// mirror privacy zone rules — every *present* criterion must match
/// (logical AND); a rule with neither criterion never matches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverrideRule {
    /// Process name to match (case-insensitive equality).
    pub match_process: Option<String>,
    /// Title fragment to match (case-insensitive substring).
    pub match_title_substring: Option<String>,
    /// Force or exclude.
    pub action: RuleAction,
    /// The project this rule forces or excludes.
    pub project_id: String,
}

impl OverrideRule {
    /// Returns `true` when the rule matches the given frame endpoint.
    pub fn matches(&self, process_name: &str, window_title: &str) -> bool {
        if self.match_process.is_none() && self.match_title_substring.is_none() {
            return false;
        }
        if let Some(process) = &self.match_process {
            if !process.eq_ignore_ascii_case(process_name) {
                return false;
            }
        }
        if let Some(fragment) = &self.match_title_substring {
            let title = window_title.to_ascii_lowercase();
            if !title.contains(&fragment.to_ascii_lowercase()) {
                return false;
            }
        }
        true
    }
}

/// One pre-hysteresis resolution: which project won, how confidently, and
/// from which tier (spec §4.3 result `{project_id, confidence}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    /// Winning project id.
    pub project_id: String,
    /// Confidence in \[0.0, 1.0\], fixed per tier (override 1.0,
    /// title-path 0.9, editor pattern 0.8, git-root 0.7, keyword 0.5).
    pub confidence: f32,
    /// Which tier produced the match (0–4).
    pub source_tier: u8,
}

/// The public, post-hysteresis current project — what every consumer
/// (ActivitySignal, semantic prefixes, skills, `ProjectWorldState`,
/// event `project_id` stamping) receives.
#[derive(Debug, Clone, PartialEq)]
pub struct CurrentProject {
    /// Project id slug.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// First configured root path, when known (raw, unscrubbed — consumers
    /// that publish it must path-scrub at emit, spec §4.1).
    pub root_path: Option<PathBuf>,
    /// Confidence of the latest winning resolution.
    pub confidence: f32,
    /// Tier of the latest winning resolution.
    pub source_tier: u8,
    /// Per-project privacy zone (spec §4.3 project zones); `None` means
    /// the `cloud_allowed` default. The git collector (Task A5) and file
    /// watcher enforce it on their events.
    pub zone: Option<Zone>,
    /// Lifecycle status of the winning entry. Always a *participating*
    /// status (`Configured` or `Confirmed`) by construction — discovered
    /// candidates never win resolution — but carried explicitly so
    /// collectors can assert the spec §4.4 "confirmed only" rule without
    /// reaching into the resolver.
    pub status: ProjectStatus,
}

/// Shared read handle onto the resolver's current project. The resolver
/// updates it on every [`ProjectResolver::observe`]; the context watcher
/// reads it for `ProjectWorldState` (fixing the old `current_dir()`
/// derivation), and the Task A5 git collector will read it for the active
/// repo.
pub type CurrentProjectHandle = Arc<RwLock<Option<CurrentProject>>>;

/// Per-frame input to the resolver — the sanitized frame facts it needs.
#[derive(Debug, Clone, Copy)]
pub struct FrameInput<'a> {
    /// Sanitized foreground process name.
    pub process_name: &'a str,
    /// Sanitized/scrubbed foreground window title.
    pub window_title: &'a str,
    /// Path of the most recent file event (**Task A7 seam** — the file
    /// watcher supplies this; `None` until it lands, which skips tier 3).
    pub recent_file_path: Option<&'a Path>,
    /// Frame timestamp (drives hysteresis and discovery accumulation).
    pub ts: DateTime<Utc>,
}

/// A post-hysteresis flip of the public current project.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSwitch {
    /// Previous current project id (`None` at first adoption).
    pub from: Option<String>,
    /// New current project id.
    pub to: String,
    /// When the flip happened (the frame that crossed `switch_min_secs`).
    pub ts: DateTime<Utc>,
}

/// A validated auto-discovery proposal (spec §4.3). The runtime persists
/// it as a `status = discovered` Projects-table row; it never resolves
/// until confirmed.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredCandidate {
    /// Proposed slug id (folder-name slug, `-2`/`-3` on collision).
    pub id: String,
    /// Folder basename as the proposed display name.
    pub name: String,
    /// Validated root: the folder's git root when found, else the folder.
    pub root_path: String,
}

/// Result of one [`ProjectResolver::observe`] call.
#[derive(Debug, Clone, Default)]
pub struct ObserveOutcome {
    /// The public current project after hysteresis (unchanged unless
    /// `switched` is set).
    pub current: Option<CurrentProject>,
    /// Set when this frame flipped the current project.
    pub switched: Option<ProjectSwitch>,
    /// New discovery proposals validated this frame (empty when
    /// auto-discovery is off).
    pub discovered: Vec<DiscoveredCandidate>,
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// Per-folder discovery accumulation state.
///
/// Both collections are bounded (M1): `accum` entries age out once a
/// folder has not been seen for [`DISCOVERY_ACCUM_STALE_FACTOR`] ×
/// `discover_min_secs`, and `resolved` is capped at
/// [`DISCOVERY_RESOLVED_CAP`] with oldest-first eviction. This is a
/// long-lived object in a process that runs for days.
#[derive(Debug, Default)]
struct DiscoveryState {
    /// normalized path → (original path, accumulated secs, last seen).
    accum: HashMap<String, (PathBuf, i64, DateTime<Utc>)>,
    /// Normalized paths already proposed or rejected — never re-validated
    /// while remembered.
    resolved: HashSet<String>,
    /// Insertion order of `resolved`, for oldest-first eviction.
    resolved_order: VecDeque<String>,
}

impl DiscoveryState {
    /// Remembers a path as resolved, evicting the oldest entries once the
    /// cap is reached.
    fn mark_resolved(&mut self, key: String) {
        if !self.resolved.insert(key.clone()) {
            return;
        }
        self.resolved_order.push_back(key);
        while self.resolved_order.len() > DISCOVERY_RESOLVED_CAP {
            if let Some(oldest) = self.resolved_order.pop_front() {
                self.resolved.remove(&oldest);
            }
        }
    }

    /// Drops accumulation entries whose folder has not been seen for
    /// `stale_after` — they will never reach the threshold, and keeping
    /// them leaks one map entry per folder ever glimpsed in a title.
    fn evict_stale_accum(&mut self, now: DateTime<Utc>, stale_after: chrono::Duration) {
        self.accum
            .retain(|_, (_, _, last_seen)| now - *last_seen <= stale_after);
    }
}

/// The project resolver (spec §4.3). Owned by the frame loop; shared via
/// `Arc<RwLock<…>>` where other tasks need to mutate (rules from Plan C
/// intents) and via [`CurrentProjectHandle`] where they only read.
#[derive(Debug)]
pub struct ProjectResolver {
    projects: Vec<ProjectEntry>,
    rules: Vec<OverrideRule>,
    switch_min_secs: u64,
    discover_min_secs: u64,
    auto_discover: bool,
    current: Option<Resolution>,
    /// Challenger project + when it started winning continuously.
    pending: Option<(String, DateTime<Utc>)>,
    discovery: DiscoveryState,
    handle: CurrentProjectHandle,
}

impl ProjectResolver {
    /// Creates a resolver over the given projects and override rules with
    /// the `[projects]` knobs.
    pub fn new(
        projects: Vec<ProjectEntry>,
        rules: Vec<OverrideRule>,
        cfg: &ProjectsConfig,
    ) -> Self {
        Self {
            projects,
            rules,
            switch_min_secs: cfg.switch_min_secs,
            discover_min_secs: cfg.discover_min_secs,
            auto_discover: cfg.auto_discover,
            current: None,
            pending: None,
            discovery: DiscoveryState::default(),
            handle: Arc::new(RwLock::new(None)),
        }
    }

    /// Creates an in-memory resolver from config alone (no table rows, no
    /// persisted rules) — the perception bin's wiring.
    pub fn from_config(cfg: &ProjectsConfig) -> Self {
        let projects = cfg.known.iter().map(ProjectEntry::from_config).collect();
        Self::new(projects, Vec::new(), cfg)
    }

    /// Shared read handle onto the public current project.
    pub fn handle(&self) -> CurrentProjectHandle {
        self.handle.clone()
    }

    /// The public current project (post-hysteresis).
    pub fn current(&self) -> Option<CurrentProject> {
        self.handle.read().clone()
    }

    /// Every project the resolver knows (all statuses). Task A5 seam: the
    /// git collector filters this to confirmed/configured roots.
    pub fn projects(&self) -> &[ProjectEntry] {
        &self.projects
    }

    /// Ids of every known project (all statuses) — the discovery slug
    /// collision set.
    fn known_ids(&self) -> HashSet<String> {
        self.projects.iter().map(|p| p.id.clone()).collect()
    }

    /// Inserts or replaces a project by id (Task C5 seam: confirm/add
    /// intents mutate the live resolver alongside the table).
    pub fn upsert_project(&mut self, entry: ProjectEntry) {
        match self.projects.iter_mut().find(|p| p.id == entry.id) {
            Some(existing) => *existing = entry,
            None => self.projects.push(entry),
        }
    }

    /// Marks a discovered candidate confirmed so it starts resolving
    /// (Task C5 seam).
    pub fn confirm_project(&mut self, id: &str) -> bool {
        match self.projects.iter_mut().find(|p| p.id == id) {
            Some(entry) => {
                entry.status = ProjectStatus::Confirmed;
                true
            }
            None => false,
        }
    }

    /// Adds a persisted override rule (tier 0) to the live resolver.
    pub fn add_rule(&mut self, rule: OverrideRule) {
        self.rules.push(rule);
    }

    /// Pure per-frame resolution — the spec §4.3 strength order, **no
    /// hysteresis**. Exclusion rules remove projects from candidacy first;
    /// discovered candidates never participate.
    pub fn resolve_candidate(&self, input: &FrameInput<'_>) -> Option<Resolution> {
        let excluded: HashSet<&str> = self
            .rules
            .iter()
            .filter(|r| {
                r.action == RuleAction::ExcludeProject
                    && r.matches(input.process_name, input.window_title)
            })
            .map(|r| r.project_id.as_str())
            .collect();
        let eligible = |p: &&ProjectEntry| p.participates() && !excluded.contains(p.id.as_str());

        // Tier 0: user override rules (force).
        for rule in &self.rules {
            if rule.action == RuleAction::ForceProject
                && rule.matches(input.process_name, input.window_title)
                && self
                    .projects
                    .iter()
                    .filter(eligible)
                    .any(|p| p.id == rule.project_id)
            {
                return Some(Resolution {
                    project_id: rule.project_id.clone(),
                    confidence: 1.0,
                    source_tier: 0,
                });
            }
        }

        let title_norm = normalize_path_str(input.window_title);

        // Tier 1: a configured/confirmed root path visible in the title.
        for project in self.projects.iter().filter(eligible) {
            for root in &project.root_paths {
                let root_norm = normalize_path_str(root);
                if root_norm.len() >= 2 && title_norm.contains(&root_norm) {
                    return Some(Resolution {
                        project_id: project.id.clone(),
                        confidence: 0.9,
                        source_tier: 1,
                    });
                }
            }
        }

        // Tier 2: editor title patterns — a segment equal to the project
        // name, id, or a root-path basename.
        let segments = editor_title_segments(input.window_title);
        if segments.len() >= 2 {
            for segment in &segments {
                for project in self.projects.iter().filter(eligible) {
                    if segment.eq_ignore_ascii_case(&project.name)
                        || segment.eq_ignore_ascii_case(&project.id)
                        || project.root_paths.iter().any(|root| {
                            path_basename(root)
                                .map(|base| segment.eq_ignore_ascii_case(base))
                                .unwrap_or(false)
                        })
                    {
                        return Some(Resolution {
                            project_id: project.id.clone(),
                            confidence: 0.8,
                            source_tier: 2,
                        });
                    }
                }
            }
        }

        // Tier 3: git root of the most recent file event (Task A7 seam).
        if let Some(file_path) = input.recent_file_path {
            let file_norm = normalize_path_str(&file_path.to_string_lossy());
            let git_root =
                find_git_root(file_path).map(|root| normalize_path_str(&root.to_string_lossy()));
            for project in self.projects.iter().filter(eligible) {
                for root in &project.root_paths {
                    let root_norm = normalize_path_str(root);
                    if root_norm.is_empty() {
                        continue;
                    }
                    let root_matches_git = git_root.as_deref() == Some(root_norm.as_str());
                    let file_under_root =
                        file_norm == root_norm || file_norm.starts_with(&format!("{root_norm}/"));
                    if root_matches_git || file_under_root {
                        return Some(Resolution {
                            project_id: project.id.clone(),
                            confidence: 0.7,
                            source_tier: 3,
                        });
                    }
                }
            }
        }

        // Tier 4: keyword match (legacy behavior, config-driven).
        let title_lower = input.window_title.to_ascii_lowercase();
        for project in self.projects.iter().filter(eligible) {
            if project
                .keywords
                .iter()
                .any(|kw| !kw.is_empty() && title_lower.contains(&kw.to_ascii_lowercase()))
            {
                return Some(Resolution {
                    project_id: project.id.clone(),
                    confidence: 0.5,
                    source_tier: 4,
                });
            }
        }

        None
    }

    /// Observes one frame: runs resolution, applies hysteresis, updates
    /// the shared handle, and accumulates discovery.
    ///
    /// Hysteresis semantics (spec §4.3, `switch_min_secs`):
    /// - First-ever winner is adopted immediately (there is no incumbent
    ///   to protect; boot should not wait 20 s to know the project).
    /// - A *different* project must win **continuously** for
    ///   `switch_min_secs` before the public current flips; any frame
    ///   where it stops winning resets its challenge.
    /// - A frame with no winner keeps the incumbent (a browser detour
    ///   does not un-set the project) and resets any pending challenger.
    pub fn observe(&mut self, input: &FrameInput<'_>) -> ObserveOutcome {
        let winner = self.resolve_candidate(input);
        let mut switched = None;

        match (&self.current, &winner) {
            (None, Some(w)) => {
                switched = Some(ProjectSwitch {
                    from: None,
                    to: w.project_id.clone(),
                    ts: input.ts,
                });
                self.current = Some(w.clone());
                self.pending = None;
            }
            (Some(c), Some(w)) if c.project_id == w.project_id => {
                // Same project — refresh confidence/tier, drop challengers.
                self.current = Some(w.clone());
                self.pending = None;
            }
            (Some(c), Some(w)) => match &self.pending {
                Some((pending_id, since)) if *pending_id == w.project_id => {
                    let held = (input.ts - *since).num_seconds();
                    if held >= self.switch_min_secs as i64 {
                        switched = Some(ProjectSwitch {
                            from: Some(c.project_id.clone()),
                            to: w.project_id.clone(),
                            ts: input.ts,
                        });
                        self.current = Some(w.clone());
                        self.pending = None;
                    }
                }
                _ => {
                    self.pending = Some((w.project_id.clone(), input.ts));
                }
            },
            (_, None) => {
                // No winner: keep the incumbent, reset challengers.
                self.pending = None;
            }
        }

        let current = self.current_project();
        *self.handle.write() = current.clone();

        let discovered = if self.auto_discover {
            self.accumulate_discovery(input)
        } else {
            Vec::new()
        };

        ObserveOutcome {
            current,
            switched,
            discovered,
        }
    }

    /// Materializes the public [`CurrentProject`] from the internal
    /// resolution + project table.
    fn current_project(&self) -> Option<CurrentProject> {
        let resolution = self.current.as_ref()?;
        let entry = self
            .projects
            .iter()
            .find(|p| p.id == resolution.project_id)?;
        Some(CurrentProject {
            id: entry.id.clone(),
            name: entry.name.clone(),
            root_path: entry.root_paths.first().map(PathBuf::from),
            confidence: resolution.confidence,
            source_tier: resolution.source_tier,
            zone: entry.zone,
            status: entry.status,
        })
    }

    /// Accumulates title-derived folder dwell and proposes validated
    /// candidates (spec §4.3 auto-discovery). Paths already inside a known
    /// project's root are skipped; validation (dir exists +
    /// [`find_git_root`]) runs only once per folder, at the threshold.
    fn accumulate_discovery(&mut self, input: &FrameInput<'_>) -> Vec<DiscoveredCandidate> {
        let mut proposals = Vec::new();
        // M1: forget folders that stopped appearing before they ever
        // crossed the dwell threshold. Runs before the per-path work so a
        // long-idle folder starts from zero rather than resuming.
        self.discovery.evict_stale_accum(
            input.ts,
            chrono::Duration::seconds(
                (self.discover_min_secs as i64 * DISCOVERY_ACCUM_STALE_FACTOR)
                    .max(DISCOVERY_ACCUM_MIN_STALE_SECS),
            ),
        );
        for path in extract_title_paths(input.window_title) {
            let key = normalize_path_str(&path.to_string_lossy());
            if key.is_empty() || self.discovery.resolved.contains(&key) {
                continue;
            }
            // Inside a known root (any status) → tier 1 territory or an
            // existing proposal; not a new candidate.
            let inside_known = self.projects.iter().any(|p| {
                p.root_paths.iter().any(|root| {
                    let root_norm = normalize_path_str(root);
                    !root_norm.is_empty()
                        && (key == root_norm || key.starts_with(&format!("{root_norm}/")))
                })
            });
            if inside_known {
                self.discovery.mark_resolved(key);
                continue;
            }

            let entry = self
                .discovery
                .accum
                .entry(key.clone())
                .or_insert_with(|| (path.clone(), 0, input.ts));
            let gap = (input.ts - entry.2).num_seconds();
            entry.1 += gap.clamp(0, DISCOVERY_MAX_GAP_SECS);
            entry.2 = input.ts;

            if entry.1 < self.discover_min_secs as i64 {
                continue;
            }

            // Threshold crossed — validate exactly once.
            let observed = entry.0.clone();
            self.discovery.accum.remove(&key);
            self.discovery.mark_resolved(key);

            let Some(root) = validate_discovery_dir(&observed) else {
                tracing::debug!(
                    layer = "context",
                    component = "project",
                    path = %observed.display(),
                    "discovery candidate failed validation (missing dir); dropped"
                );
                continue;
            };
            let Some(name) = path_basename(&root.to_string_lossy()).map(str::to_string) else {
                continue;
            };
            let id = unique_slug(&name, &self.known_ids());
            let candidate = DiscoveredCandidate {
                id: id.clone(),
                name: name.clone(),
                root_path: root.to_string_lossy().into_owned(),
            };
            // Track the proposal as a Discovered entry so future slugs
            // collide correctly and repeated sightings are ignored —
            // it never participates in resolution.
            self.projects.push(ProjectEntry {
                id,
                name,
                root_paths: vec![candidate.root_path.clone()],
                repo: None,
                keywords: Vec::new(),
                zone: None,
                status: ProjectStatus::Discovered,
            });
            tracing::info!(
                layer = "context",
                component = "project",
                id = %candidate.id,
                root = %candidate.root_path,
                "auto-discovery proposed a project candidate"
            );
            proposals.push(candidate);
        }
        proposals
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Walks `start` and its ancestors for a `.git` marker (dir or worktree
/// file) and returns the containing directory. Moved here from
/// `senses::context` (Task A4) so the ungated resolver and the runtime
/// watchers share one implementation.
pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Legacy frame-only keyword hint — the backward-compat shim behind
/// `memory::retrieval::infer_project_hint`. Matches the historical
/// hardcoded pairs via the resolver's tier-4 semantics (case-insensitive
/// title substring).
pub fn legacy_keyword_hint(window_title: &str) -> Option<String> {
    const LEGACY: &[(&str, &str)] = &[("continuum", "continuum"), ("simcharts", "simcharts")];
    let title = window_title.to_ascii_lowercase();
    LEGACY
        .iter()
        .find(|(_, keyword)| title.contains(keyword))
        .map(|(id, _)| (*id).to_string())
}

/// Normalizes a path-ish string for comparison: trimmed, backslashes →
/// slashes, no trailing slash, ASCII-lowercased.
fn normalize_path_str(s: &str) -> String {
    let mut n = s.trim().replace('\\', "/").to_ascii_lowercase();
    while n.ends_with('/') && n.len() > 1 {
        n.pop();
    }
    n
}

/// Basename of a path string (last non-empty component), if any.
fn path_basename(path: &str) -> Option<&str> {
    path.trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .filter(|s| !s.is_empty())
}

/// Splits a window title on editor separators: `" — "` (em dash), `" – "`
/// (en dash), and `" - "` (hyphen). Spaced separators only, so hyphenated
/// names like `Continuum-main` survive as one segment. Leading VS Code
/// dirty markers (`●`, `*`) are trimmed.
fn editor_title_segments(title: &str) -> Vec<String> {
    title
        .replace(" — ", " - ")
        .replace(" – ", " - ")
        .split(" - ")
        .map(|s| s.trim().trim_start_matches(['●', '*']).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Extracts absolute (drive-letter) paths from a window title: within each
/// editor segment, the run from the first `X:\` / `X:/` pattern to the
/// segment end. UNC and `~`-scrubbed paths are deliberately not candidates
/// (a scrubbed `~` path cannot be validated on disk).
fn extract_title_paths(title: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for segment in editor_title_segments(title) {
        let bytes = segment.as_bytes();
        let start = (0..bytes.len().saturating_sub(2)).find(|&i| {
            bytes[i].is_ascii_alphabetic()
                && bytes[i + 1] == b':'
                && (bytes[i + 2] == b'\\' || bytes[i + 2] == b'/')
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        });
        if let Some(start) = start {
            let raw = segment[start..].trim_end_matches(['"', '\'', ')', ']', '.', ',']);
            if raw.len() > 3 {
                paths.push(PathBuf::from(raw));
            }
        }
    }
    paths
}

/// Validates a discovery folder: the observed path (or its parent, when
/// the title showed a file) must exist as a directory; the proposal root
/// is its git root when one is found, else the directory itself.
fn validate_discovery_dir(observed: &Path) -> Option<PathBuf> {
    let dir = if observed.is_dir() {
        observed.to_path_buf()
    } else {
        let parent = observed.parent()?;
        if parent.is_dir() {
            parent.to_path_buf()
        } else {
            return None;
        }
    };
    Some(find_git_root(&dir).unwrap_or(dir))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    fn project(id: &str, name: &str, roots: &[&str], keywords: &[&str]) -> ProjectEntry {
        ProjectEntry {
            id: id.to_string(),
            name: name.to_string(),
            root_paths: roots.iter().map(|s| s.to_string()).collect(),
            repo: None,
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            zone: None,
            status: ProjectStatus::Configured,
        }
    }

    fn cfg() -> ProjectsConfig {
        ProjectsConfig::default()
    }

    fn resolver(projects: Vec<ProjectEntry>, rules: Vec<OverrideRule>) -> ProjectResolver {
        ProjectResolver::new(projects, rules, &cfg())
    }

    fn input<'a>(process: &'a str, title: &'a str, at: DateTime<Utc>) -> FrameInput<'a> {
        FrameInput {
            process_name: process,
            window_title: title,
            recent_file_path: None,
            ts: at,
        }
    }

    fn test_projects() -> Vec<ProjectEntry> {
        vec![
            project(
                "continuum",
                "Continuum",
                &["D:\\Continuum\\Continuum-main"],
                &["continuum"],
            ),
            project(
                "simcharts",
                "SimCharts",
                &["D:\\Dev\\simcharts"],
                &["simcharts", "chart editor"],
            ),
        ]
    }

    // --- Slugs ---

    #[test]
    fn slug_validation_enforces_the_spec_format() {
        assert!(is_valid_project_id("continuum"));
        assert!(is_valid_project_id("my-project-2"));
        assert!(is_valid_project_id("a"));
        assert!(!is_valid_project_id(""));
        assert!(!is_valid_project_id("Continuum")); // uppercase
        assert!(!is_valid_project_id("has.dot")); // no dots
        assert!(!is_valid_project_id("has space"));
        assert!(!is_valid_project_id(&"x".repeat(65))); // too long
        assert!(is_valid_project_id(&"x".repeat(64)));
    }

    #[test]
    fn slugify_produces_valid_slugs() {
        assert_eq!(slugify("Continuum-main"), "continuum-main");
        assert_eq!(slugify("My Cool Project!"), "my-cool-project");
        assert_eq!(slugify("v2.0 (beta)"), "v2-0-beta");
        assert_eq!(slugify("---"), "project");
        assert_eq!(slugify(""), "project");
        let long = slugify(&"a".repeat(100));
        assert_eq!(long.len(), 64);
        for name in ["Continuum-main", "über café", "..x..", "AB__cd"] {
            assert!(is_valid_project_id(&slugify(name)), "slugify({name:?})");
        }
    }

    #[test]
    fn unique_slug_appends_collision_suffixes() {
        let mut existing = HashSet::new();
        assert_eq!(unique_slug("Tools", &existing), "tools");
        existing.insert("tools".to_string());
        assert_eq!(unique_slug("Tools", &existing), "tools-2");
        existing.insert("tools-2".to_string());
        assert_eq!(unique_slug("Tools", &existing), "tools-3");
        // Suffix still fits the 64-char cap on long names.
        let long_name = "a".repeat(100);
        existing.insert(slugify(&long_name));
        let suffixed = unique_slug(&long_name, &existing);
        assert!(suffixed.ends_with("-2"));
        assert!(suffixed.len() <= PROJECT_SLUG_MAX_LEN);
        assert!(is_valid_project_id(&suffixed));
    }

    // --- Resolution matrix ---

    #[test]
    fn tier1_title_path_beats_keywords() {
        let r = resolver(test_projects(), vec![]);
        // The title mentions the simcharts keyword but *shows* a continuum
        // root path — the path wins (tier 1 > tier 4).
        let res = r
            .resolve_candidate(&input(
                "Code.exe",
                "simcharts notes - D:\\Continuum\\Continuum-main\\notes.md - Code",
                ts(0),
            ))
            .unwrap();
        assert_eq!(res.project_id, "continuum");
        assert_eq!(res.source_tier, 1);
        assert!((res.confidence - 0.9).abs() < f32::EPSILON);
        // Forward slashes and case differences still match.
        let res = r
            .resolve_candidate(&input(
                "Code.exe",
                "d:/continuum/continuum-main/src/lib.rs",
                ts(0),
            ))
            .unwrap();
        assert_eq!(res.project_id, "continuum");
        assert_eq!(res.source_tier, 1);
    }

    #[test]
    fn tier2_editor_patterns_match_all_separator_variants() {
        let r = resolver(test_projects(), vec![]);
        for title in [
            "main.rs — Continuum-main — Visual Studio Code", // em dash + root basename
            "main.rs - Continuum-main - Visual Studio Code", // hyphen variant
            "● main.rs – Continuum-main – Code",             // en dash + dirty marker
            "Continuum – wake.rs",                           // JetBrains "project – file"
        ] {
            let res = r
                .resolve_candidate(&input("Code.exe", title, ts(0)))
                .unwrap_or_else(|| panic!("no resolution for {title:?}"));
            assert_eq!(res.project_id, "continuum", "title {title:?}");
            assert_eq!(res.source_tier, 2, "title {title:?}");
            assert!((res.confidence - 0.8).abs() < f32::EPSILON);
        }
        // Segment matching is equality, not substring: a hyphenated file
        // name does not match.
        assert!(r
            .resolve_candidate(&input("Code.exe", "notes.md - other-folder - Code", ts(0)))
            .is_none());
    }

    #[test]
    fn tier3_git_root_of_recent_file_event() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("myrepo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let file = repo.join("src").join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let projects = vec![project(
            "myrepo",
            "My Repo",
            &[&repo.to_string_lossy()],
            &[],
        )];
        let r = resolver(projects, vec![]);
        let frame = FrameInput {
            process_name: "pwsh.exe",
            window_title: "Windows PowerShell",
            recent_file_path: Some(&file),
            ts: ts(0),
        };
        let res = r.resolve_candidate(&frame).unwrap();
        assert_eq!(res.project_id, "myrepo");
        assert_eq!(res.source_tier, 3);
        assert!((res.confidence - 0.7).abs() < f32::EPSILON);
        // Without the file event, the terminal title resolves nothing.
        assert!(r
            .resolve_candidate(&input("pwsh.exe", "Windows PowerShell", ts(0)))
            .is_none());
    }

    #[test]
    fn tier4_keyword_match_is_the_weakest_tier() {
        let r = resolver(test_projects(), vec![]);
        let res = r
            .resolve_candidate(&input(
                "chrome.exe",
                "SimCharts docs - Google Chrome",
                ts(0),
            ))
            .unwrap();
        assert_eq!(res.project_id, "simcharts");
        assert_eq!(res.source_tier, 4);
        assert!((res.confidence - 0.5).abs() < f32::EPSILON);
        // Multi-word keyword.
        let res = r
            .resolve_candidate(&input("chrome.exe", "the Chart Editor guide", ts(0)))
            .unwrap();
        assert_eq!(res.project_id, "simcharts");
        assert!(r
            .resolve_candidate(&input("chrome.exe", "unrelated page", ts(0)))
            .is_none());
    }

    #[test]
    fn tier0_force_rule_overrides_everything() {
        let rules = vec![OverrideRule {
            match_process: Some("obsidian.exe".to_string()),
            match_title_substring: None,
            action: RuleAction::ForceProject,
            project_id: "simcharts".to_string(),
        }];
        let r = resolver(test_projects(), rules);
        // Title clearly says continuum, but the force rule wins.
        let res = r
            .resolve_candidate(&input(
                "obsidian.exe",
                "D:\\Continuum\\Continuum-main notes",
                ts(0),
            ))
            .unwrap();
        assert_eq!(res.project_id, "simcharts");
        assert_eq!(res.source_tier, 0);
        assert!((res.confidence - 1.0).abs() < f32::EPSILON);
        // Non-matching process → normal resolution.
        let res = r
            .resolve_candidate(&input(
                "Code.exe",
                "D:\\Continuum\\Continuum-main\\x.rs",
                ts(0),
            ))
            .unwrap();
        assert_eq!(res.project_id, "continuum");
    }

    #[test]
    fn exclusion_rules_remove_candidates_before_ranking() {
        let rules = vec![OverrideRule {
            match_process: None,
            match_title_substring: Some("blog draft".to_string()),
            action: RuleAction::ExcludeProject,
            project_id: "continuum".to_string(),
        }];
        let r = resolver(test_projects(), rules);
        // continuum keyword present but excluded → next candidate (none
        // here) or nothing.
        assert!(r
            .resolve_candidate(&input("Code.exe", "blog draft about continuum", ts(0)))
            .is_none());
        // With another project still eligible, the runner-up wins.
        let res = r
            .resolve_candidate(&input(
                "Code.exe",
                "blog draft about continuum and simcharts",
                ts(0),
            ))
            .unwrap();
        assert_eq!(res.project_id, "simcharts");
        // The exclusion is scoped to matching frames only.
        let res = r
            .resolve_candidate(&input("Code.exe", "continuum roadmap", ts(0)))
            .unwrap();
        assert_eq!(res.project_id, "continuum");
    }

    #[test]
    fn exclusion_also_blocks_force_rules_and_empty_rules_never_match() {
        let rules = vec![
            OverrideRule {
                match_process: Some("code.exe".to_string()),
                match_title_substring: None,
                action: RuleAction::ForceProject,
                project_id: "continuum".to_string(),
            },
            OverrideRule {
                match_process: Some("code.exe".to_string()),
                match_title_substring: None,
                action: RuleAction::ExcludeProject,
                project_id: "continuum".to_string(),
            },
        ];
        let r = resolver(test_projects(), rules);
        // Excluded project cannot be forced onto the same frame.
        assert!(r
            .resolve_candidate(&input("Code.exe", "untitled", ts(0)))
            .is_none());
        // A rule with neither criterion never matches anything.
        let empty_rule = OverrideRule {
            match_process: None,
            match_title_substring: None,
            action: RuleAction::ForceProject,
            project_id: "continuum".to_string(),
        };
        assert!(!empty_rule.matches("Code.exe", "anything"));
    }

    #[test]
    fn discovered_candidates_never_participate_in_resolution() {
        // Spec §4.3: unconfirmed candidates are proposals only.
        let mut discovered = project(
            "sideproject",
            "SideProject",
            &["D:\\Dev\\sideproject"],
            &["sideproject"],
        );
        discovered.status = ProjectStatus::Discovered;
        let mut r = resolver(vec![discovered], vec![]);
        for title in [
            "D:\\Dev\\sideproject\\main.py - Code",       // tier 1 shape
            "main.py - sideproject - Visual Studio Code", // tier 2 shape
            "sideproject readme",                         // tier 4 shape
        ] {
            assert!(
                r.resolve_candidate(&input("Code.exe", title, ts(0)))
                    .is_none(),
                "discovered candidate must not resolve for {title:?}"
            );
        }
        // Confirming flips it into resolution.
        assert!(r.confirm_project("sideproject"));
        let res = r
            .resolve_candidate(&input("Code.exe", "sideproject readme", ts(0)))
            .unwrap();
        assert_eq!(res.project_id, "sideproject");
    }

    // --- Hysteresis ---

    #[test]
    fn first_resolution_is_adopted_immediately() {
        let mut r = resolver(test_projects(), vec![]);
        let out = r.observe(&input("Code.exe", "continuum roadmap", ts(0)));
        let current = out.current.expect("adopted");
        assert_eq!(current.id, "continuum");
        assert_eq!(current.name, "Continuum");
        assert_eq!(
            current.root_path.as_deref(),
            Some(Path::new("D:\\Continuum\\Continuum-main"))
        );
        let switch = out.switched.expect("first adoption reports a switch");
        assert_eq!(switch.from, None);
        assert_eq!(switch.to, "continuum");
        // The shared handle sees the same value.
        assert_eq!(r.handle().read().as_ref().unwrap().id, "continuum");
    }

    #[test]
    fn a_different_project_must_win_for_switch_min_secs() {
        let mut r = resolver(test_projects(), vec![]);
        r.observe(&input("Code.exe", "continuum roadmap", ts(0)));

        // simcharts starts winning at t=10 — no flip until 20 s held.
        let out = r.observe(&input("Code.exe", "simcharts editor", ts(10)));
        assert_eq!(out.current.unwrap().id, "continuum");
        assert!(out.switched.is_none());
        let out = r.observe(&input("Code.exe", "simcharts editor", ts(25)));
        assert_eq!(out.current.unwrap().id, "continuum", "15 s held < 20 s");
        assert!(out.switched.is_none());
        // At t=30 the challenger has held for 20 s → flip.
        let out = r.observe(&input("Code.exe", "simcharts editor", ts(30)));
        let current = out.current.unwrap();
        assert_eq!(current.id, "simcharts");
        let switch = out.switched.expect("post-hysteresis flip");
        assert_eq!(switch.from.as_deref(), Some("continuum"));
        assert_eq!(switch.to, "simcharts");
        assert_eq!(r.current().unwrap().id, "simcharts");
    }

    #[test]
    fn an_interrupted_challenge_resets_the_clock() {
        let mut r = resolver(test_projects(), vec![]);
        r.observe(&input("Code.exe", "continuum roadmap", ts(0)));
        // simcharts wins 10..15, then continuum retakes at 18.
        r.observe(&input("Code.exe", "simcharts editor", ts(10)));
        r.observe(&input("Code.exe", "simcharts editor", ts(15)));
        r.observe(&input("Code.exe", "continuum roadmap", ts(18)));
        // simcharts again at 20 — its earlier 10..15 span must not count.
        let out = r.observe(&input("Code.exe", "simcharts editor", ts(20)));
        assert!(out.switched.is_none());
        let out = r.observe(&input("Code.exe", "simcharts editor", ts(35)));
        assert_eq!(out.current.unwrap().id, "continuum", "held only 15 s");
        let out = r.observe(&input("Code.exe", "simcharts editor", ts(40)));
        assert_eq!(out.current.unwrap().id, "simcharts", "held 20 s → flip");
    }

    #[test]
    fn frames_with_no_winner_keep_the_incumbent_and_reset_challengers() {
        let mut r = resolver(test_projects(), vec![]);
        r.observe(&input("Code.exe", "continuum roadmap", ts(0)));
        r.observe(&input("Code.exe", "simcharts editor", ts(10)));
        // A resolution gap (browser detour) resets simcharts' challenge…
        let out = r.observe(&input("chrome.exe", "news site", ts(15)));
        assert_eq!(out.current.as_ref().unwrap().id, "continuum");
        assert!(out.switched.is_none());
        // …so simcharts needs a fresh 20 s from t=20.
        r.observe(&input("Code.exe", "simcharts editor", ts(20)));
        let out = r.observe(&input("Code.exe", "simcharts editor", ts(35)));
        assert_eq!(out.current.unwrap().id, "continuum");
        let out = r.observe(&input("Code.exe", "simcharts editor", ts(40)));
        assert_eq!(out.current.unwrap().id, "simcharts");
    }

    // --- Discovery ---

    #[test]
    fn discovery_proposes_a_validated_folder_after_min_dwell() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("NewThing");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let title = format!("notes.md - {} - Code", repo.display());

        let mut r = resolver(test_projects(), vec![]);
        // Accumulate dwell in ≤10 s credited steps: 0, +5 … until ≥ 30 s.
        let mut proposals = Vec::new();
        for i in 0..8 {
            let out = r.observe(&input("Code.exe", &title, ts(i * 5)));
            proposals.extend(out.discovered);
        }
        assert_eq!(proposals.len(), 1, "proposed exactly once");
        let candidate = &proposals[0];
        assert_eq!(candidate.id, "newthing");
        assert_eq!(candidate.name, "NewThing");
        assert_eq!(candidate.root_path, repo.to_string_lossy());
        assert!(is_valid_project_id(&candidate.id));
        // The proposal is tracked as Discovered and never resolves.
        let entry = r.projects().iter().find(|p| p.id == "newthing").unwrap();
        assert_eq!(entry.status, ProjectStatus::Discovered);
        assert!(r
            .resolve_candidate(&input("Code.exe", &title, ts(100)))
            .is_none());
        // Continued sightings do not re-propose.
        let out = r.observe(&input("Code.exe", &title, ts(100)));
        assert!(out.discovered.is_empty());
    }

    /// M1: an un-promoted accumulation entry must not live forever. A
    /// folder glimpsed once and never seen again is forgotten, and its
    /// partial dwell does not carry over to a much later sighting.
    #[test]
    fn discovery_accumulation_ages_out_and_restarts_dwell() {
        let ghost = "Z:\\seen\\once\\only";
        let title = format!("notes - {ghost} - Code");
        let mut r = resolver(vec![], vec![]);
        // Two sightings 5 s apart build up partial dwell.
        r.observe(&input("Code.exe", &title, ts(0)));
        r.observe(&input("Code.exe", &title, ts(5)));
        assert_eq!(r.discovery.accum.len(), 1);
        let partial = r.discovery.accum.values().next().unwrap().1;
        assert!(partial > 0 && partial < cfg().discover_min_secs as i64);

        // Nothing more for an hour: an unrelated title evicts the entry.
        let out = r.observe(&input("Code.exe", "Inbox - Outlook", ts(3600)));
        assert!(out.discovered.is_empty());
        assert!(
            r.discovery.accum.is_empty(),
            "stale accumulation must be evicted, not kept forever"
        );

        // A later sighting starts from zero rather than resuming.
        r.observe(&input("Code.exe", &title, ts(3605)));
        assert_eq!(r.discovery.accum.len(), 1);
        assert_eq!(
            r.discovery.accum.values().next().unwrap().1,
            0,
            "dwell restarts after the entry aged out"
        );
    }

    /// M1: the resolved set is capped, oldest-first, so a browsing-heavy
    /// day cannot grow it without bound.
    #[test]
    fn discovery_resolved_set_is_capped_oldest_first() {
        let mut state = DiscoveryState::default();
        for i in 0..(DISCOVERY_RESOLVED_CAP + 10) {
            state.mark_resolved(format!("path-{i}"));
        }
        assert_eq!(state.resolved.len(), DISCOVERY_RESOLVED_CAP);
        assert_eq!(state.resolved_order.len(), DISCOVERY_RESOLVED_CAP);
        assert!(!state.resolved.contains("path-0"), "oldest evicted");
        assert!(!state.resolved.contains("path-9"));
        assert!(state.resolved.contains("path-10"), "newest kept");
        assert!(state
            .resolved
            .contains(&format!("path-{}", DISCOVERY_RESOLVED_CAP + 9)));

        // Re-marking an existing key must not grow the order queue.
        state.mark_resolved("path-10".to_string());
        assert_eq!(state.resolved_order.len(), DISCOVERY_RESOLVED_CAP);
    }

    #[test]
    fn discovery_validates_dir_exists_and_uses_git_root_of_file_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("FileRepo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let file = repo.join("main.rs");
        std::fs::write(&file, "").unwrap();
        // Title shows the *file* — the proposal must be its git root.
        let title = format!("{} - Code", file.display());
        let mut r = resolver(vec![], vec![]);
        let mut proposals = Vec::new();
        for i in 0..8 {
            proposals.extend(r.observe(&input("Code.exe", &title, ts(i * 5))).discovered);
        }
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].root_path, repo.to_string_lossy());
        assert_eq!(proposals[0].id, "filerepo");

        // A nonexistent path accumulates but never validates.
        let ghost = "Z:\\definitely\\not\\a-real-dir-xyz";
        let ghost_title = format!("notes - {ghost} - Code");
        let mut r = resolver(vec![], vec![]);
        for i in 0..10 {
            let out = r.observe(&input("Code.exe", &ghost_title, ts(i * 5)));
            assert!(out.discovered.is_empty(), "ghost path must never propose");
        }
    }

    #[test]
    fn discovery_skips_known_roots_and_suffixes_slug_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        // A folder whose slug collides with an existing project id.
        let clash = tmp.path().join("Continuum");
        std::fs::create_dir_all(clash.join(".git")).unwrap();
        let clash_title = format!("x.rs - {} - Code", clash.display());
        // A path inside an already-configured root: never a candidate.
        let known_title = "a.rs - D:\\Continuum\\Continuum-main\\src - Code";

        let mut r = resolver(test_projects(), vec![]);
        let mut proposals = Vec::new();
        for i in 0..8 {
            proposals.extend(
                r.observe(&input("Code.exe", &clash_title, ts(i * 5)))
                    .discovered,
            );
            proposals.extend(
                r.observe(&input("Code.exe", known_title, ts(i * 5)))
                    .discovered,
            );
        }
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].id, "continuum-2", "collision suffix");
        assert!(proposals
            .iter()
            .all(|p| !p.root_path.contains("Continuum-main")));
    }

    #[test]
    fn discovery_respects_the_auto_discover_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("OffRepo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let title = format!("x - {} - Code", repo.display());
        let mut config = cfg();
        config.auto_discover = false;
        let mut r = ProjectResolver::new(vec![], vec![], &config);
        for i in 0..10 {
            let out = r.observe(&input("Code.exe", &title, ts(i * 5)));
            assert!(out.discovered.is_empty());
        }
    }

    // --- Helpers / config plumbing ---

    #[test]
    fn from_config_slugifies_invalid_ids_and_defaults_names() {
        let entry = ProjectConfigEntry {
            id: "My Project!".to_string(),
            name: String::new(),
            root_paths: vec![],
            repo: None,
            keywords: vec![],
            zone: None,
        };
        let project = ProjectEntry::from_config(&entry);
        assert_eq!(project.id, "my-project");
        assert_eq!(project.name, "my-project", "empty name falls back to id");
        assert_eq!(project.status, ProjectStatus::Configured);
    }

    #[test]
    fn status_and_action_roundtrip_their_db_strings() {
        for status in [
            ProjectStatus::Configured,
            ProjectStatus::Discovered,
            ProjectStatus::Confirmed,
        ] {
            assert_eq!(ProjectStatus::parse(status.as_str()), status);
        }
        // Unknown strings map to the least-privileged status.
        assert_eq!(ProjectStatus::parse("garbage"), ProjectStatus::Discovered);
        for action in [RuleAction::ForceProject, RuleAction::ExcludeProject] {
            assert_eq!(RuleAction::parse(action.as_str()), Some(action));
        }
        assert_eq!(RuleAction::parse("garbage"), None);
    }

    #[test]
    fn find_git_root_walks_ancestors_and_handles_missing_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let nested = repo.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(find_git_root(&nested).as_deref(), Some(repo.as_path()));
        let bare = tmp.path().join("no-repo");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(find_git_root(&bare), None);
    }

    #[test]
    fn legacy_keyword_hint_matches_the_historic_pairs() {
        assert_eq!(
            legacy_keyword_hint("main.rs - Continuum-main - Code").as_deref(),
            Some("continuum")
        );
        assert_eq!(
            legacy_keyword_hint("SimCharts editor").as_deref(),
            Some("simcharts")
        );
        assert_eq!(legacy_keyword_hint("something else"), None);
    }

    #[test]
    fn extract_title_paths_finds_drive_letter_runs() {
        let paths = extract_title_paths("● main.rs - D:\\Dev\\Thing - Visual Studio Code");
        assert_eq!(paths, vec![PathBuf::from("D:\\Dev\\Thing")]);
        let paths = extract_title_paths("Admin: C:/Users/x/proj");
        assert_eq!(paths, vec![PathBuf::from("C:/Users/x/proj")]);
        assert!(extract_title_paths("no paths here").is_empty());
        assert!(extract_title_paths("ratio 1:2 in text").is_empty());
    }
}
