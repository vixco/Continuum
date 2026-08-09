// TypeScript mirrors of continuum-core's state structs. Kept flat to ease
// per-tab destructuring. Keep these in sync with:
//   crates/continuum-core/src/state.rs
//   crates/continuum-core/src/logs.rs
//   crates/continuum-core/src/automations.rs
//   crates/continuum-core/src/health/repair.rs

export type VoiceMode = "idle" | "listening" | "thinking" | "speaking" | "muted" | "error";

export type ComponentStatus = "healthy" | "degrading" | "error" | "unknown";

export type ObservationPausePreset =
  "fifteen_minutes" | "one_hour" | "four_hours" | "until_tomorrow" | "indefinite";

export interface ObservationPauseStatus {
  paused: boolean;
  until: string | null;
}

export type RecentActionKind = "triage" | "wake" | "worker" | "voice" | "repair";

export interface PerceptionState {
  last_frame_id: string | null;
  last_frame_ts: string | null;
  last_description: string;
  last_foreground_app: string;
  last_screenshot_path: string | null;
  last_salience: number;
  has_error_visible: boolean;
  frames_today: number;
  monitor_count: number;
  capture_events: number;
  dropped_capture_events: number;
  last_capture_at: string | null;
}

export interface TriageState {
  last_decision: string | null;
  last_decision_ts: string | null;
  last_latency_ms: number | null;
  decision_counts_today: Record<string, number>;
}

export interface OrchestratorState {
  active: boolean;
  current_session_id: string | null;
  last_wake_reason: string | null;
  last_wake_ts: string | null;
  last_duration_ms: number | null;
  cost_usd_today: number;
  wakes_today: number;
}

export interface WorkerInfo {
  id: string;
  task: string;
  model: string;
  started_at: string;
  progress: number;
  status: string;
}

export interface WorkersState {
  active: WorkerInfo[];
  queue_depth: number;
  completed_today: number;
}

export interface VoiceState {
  mode: VoiceMode;
  partial_transcript: string;
  tts_queue_len: number;
  volume: number;
  muted: boolean;
  ambient_mute_active: boolean;
  detected_call_app: string | null;
  wake_word_enabled: boolean;
  last_heard_at: string | null;
  /** Active voice front-end: `"pipeline"` or `"moshi"`. Mirrors
   *  `voice.frontend.mode` applied at daemon boot. */
  voice_frontend_mode: string;
  /** Whether the Moshi S2S backend subprocess is loaded+connected. Only
   *  meaningful when `voice_frontend_mode === "moshi"`. */
  moshi_loaded: boolean;
}

// Mirrors crates/continuum-core/src/runtime_publish.rs's CuratorSnapshot,
// forwarded into MemoryState.curator by the dashboard's runtime bridge
// (apps/desktop/src-tauri/src/runtime_bridge.rs, Task 11).
export interface CuratorSnapshot {
  last_pass_at: string | null;
  consecutive_failures: number;
  candidates_written_total: number;
  pending_count: number;
  enabled: boolean;
}

export interface MemoryState {
  raw_log_rows: number;
  raw_log_bytes: number;
  episodic_count: number;
  semantic_count: number;
  last_distill_ts: string | null;
  // `null` until the dashboard's runtime bridge has read a state.json from
  // a running `continuum` process at least once — distinct from a present
  // snapshot with `enabled: false` (curator reachable but not running).
  curator?: CuratorSnapshot | null;
}

export interface ComponentHealth {
  name: string;
  status: ComponentStatus;
  last_check_ts: string | null;
  last_error: string | null;
  error_count_24h: number;
  avg_response_ms: number | null;
  log_path: string | null;
  recovery_note: string | null;
}

export interface HealthState {
  components: ComponentHealth[];
  last_check_ts: string | null;
  error_count_24h: number;
  repair_running: boolean;
  last_repair_ts: string | null;
  last_backup_ts: string | null;
  backups_retained: number;
}

export interface SystemState {
  started_at: string | null;
  uptime_secs: number;
  cpu_percent: number;
  ram_used_mb: number;
  ram_total_mb: number;
  gpu_percent: number | null;
  triage_model_loaded: boolean;
  vision_model_loaded: boolean;
  tts_loaded: boolean;
  stt_loaded: boolean;
  orchestrator_ready: boolean;
  paused: boolean;
  version: string;
}

export interface RecentAction {
  ts: string;
  kind: RecentActionKind;
  summary: string;
  detail: string | null;
}

// --- Context engine (Task C1) ---
// Mirrors crates/continuum-core/src/context/session_state.rs and the
// ContextEngineSnapshot half of runtime_publish.rs, forwarded into
// ContinuumState.context by the dashboard's runtime bridge
// (apps/desktop/src-tauri/src/runtime_bridge.rs::apply_snapshot).

/// A one-line fact plus when it was observed. Serialised as `{text, at}`;
/// the Rust side also *accepts* a bare string from older producers, but
/// what the runtime publishes is always this object.
export interface StampedText {
  text: string;
  at: string;
}

/// Live session state (spec §4.8). `current_goal`/`current_task` are the
/// inferred fields — hide them below `[session_state] confidence_floor`,
/// and render `local_only` ones as "working in a private context" at any
/// cloud egress point.
export interface SessionState {
  active_project: string | null;
  current_goal: string | null;
  current_task: string | null;
  active_app: string | null;
  window_title: string | null;
  open_files: string[];
  last_error: StampedText | null;
  last_success: StampedText | null;
  last_user_command: StampedText | null;
  confidence: number;
  local_only: boolean;
  /// Field tokens ("project" | "goal" | "task") the user pinned on the
  /// Context page (spec §4.13). A pinned field is never overwritten by the
  /// frame loop or by inference.
  pinned: string[];
  /// Field tokens the user explicitly corrected. Informational — unlike a
  /// pin, a correction does not block later inference.
  user_confirmed: string[];
  since: string;
  updated: string;
  /// When `current_goal`/`current_task` were last established (inference
  /// or correction). `null` means never inferred. This — not `updated`,
  /// which the frame loop rewrites every second — is the clock the §4.12
  /// continuation resolver ages the task on.
  inferred_at: string | null;
}

/// Per-component health summary (spec §7). Disabled-with-reason is a
/// healthy state: `healthy: true, enabled: false` with the reason in
/// `detail`. `should_restart` is the only restart signal.
export interface ComponentHealthSummary {
  healthy: boolean;
  enabled: boolean;
  should_restart: boolean;
  detail: string | null;
}

export interface ContextEngineSnapshot {
  idle: boolean;
  context_watcher: ComponentHealthSummary | null;
  live_context: ComponentHealthSummary | null;
  git_watcher: ComponentHealthSummary | null;
  file_watcher: ComponentHealthSummary | null;
  process_watcher: ComponentHealthSummary | null;
  events_writer: ComponentHealthSummary | null;
  /// Off-loop triage evaluation. `should_restart` fires when a single
  /// evaluation has been in flight far past any plausible model latency —
  /// the signature of an evaluation task that died without releasing the
  /// coalescer, which silently parks every later frame.
  triage: ComponentHealthSummary | null;
}

// --- Context page (Task C5, spec §4.13) ---
// Mirrors crates/continuum-core/src/runtime_publish.rs::ContextPageSnapshot.
// The dashboard process links continuum-core with `default-features = false`,
// so it can never open the raw-log SQLite database itself — everything the
// Context page lists (projects, override rules, pins, the recent-events
// strip, the live toggle values) is published by the runtime into
// `state.json` and forwarded by the runtime bridge.

export type ProjectStatus = "configured" | "discovered" | "confirmed";

/// One row of the Projects table (spec §4.3). `status: "discovered"` is an
/// unconfirmed auto-discovery candidate: it never participates in
/// resolution and is never collected from until the user confirms it.
export interface ProjectSummaryView {
  id: string;
  name: string;
  status: ProjectStatus;
  root_paths: string[];
  last_active: string | null;
  frames_count: number;
  /// True for the resolver's current post-hysteresis project.
  active: boolean;
}

/// A persisted tier-0 resolver override rule (spec §4.3).
export interface OverrideRuleView {
  match_process: string | null;
  match_title_substring: string | null;
  action: "force_project" | "exclude_project";
  project_id: string;
}

/// A persisted session pin (spec §4.13). Pins block session-state
/// *overwrite* only, never resolution.
export interface SessionPinView {
  field: string;
  value: string | null;
}

/// One deduped `context_events` row, already privacy-gated by the runtime.
export interface ContextEventView {
  id: number;
  ts: string;
  source: string;
  event_type: string;
  application: string;
  summary: string;
  count: number;
  project_id: string | null;
  /// Pointer into the raw log (a perception-frame id for screen/audio
  /// events). Required for the Forget cascade.
  raw_reference: string | null;
}

/// Live values of the honest per-source observation toggles (spec §4.1).
export interface ObservationTogglesView {
  mic: boolean;
  screen: boolean;
  files: boolean;
  git: boolean;
  pause_all: boolean;
}

/// A ranked continuation candidate (spec §4.12).
export interface ContinuationCandidateView {
  kind: string;
  label: string;
  text: string;
  confidence: number;
}

export interface ContextPageSnapshot {
  projects: ProjectSummaryView[];
  rules: OverrideRuleView[];
  pins: SessionPinView[];
  recent_events: ContextEventView[];
  toggles: ObservationTogglesView;
  continuation: ContinuationCandidateView[];
}

/// All three fields are `null` until the runtime bridge has read a
/// `state.json` from a running `continuum` process that publishes them —
/// which is also what "runtime not running" looks like to the Context page.
export interface ContextState {
  session: SessionState | null;
  engine: ContextEngineSnapshot | null;
  page: ContextPageSnapshot | null;
}

/// Payload of a `context-intents/*.json` file (spec §4.13), written by
/// `context_write_intent` and drained by the runtime's main loop. The
/// `kind` discriminant and the field names must match
/// `continuum_core::context::intents::ContextAction` exactly.
export type ContextIntentInput =
  | {
      kind: "add_project";
      name: string;
      root_path: string;
      /// Explicit slug; omit to have the runtime slugify `name`. NOT `id` —
      /// the envelope owns that key at the same JSON level.
      project_id?: string | null;
    }
  | { kind: "confirm_project"; project_id: string }
  | {
      kind: "correct";
      field: "project" | "goal" | "task";
      value: string;
      match_process?: string | null;
      match_title_substring?: string | null;
    }
  | {
      kind: "not_this_project";
      project_id: string;
      match_process?: string | null;
      match_title_substring?: string | null;
    }
  | { kind: "pin"; field: "project" | "goal" | "task"; value: string | null }
  | { kind: "forget"; raw_reference?: string | null; event_id?: number | null }
  | { kind: "delete_range"; from: string; to: string }
  | {
      kind: "set_toggle";
      name: "mic" | "screen" | "files" | "git" | "pause_all";
      value: boolean;
    };

export interface ContinuumState {
  perception: PerceptionState;
  triage: TriageState;
  orchestrator: OrchestratorState;
  workers: WorkersState;
  voice: VoiceState;
  memory: MemoryState;
  health: HealthState;
  system: SystemState;
  context: ContextState;
  recent_actions: RecentAction[];
}

export interface LogEntry {
  id: number;
  ts: string;
  level: string;
  layer: string | null;
  component: string | null;
  target: string;
  message: string;
  fields: Array<[string, string]>;
}

export interface Automation {
  id: string;
  task: string;
  kind: "one_shot" | "recurring";
  schedule: string;
  enabled: boolean;
  created_at: string;
  last_run: string | null;
  next_run: string | null;
  last_status: string | null;
}

export interface AutomationInput {
  task: string;
  kind: "one_shot" | "recurring";
  schedule: string;
  enabled: boolean;
}

// --- Memory vault ---

export type MemoryNodeType =
  | "project"
  | "goal"
  | "task"
  | "decision"
  | "person"
  | "preference"
  | "fact"
  | "error"
  | "session"
  | "note";

export type MemoryNodeStatus = "candidate" | "confirmed" | "rejected" | "superseded" | "archived";
export type MemorySource =
  "user_statement" | "observed" | "inferred" | "agent_run" | "chat" | "manual";
export type MemorySensitivity = "public" | "internal" | "sensitive";

export interface MemoryRelation {
  to: string;
  rel: string;
  confidence: number;
}

export interface MemoryFrontmatter {
  id: string;
  type: MemoryNodeType;
  title: string;
  status: MemoryNodeStatus;
  project?: string | null;
  confidence: number;
  importance: number;
  source: MemorySource;
  source_ref?: string | null;
  sensitivity: MemorySensitivity;
  created: string;
  updated?: string | null;
  last_used?: string | null;
  expires?: string | null;
  supersedes?: string | null;
  superseded_by?: string | null;
  relations?: MemoryRelation[];
  tags?: string[];
}

export interface MemoryNodeSummary {
  id: string;
  slug: string;
  title: string;
  type: MemoryNodeType;
  status: MemoryNodeStatus;
  project: string | null;
  confidence: number;
  importance: number;
  source: MemorySource;
  sensitivity: MemorySensitivity;
  created: string;
  updated: string;
  tags: string[];
  snippet: string | null;
}

export interface MemoryNote {
  frontmatter: MemoryFrontmatter;
  body: string;
  path: string;
  slug: string;
  backlinks: MemoryNodeSummary[];
}

export interface MemoryNoteDraft {
  type: MemoryNodeType;
  title: string;
  body?: string;
  project?: string | null;
  status?: MemoryNodeStatus;
  confidence?: number;
  importance?: number;
  source?: MemorySource;
  source_ref?: string | null;
  sensitivity?: MemorySensitivity;
  relations?: MemoryRelation[];
  tags?: string[];
}

export interface MemoryGraphFilter {
  types?: MemoryNodeType[] | null;
  statuses?: MemoryNodeStatus[] | null;
  project?: string | null;
  query?: string | null;
  updated_since?: string | null;
  updated_until?: string | null;
  limit?: number | null;
}

export interface MemoryGraphNode {
  id: string;
  slug: string;
  title: string;
  type: MemoryNodeType;
  status: MemoryNodeStatus;
  project: string | null;
  confidence: number;
  importance: number;
  created: string;
  updated: string;
}

export interface MemoryGraphEdge {
  from: string;
  to: string;
  rel: string;
  confidence: number;
  origin: "frontmatter" | "body";
}

export interface MemoryGhostNode {
  target: string;
  ref_count: number;
}

export interface MemoryGraphData {
  nodes: MemoryGraphNode[];
  edges: MemoryGraphEdge[];
  ghosts: MemoryGhostNode[];
  truncated: boolean;
}

export type MemoryResolution =
  { action: "confirm" } | { action: "reject" } | { action: "supersede"; replaces: string };

export interface MemoryEvent {
  id: number;
  ts: string;
  kind: string;
  text: string;
  project: string | null;
  node_id: string | null;
  ref: string | null;
}

export interface MemoryEventRange {
  since?: string | null;
  until?: string | null;
  limit?: number | null;
}

export interface MemoryQuarantineEntry {
  path: string;
  error: string;
}

export interface MemoryVaultInfo {
  path: string;
  note_count: number;
  counts_by_status: Record<string, number>;
  quarantined: MemoryQuarantineEntry[];
  last_full_index_at: string | null;
  legacy_semantic_present: boolean;
}

export interface MemoryIndexStats {
  indexed: number;
  quarantined: number;
  removed: number;
}

export interface MemoryMigrationReport {
  migrated: number;
  skipped: number;
  errors: string[];
}

export interface MemoryEventPayload {
  kind: "changed" | "rebuilt";
  ids: string[];
}

export type RepairEvent =
  | { kind: "started"; ts: string }
  | { kind: "context_written"; path: string }
  | { kind: "backup_created"; path: string; bytes: number; verified: boolean }
  | { kind: "action_result"; action: string; success: boolean; detail: string }
  | { kind: "assistant_delta"; text: string }
  | { kind: "tool_call"; name: string }
  | { kind: "tool_result"; name: string; summary: string }
  | { kind: "stderr"; line: string }
  | { kind: "finished"; ts: string; success: boolean; cost_usd: number | null }
  | { kind: "verification"; checked_at: string; unresolved: ComponentHealth[] }
  | { kind: "error"; message: string };

export interface RepairPreviewIssue {
  component: string;
  status: ComponentStatus;
  detail: string;
  proposed_action: string;
  actionable: boolean;
}

export interface RepairPreview {
  id: string;
  created_at: string;
  expires_at: string;
  issues: RepairPreviewIssue[];
  backup_required: boolean;
  allowed_actions: string[];
}

// Phase 8: skills + workers

export interface Skill {
  name: string;
  description: string;
  triggers: string[];
  source: string | null;
  manual_only: boolean;
  enabled: boolean;
  body: string;
  path: string;
}

export interface SaveSkillInput {
  name: string;
  description: string;
  triggers: string[];
  body: string;
  source?: string | null;
  manual_only?: boolean;
}

export type WorkerStatus =
  | "queued"
  | "starting"
  | "running"
  | "completed"
  | "failed"
  | "cancelled"
  | "timed_out"
  | "pending";

export interface WorkerSnapshot {
  id: string;
  task: string;
  cwd: string;
  model: string;
  model_reason: string;
  status: WorkerStatus;
  priority: string;
  requested_by: string | null;
  skills: string[];
  tags: string[];
  started_at: string | null;
  finished_at: string | null;
  queued_at: string;
  elapsed_ms: number;
  progress: number;
  last_line: string;
  tool_calls: number;
  cost_usd: number | null;
  session_id: string | null;
  result: string | null;
  error: string | null;
}

export interface ContinuumConfig {
  health: {
    repair_timeout_secs: number;
    runtime_start_timeout_secs: number;
    repair_session_ttl_secs: number;
    backup_retention: number;
  };
  chat: {
    max_tokens: number;
    temperature: number | null;
    connect_timeout_secs: number;
    stream_idle_timeout_secs: number;
    cli_timeout_secs: number;
    model_refresh_interval_secs: number;
    system_prompt_path: string | null;
  };
  github: {
    enabled: boolean;
    connect_timeout_secs: number;
    api_timeout_secs: number;
    max_response_bytes: number;
    max_mutation_body_bytes: number;
    max_title_chars: number;
  };
  vision: {
    name: string;
    model_path: string;
    gpu_enabled: boolean;
    input_width: number;
    input_height: number;
  };
  screen: {
    enabled: boolean;
    interval_secs: number;
    capture_interval_ms: number;
    capture_width: number;
    capture_height: number;
    save_screenshots: boolean;
    all_monitors: boolean;
    excluded_monitor_ids: string[];
    buffer_capacity: number;
    meaningful_change_threshold: number;
    vision_min_interval_ms: number;
  };
  audio: {
    enabled: boolean;
    whisper_model_path: string;
    whisper_language: string;
    vad_threshold: number;
    silence_duration_ms: number;
    max_segment_secs: number;
    device_name: string;
    device_index: number | null;
  };
  context: {
    poll_interval_secs: number;
    redact_sensitive_titles: boolean;
    sensitive_process_names: string[];
    sensitive_title_keywords: string[];
    terminal_process_names: string[];
  };
  frame: { interval_secs: number; salience_threshold: number };
  storage: {
    db_path: string;
    screenshots_dir: string;
    retention_days: number;
  };
  memory: {
    distillation_enabled: boolean;
    distillation_interval_minutes: number;
    distillation_lookback_minutes: number;
    distillation_min_salience: number;
    distillation_batch_size: number;
  };
  voice: {
    enabled: boolean;
    wake_word_enabled: boolean;
    wake_keyword: string;
    wake_sensitivity: number;
    custom_keyword_path: string;
    listen_timeout_ms: number;
    endpoint_silence_ms: number;
    min_utterance_chars: number;
    barge_in_enabled: boolean;
    ambient_mute_enabled: boolean;
    language_detection_enabled: boolean;
    default_language: string;
    volume: number;
    feedback_sounds: boolean;
    hotkey: string;
    conversation_followup_seconds: number;
    frontend: {
      mode: string;
      moshi_model_repo: string;
      moshi_device: string;
      moshi_port: number;
      moshi_bin: string;
      moshi_tap_enabled: boolean;
    };
  };
  tts: {
    enabled: boolean;
    engine: string;
    espeak_data_dir: string;
    voices: Record<string, { model_path: string; config_path: string; speaker_id: number | null }>;
    primary: string;
    length_scale: number | null;
    elevenlabs: {
      api_key: string;
      voice_id: string;
      model_id: string;
      stability: number;
      similarity_boost: number;
    };
    kokoros: {
      model_path: string;
      voices_path: string;
      voice_name: string;
      speed: number;
    };
  };
  workers: {
    mode: string;
    budget_model: string;
    power_model: string;
    max_concurrent: number;
    default_timeout_secs: number;
    default_allowed_tools: string;
    status_refresh_ms: number;
    failure_streak_limit: number;
    failure_window_secs: number;
  };
  skills: {
    enabled: boolean;
    dir: string;
    hot_reload: boolean;
    token_budget: number;
    disabled: string[];
  };
  resources: ResourceConfig;
}

// --- Adaptive resource policy (mirrors crates/continuum-core/src/hardware.rs
//     + config.rs ResourceConfig). Keep in sync with the Rust structs. ---

export type ProfileMode = "auto" | "barely_notice" | "balanced" | "performance" | "custom";

export interface ResourceConfig {
  profile: ProfileMode;
  cpu_core_fraction: number;
  cpu_min_threads: number;
  cpu_max_threads: number;
  ram_reserve_fraction: number;
  gpu_enabled: boolean | null;
  gpu_min_vram_mb: number;
  battery_throttle: boolean;
  battery_core_fraction: number;
  vision_min_ram_mb: number;
  vision_enabled: boolean | null;
  workers_max_concurrent: number | null;
  screen_interval_secs: number | null;
  context_interval_secs: number | null;
}

export interface HardwareSpecs {
  physical_cores: number;
  logical_cores: number;
  total_ram_mb: number;
  cpu_brand: string;
  has_cuda: boolean;
  vram_mb: number | null;
  on_battery: boolean;
  is_laptop: boolean;
}

export interface ResolvedResourcePlan {
  triage_threads: number;
  triage_gpu_layers: number;
  vision_enabled: boolean;
  vision_gpu: boolean;
  whisper_threads: number;
  workers_max_concurrent: number;
  screen_interval_secs: number;
  context_interval_secs: number;
}

export interface ResourceProfile {
  specs: HardwareSpecs;
  plan: ResolvedResourcePlan;
  config: ResourceConfig;
  /** True when the running runtime has published a matching plan. */
  applied: boolean;
}

export interface ResourceProfileUpdate {
  profile?: ProfileMode;
  cpu_core_fraction?: number;
  gpu_enabled?: boolean | null;
  vision_enabled?: boolean | null;
  workers_max_concurrent?: number | null;
  screen_interval_secs?: number | null;
  context_interval_secs?: number | null;
  battery_throttle?: boolean;
}

export interface ResourceProfileUpdateResult {
  config: ResourceConfig;
  plan: ResolvedResourcePlan;
  restart_required: boolean;
}

// --- Chat + provider connections (mirrors
//     crates/continuum-gateway/src/types.rs,
//     apps/desktop/src-tauri/src/chat_store.rs,
//     apps/desktop/src-tauri/src/providers.rs). Keep in sync. ---

export type ProviderKind = "open_ai_compat" | "anthropic" | "claude_cli";

export interface ProviderConnection {
  id: string;
  display_name: string;
  kind: ProviderKind;
  base_url: string | null;
  catalog_id: string | null;
  models: string[];
  default_model: string | null;
  roles: string[];
  requires_key: boolean;
  last_tested_at: string | null;
  last_test_ok: boolean | null;
}

export interface GitHubAuthStatus {
  installed: boolean;
  connected: boolean;
  login: string | null;
  token_source: string | null;
  scopes: string[];
  secure_storage: boolean;
  detail: string;
}

export interface CatalogEntry {
  id: string;
  label: string;
  kind: ProviderKind;
  default_base_url: string | null;
  needs_key: boolean;
  key_hint: string;
}

export interface ConnectionTestReport {
  ok: boolean;
  latency_ms: number;
  models: string[];
  detail: string;
}

export interface TokenUsage {
  input_tokens: number | null;
  output_tokens: number | null;
}

export type ChatRole = "user" | "assistant";

/** One tool invocation persisted on an assistant chat message. Serde may
 *  omit defaults: missing `is_error` means false, missing `duration_ms`
 *  means 0 (unknown). */
export interface StoredToolCall {
  id: string;
  name: string;
  input: unknown;
  /** Raw result payload; null/undefined when the turn ended without a result. */
  output?: string | null;
  is_error?: boolean;
  /** Milliseconds; 0 means unknown (hidden in the UI). */
  duration_ms?: number;
}

export interface StoredMessage {
  role: ChatRole;
  content: string;
  ts: string;
  model: string | null;
  duration_ms: number | null;
  usage: TokenUsage | null;
  aborted: boolean;
  /** Tool calls made during this assistant turn, in call order. */
  tool_calls?: StoredToolCall[];
}

export interface Conversation {
  id: string;
  title: string;
  provider_id: string;
  model: string;
  created_at: string;
  updated_at: string;
  messages: StoredMessage[];
}

export interface ConversationSummary {
  id: string;
  title: string;
  provider_id: string;
  model: string;
  updated_at: string;
  message_count: number;
}

export type ChatStreamEvent =
  | { type: "delta"; text: string }
  | { type: "tool_call"; id: string; name: string; input: unknown }
  | { type: "tool_result"; id: string; output: string; is_error: boolean; duration_ms: number }
  | { type: "done"; usage: TokenUsage; stop_reason: string | null }
  | { type: "error"; message: string; retryable: boolean };

export interface ChatEventPayload {
  conversation_id: string;
  event: ChatStreamEvent;
}

export interface ProviderAddInput {
  catalog_id: string | null;
  display_name: string;
  base_url: string | null;
  api_key: string | null;
  save_anyway: boolean;
}

/** One entry in the static MCP-tool manifest (see commands.rs::list_mcp_tools). */
export interface McpTool {
  namespace: string;
  name: string;
  description: string;
}

export type PermissionTier = "auto" | "session-approved" | "always-confirm" | "blocked";

export interface PermissionPolicyEntry {
  action: string;
  tier: PermissionTier;
  overridden: boolean;
}

export interface PermissionRequest {
  id: string;
  session_id: string;
  action: string;
  resource: string | null;
  tier: PermissionTier;
  summary: string;
  created_at: string;
}

export type GrantScope = "once" | "session";

export interface PermissionGrant {
  id: string;
  session_id: string;
  action: string;
  resource: string | null;
  scope: GrantScope;
  created_at: string;
  expires_at: string;
}

/** A validated local stdio MCP server registration. */
export interface McpServerRegistration {
  name: string;
  command: string;
  args: string[];
}

export interface InstallMcpServerInput {
  name: string;
  command: string;
  args: string[];
}
