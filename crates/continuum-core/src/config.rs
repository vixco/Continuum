//! # Configuration
//!
//! Loads and manages Continuum's runtime configuration. Every model, interval,
//! threshold, and prompt is readable from config and overridable via the
//! dashboard.
//!
//! Configuration is stored at `~/.continuum-dev/config.toml` with defaults loaded
//! from the bundled `config/` directory in the repository.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Root configuration for the Continuum runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContinuumConfig {
    /// Health polling, backup and guarded repair settings.
    pub health: HealthConfig,
    /// Vision model configuration.
    pub vision: VisionConfig,
    /// Screen capture configuration.
    pub screen: ScreenConfig,
    /// Audio pipeline configuration.
    pub audio: AudioConfig,
    /// Context poller configuration.
    pub context: ContextConfig,
    /// Perception frame builder configuration.
    pub frame: FrameConfig,
    /// Raw log storage configuration.
    pub storage: StorageConfig,
    /// Memory distillation configuration.
    pub memory: MemoryConfig,
    /// Voice input and routing configuration.
    pub voice: VoiceConfig,
    /// Text-to-speech configuration (Phase 5).
    pub tts: TtsConfig,
    /// Worker pool configuration (Phase 8).
    pub workers: WorkersConfig,
    /// Skills system configuration (Phase 8).
    pub skills: SkillsConfig,
    /// Orchestrator (Claude Opus) subprocess configuration.
    pub orchestrator: OrchestratorSection,
    /// Triage (local LLM) runtime configuration.
    pub triage: TriageSection,
    /// Adaptive resource policy — auto-detects PC specs at boot and tunes
    /// local-model threads, GPU offload, vision, poll intervals, and worker
    /// concurrency so Continuum does not eat the whole machine. Per
    /// non-negotiable #3 every value here is overridable.
    pub resources: ResourceConfig,
    /// Chat tab settings.
    #[serde(default)]
    pub chat: ChatConfig,
    /// Privacy filter settings (context engine spec §4.1/§6): scrubber
    /// toggles, zone rules, and honest per-source observation switches.
    #[serde(default)]
    pub privacy: PrivacyConfig,
    /// Project resolver settings + known projects (context engine spec
    /// §4.3/§6).
    #[serde(default)]
    pub projects: ProjectsConfig,
    /// Git collector settings (context engine spec §4.4/§6).
    #[serde(default)]
    pub git_context: GitContextConfig,
    /// Context-events writer settings (context engine spec §4.6/§6):
    /// dedupe collapse window, caps, queue size, retention.
    #[serde(default)]
    pub events: EventsConfig,
    /// File watcher settings (context engine spec §4.5/§6) — opt-in,
    /// default OFF.
    #[serde(default)]
    pub file_watcher: FileWatcherConfig,
    /// Background-process activity collector. Opt-in because even process
    /// names can reveal user activity; it never reads command lines,
    /// environment variables, or process memory.
    #[serde(default)]
    pub process_watcher: ProcessWatcherConfig,
    /// Idle-mode performance knobs (context engine spec §4.11/§6): when
    /// the user goes idle, capture and vision cadences relax via the
    /// shared [`crate::senses::cadence::CadenceControl`].
    #[serde(default)]
    pub performance: PerformanceConfig,
    /// Session-state tracker knobs (context engine spec §4.8/§6): when
    /// goal/task inference is allowed to spend a background LLM call.
    #[serde(default)]
    pub session_state: SessionStateConfig,
    /// Context-package budgets and per-section caps (context engine spec
    /// §4.9/§6): how much context each consumer profile may spend.
    #[serde(default)]
    pub context_package: ContextPackageConfig,
    /// Continuation-resolver knobs (context engine spec §4.12/§6): which
    /// utterances mean "continue", and how confident the resolver must be
    /// before it recommends a target instead of asking.
    #[serde(default)]
    pub continuation: ContinuationConfig,
    /// MCP context-tool switches (context engine spec §5/§6): the family
    /// kill-switch plus the clipboard-tool kill-switch.
    #[serde(default)]
    pub context_tools: ContextToolsConfig,
    /// Agent OS interaction UX. Execution policy remains in Agent OS's
    /// independently protected policy store; these values only control the
    /// visible action indicator.
    #[serde(default)]
    pub agent_os: AgentOsConfig,
    /// Optional GitHub CLI integration settings.
    #[serde(default)]
    pub github: GitHubConfig,
}

/// Visible Agent OS interaction settings (`[agent_os]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentOsConfig {
    /// Show a distinct amber AI pointer marker after computer clicks.
    pub show_action_cursor: bool,
    /// How long the marker remains visible after each click.
    pub action_cursor_duration_ms: u64,
    /// Marker diameter in physical pixels.
    pub action_cursor_size_px: u32,
}

impl Default for AgentOsConfig {
    fn default() -> Self {
        Self {
            show_action_cursor: true,
            action_cursor_duration_ms: 420,
            action_cursor_size_px: 30,
        }
    }
}

/// Official GitHub CLI bridge settings (`[github]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct GitHubConfig {
    /// Master switch. No network request occurs until the user connects/calls a tool.
    pub enabled: bool,
    /// Browser/device login deadline.
    pub connect_timeout_secs: u64,
    /// Deadline for status and API calls.
    pub api_timeout_secs: u64,
    /// Maximum bytes returned from one API call.
    pub max_response_bytes: usize,
    /// Maximum UTF-8 bytes sent in one issue/comment/PR body.
    pub max_mutation_body_bytes: usize,
    /// Maximum title characters for issues and pull requests.
    pub max_title_chars: usize,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            connect_timeout_secs: 600,
            api_timeout_secs: 30,
            max_response_bytes: 512 * 1024,
            max_mutation_body_bytes: 64 * 1024,
            max_title_chars: 256,
        }
    }
}

/// MCP context-tool switches (`[context_tools]`, context engine spec
/// §5.1/§5.2/§6).
///
/// These are read by the **MCP server process**, which loads the same
/// `config.toml` the runtime does. They are kill-switches, not caps: per
/// non-negotiable #3 every observation tool must be disableable without
/// editing code, and per spec §5.1 the clipboard tool needs its own switch
/// (it is the one observation tool that reads content the user never
/// pointed a window at).
///
/// Turning a switch off never changes a tool's schema — the tool stays
/// registered and answers with empty/sentinel content, so non-negotiable #7
/// holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ContextToolsConfig {
    /// Master switch for the MCP context tool family (spec §5.2). When
    /// false the context tools answer as if nothing had been observed yet.
    pub enabled: bool,
    /// Kill-switch for `system_clipboard_get` (spec §5.1). When false the
    /// tool always returns `text: null` — the clipboard is never read at
    /// all, so nothing can leak even in scrubbed form.
    pub clipboard_tool_enabled: bool,
}

impl Default for ContextToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            clipboard_tool_enabled: true,
        }
    }
}

/// Continuation-resolver knobs (`[continuation]`, context engine spec
/// §4.12/§6).
///
/// The resolver answers "the user said *ga door* — continue **what**?" from
/// ranked candidates with real producers (session task, the last wake's
/// next step, the curator's `open_task:` trailer, the last user command,
/// the last error). It is pure logic — no LLM call — and lives in
/// [`crate::context::continuation`], where the ranking formula and the
/// per-kind priors are documented.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContinuationConfig {
    /// Score a candidate must reach for the packager to render it as
    /// "## Recommended next step". Below it, the wake reason instructs the
    /// orchestrator to ask one short disambiguation question instead.
    pub confidence_floor: f32,
    /// Utterances that mean "pick up where we left off". Matched
    /// case-insensitively as a whole utterance or a word-boundary prefix
    /// (see [`crate::context::continuation::is_continue_trigger`]). An
    /// empty ask (bare hotkey press) counts as a trigger by itself.
    pub trigger_phrases: Vec<String>,
    /// How far back the runtime looks for the last `wake_result` event
    /// when assembling continuation candidates. Longer than the packager's
    /// `[context_package] events_window_minutes` on purpose: "continue"
    /// after lunch must still find this morning's next step.
    pub wake_result_lookback_hours: i64,
}

impl Default for ContinuationConfig {
    fn default() -> Self {
        Self {
            confidence_floor: 0.6,
            trigger_phrases: vec![
                "ga door".to_string(),
                "continue".to_string(),
                "verder".to_string(),
                "waar was ik".to_string(),
                "pak weer op".to_string(),
            ],
            wake_result_lookback_hours: 12,
        }
    }
}

/// Context-package budgets + per-section caps (`[context_package]`,
/// context engine spec §4.9/§6).
///
/// One package, three consumer profiles: the wake message (runtime),
/// `context_package` (MCP) and the chat system prompt (desktop). The two
/// budgets below are the whole-package token caps for the wake and chat
/// profiles; everything else caps a single section so one runaway source
/// can never starve the rest.
///
/// **Drop order under budget pressure** (spec §4.9, implemented in
/// [`crate::context::package::DROP_ORDER`]): open files → recent changes →
/// just-before tail → memories tail. Never dropped: why-woken, current
/// moment, session state, pending memory decisions.
///
/// Token counts are estimated with the
/// [`crate::context::package::BYTES_PER_TOKEN`] proxy (3.5 bytes/token),
/// the same heuristic the triage prompt-fit gate uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextPackageConfig {
    /// Whole-package token budget for the wake profile.
    pub token_budget: usize,
    /// Whole-package token budget for the desktop chat profile (Task B8).
    pub chat_token_budget: usize,
    /// How far back the packager reads deduped `context_events` for its
    /// "just before" / "recent changes" / "failed attempts" sections.
    pub events_window_minutes: i64,
    /// Max deduped-event lines in "just before".
    pub max_just_before: usize,
    /// Max episodic memories.
    pub max_memories: usize,
    /// Max semantic facts.
    pub max_facts: usize,
    /// Max confirmed vault notes.
    pub max_vault_notes: usize,
    /// Max pending vault decisions.
    pub max_pending_decisions: usize,
    /// Max file/git change lines.
    pub max_recent_changes: usize,
    /// Max failed-attempt lines.
    pub max_failed_attempts: usize,
    /// Max open files listed in the session section.
    pub max_open_files: usize,
    /// Max tool names listed before collapsing into "+N more".
    pub max_tools: usize,
    /// Max characters of the `world_compact` blob (Task B4).
    pub world_compact_max_chars: usize,
}

impl Default for ContextPackageConfig {
    fn default() -> Self {
        Self {
            token_budget: 1_000,
            chat_token_budget: 600,
            events_window_minutes: 20,
            max_just_before: 5,
            max_memories: 3,
            max_facts: 8,
            max_vault_notes: 5,
            max_pending_decisions: 5,
            max_recent_changes: 5,
            max_failed_attempts: 3,
            max_open_files: 5,
            max_tools: 12,
            world_compact_max_chars: 1_200,
        }
    }
}

impl ContextPackageConfig {
    /// The per-section caps as the packager's own type.
    pub fn caps(&self) -> crate::context::package::SectionCaps {
        crate::context::package::SectionCaps {
            just_before: self.max_just_before,
            memories: self.max_memories,
            facts: self.max_facts,
            vault_notes: self.max_vault_notes,
            pending_decisions: self.max_pending_decisions,
            recent_changes: self.max_recent_changes,
            failed_attempts: self.max_failed_attempts,
            open_files: self.max_open_files,
            tools: self.max_tools,
            world_compact_chars: self.world_compact_max_chars,
        }
    }

    /// The cloud-bound budget for the wake profile.
    pub fn wake_budget(&self) -> crate::context::package::PackageBudget {
        crate::context::package::PackageBudget::cloud(self.token_budget).with_caps(self.caps())
    }

    /// The cloud-bound budget for the desktop chat profile (Task B8).
    pub fn chat_budget(&self) -> crate::context::package::PackageBudget {
        crate::context::package::PackageBudget::cloud(self.chat_token_budget).with_caps(self.caps())
    }
}

/// Session-state inference knobs (`[session_state]`, context engine spec
/// §4.8/§6). These govern the ONLY background LLM call session state ever
/// makes: goal/activity interpretation. Everything else in
/// [`crate::context::session_state`] is mechanical and free.
///
/// The defaults are deliberately conservative — at most one call every
/// `infer_min_interval_secs` (2 min) of at most `infer_max_tokens` (256)
/// tokens, never while idle (spec §4.11), always behind interactive triage
/// (Task B2's [`crate::llm_gate::LlmGate`] background tier).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionStateConfig {
    /// Minimum seconds between two inference *attempts*, whatever fired
    /// them. The hard rate limit on session state's GPU budget.
    pub infer_min_interval_secs: u64,
    /// Re-infer when the current goal/task are older than this many
    /// minutes, even if nothing else happened.
    pub infer_max_age_minutes: u64,
    /// Re-infer once this many *significant* events accumulated since the
    /// last attempt.
    pub infer_min_new_events: usize,
    /// Re-infer after this many focus switches. This catches short app
    /// sequences such as editor -> browser -> chat even when each switch is
    /// mechanically low-importance.
    pub infer_min_focus_switches: usize,
    /// Importance threshold an event must reach to count toward
    /// `infer_min_new_events`.
    pub significant_importance: f32,
    /// Below this confidence, all inferred claims are discarded and
    /// consumers render `"unknown"` (spec §4.8). Also the cap applied to a
    /// very stale rehydrated snapshot.
    pub confidence_floor: f32,
    /// `max_tokens` for one inference call. The background tier clamps
    /// anything above [`crate::llm_gate::BACKGROUND_MAX_TOKENS`] anyway.
    pub infer_max_tokens: u32,
}

impl Default for SessionStateConfig {
    fn default() -> Self {
        Self {
            infer_min_interval_secs: 120,
            infer_max_age_minutes: 10,
            infer_min_new_events: 8,
            infer_min_focus_switches: 2,
            significant_importance: 0.5,
            confidence_floor: 0.4,
            infer_max_tokens: 256,
        }
    }
}

/// Idle-mode performance knobs (`[performance]`, context engine spec
/// §4.11/§6). When `idle_seconds` exceeds `idle_pause_after_secs`, the
/// idle controller switches the shared cadence control to the idle
/// values; any restore trigger (input activity, voice wake, hotkey, any
/// orchestrator wake) switches back. Continuous visual context keeps the same
/// 20 ms capture/vision scheduling defaults while idle; users who prefer lower
/// resource use can explicitly configure slower idle values, or set
/// `idle_vision_interval_ms = 0` to pause vision entirely.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    /// Seconds of input inactivity before idle mode engages. `0`
    /// disables idle mode entirely (cadences never change).
    pub idle_pause_after_secs: u64,
    /// Per-monitor capture cadence while idle (normal cadence comes from
    /// `[screen].capture_interval_ms`).
    pub idle_capture_interval_ms: u64,
    /// Minimum delay between local vision calls per monitor while idle.
    /// `0` pauses vision entirely during idle (documented trade-off:
    /// unattended on-screen errors go undetected until activity resumes).
    pub idle_vision_interval_ms: u64,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            idle_pause_after_secs: 300,
            idle_capture_interval_ms: 20,
            idle_vision_interval_ms: 20,
        }
    }
}

/// Configuration for the file watcher (`[file_watcher]`, context engine
/// spec §4.5/§6). **Opt-in, default OFF.** Watches the root paths of every
/// confirmed/configured project whose zone allows observation; also gated
/// at runtime by `[privacy.toggles].files` (spec §4.1 honest toggles) —
/// both must be on for any `notify` watch to be armed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileWatcherConfig {
    /// Master switch for the file watcher. `false` by default — file
    /// watching is opt-in (spec §4.5).
    pub enabled: bool,
    /// Per-path debounce window in milliseconds: rapid events on the same
    /// path coalesce (last kind wins, except created+modified stays
    /// created) and rename From/To halves pair within this window.
    pub debounce_ms: u64,
    /// Ignore patterns matched against every path component (and, for
    /// wildcard patterns like `*.tmp`, the file name). Defaults skip
    /// dependency/build trees and `.git` entirely — git facts are the git
    /// collector's job (spec §4.4); duplicating them here would double
    /// signals.
    pub ignore_globs: Vec<String>,
    /// Seconds between rearm attempts for unavailable roots — and the
    /// cadence at which the confirmed-project set is re-read so projects
    /// confirmed at runtime get watched without a restart.
    pub rearm_secs: u64,
    /// Storm threshold: more than this many pending events for one
    /// project in a single debounce flush collapse into ONE
    /// `files_bulk_change` event (branch switches touch thousands of
    /// tracked files; debounce alone is per-path).
    pub storm_threshold: usize,
}

/// Default `[file_watcher].ignore_globs` (spec §4.5).
pub fn default_file_watcher_ignore_globs() -> Vec<String> {
    [
        "node_modules",
        "target",
        ".git",
        ".next",
        "dist",
        "build",
        "__pycache__",
        "*.tmp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for FileWatcherConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            debounce_ms: 1000,
            ignore_globs: default_file_watcher_ignore_globs(),
            rearm_secs: 60,
            storm_threshold: 50,
        }
    }
}

/// Configuration for the background-process activity collector
/// (`[process_watcher]`). The collector is opt-in and change-driven: it
/// publishes relevant starts/stops plus sustained resource pressure instead
/// of continuously persisting the complete operating-system process table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessWatcherConfig {
    /// Master consent boundary. When false no process table is sampled.
    pub enabled: bool,
    /// Seconds between process-table samples.
    pub poll_secs: u64,
    /// CPU usage that becomes meaningful after `sustained_samples` polls.
    pub cpu_threshold_percent: f32,
    /// Resident memory that becomes meaningful after `sustained_samples`
    /// polls.
    pub memory_threshold_mb: u64,
    /// Consecutive threshold crossings required before emitting a pressure
    /// event, preventing single-sample noise.
    pub sustained_samples: u32,
    /// Process basenames whose lifecycle is useful even below thresholds.
    pub include_names: Vec<String>,
    /// Process basenames that are never published by this collector.
    pub exclude_names: Vec<String>,
    /// Maximum number of active entries in the published snapshot.
    pub snapshot_limit: usize,
}

impl Default for ProcessWatcherConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_secs: 2,
            cpu_threshold_percent: 75.0,
            memory_threshold_mb: 2_048,
            sustained_samples: 3,
            include_names: [
                "cargo",
                "rustc",
                "node",
                "python",
                "python3",
                "deno",
                "bun",
                "ollama",
                "docker",
                "cmake",
                "ninja",
                "msbuild",
                "dotnet",
                "java",
                "continuum",
                "continuum-mcp",
            ]
            .iter()
            .map(|name| name.to_string())
            .collect(),
            exclude_names: [
                "system",
                "idle",
                "registry",
                "smss",
                "csrss",
                "wininit",
                "services",
                "lsass",
                "svchost",
                "fontdrvhost",
                "dwm",
            ]
            .iter()
            .map(|name| name.to_string())
            .collect(),
            snapshot_limit: 50,
        }
    }
}

/// Configuration for the context-events writer (`[events]`, context engine
/// spec §4.6/§6). Governs the dedicated events-writer task that owns the
/// `context_events` table: how long a dedupe row stays open, how large a
/// collapsed row may grow, how big the producer channel is, and how long
/// rows are retained.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EventsConfig {
    /// Dedupe collapse window in minutes, anchored on `ts_last` (spec
    /// §4.6): an event matching an open dedupe row within this window of
    /// the row's *last* occurrence bumps `count` instead of inserting, so
    /// ongoing repetition keeps collapsing.
    pub collapse_window_minutes: u64,
    /// Days `context_events` rows are retained before rotation (rotated
    /// with the raw log).
    pub retention_days: u32,
    /// Maximum `count` a collapsed row may reach; the next match starts a
    /// fresh row.
    pub count_cap: u32,
    /// Maximum `ts_first` → `ts_last` span in hours for one collapsed
    /// row; beyond it the next match starts a fresh row.
    pub span_cap_hours: u64,
    /// Bounded events-channel capacity. Producers never block: overflow
    /// increments a per-source dropped counter that the writer coalesces
    /// into `events_dropped` rows.
    pub queue_cap: usize,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            collapse_window_minutes: 10,
            retention_days: 30,
            count_cap: 500,
            span_cap_hours: 24,
            queue_cap: 1024,
        }
    }
}

/// Configuration for the git collector (`[git_context]`, context engine
/// spec §4.4/§6). The collector watches the **active, confirmed** project
/// only (resolver output — never the daemon's CWD, never unconfirmed
/// discovery candidates).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitContextConfig {
    /// Master switch for the git collector. Also gated at runtime by
    /// `[privacy.toggles].git` (spec §4.1 honest toggles) — both must be
    /// on for any `git` subprocess to spawn.
    pub enabled: bool,
    /// Seconds between status probes of the active project. Working-tree
    /// dirtiness always polls at this cadence; ref-derived facts are
    /// additionally mtime-gated (spec §4.4).
    pub poll_secs: u64,
    /// Hard timeout for one `git` subprocess invocation.
    pub command_timeout_secs: u64,
    /// Minimum seconds between two probe spawns — bounds the immediate
    /// post-hysteresis project-switch probe (spec §4.4).
    pub min_spawn_interval_secs: u64,
}

impl Default for GitContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_secs: 30,
            command_timeout_secs: 10,
            min_spawn_interval_secs: 5,
        }
    }
}

/// Configuration for the project resolver (`[projects]`, context engine
/// spec §4.3/§6).
///
/// Known projects live in `known` (`[[projects.known]]` in TOML). The spec
/// writes them as `[[projects]]`, but TOML cannot host an array-of-tables
/// and a table of knobs under the same key, so the entries are nested one
/// level down; boot reconciliation seeds them into the Projects table
/// (raw-log DB), where config wins by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectsConfig {
    /// Propose project candidates from title-derived paths (spec §4.3
    /// auto-discovery). Unconfirmed candidates are never collected from.
    pub auto_discover: bool,
    /// Hysteresis: a *different* project must win resolution continuously
    /// for this many seconds before the public current project flips.
    pub switch_min_secs: u64,
    /// Minimum accumulated seconds a title-derived folder must be observed
    /// before it is validated and proposed as a candidate.
    pub discover_min_secs: u64,
    /// Known projects (`[[projects.known]]`), mirrored into the Projects
    /// table at boot (config wins by id).
    pub known: Vec<ProjectConfigEntry>,
}

impl Default for ProjectsConfig {
    fn default() -> Self {
        Self {
            auto_discover: true,
            switch_min_secs: 20,
            discover_min_secs: 30,
            known: Vec::new(),
        }
    }
}

/// One configured project (`[[projects.known]]`, context engine spec §4.3):
/// `{ id, name, root_paths, repo?, keywords, zone? }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectConfigEntry {
    /// Project id slug (`[a-z0-9-]{1,64}`, no dots). Invalid ids are
    /// slugified at load with a warning.
    pub id: String,
    /// Human-readable name; defaults to the id when empty.
    #[serde(default)]
    pub name: String,
    /// Absolute root directories of the project.
    #[serde(default)]
    pub root_paths: Vec<String>,
    /// Optional remote repo URL (informational).
    #[serde(default)]
    pub repo: Option<String>,
    /// Title keywords for the resolver's keyword tier.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Optional per-project privacy zone (spec §4.3 project zones),
    /// reusing the `[privacy]` zone vocabulary.
    #[serde(default)]
    pub zone: Option<crate::senses::privacy::Zone>,
}

/// Configuration for the privacy filter (`[privacy]`, context engine spec
/// §4.1/§6).
///
/// The scrubber flags gate the pattern families applied by
/// [`crate::senses::privacy::PrivacyFilter::scrub_text`] to **free-text
/// fields only** (structured fields are exempt by construction — see the
/// `senses::privacy` module docs). `zones` holds explicit zone rules; at
/// load time they are unioned with rules synthesised from the legacy
/// `[context].sensitive_process_names` / `sensitive_title_keywords` lists
/// and the built-in private-browsing defaults, with the strictest matching
/// zone winning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PrivacyConfig {
    /// Scrub API keys and bearer tokens (`sk-…`, GitHub `gh?_…`, `AKIA…`,
    /// `Bearer …`) plus the generic high-entropy patterns (hex/base64
    /// runs ≥ 32 chars, UUIDs) from free text.
    pub scrub_api_keys: bool,
    /// Scrub Luhn-valid card numbers from free text.
    pub scrub_cards: bool,
    /// Scrub mod-97-valid IBANs from free text.
    pub scrub_iban: bool,
    /// Scrub email addresses from free text. Default OFF — emails are
    /// routinely part of legitimate context (window titles, commit
    /// subjects) and rarely secret.
    pub scrub_emails: bool,
    /// Explicit zone rules (`[[privacy.zones]]`), each
    /// `{match_process?, match_title_keyword?, zone}`. Unioned with the
    /// legacy-list synthesis; stricter wins on overlap.
    pub zones: Vec<crate::senses::privacy::ZoneRule>,
    /// Honest per-source observation switches (spec §4.1): a disabled
    /// source emits nothing; `pause_all` gates the whole frame loop.
    /// Enforced at each collector (Task A2); every toggle change writes a
    /// `system` event.
    pub toggles: ObservationToggles,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            scrub_api_keys: true,
            scrub_cards: true,
            scrub_iban: true,
            scrub_emails: false,
            zones: Vec::new(),
            toggles: ObservationToggles::default(),
        }
    }
}

/// Per-source observation switches (`[privacy.toggles]`, spec §4.1
/// "honest toggles"). Toggles are independent — no dishonest mute where
/// one switch silently disables another source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservationToggles {
    /// Microphone / audio observation. Enforced by `AudioWatcher::run`
    /// (Task A2): the device is never opened when off.
    pub mic: bool,
    /// Screen capture + vision observation. Enforced by
    /// `VisionWatcher::run` (Task A2): the capture scheduler never starts
    /// when off.
    pub screen: bool,
    /// File watcher observation. Enforcement seam: checked via
    /// `senses::privacy::source_enabled(ObservedSource::Files)` by the
    /// file watcher (`senses/file_watch.rs`) when it lands (Task A5/A7).
    pub files: bool,
    /// Git collector observation. Enforced by `GitWatcher::run`
    /// (`senses/git_watch.rs`, Task A5) via
    /// `senses::privacy::source_enabled(ObservedSource::Git)`: no `git`
    /// subprocess is ever spawned when off.
    pub git: bool,
    /// Master pause: gates the entire frame loop (no watchers or frame
    /// builder are spawned; frames are neither built nor persisted) and
    /// every individual source via `source_enabled`.
    pub pause_all: bool,
}

impl Default for ObservationToggles {
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

/// Guardrails for Health-tab repair sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HealthConfig {
    /// Hard wall-clock timeout for one repair-agent attempt.
    pub repair_timeout_secs: u64,
    /// Maximum time to wait for a newly-started runtime heartbeat.
    pub runtime_start_timeout_secs: u64,
    /// Lifetime of the one-time MCP repair grant.
    pub repair_session_ttl_secs: u64,
    /// Number of verified versioned backups retained.
    pub backup_retention: u32,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            repair_timeout_secs: 10 * 60,
            runtime_start_timeout_secs: 90,
            repair_session_ttl_secs: 15 * 60,
            backup_retention: 7,
        }
    }
}

/// Resource-aware profile. Selects a baseline policy; `Custom` uses the
/// individual fields of [`ResourceConfig`] verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileMode {
    /// Pick from detected specs: a laptop or a box without a GPU falls back
    /// to [`BarelyNotice`]; a desktop with a capable GPU uses [`Balanced`].
    Auto,
    /// Conservative CPU/RAM footprint (≤ ~30% of cores, ≥ 50% RAM free), but
    /// GPU/VRAM used freely for quality. The default — Continuum is barely
    /// noticeable while quality stays high.
    BarelyNotice,
    /// Middle ground: more threads/concurrency, shorter poll intervals. For
    /// desktops with headroom.
    Balanced,
    /// Use most of the machine. For dedicated Continuum appliances.
    Performance,
    /// Honour every field of [`ResourceConfig`] as written — no presets.
    Custom,
}

/// Adaptive resource policy. Resolved once at boot by
/// [`crate::hardware::resolve_resource_policy`] against detected
/// [`crate::hardware::HardwareSpecs`] to produce concrete knobs applied to
/// the triage LLM, vision model, whisper, screen/context pollers, and worker
/// pool. Every field is overridable via `config.toml` / the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceConfig {
    /// Which policy preset to use. Defaults to [`BarelyNotice`].
    pub profile: ProfileMode,
    /// Fraction (0.0-1.0) of logical cores local models may use on AC power.
    pub cpu_core_fraction: f32,
    /// Floor for local-model thread counts — never go below this even on a
    /// 2-core box, else the models stall.
    pub cpu_min_threads: u32,
    /// Cap for local-model thread counts — keeps headroom even on a 32-core
    /// box (the OS, dashboard, and orchestrator need cores too).
    pub cpu_max_threads: u32,
    /// Fraction (0.0-1.0) of total RAM to leave free for the OS and other apps.
    pub ram_reserve_fraction: f32,
    /// Override GPU detection: `None` = auto-detect via nvcuda.dll + nvidia-smi,
    /// `Some(true)` = force GPU, `Some(false)` = force CPU.
    pub gpu_enabled: Option<bool>,
    /// Minimum VRAM (MB) required to enable GPU offload. Below this, fall
    /// back to CPU so a small VRAM GPU isn't swapped to death. Default 3000
    /// fits Qwen3-8B Q4_K_M (~4 GB) with headroom.
    pub gpu_min_vram_mb: u32,
    /// When true, throttle harder on battery power.
    pub battery_throttle: bool,
    /// Fraction of logical cores to use on battery (applied when
    /// `battery_throttle` is true and the machine is on battery).
    pub battery_core_fraction: f32,
    /// Minimum total RAM (MB) to keep the vision model loaded. Below this,
    /// vision is disabled and perception falls back to text-only context.
    pub vision_min_ram_mb: u32,
    /// Override whether the vision model is loaded at all. `None` = auto
    /// (enabled when total RAM ≥ `vision_min_ram_mb`), `Some(true)` = force
    /// on, `Some(false)` = force off (text-only perception).
    pub vision_enabled: Option<bool>,
    /// Override the derived worker concurrency cap. `None` = derive from
    /// cores + RAM; `Some(n)` = use n (clamped to 1-10 by the worker pool).
    pub workers_max_concurrent: Option<u32>,
    /// Override the derived screen-capture interval (seconds). `None` =
    /// derive (3 s on AC, 5 s on battery).
    pub screen_interval_secs: Option<u64>,
    /// Override the derived context-poll interval (seconds). `None` = derive
    /// (1 s on AC, 2 s on battery).
    pub context_interval_secs: Option<u64>,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            profile: ProfileMode::BarelyNotice,
            cpu_core_fraction: 0.30,
            cpu_min_threads: 2,
            cpu_max_threads: 8,
            ram_reserve_fraction: 0.50,
            gpu_enabled: None,
            gpu_min_vram_mb: 3000,
            battery_throttle: true,
            battery_core_fraction: 0.20,
            vision_min_ram_mb: 6144,
            vision_enabled: None,
            workers_max_concurrent: None,
            screen_interval_secs: None,
            context_interval_secs: None,
        }
    }
}

impl ResourceConfig {
    /// Validate ranges. Called from [`load_config`] so bad values fail fast
    /// instead of producing a silently-broken resource plan.
    pub fn validate(&self) -> Result<()> {
        if self.cpu_core_fraction <= 0.0 || self.cpu_core_fraction > 1.0 {
            anyhow::bail!("resources.cpu_core_fraction must be in (0.0, 1.0]");
        }
        if self.battery_core_fraction <= 0.0 || self.battery_core_fraction > 1.0 {
            anyhow::bail!("resources.battery_core_fraction must be in (0.0, 1.0]");
        }
        if self.ram_reserve_fraction < 0.0 || self.ram_reserve_fraction > 1.0 {
            anyhow::bail!("resources.ram_reserve_fraction must be in [0.0, 1.0]");
        }
        if self.cpu_min_threads == 0 {
            anyhow::bail!("resources.cpu_min_threads must be >= 1");
        }
        if self.cpu_max_threads < self.cpu_min_threads {
            anyhow::bail!("resources.cpu_max_threads must be >= cpu_min_threads");
        }
        if let Some(n) = self.workers_max_concurrent {
            if n == 0 {
                anyhow::bail!("resources.workers_max_concurrent must be >= 1");
            }
        }
        if let Some(s) = self.screen_interval_secs {
            if s == 0 {
                anyhow::bail!("resources.screen_interval_secs must be >= 1");
            }
        }
        if let Some(s) = self.context_interval_secs {
            if s == 0 {
                anyhow::bail!("resources.context_interval_secs must be >= 1");
            }
        }
        Ok(())
    }
}

/// Chat tab settings (`[chat]`). Every knob overridable (non-negotiable #3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatConfig {
    /// Max output tokens per response.
    pub max_tokens: u32,
    /// Sampling temperature — only forwarded to OpenAI-compatible providers.
    pub temperature: Option<f32>,
    /// HTTP connect timeout.
    pub connect_timeout_secs: u64,
    /// Abort a stream after this many seconds without a delta.
    pub stream_idle_timeout_secs: u64,
    /// Idle timeout for claude-CLI sends: the max seconds allowed to pass
    /// WITHOUT a new line of CLI stdout before the send is aborted. This
    /// resets on every line the CLI emits, so it is not an overall/
    /// end-to-end timeout — a long generation that keeps steadily
    /// streaming output is never killed by this, only a CLI that goes
    /// silent mid-send is.
    pub cli_timeout_secs: u64,
    /// How often the desktop refreshes cached provider model catalogs.
    /// Set to 0 to disable automatic refresh.
    pub model_refresh_interval_secs: u64,
    /// Optional path to a custom system prompt file (overrides the built-in).
    pub system_prompt_path: Option<String>,
    /// Whether the chat AI gets memory tools (search/get/save/delete over
    /// the memory vault) plus retrieved memory context in its prompt.
    pub memory_tools_enabled: bool,
    /// Cap on tool-call rounds within a single chat turn, to bound runaway
    /// tool-calling loops.
    pub memory_tool_max_rounds: u32,
    /// Max vault notes injected as a "## Memory context" system-prompt
    /// section per turn. 0 disables prompt injection entirely.
    pub memory_context_notes_max: u32,
    /// Whether `sensitivity: sensitive` vault notes are exposed to chat
    /// memory search results and prompt context.
    pub include_sensitive_memory: bool,
    /// Whether the chat system prompt gains a "## Session state" section
    /// read from the runtime's published `state.json` (context engine spec
    /// §4.9 chat profile, Task B8).
    ///
    /// The section is *additive*: the in-process vault search that already
    /// feeds "## Memory context" is untouched, so turning this off — or
    /// running the desktop with no background runtime at all — leaves chat
    /// exactly as it was. When the runtime is not publishing, or its
    /// snapshot is stale, no section is rendered and the existing "Live
    /// status" footer carries "Background runtime: not running".
    pub include_session_context: bool,
    /// How many of the most recent messages to send to the model each turn.
    /// The complete conversation remains persisted; only the request tail is
    /// bounded. `0` preserves the legacy whole-history behavior.
    pub history_message_window: u32,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            max_tokens: 8192,
            temperature: None,
            connect_timeout_secs: 10,
            stream_idle_timeout_secs: 60,
            cli_timeout_secs: 120,
            model_refresh_interval_secs: 300,
            system_prompt_path: None,
            memory_tools_enabled: true,
            memory_tool_max_rounds: 8,
            memory_context_notes_max: 6,
            include_sensitive_memory: false,
            include_session_context: true,
            history_message_window: 20,
        }
    }
}

///
/// Every field is user-overridable via `config.toml` — per non-negotiable #3,
/// there are no hardcoded model IDs or timeouts anywhere else in the runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestratorSection {
    /// Agent CLI used for orchestration: `claude`, `codex`, or `hermes`.
    /// The runtime resolves the executable from PATH at boot.
    pub agent: String,
    /// Optional provider override for agent runtimes that support one
    /// (currently Hermes). Empty lets the selected agent use its own default.
    pub provider: String,
    /// Model ID passed to `claude --model`. Must be a currently-supported
    /// Claude Code model (e.g. `claude-opus-4-6`, `claude-sonnet-4-6`).
    pub model_id: String,
    /// Wall-clock timeout for a single wake cycle, in seconds. If the
    /// orchestrator doesn't emit a `result` event within this window the
    /// child process is killed and the wake is marked failed.
    pub wake_timeout_secs: u64,
    /// If true, pass `--bare` to Claude Code (skip hooks / plugins).
    /// Defaults to `false` so the user's normal Claude Code configuration
    /// applies; set to `true` for deterministic, reproducible wakes.
    pub bare_mode: bool,
}

/// Runtime knobs for the local triage LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TriageSection {
    /// Path to the `.gguf` triage model. Empty string means "use default
    /// location" (`<dev_dir>/models/triage/<default_file>`).
    pub model_path: String,
    /// llama.cpp context size in tokens. 4096 per spec §4.7 — the triage
    /// prompt (~2k tokens worst-case) plus the classification-bearing
    /// output no longer fits 2048 with headroom; keep as low as fits to
    /// minimise KV cache pressure.
    pub context_size: u32,
    /// Maximum tokens per triage response.
    pub max_tokens: u32,
    /// Sampling temperature. 0.0 is deterministic and fine for triage —
    /// we want a stable yes/no on each frame.
    pub temperature: f32,
    /// Layers to offload to GPU. `999` means "all available"; 0 is CPU-only.
    pub gpu_layers: u32,
    /// Log a warning when a triage decision exceeds this latency, in ms.
    pub latency_warn_ms: u64,
}

/// Configuration for the local vision model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VisionConfig {
    /// Name of the vision model (for display).
    pub name: String,
    /// Backend selection: `auto`, `gguf`, or `onnx`.
    pub backend: String,
    /// Directory containing the primary vision model files.
    pub model_path: String,
    /// ONNX fallback directory used if the primary model fails health checks.
    pub fallback_model_path: String,
    /// Whether GPU acceleration is requested when compiled into the runtime.
    pub gpu_enabled: bool,
    /// Input image width for the model.
    pub input_width: u32,
    /// Input image height for the model.
    pub input_height: u32,
    /// Prompt used to turn local screenshots into compact world-state text.
    pub prompt: String,
    /// Maximum number of tokens generated for one screen observation.
    pub max_new_tokens: usize,
    /// Longest edge before the image is split into 512px detail tiles.
    pub processor_max_edge: u32,
    /// Whether the processor emits detail tiles plus a global overview.
    pub image_splitting: bool,
}

impl VisionConfig {
    /// Validate the configurable SmolVLM generation and tiling contract.
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.backend.as_str(), "auto" | "gguf" | "onnx") {
            anyhow::bail!("vision.backend must be one of: auto, gguf, onnx");
        }
        if self.model_path.trim().is_empty() {
            anyhow::bail!("vision.model_path must not be empty");
        }
        if self.prompt.trim().is_empty() {
            anyhow::bail!("vision.prompt must not be empty");
        }
        if !(1..=256).contains(&self.max_new_tokens) {
            anyhow::bail!("vision.max_new_tokens must be between 1 and 256");
        }
        if !(512..=2048).contains(&self.processor_max_edge)
            || !self.processor_max_edge.is_multiple_of(512)
        {
            anyhow::bail!(
                "vision.processor_max_edge must be a multiple of 512 between 512 and 2048"
            );
        }
        Ok(())
    }
}

/// Configuration for screen capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenConfig {
    /// Master consent boundary for local screen capture.
    pub enabled: bool,
    /// Legacy coarse interval retained for backwards-compatible config/UI reads.
    pub interval_secs: u64,
    /// Best-effort target cadence per monitor. Defaults to 20 ms (50 captures/second).
    pub capture_interval_ms: u64,
    /// Width to downscale captured images to.
    pub capture_width: u32,
    /// Height to downscale captured images to.
    pub capture_height: u32,
    /// Whether to save screenshots to disk.
    pub save_screenshots: bool,
    /// Capture every connected monitor rather than only the primary display.
    pub all_monitors: bool,
    /// Stable xcap monitor ids the user explicitly excluded.
    pub excluded_monitor_ids: Vec<String>,
    /// Bounded local capture-event queue capacity.
    pub buffer_capacity: usize,
    /// Mean luma-difference threshold that triggers local vision processing.
    pub meaningful_change_threshold: f32,
    /// Minimum delay between expensive local VLM calls for one monitor.
    pub vision_min_interval_ms: u64,
}

impl ScreenConfig {
    /// Validate the mechanical capture and local-vision cadence contract.
    pub fn validate(&self) -> Result<()> {
        if self.capture_interval_ms < 20 {
            anyhow::bail!("screen.capture_interval_ms must be at least 20");
        }
        if self.capture_width == 0 || self.capture_height == 0 {
            anyhow::bail!("screen capture dimensions must be greater than zero");
        }
        if self.buffer_capacity == 0 {
            anyhow::bail!("screen.buffer_capacity must be at least 1");
        }
        if !(0.0..=1.0).contains(&self.meaningful_change_threshold) {
            anyhow::bail!("screen.meaningful_change_threshold must be between 0.0 and 1.0");
        }
        if self.vision_min_interval_ms != 0 && self.vision_min_interval_ms < 20 {
            anyhow::bail!("screen.vision_min_interval_ms must be 0 (paused) or at least 20");
        }
        Ok(())
    }
}

/// Configuration for the audio pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Whether audio capture is enabled.
    pub enabled: bool,
    /// Path to the whisper model file.
    pub whisper_model_path: String,
    /// Whisper language code (e.g. "nl", "en", "de"). Forcing a language
    /// avoids the auto-detector's low-confidence failures on short clips,
    /// where whisper-small tends to emit `[BLANK_AUDIO]` rather than risk a
    /// wrong transcript. Set to "auto" to let whisper guess per segment.
    pub whisper_language: String,
    /// Floor for the adaptive VAD threshold. The actual threshold used is
    /// `max(vad_threshold, noise_floor × vad_noise_floor_multiplier)`, so
    /// this value serves as an absolute minimum in very quiet environments.
    /// Default 0.005 catches genuine speech without tripping on a typical
    /// noise floor of 0.0001–0.001.
    pub vad_threshold: f32,
    /// Multiplier applied to the rolling noise floor when computing the
    /// effective VAD threshold. Default 5.0 — speech must be at least 5×
    /// the ambient level to trigger. Raise if your room is noisy, lower if
    /// you speak quietly.
    pub vad_noise_floor_multiplier: f32,
    /// Silence duration in ms before a speech segment ends.
    pub silence_duration_ms: u64,
    /// Maximum speech segment length in seconds before forced split.
    pub max_segment_secs: u64,
    /// Number of CPU threads for whisper transcription. Set at boot from the
    /// resolved resource plan (`whisper_threads`); defaults to 4. Lower
    /// values reduce CPU spikes during transcription at some latency cost.
    pub whisper_threads: i32,
    /// Whether whisper should try the GPU. Only effective when Continuum is
    /// built with the `cuda` feature (`whisper-rs/cuda`); otherwise whisper.cpp
    /// falls back to CPU silently. Set at boot from the resolved resource plan.
    pub whisper_use_gpu: bool,
    /// Display name of the chosen input device. Saved by the interactive
    /// picker on first run. Paired with `device_index` — both are checked on
    /// startup; if the name at that index no longer matches, the picker is
    /// re-invoked.
    pub device_name: String,
    /// cpal enumeration index of the chosen input device. `None` means
    /// "not yet picked, run the interactive picker on startup".
    pub device_index: Option<usize>,
}

/// Configuration for the context poller.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    /// Polling interval in seconds.
    pub poll_interval_secs: u64,
    /// Replace sensitive foreground titles before they enter shared context.
    pub redact_sensitive_titles: bool,
    /// Process names that suppress visual processing while foreground.
    pub sensitive_process_names: Vec<String>,
    /// Case-insensitive title fragments that suppress visual processing.
    pub sensitive_title_keywords: Vec<String>,
    /// Processes classified as terminal activity without capturing terminal text.
    pub terminal_process_names: Vec<String>,
}

/// Configuration for the perception frame builder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameConfig {
    /// Interval between frames in seconds (2-10).
    pub interval_secs: u64,
    /// Minimum salience score for a frame to reach triage (0.0-1.0).
    pub salience_threshold: f32,
}

/// Configuration for raw log storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Path to the SQLite database file.
    pub db_path: String,
    /// Directory for screenshot JPEG files.
    pub screenshots_dir: String,
    /// Number of days to retain frames before rotation.
    pub retention_days: u32,
    /// Delete screenshot files as part of raw-log rotation, and run the
    /// mtime backstop sweep of [`Self::screenshots_dir`] (context engine
    /// spec §4.11 / §6). Set to `false` to keep every screenshot on disk
    /// (rows still rotate) — the escape hatch for anyone who wants to
    /// manage the image cache themselves.
    pub delete_screenshots_with_rotation: bool,
    /// mtime backstop: screenshot files older than this many hours are
    /// deleted by the sweep even when no `perception_frames` row ever
    /// referenced them (a crash between "file written" and "row written"
    /// otherwise leaks the file forever). `0` disables the age sweep
    /// while still allowing row-driven deletes.
    ///
    /// Keep this **at or above** `retention_days * 24` — the sweep does
    /// not consult the database, so a smaller value deletes images whose
    /// frames are still live.
    pub screenshot_max_age_hours: u64,
}

/// Configuration for vault storage (Obsidian-like markdown memory).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryVaultConfig {
    /// Directory path for vault storage. Empty string means "use default location"
    /// (resolved via [`resolve_vault_dir`]).
    pub vault_dir: String,
    /// Debounce delay for file watcher events in milliseconds.
    pub watcher_debounce_ms: u64,
    /// Number of days to retain vault events before cleanup.
    pub events_retention_days: u32,
    /// Maximum number of nodes in the memory graph before truncation.
    pub graph_max_nodes: u32,
}

impl Default for MemoryVaultConfig {
    fn default() -> Self {
        Self {
            vault_dir: String::new(),
            watcher_debounce_ms: 500,
            events_retention_days: 30,
            graph_max_nodes: 1500,
        }
    }
}

impl MemoryVaultConfig {
    /// Resolve the effective vault directory, filling in the default under
    /// `base/vault` when the config value is empty.
    pub fn resolve_vault_dir(&self, base: &Path) -> PathBuf {
        if !self.vault_dir.is_empty() {
            return PathBuf::from(&self.vault_dir);
        }
        base.join("vault")
    }
}

/// Configuration for the memory curator pipeline (Plan B).
///
/// The curator periodically reviews stored memories, scores them for
/// semantic importance, and discards low-confidence duplicates while
/// promoting high-signal episodic entries. Every field is overridable
/// via config per non-negotiable #3.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CuratorConfig {
    /// Whether the curator pipeline is active.
    pub enabled: bool,
    /// Interval between curator passes in minutes.
    pub interval_minutes: u64,
    /// Maximum number of candidate memories to evaluate per pass.
    pub max_candidates_per_pass: u32,
    /// Score threshold [0.0, 1.0] above which memories are auto-promoted
    /// without Claude review.
    pub auto_confirm_threshold: f32,
    /// Score threshold [0.0, 1.0] below which memories are auto-discarded.
    pub discard_floor: f32,
    /// Batch size for Claude evaluation when scoring borderline memories.
    pub claude_batch: u32,
    /// Idle timeout in minutes after which to summarise the current session
    /// into vault notes.
    pub session_summary_idle_minutes: u64,
    /// Maximum number of vault notes to include in wake context.
    pub wake_vault_notes_max: u32,
    /// Whether to include sensitive/personal information in orchestrator context.
    pub include_sensitive_in_context: bool,
    /// Score threshold [0.0, 1.0] at or above which a conflict-detection
    /// verdict of `"supersedes"`/`"contradicts"` (see
    /// `crate::curator::conflict::detect_conflicts`) is strong enough to
    /// propose a `proposes_supersede` relation. Below this, the verdict is
    /// treated as too weak to act on.
    pub supersede_confidence_floor: f32,
    /// Local hour (0-23) for the daily memory-maintenance wake — a
    /// quiet-day guarantee that drains any pending vault decisions even if
    /// nothing else would have woken the orchestrator. Negative disables
    /// the wake entirely. Values >= 24 are clamped to 23 (with a `warn`
    /// logged at the use site) rather than rejected, since a bad config
    /// value should degrade, not crash the runtime.
    pub maintenance_wake_hour: i32,
    /// Minimum elapsed time (`last_activity - started`) a session must
    /// have run before a foreground-process change counts as a real
    /// session boundary worth summarizing, rather than a brief alt-tab
    /// (checking Slack, glancing at a browser tab) getting fragmented into
    /// its own noise-sized session. M1 fix — replaces the previously
    /// hardcoded `MIN_SESSION_MINUTES` constant in
    /// `curator::session::SessionTracker`.
    pub session_min_minutes: u64,
    /// Minimum age (in minutes) a pending vault candidate must have before
    /// it's surfaced in wake context or counted by the daily
    /// memory-maintenance ticker — gives the curator a moment after
    /// writing a candidate before nudging for review of it. M9 fix —
    /// replaces the previously hardcoded 30-minute cutoff in
    /// `memory::retrieval::retrieve_vault_context`/`filter_pending`.
    pub pending_min_age_minutes: u64,
}

impl Default for CuratorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: 10,
            max_candidates_per_pass: 3,
            auto_confirm_threshold: 0.85,
            discard_floor: 0.4,
            claude_batch: 10,
            session_summary_idle_minutes: 20,
            wake_vault_notes_max: 8,
            include_sensitive_in_context: false,
            supersede_confidence_floor: 0.5,
            maintenance_wake_hour: 4,
            session_min_minutes: 5,
            pending_min_age_minutes: 30,
        }
    }
}

/// Configuration for background memory distillation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Whether raw-log to episodic distillation is enabled.
    pub distillation_enabled: bool,
    /// Interval between distillation passes in minutes.
    pub distillation_interval_minutes: u64,
    /// How far back each pass looks for undistilled frames.
    pub distillation_lookback_minutes: u64,
    /// Minimum salience for a frame to be distilled without audio/error signals.
    ///
    /// Applies to the **fallback** (raw-frame) rung of the §4.11
    /// compression ladder only. The primary (deduped `context_events`)
    /// rung has its own threshold — see
    /// [`Self::distillation_min_event_importance`].
    pub distillation_min_salience: f32,
    /// Minimum importance for a deduped `context_events` row to be
    /// distilled — the **primary** rung of the §4.11 compression ladder
    /// (fixwave 3a, I2).
    ///
    /// Separate from [`Self::distillation_min_salience`] because the two
    /// numbers measure different things and the frame threshold silently
    /// disabled the whole primary rung. Deterministic collector events
    /// (focus switches, commits, file changes, system events) are emitted
    /// at [`crate::memory::events::COLLECTOR_EVENT_IMPORTANCE`] = 0.2 —
    /// deliberately low, because individually they are bookkeeping — but
    /// they are exactly the rows the ladder is supposed to compress into
    /// "what happened today". With one shared threshold of 0.35 every one
    /// of them was excluded and the "primary" source could only ever see
    /// classifier output scored above 0.35.
    ///
    /// Default 0.15: below `COLLECTOR_EVENT_IMPORTANCE` so routine
    /// collector events do reach the distiller, above 0.0 so a
    /// classification that scored a frame as genuinely worthless still
    /// drops out. Raise it toward 0.35 for a quieter episodic store.
    pub distillation_min_event_importance: f32,
    /// Maximum frames to distill in one pass.
    pub distillation_batch_size: usize,
    /// Vault storage settings (Obsidian-like markdown memory).
    #[serde(default)]
    pub vault: MemoryVaultConfig,
    /// Curator pipeline settings (Plan B consumes these; configurable now
    /// per non-negotiable #3).
    #[serde(default)]
    pub curator: CuratorConfig,
    /// Per-type expiry for observation-derived vault candidates (context
    /// engine spec §4.7/§6, Task B3).
    #[serde(default)]
    pub candidate_ttl_days: CandidateTtlDays,
}

/// Per-vault-type time-to-live, in days, for the memory candidates the
/// triage classifier proposes (context engine spec §4.7 consumption, §6
/// config surface).
///
/// A candidate that nobody confirms should not sit in the pending queue
/// forever: `Vault::sweep_expired` archives candidates whose `expires`
/// timestamp has passed, and this table is where that timestamp comes
/// from. `None` (and `0`, treated identically — see
/// [`CandidateTtlDays::for_node_type`]) means "never expires", which is
/// the right default for decisions and preferences: they are statements
/// about how the user works, not transient state.
///
/// Only the types the §4.7 mapping table can produce have knobs. Every
/// other vault type routes through [`CandidateTtlDays::note`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CandidateTtlDays {
    /// `task_progress` → `task` candidates (default 30 days).
    pub task: Option<u32>,
    /// `error` candidates (default 30 days).
    pub error: Option<u32>,
    /// `decision` candidates (default: never expires).
    pub decision: Option<u32>,
    /// `preference` candidates (default: never expires).
    pub preference: Option<u32>,
    /// `success`/`communication`/`other` → `note` candidates, and the
    /// fallback for every other vault type (default 90 days).
    pub note: Option<u32>,
}

impl Default for CandidateTtlDays {
    fn default() -> Self {
        Self {
            task: Some(30),
            error: Some(30),
            decision: None,
            preference: None,
            note: Some(90),
        }
    }
}

impl CandidateTtlDays {
    /// TTL in days for a vault node type, or `None` when candidates of
    /// that type never expire. `Some(0)` in config is normalized to
    /// `None` — a zero-day TTL would expire the candidate before a human
    /// could ever review it, which is never what a user means by "0".
    pub fn for_node_type(&self, node_type: continuum_memory::NodeType) -> Option<u32> {
        use continuum_memory::NodeType;
        let days = match node_type {
            NodeType::Task => self.task,
            NodeType::Error => self.error,
            NodeType::Decision => self.decision,
            NodeType::Preference => self.preference,
            _ => self.note,
        };
        days.filter(|d| *d > 0)
    }
}

/// Configuration for the realtime voice front-end selector.
///
/// Continuum has two voice front-ends:
/// - `"pipeline"` (default) — the existing wake → whisper STT → triage →
///   orchestrator → TTS loop. Segment-granular, interruptible, works on CPU.
/// - `"moshi"` — Kyutai Moshi full-duplex speech-to-speech, run as a
///   `moshi-backend.exe` subprocess speaking a local WebSocket protocol.
///   Requires the `moshi` cargo feature and a CUDA-capable GPU. Short
///   conversational turns are handled natively by Moshi; reasoning/tool
///   turns escalate to the orchestrator (triage-driven, see Phase 3).
///
/// Mode switching takes effect on the next daemon start (the daemon loads
/// voice config once at boot); the dashboard surfaces this via RestartNotice.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceFrontendConfig {
    /// `"pipeline"` (default) or `"moshi"`.
    pub mode: String,
    /// Hugging Face repo id for the Moshi model checkpoint, e.g.
    /// `kyutai/moshiko-candle-q8`. Passed to `moshi-backend` via its config.
    pub moshi_model_repo: String,
    /// Compute device for Moshi: `"cuda"` (recommended, ~8 GB VRAM q8) or
    /// `"cpu"` (far too slow for realtime — documented fallback only).
    pub moshi_device: String,
    /// Local TCP port the Moshi backend should serve its WebSocket on.
    /// Continuum spawns the subprocess with this port and connects to
    /// `ws://127.0.0.1:<port>`.
    pub moshi_port: u16,
    /// Override path to the `moshi-backend` executable. Empty resolves via
    /// `CONTINUUM_MOSHI_BIN` env → `~/.continuum-dev/bin/moshi/moshi-backend.exe`
    /// → `moshi-backend` on PATH (see `resolve_moshi_binary` in voice/moshi.rs).
    pub moshi_bin: String,
    /// Whether the audio tap in `senses/audio/full.rs` forks 16 kHz mono PCM
    /// to the Moshi front-end. Separate from `mode` so the tap can be
    /// disabled for debugging without flipping the whole mode. Only
    /// consulted when `mode == "moshi"`.
    pub moshi_tap_enabled: bool,
}

impl Default for VoiceFrontendConfig {
    fn default() -> Self {
        Self {
            mode: "pipeline".to_string(),
            // Kyutai's quantized 8-bit Moshi checkpoint — ~8 GB VRAM, the
            // lowest-VRAM realtime option. See scripts/download-models.ps1.
            moshi_model_repo: "kyutai/moshiko-candle-q8".to_string(),
            moshi_device: "cuda".to_string(),
            moshi_port: 8084,
            moshi_bin: String::new(),
            moshi_tap_enabled: true,
        }
    }
}

/// Configuration for the voice input loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// Whether voice input routing is enabled.
    pub enabled: bool,
    /// Whether one-click live voice should arm automatically when the desktop
    /// opens. This is the durable counterpart of the Home button's state.
    pub live_voice_auto_start: bool,
    /// Whether a wake phrase is required before voice commands wake the orchestrator.
    pub wake_word_enabled: bool,
    /// Wake phrase to detect in local STT transcripts.
    pub wake_keyword: String,
    /// Porcupine sensitivity placeholder, kept configurable for the native detector path.
    pub wake_sensitivity: f32,
    /// Optional Porcupine .ppn path. Empty means transcript wake detection is used.
    pub custom_keyword_path: String,
    /// Maximum time to keep a voice session open after wake.
    pub listen_timeout_ms: u64,
    /// Silence/endpointer timeout after the last transcript update.
    pub endpoint_silence_ms: u64,
    /// Minimum non-wake transcript length before a command is considered complete.
    pub min_utterance_chars: usize,
    /// Stop playback when fresh user speech arrives while Continuum is speaking.
    pub barge_in_enabled: bool,
    /// Suppress spoken output while call detection reports a live call.
    pub ambient_mute_enabled: bool,
    /// Route TTS voice by detected language when a matching voice is configured.
    pub language_detection_enabled: bool,
    /// Language used when STT language is unknown or unsupported.
    pub default_language: String,
    /// Master playback gain [0.0, 1.0]. Applied in the cpal fill callback so
    /// the config change takes effect on the next audio buffer.
    pub volume: f32,
    /// Play short audio cues (chime on wake, click on active-listen, double-beep
    /// on error) when the voice state transitions. Disable for pure silence.
    pub feedback_sounds: bool,
    /// Global hotkey for toggle-listen (empty string disables the hotkey).
    /// Modifier chord in the form "Ctrl+Shift+K" / "Alt+F12" / "Win+Space".
    pub hotkey: String,
    /// After Continuum finishes speaking, keep the voice session alive this many
    /// seconds so the user can ask a follow-up without re-triggering the wake
    /// word. `0` disables conversation mode.
    pub conversation_followup_seconds: u64,
    /// Selects the realtime voice front-end (`pipeline` vs `moshi`). See
    /// [`VoiceFrontendConfig`].
    pub frontend: VoiceFrontendConfig,
}

/// Configuration for the text-to-speech pipeline.
///
/// Continuum supports multiple Piper voices — typically one per language.
/// The language-routing logic picks a voice based on the detected speech
/// language. ElevenLabs is an optional cloud plugin, disabled by default
/// per the ROADMAP's local-first stance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    /// Whether TTS is enabled at all. When `false`, whisper triage
    /// decisions and orchestrator responses are logged but not spoken.
    pub enabled: bool,
    /// Which TTS engine to use. `"piper"` (default, local),
    /// `"kokoros"` (local ONNX, higher quality), or `"elevenlabs"` (cloud
    /// plugin, requires API key — stub).
    pub engine: String,
    /// Directory holding the espeak-ng dictionary files required by
    /// Piper's phonemizer. Must exist at runtime.
    pub espeak_data_dir: String,
    /// Per-language voice catalog. Keys are BCP-47 short codes (`en`,
    /// `nl`). The `primary` field selects which one is used when language
    /// detection is unavailable or disabled.
    pub voices: std::collections::HashMap<String, TtsVoiceConfig>,
    /// BCP-47 language code of the default voice (must appear in `voices`).
    pub primary: String,
    /// Piper `length_scale` parameter; `None` uses the voice's native
    /// value. Values below 1.0 speed up speech, above 1.0 slow it down.
    pub length_scale: Option<f32>,
    /// ElevenLabs streaming cloud TTS (optional plugin backend).
    pub elevenlabs: ElevenLabsConfig,
    /// Kokoros local ONNX TTS (optional, higher-quality Piper alternative).
    pub kokoros: KokorosConfig,
}

/// A single Piper voice entry.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsVoiceConfig {
    /// Absolute path to the `.onnx` model file.
    pub model_path: String,
    /// Absolute path to the `.onnx.json` config sidecar.
    pub config_path: String,
    /// Optional speaker id for multi-speaker models. `None` for
    /// single-speaker voices like `en_US-norman-medium`.
    pub speaker_id: Option<i64>,
}

/// ElevenLabs streaming TTS configuration (optional cloud plugin).
///
/// Phase 5 is local-first: Piper is the only supported production backend.
/// This struct defines the config surface so the cloud plugin can be wired
/// up in a later minor release without breaking user configs in the
/// interim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ElevenLabsConfig {
    /// User-provided ElevenLabs API key. Empty string means the backend
    /// is disabled regardless of `tts.engine`.
    pub api_key: String,
    /// ElevenLabs voice ID (see https://elevenlabs.io/app/voice-library).
    pub voice_id: String,
    /// Model ID — `eleven_turbo_v2_5` (fastest) is the default target.
    pub model_id: String,
    /// Voice stability [0.0, 1.0]. Higher = more predictable prosody.
    pub stability: f32,
    /// Similarity boost [0.0, 1.0]. Higher = closer to reference voice.
    pub similarity_boost: f32,
}

impl Default for ElevenLabsConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            voice_id: String::new(),
            model_id: "eleven_turbo_v2_5".to_string(),
            stability: 0.5,
            similarity_boost: 0.75,
        }
    }
}

/// Kokoros local TTS configuration (ONNX backend, optional).
///
/// Kokoros ([Kokoro-82M](https://github.com/lucasjinreal/Kokoros)) is a
/// higher-quality, more natural-sounding local alternative to Piper at the
/// cost of slightly higher latency. Like Piper it is invoked as a subprocess
/// (`koko`) so the ONNX Runtime dependency stays out of Continuum's Rust
/// dependency graph. The `koko` binary is resolved at runtime by
/// [`crate::voice::tts::resolve_koko_binary`] (env override
/// `CONTINUUM_KOKO_BIN`, then `~/.continuum-dev/bin/kokoros/koko.exe`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KokorosConfig {
    /// Path to `kokoro-v1.0.onnx`.
    pub model_path: String,
    /// Path to `voices-v1.0.bin` (the bundled voice-style catalog).
    pub voices_path: String,
    /// Kokoros voice style, e.g. `af_sky` or a blend like
    /// `af_sarah.4+af_nicole.6`. Passed to `koko --style`.
    pub voice_name: String,
    /// Speech-rate multiplier (1.0 = native). Passed to `koko --speed`.
    pub speed: f32,
}

impl Default for KokorosConfig {
    fn default() -> Self {
        let kokoro_dir = continuum_dev_dir()
            .join("models")
            .join("tts")
            .join("kokoro");
        Self {
            model_path: kokoro_dir
                .join("kokoro-v1.0.onnx")
                .to_string_lossy()
                .into_owned(),
            voices_path: kokoro_dir
                .join("voices-v1.0.bin")
                .to_string_lossy()
                .into_owned(),
            voice_name: "af_sky".to_string(),
            speed: 1.0,
        }
    }
}

/// Configuration for the worker pool (Phase 8).
///
/// Workers are independent Claude Code subprocesses spawned by the orchestrator
/// to do actual work. The pool enforces concurrency limits, selects models, and
/// tracks lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkersConfig {
    /// Agent CLI used for worker execution: `claude`, `codex`, or `hermes`.
    pub agent: String,
    /// Optional provider override for agent runtimes that support one.
    pub provider: String,
    /// Global mode. `"auto"` lets the orchestrator's explicit choice win, and
    /// falls back to a keyword heuristic. `"budget"` forces Sonnet for every
    /// worker. `"power"` forces Opus.
    pub mode: String,
    /// Model id used when mode is `"budget"` or the heuristic picks Sonnet.
    pub budget_model: String,
    /// Model id used when mode is `"power"` or the heuristic picks Opus.
    pub power_model: String,
    /// Maximum workers running at once. Excess requests queue. Hard cap: 10.
    pub max_concurrent: usize,
    /// Default wall-clock timeout per worker (seconds).
    pub default_timeout_secs: u64,
    /// Default CSV of tool names the worker may use. `mcp__continuum__*` by default
    /// plus the standard Claude Code built-ins. Workers never get
    /// `mcp__continuum__workers__*` — that would allow spawning sub-workers
    /// directly from a worker, which we route through the orchestrator instead.
    pub default_allowed_tools: String,
    /// How often the dashboard and MCP server see worker state updates, in ms.
    pub status_refresh_ms: u64,
    /// If a single task pattern fails this many times within `failure_window_secs`,
    /// the pool refuses to run it again until restart and surfaces an escalation.
    pub failure_streak_limit: u32,
    /// Window over which `failure_streak_limit` is counted.
    pub failure_window_secs: u64,
}

/// Configuration for the skills system (Phase 8).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Whether the skills system is active at all.
    pub enabled: bool,
    /// Directory holding `<name>/SKILL.md` files. Relative paths resolve
    /// relative to the current working directory at load time.
    pub dir: String,
    /// Reload SKILL.md files when they change on disk.
    pub hot_reload: bool,
    /// Approximate token budget for injected skill content per wake. Matching
    /// skills are appended until the budget is reached, in match-score order.
    pub token_budget: usize,
    /// Skills explicitly disabled by name. Third-party or noisy skills can be
    /// silenced without deleting the directory.
    pub disabled: Vec<String>,
}

impl Default for WorkersConfig {
    fn default() -> Self {
        Self {
            agent: "claude".to_string(),
            provider: String::new(),
            mode: "auto".to_string(),
            budget_model: "claude-sonnet-4-6".to_string(),
            power_model: "claude-opus-4-6".to_string(),
            max_concurrent: 3,
            default_timeout_secs: 1800,
            default_allowed_tools:
                "Read,Write,Edit,Glob,Grep,Bash,mcp__continuum__memory_*,mcp__continuum__system_*,\
                 mcp__continuum__fs_*,mcp__continuum__web_fetch"
                    .to_string(),
            status_refresh_ms: 500,
            failure_streak_limit: 3,
            failure_window_secs: 600,
        }
    }
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: "skills".to_string(),
            hot_reload: true,
            token_budget: 2000,
            disabled: Vec::new(),
        }
    }
}

// --- Defaults ---

impl Default for ContinuumConfig {
    fn default() -> Self {
        let base = continuum_dev_dir();
        Self {
            health: HealthConfig::default(),
            vision: VisionConfig::default(),
            screen: ScreenConfig::default(),
            audio: AudioConfig::default(),
            context: ContextConfig::default(),
            frame: FrameConfig::default(),
            storage: StorageConfig {
                db_path: base.join("raw_log.sqlite").to_string_lossy().into_owned(),
                screenshots_dir: base.join("screenshots").to_string_lossy().into_owned(),
                retention_days: 30,
                delete_screenshots_with_rotation: true,
                screenshot_max_age_hours: 720,
            },
            memory: MemoryConfig::default(),
            voice: VoiceConfig::default(),
            tts: TtsConfig::default(),
            workers: WorkersConfig::default(),
            skills: SkillsConfig::default(),
            orchestrator: OrchestratorSection::default(),
            triage: TriageSection::default(),
            resources: ResourceConfig::default(),
            chat: ChatConfig::default(),
            privacy: PrivacyConfig::default(),
            projects: ProjectsConfig::default(),
            git_context: GitContextConfig::default(),
            events: EventsConfig::default(),
            file_watcher: FileWatcherConfig::default(),
            process_watcher: ProcessWatcherConfig::default(),
            performance: PerformanceConfig::default(),
            session_state: SessionStateConfig::default(),
            context_package: ContextPackageConfig::default(),
            continuation: ContinuationConfig::default(),
            context_tools: ContextToolsConfig::default(),
            agent_os: AgentOsConfig::default(),
            github: GitHubConfig::default(),
        }
    }
}

impl Default for OrchestratorSection {
    fn default() -> Self {
        Self {
            agent: "claude".to_string(),
            provider: String::new(),
            model_id: "claude-opus-4-6".to_string(),
            wake_timeout_secs: 60,
            bare_mode: false,
        }
    }
}

impl Default for TriageSection {
    fn default() -> Self {
        Self {
            // Empty means "derive from dev_dir" at load time.
            model_path: String::new(),
            context_size: 4096,
            max_tokens: 256,
            temperature: 0.0,
            gpu_layers: 999,
            latency_warn_ms: 2000,
        }
    }
}

impl TriageSection {
    /// Resolve the effective `.gguf` path, filling in the default under
    /// `dev_dir/models/triage/qwen3-8b-q4_k_m.gguf` when the config value
    /// is empty. Callers pass the current `dev_dir`; this avoids baking a
    /// user-specific path into the serialised defaults.
    pub fn resolve_model_path(&self, dev_dir: &Path) -> PathBuf {
        if !self.model_path.is_empty() {
            return PathBuf::from(&self.model_path);
        }
        dev_dir
            .join("models")
            .join("triage")
            .join("qwen3-8b-q4_k_m.gguf")
    }
}

impl Default for VisionConfig {
    fn default() -> Self {
        let models_dir = continuum_dev_dir().join("models").join("vision");
        Self {
            name: "SmolVLM2-2.2B Q4_K_M".to_string(),
            backend: "auto".to_string(),
            model_path: models_dir
                .join("smolvlm2-2.2b-q4")
                .to_string_lossy()
                .into_owned(),
            fallback_model_path: models_dir
                .join("smolvlm-500m")
                .to_string_lossy()
                .into_owned(),
            gpu_enabled: true,
            input_width: 384,
            input_height: 384,
            prompt: "Describe the visible computer screen accurately. Include the application or scene, the main action or status, important readable text, and any visible error. Use one concise factual sentence.".to_string(),
            max_new_tokens: 64,
            processor_max_edge: 1536,
            image_splitting: true,
        }
    }
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 3,
            capture_interval_ms: 20,
            capture_width: 1280,
            capture_height: 720,
            save_screenshots: false,
            all_monitors: true,
            excluded_monitor_ids: Vec::new(),
            buffer_capacity: 64,
            meaningful_change_threshold: 0.025,
            // No artificial delay: changed keyframes are described
            // continuously, at the throughput the selected local model and
            // hardware can actually sustain.
            vision_min_interval_ms: 20,
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        let models_dir = continuum_dev_dir().join("models").join("stt");
        Self {
            enabled: true,
            // whisper-medium (1.5 GB) recognises proper nouns like "Continuum"
            // reliably where whisper-small (244 MB) hallucinates real-word
            // mistranscriptions. The 3x size cost is paid for by the wake
            // gate actually working. Swap to whisper-small.bin if memory
            // is constrained or you don't mind more mis-detections.
            whisper_model_path: models_dir
                .join("whisper-medium.bin")
                .to_string_lossy()
                .into_owned(),
            // Force English for the default wake path. Whisper-small's
            // language auto-detect on short 1-2 second clips is unreliable
            // (we've seen "hey continuum" mis-detected as Portuguese "Ei,
            // Cairo!" with p=0.55). Forcing "en" keeps the wake word
            // intelligible. Users who primarily speak a different language
            // override this in ~/.continuum-dev/config.toml.
            whisper_language: "en".to_string(),
            // Adaptive VAD: floor 0.005 catches quiet speech; the 5×
            // noise-floor multiplier raises the effective threshold on
            // noisy setups automatically. See `AdaptiveVad` for details.
            vad_threshold: 0.005,
            vad_noise_floor_multiplier: 5.0,
            // 800 ms of trailing silence before a segment closes. Natural
            // mid-sentence pauses (commas, thinking) stay inside one segment
            // so Whisper sees the whole utterance.
            silence_duration_ms: 800,
            max_segment_secs: 8,
            // Whisper threads default 4; the runtime overrides this at boot
            // with the resolved resource plan (a fraction of cores with
            // headroom). whisper_use_gpu defaults off; the runtime sets it
            // from the plan's GPU decision (effective only with the `cuda`
            // build feature).
            whisper_threads: 4,
            whisper_use_gpu: false,
            device_name: String::new(),
            device_index: None,
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 1,
            redact_sensitive_titles: true,
            sensitive_process_names: vec![
                "1password.exe".into(),
                "bitwarden.exe".into(),
                "keepass.exe".into(),
                "keepassxc.exe".into(),
                "credentialuibroker.exe".into(),
            ],
            sensitive_title_keywords: vec![
                "password".into(),
                "passkey".into(),
                "two-factor".into(),
                "2fa".into(),
                "private key".into(),
                "seed phrase".into(),
            ],
            terminal_process_names: vec![
                "windowsterminal.exe".into(),
                "powershell.exe".into(),
                "pwsh.exe".into(),
                "cmd.exe".into(),
                "bash.exe".into(),
                "wsl.exe".into(),
            ],
        }
    }
}

impl Default for FrameConfig {
    fn default() -> Self {
        Self {
            interval_secs: 3,
            // Lowered from 0.15 to 0.10: most frames score 0.00 in steady state,
            // and the triage layer (Phase 2) is cheap enough to run on
            // window-change events (salience ~0.20). Tune up if triage call
            // volume is too high.
            salience_threshold: 0.10,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        let base = continuum_dev_dir();
        Self {
            db_path: base.join("raw_log.sqlite").to_string_lossy().into_owned(),
            screenshots_dir: base.join("screenshots").to_string_lossy().into_owned(),
            retention_days: 30,
            delete_screenshots_with_rotation: true,
            screenshot_max_age_hours: 720,
        }
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            distillation_enabled: true,
            distillation_interval_minutes: 15,
            distillation_lookback_minutes: 20,
            distillation_min_salience: 0.35,
            distillation_min_event_importance: 0.15,
            distillation_batch_size: 100,
            vault: MemoryVaultConfig::default(),
            curator: CuratorConfig::default(),
            candidate_ttl_days: CandidateTtlDays::default(),
        }
    }
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            live_voice_auto_start: true,
            wake_word_enabled: true,
            wake_keyword: "hey continuum".to_string(),
            wake_sensitivity: 0.5,
            custom_keyword_path: String::new(),
            listen_timeout_ms: 12_000,
            endpoint_silence_ms: 700,
            min_utterance_chars: 3,
            barge_in_enabled: true,
            ambient_mute_enabled: true,
            // Disabled by default: Continuum speaks English only. Whisper still
            // transcribes any spoken language (audio.whisper_language =
            // "auto"), so Continuum understands multilingual input but routes
            // all TTS through the English primary voice. Enable when every
            // target language has a quality Piper voice configured.
            language_detection_enabled: false,
            default_language: "en".to_string(),
            volume: 0.8,
            feedback_sounds: true,
            hotkey: "Ctrl+Shift+K".to_string(),
            conversation_followup_seconds: 5,
            frontend: VoiceFrontendConfig::default(),
        }
    }
}

impl Default for TtsConfig {
    fn default() -> Self {
        let tts_dir = continuum_dev_dir().join("models").join("tts");
        // English-only by default. The Dutch Piper voices available as of
        // 2026-04 (nl_NL-mls-medium) produce barely-intelligible speech, so
        // we don't ship a second voice in the default bank. Users who want
        // multilingual TTS add a [tts.voices.<lang>] section and flip
        // voice.language_detection_enabled to true.
        let mut voices = std::collections::HashMap::new();
        voices.insert(
            "en".to_string(),
            TtsVoiceConfig {
                model_path: tts_dir
                    .join("en_US-norman-medium.onnx")
                    .to_string_lossy()
                    .into_owned(),
                config_path: tts_dir
                    .join("en_US-norman-medium.onnx.json")
                    .to_string_lossy()
                    .into_owned(),
                speaker_id: None,
            },
        );
        Self {
            enabled: true,
            engine: "piper".to_string(),
            espeak_data_dir: tts_dir
                .join("espeak-ng-data")
                .to_string_lossy()
                .into_owned(),
            voices,
            primary: "en".to_string(),
            length_scale: None,
            elevenlabs: ElevenLabsConfig::default(),
            kokoros: KokorosConfig::default(),
        }
    }
}

/// Returns the Continuum development directory (`~/.continuum-dev/`).
///
/// On first call after a Kairo→Continuum upgrade, this migrates the legacy
/// `~/.kairo-dev/` into `~/.continuum-dev/` (atomic rename on the same volume).
/// If the rename fails (e.g. an anti-virus lock on a model file), the legacy
/// directory is returned for this run so the user keeps their data; the next
/// run retries the migration.
pub fn continuum_dev_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    migrated_dir(&home.join(".kairo-dev"), &home.join(".continuum-dev"))
}

/// Returns the Continuum production data directory (`~/.continuum/`), migrating
/// it from the legacy `~/.kairo/` on first use. See [`continuum_dev_dir`] for
/// migration semantics.
pub fn continuum_data_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    migrated_dir(&home.join(".kairo"), &home.join(".continuum"))
}

/// Returns the Continuum backups directory (`~/.continuum-backups/`), migrating
/// it from the legacy `~/.kairo-backups/` on first use. See
/// [`continuum_dev_dir`] for migration semantics.
pub fn continuum_backups_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    migrated_dir(
        &home.join(".kairo-backups"),
        &home.join(".continuum-backups"),
    )
}

/// Read an env var, falling back to its legacy `KAIRO_*` name during the
/// Kairo→Continuum migration. Returns the value of `new` when set and non-empty,
/// otherwise the value of `old` when set and non-empty, otherwise `None`.
///
/// This is the only place the legacy `KAIRO_*` literals are kept; they are
/// transitional and can be removed once users have migrated to the `CONTINUUM_*`
/// names.
pub fn env_or_legacy(new: &str, old: &str) -> Option<String> {
    match std::env::var(new) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => std::env::var(old).ok().filter(|v| !v.is_empty()),
    }
}

/// Resolve `new`, migrating the contents of `old` into it on first use.
/// Returns `new` when it already exists or was migrated, or `old` when the
/// migration failed and `old` still exists (so this run keeps using the legacy
/// data). When neither exists, returns `new` so the caller creates it.
fn migrated_dir(old: &Path, new: &Path) -> PathBuf {
    if new.exists() {
        return new.to_path_buf();
    }
    if !old.exists() {
        return new.to_path_buf();
    }
    match std::fs::rename(old, new) {
        Ok(()) => {
            tracing::info!(
                layer = "config",
                component = "continuum",
                old = %old.display(),
                new = %new.display(),
                "Migrated legacy data directory"
            );
            new.to_path_buf()
        }
        Err(e) => {
            tracing::warn!(
                layer = "config",
                component = "continuum",
                old = %old.display(),
                new = %new.display(),
                error = %e,
                "Legacy data directory migration failed; using legacy path for this run"
            );
            old.to_path_buf()
        }
    }
}

/// Load configuration from a TOML file, falling back to defaults for missing keys.
pub fn load_config(path: &Path) -> Result<ContinuumConfig> {
    if path.exists() {
        let contents = std::fs::read_to_string(path).context("Failed to read config file")?;
        let config: ContinuumConfig =
            toml::from_str(&contents).context("Failed to parse config TOML")?;
        config
            .resources
            .validate()
            .context("Invalid [resources] config")?;
        config
            .vision
            .validate()
            .context("Invalid [vision] config")?;
        config
            .screen
            .validate()
            .context("Invalid [screen] config")?;
        Ok(config)
    } else {
        tracing::info!(
            layer = "senses",
            component = "config",
            "No config file at {}, using defaults",
            path.display()
        );
        Ok(ContinuumConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_valid() {
        let config = ContinuumConfig::default();
        config.screen.validate().expect("default screen config");
        assert_eq!(config.screen.interval_secs, 3);
        assert_eq!(config.screen.capture_interval_ms, 20);
        assert_eq!(config.screen.vision_min_interval_ms, 20);
        assert_eq!(config.health.backup_retention, 7);
        assert_eq!(config.health.runtime_start_timeout_secs, 90);
        assert_eq!(config.frame.salience_threshold, 0.10);
        assert_eq!(config.storage.retention_days, 30);
        // Task B6 (spec §6): screenshots are deleted with rotation, and
        // the mtime backstop matches retention_days * 24.
        assert!(config.storage.delete_screenshots_with_rotation);
        assert_eq!(config.storage.screenshot_max_age_hours, 720);
        assert_eq!(
            config.storage.screenshot_max_age_hours,
            config.storage.retention_days as u64 * 24,
            "the backstop must not delete images whose frames are still live"
        );
        assert!(config.memory.distillation_enabled);
        assert_eq!(config.memory.distillation_interval_minutes, 15);
        assert!(config.voice.enabled);
        assert_eq!(config.voice.wake_keyword, "hey continuum");
        assert_eq!(config.audio.max_segment_secs, 8);
        assert!(config.vision.gpu_enabled);
        assert!(config.tts.enabled);
        assert_eq!(config.tts.primary, "en");
        assert!(config.tts.voices.contains_key("en"));
        // Continuum is English-only by default; Dutch is opt-in (see
        // TtsConfig::default docs).
        assert!(!config.tts.voices.contains_key("nl"));
        assert!(!config.voice.language_detection_enabled);
        // Default is forced to "en" to make the wake word reliable on short
        // clips — see AudioConfig::default docs.
        assert_eq!(config.audio.whisper_language, "en");
    }

    #[test]
    fn test_tts_config_parses_from_toml() {
        let toml_str = r#"
[tts]
enabled = false
espeak_data_dir = "/tmp/espeak-ng-data"
primary = "nl"

[tts.voices.nl]
model_path = "/tmp/voice.onnx"
config_path = "/tmp/voice.onnx.json"
speaker_id = 2
"#;
        let config: ContinuumConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.tts.enabled);
        assert_eq!(config.tts.primary, "nl");
        let nl = &config.tts.voices["nl"];
        assert_eq!(nl.model_path, "/tmp/voice.onnx");
        assert_eq!(nl.speaker_id, Some(2));
    }

    #[test]
    fn test_voice_config_parses_from_toml() {
        let toml_str = r#"
[voice]
enabled = true
wake_word_enabled = false
wake_keyword = "continuum"
listen_timeout_ms = 5000
barge_in_enabled = false
"#;
        let config: ContinuumConfig = toml::from_str(toml_str).unwrap();
        assert!(config.voice.enabled);
        assert!(!config.voice.wake_word_enabled);
        assert_eq!(config.voice.wake_keyword, "continuum");
        assert_eq!(config.voice.listen_timeout_ms, 5000);
        assert!(!config.voice.barge_in_enabled);
    }

    #[test]
    fn test_memory_config_parses_from_toml() {
        let toml_str = r#"
[memory]
distillation_enabled = false
distillation_interval_minutes = 5
distillation_min_salience = 0.25
distillation_batch_size = 12
"#;
        let config: ContinuumConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.memory.distillation_enabled);
        assert_eq!(config.memory.distillation_interval_minutes, 5);
        assert_eq!(config.memory.distillation_min_salience, 0.25);
        assert_eq!(config.memory.distillation_batch_size, 12);
    }

    #[test]
    fn storage_screenshot_policy_parses_from_toml_and_fills_defaults() {
        // Both keys override…
        let config: ContinuumConfig = toml::from_str(
            r#"
[storage]
delete_screenshots_with_rotation = false
screenshot_max_age_hours = 24
"#,
        )
        .unwrap();
        assert!(!config.storage.delete_screenshots_with_rotation);
        assert_eq!(config.storage.screenshot_max_age_hours, 24);
        assert_eq!(
            config.storage.retention_days, 30,
            "unset keys keep defaults"
        );

        // …and a `[storage]` section written before Task B6 still loads.
        let legacy: ContinuumConfig = toml::from_str(
            r#"
[storage]
retention_days = 7
"#,
        )
        .unwrap();
        assert_eq!(legacy.storage.retention_days, 7);
        assert!(legacy.storage.delete_screenshots_with_rotation);
        assert_eq!(legacy.storage.screenshot_max_age_hours, 720);
    }

    #[test]
    fn test_load_missing_config_returns_defaults() {
        let config = load_config(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(config.screen.interval_secs, 3);
    }

    #[test]
    fn test_partial_toml_fills_defaults() {
        let toml_str = r#"
[screen]
interval_secs = 5
"#;
        let config: ContinuumConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.screen.interval_secs, 5);
        // Other fields should be defaults
        assert_eq!(config.frame.salience_threshold, 0.10);
    }

    #[test]
    fn vision_config_rejects_invalid_generation_contract() {
        let mut config = VisionConfig::default();
        assert!(config.validate().is_ok());

        config.prompt.clear();
        assert!(config.validate().is_err());
        config = VisionConfig::default();
        config.max_new_tokens = 0;
        assert!(config.validate().is_err());
        config = VisionConfig::default();
        config.processor_max_edge = 1000;
        assert!(config.validate().is_err());
    }

    #[test]
    fn process_watcher_is_opt_in_and_every_threshold_is_overridable() {
        let defaults = ContinuumConfig::default().process_watcher;
        assert!(!defaults.enabled);
        assert_eq!(defaults.poll_secs, 2);
        assert!(defaults.include_names.iter().any(|name| name == "cargo"));

        let parsed: ContinuumConfig = toml::from_str(
            r#"
[process_watcher]
enabled = true
poll_secs = 5
cpu_threshold_percent = 60.0
memory_threshold_mb = 1024
sustained_samples = 4
include_names = ["my-builder"]
exclude_names = ["private-app"]
snapshot_limit = 12
"#,
        )
        .expect("parse process watcher");
        assert!(parsed.process_watcher.enabled);
        assert_eq!(parsed.process_watcher.poll_secs, 5);
        assert_eq!(parsed.process_watcher.cpu_threshold_percent, 60.0);
        assert_eq!(parsed.process_watcher.memory_threshold_mb, 1024);
        assert_eq!(parsed.process_watcher.sustained_samples, 4);
        assert_eq!(parsed.process_watcher.include_names, ["my-builder"]);
        assert_eq!(parsed.process_watcher.exclude_names, ["private-app"]);
        assert_eq!(parsed.process_watcher.snapshot_limit, 12);
    }

    #[test]
    fn chat_config_defaults_and_parse() {
        let cfg = ChatConfig::default();
        assert_eq!(cfg.max_tokens, 8192);
        assert_eq!(cfg.connect_timeout_secs, 10);
        assert_eq!(cfg.stream_idle_timeout_secs, 60);
        assert_eq!(cfg.cli_timeout_secs, 120);
        assert_eq!(cfg.model_refresh_interval_secs, 300);
        assert!(cfg.temperature.is_none());
        assert!(cfg.system_prompt_path.is_none());
        assert!(cfg.memory_tools_enabled);
        assert_eq!(cfg.memory_tool_max_rounds, 8);
        assert_eq!(cfg.memory_context_notes_max, 6);
        assert!(!cfg.include_sensitive_memory);
        assert_eq!(cfg.history_message_window, 20);

        let parsed: ContinuumConfig =
            toml::from_str(
                "[chat]\nmax_tokens = 2048\nstream_idle_timeout_secs = 30\nmodel_refresh_interval_secs = 90\n",
            )
                .expect("parse");
        assert_eq!(parsed.chat.max_tokens, 2048);
        assert_eq!(parsed.chat.stream_idle_timeout_secs, 30);
        assert_eq!(parsed.chat.model_refresh_interval_secs, 90);
        // A partial [chat] section keeps the memory-tool defaults:
        assert!(parsed.chat.memory_tools_enabled);
        assert_eq!(parsed.chat.memory_tool_max_rounds, 8);
        assert_eq!(parsed.chat.memory_context_notes_max, 6);
        assert!(!parsed.chat.include_sensitive_memory);
        // Omitting [chat] entirely must also work:
        let empty: ContinuumConfig = toml::from_str("").expect("parse empty");
        assert_eq!(empty.chat.max_tokens, 8192);
        // And every memory-tool knob is overridable (non-negotiable #3):
        let overridden: ContinuumConfig = toml::from_str(
            "[chat]\nmemory_tools_enabled = false\nmemory_tool_max_rounds = 3\nmemory_context_notes_max = 0\ninclude_sensitive_memory = true\n",
        )
        .expect("parse overrides");
        assert!(!overridden.chat.memory_tools_enabled);
        assert_eq!(overridden.chat.memory_tool_max_rounds, 3);
        assert_eq!(overridden.chat.memory_context_notes_max, 0);
        assert!(overridden.chat.include_sensitive_memory);
        // The history window is overridable, and 0 opts out of windowing:
        let windowed: ContinuumConfig =
            toml::from_str("[chat]\nhistory_message_window = 0\n").expect("parse window");
        assert_eq!(windowed.chat.history_message_window, 0);
    }

    #[test]
    fn privacy_config_defaults_match_spec() {
        // Spec §6 defaults are normative.
        let cfg = ContinuumConfig::default();
        assert!(cfg.privacy.scrub_api_keys);
        assert!(cfg.privacy.scrub_cards);
        assert!(cfg.privacy.scrub_iban);
        assert!(!cfg.privacy.scrub_emails);
        assert!(cfg.privacy.zones.is_empty());
        assert!(cfg.privacy.toggles.mic);
        assert!(cfg.privacy.toggles.screen);
        assert!(cfg.privacy.toggles.files);
        assert!(cfg.privacy.toggles.git);
        assert!(!cfg.privacy.toggles.pause_all);
        // Omitting [privacy] entirely parses to the same defaults.
        let empty: ContinuumConfig = toml::from_str("").unwrap();
        assert!(empty.privacy.scrub_api_keys);
        assert!(!empty.privacy.toggles.pause_all);
    }

    #[test]
    fn privacy_config_parses_from_toml() {
        use crate::senses::privacy::Zone;
        let toml_str = r#"
[privacy]
scrub_emails = true
scrub_iban = false

[privacy.toggles]
screen = false
pause_all = true

[[privacy.zones]]
match_process = "signal.exe"
zone = "never_observe"

[[privacy.zones]]
match_title_keyword = "banking"
zone = "local_only"
"#;
        let config: ContinuumConfig = toml::from_str(toml_str).unwrap();
        assert!(config.privacy.scrub_emails);
        assert!(!config.privacy.scrub_iban);
        assert!(config.privacy.scrub_api_keys); // default backfill
        assert!(!config.privacy.toggles.screen);
        assert!(config.privacy.toggles.pause_all);
        assert!(config.privacy.toggles.mic); // default backfill
        assert_eq!(config.privacy.zones.len(), 2);
        assert_eq!(
            config.privacy.zones[0].match_process.as_deref(),
            Some("signal.exe")
        );
        assert_eq!(config.privacy.zones[0].zone, Zone::NeverObserve);
        assert_eq!(
            config.privacy.zones[1].match_title_keyword.as_deref(),
            Some("banking")
        );
        assert_eq!(config.privacy.zones[1].zone, Zone::LocalOnly);
        // Round-trips through TOML serialisation (dashboard writes config).
        let serialised = toml::to_string(&config).unwrap();
        let reparsed: ContinuumConfig = toml::from_str(&serialised).unwrap();
        assert_eq!(reparsed.privacy.zones, config.privacy.zones);
        assert!(reparsed.privacy.toggles.pause_all);
    }

    #[test]
    fn projects_config_defaults_match_spec() {
        // Spec §6 defaults are normative.
        let cfg = ContinuumConfig::default();
        assert!(cfg.projects.auto_discover);
        assert_eq!(cfg.projects.switch_min_secs, 20);
        assert_eq!(cfg.projects.discover_min_secs, 30);
        assert!(cfg.projects.known.is_empty());
        // Omitting [projects] entirely parses to the same defaults.
        let empty: ContinuumConfig = toml::from_str("").unwrap();
        assert!(empty.projects.auto_discover);
        assert_eq!(empty.projects.switch_min_secs, 20);
    }

    #[test]
    fn projects_config_parses_from_toml() {
        use crate::senses::privacy::Zone;
        let toml_str = r#"
[projects]
auto_discover = false
switch_min_secs = 45

[[projects.known]]
id = "continuum"
name = "Continuum"
root_paths = ["D:\\Continuum\\Continuum-main"]
repo = "https://github.com/x/continuum"
keywords = ["continuum"]

[[projects.known]]
id = "finances"
root_paths = ["D:\\Private\\finances"]
zone = "local_only"
"#;
        let config: ContinuumConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.projects.auto_discover);
        assert_eq!(config.projects.switch_min_secs, 45);
        assert_eq!(config.projects.discover_min_secs, 30); // default backfill
        assert_eq!(config.projects.known.len(), 2);
        let first = &config.projects.known[0];
        assert_eq!(first.id, "continuum");
        assert_eq!(first.name, "Continuum");
        assert_eq!(first.root_paths, ["D:\\Continuum\\Continuum-main"]);
        assert_eq!(
            first.repo.as_deref(),
            Some("https://github.com/x/continuum")
        );
        assert_eq!(first.keywords, ["continuum"]);
        assert_eq!(first.zone, None);
        let second = &config.projects.known[1];
        assert_eq!(second.id, "finances");
        assert!(second.name.is_empty());
        assert_eq!(second.zone, Some(Zone::LocalOnly));
        // Round-trips through TOML serialisation (dashboard writes config).
        let serialised = toml::to_string(&config).unwrap();
        let reparsed: ContinuumConfig = toml::from_str(&serialised).unwrap();
        assert_eq!(reparsed.projects.known, config.projects.known);
        assert!(!reparsed.projects.auto_discover);
    }

    #[test]
    fn events_config_defaults_and_partial_toml() {
        // Spec §6 defaults for the `[events]` section.
        let cfg = ContinuumConfig::default();
        assert_eq!(cfg.events.collapse_window_minutes, 10);
        assert_eq!(cfg.events.retention_days, 30);
        assert_eq!(cfg.events.count_cap, 500);
        assert_eq!(cfg.events.span_cap_hours, 24);
        assert_eq!(cfg.events.queue_cap, 1024);

        // Partial TOML backfills the rest with defaults.
        let parsed: ContinuumConfig =
            toml::from_str("[events]\ncollapse_window_minutes = 5\nqueue_cap = 64\n").unwrap();
        assert_eq!(parsed.events.collapse_window_minutes, 5);
        assert_eq!(parsed.events.queue_cap, 64);
        assert_eq!(parsed.events.count_cap, 500);
        assert_eq!(parsed.events.span_cap_hours, 24);
    }

    #[test]
    fn performance_config_defaults_and_partial_toml() {
        // Continuous visual context does not silently throttle after idle.
        let cfg = ContinuumConfig::default();
        assert_eq!(cfg.performance.idle_pause_after_secs, 300);
        assert_eq!(cfg.performance.idle_capture_interval_ms, 20);
        assert_eq!(cfg.performance.idle_vision_interval_ms, 20);

        // Partial TOML backfills the rest with serde defaults; 0 is a
        // meaningful value (vision fully paused during idle).
        let parsed: ContinuumConfig =
            toml::from_str("[performance]\nidle_vision_interval_ms = 0\n").unwrap();
        assert_eq!(parsed.performance.idle_vision_interval_ms, 0);
        assert_eq!(parsed.performance.idle_pause_after_secs, 300);
        assert_eq!(parsed.performance.idle_capture_interval_ms, 20);
    }

    #[test]
    fn memory_vault_defaults_and_toml_nesting() {
        let cfg = ContinuumConfig::default();
        assert_eq!(cfg.memory.vault.watcher_debounce_ms, 500);
        assert_eq!(cfg.memory.vault.events_retention_days, 30);
        assert_eq!(cfg.memory.vault.graph_max_nodes, 1500);
        assert!(cfg.memory.vault.vault_dir.is_empty());
        assert!(cfg.memory.curator.enabled);
        assert_eq!(cfg.memory.curator.interval_minutes, 10);
        assert_eq!(cfg.memory.curator.max_candidates_per_pass, 3);
        assert_eq!(cfg.memory.curator.auto_confirm_threshold, 0.85);
        assert_eq!(cfg.memory.curator.discard_floor, 0.4);
        assert_eq!(cfg.memory.curator.claude_batch, 10);
        assert_eq!(cfg.memory.curator.session_summary_idle_minutes, 20);
        assert_eq!(cfg.memory.curator.wake_vault_notes_max, 8);
        assert!(!cfg.memory.curator.include_sensitive_in_context);
        assert_eq!(cfg.memory.curator.supersede_confidence_floor, 0.5);
        assert_eq!(cfg.memory.curator.maintenance_wake_hour, 4);
        assert_eq!(cfg.memory.curator.session_min_minutes, 5);
        assert_eq!(cfg.memory.curator.pending_min_age_minutes, 30);

        let parsed: ContinuumConfig = toml::from_str(
            "[memory.vault]\nvault_dir = \"D:/x\"\n[memory.curator]\nenabled = false\n",
        )
        .unwrap();
        assert_eq!(parsed.memory.vault.vault_dir, "D:/x");
        assert!(!parsed.memory.curator.enabled);
        assert_eq!(parsed.memory.vault.watcher_debounce_ms, 500); // default backfill

        // Negative disables the maintenance wake; must parse cleanly via TOML.
        let disabled: ContinuumConfig =
            toml::from_str("[memory.curator]\nmaintenance_wake_hour = -1\n").unwrap();
        assert_eq!(disabled.memory.curator.maintenance_wake_hour, -1);

        let base = std::path::Path::new("/tmp/base");
        assert_eq!(
            ContinuumConfig::default()
                .memory
                .vault
                .resolve_vault_dir(base),
            base.join("vault")
        );
    }

    #[test]
    fn candidate_ttl_defaults_match_the_spec_table() {
        use continuum_memory::NodeType;
        let ttl = CandidateTtlDays::default();
        assert_eq!(ttl.for_node_type(NodeType::Task), Some(30));
        assert_eq!(ttl.for_node_type(NodeType::Error), Some(30));
        assert_eq!(ttl.for_node_type(NodeType::Decision), None);
        assert_eq!(ttl.for_node_type(NodeType::Preference), None);
        assert_eq!(ttl.for_node_type(NodeType::Note), Some(90));
        // Types outside the §4.7 mapping table fall back to the note TTL.
        assert_eq!(ttl.for_node_type(NodeType::Fact), Some(90));
        assert_eq!(ttl.for_node_type(NodeType::Session), Some(90));
    }

    #[test]
    fn candidate_ttl_is_overridable_and_zero_means_never() {
        use continuum_memory::NodeType;
        let parsed: ContinuumConfig =
            toml::from_str("[memory.candidate_ttl_days]\ntask = 7\nnote = 0\ndecision = 365\n")
                .unwrap();
        let ttl = parsed.memory.candidate_ttl_days;
        assert_eq!(ttl.for_node_type(NodeType::Task), Some(7));
        // 0 days would expire a candidate before anyone could review it.
        assert_eq!(ttl.for_node_type(NodeType::Note), None);
        assert_eq!(ttl.for_node_type(NodeType::Decision), Some(365));
        // Unspecified keys keep their defaults.
        assert_eq!(ttl.for_node_type(NodeType::Error), Some(30));
    }

    #[test]
    fn context_package_defaults_match_the_spec() {
        let cfg = ContinuumConfig::default().context_package;
        // Spec §6: token_budget 1000 (wake) / chat_token_budget 600.
        assert_eq!(cfg.token_budget, 1_000);
        assert_eq!(cfg.chat_token_budget, 600);
        let caps = cfg.caps();
        assert_eq!(caps, crate::context::package::SectionCaps::default());
        assert_eq!(cfg.wake_budget().token_budget, 1_000);
        assert!(cfg.wake_budget().cloud_bound, "the wake leaves the machine");
        assert_eq!(cfg.chat_budget().token_budget, 600);
        assert!(cfg.chat_budget().cloud_bound);
    }

    #[test]
    fn context_package_knobs_are_overridable_from_toml() {
        let parsed: ContinuumConfig = toml::from_str(
            "[context_package]\ntoken_budget = 1500\nmax_memories = 1\nmax_tools = 2\n",
        )
        .unwrap();
        assert_eq!(parsed.context_package.token_budget, 1_500);
        assert_eq!(parsed.context_package.caps().memories, 1);
        assert_eq!(parsed.context_package.caps().tools, 2);
        // Unspecified keys keep their defaults (non-negotiable #3).
        assert_eq!(parsed.context_package.chat_token_budget, 600);
        assert_eq!(parsed.context_package.caps().just_before, 5);
    }

    #[test]
    fn continuation_defaults_match_the_spec() {
        let cfg = ContinuumConfig::default().continuation;
        // Spec §6: `[continuation] confidence_floor=0.6, trigger_phrases`.
        assert_eq!(cfg.confidence_floor, 0.6);
        for phrase in [
            "ga door",
            "continue",
            "verder",
            "waar was ik",
            "pak weer op",
        ] {
            assert!(
                cfg.trigger_phrases.iter().any(|p| p == phrase),
                "missing default trigger phrase {phrase}"
            );
        }
        // The wake_result lookback must outlive the packager's event
        // window — "continue" after lunch still needs this morning's step.
        let package = ContinuumConfig::default().context_package;
        assert!(cfg.wake_result_lookback_hours * 60 > package.events_window_minutes);
    }

    #[test]
    fn continuation_knobs_are_overridable_from_toml() {
        let parsed: ContinuumConfig = toml::from_str(
            "[continuation]\nconfidence_floor = 0.9\ntrigger_phrases = [\"resume\"]\n",
        )
        .unwrap();
        assert_eq!(parsed.continuation.confidence_floor, 0.9);
        assert_eq!(parsed.continuation.trigger_phrases, vec!["resume"]);
        // Unspecified keys keep their defaults (non-negotiable #3).
        assert_eq!(parsed.continuation.wake_result_lookback_hours, 12);
    }

    #[test]
    fn chat_session_context_is_on_by_default_and_overridable() {
        assert!(ContinuumConfig::default().chat.include_session_context);
        let parsed: ContinuumConfig =
            toml::from_str("[chat]\ninclude_session_context = false\n").unwrap();
        assert!(!parsed.chat.include_session_context);
        // The vault-backed memory context is a separate knob and unchanged
        // — chat must keep working with no runtime at all (spec §4.9).
        assert_eq!(parsed.chat.memory_context_notes_max, 6);
    }
}
