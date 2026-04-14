// TypeScript mirrors of kairo-core's state structs. Kept flat to ease
// per-tab destructuring. Keep these in sync with:
//   crates/kairo-core/src/state.rs
//   crates/kairo-core/src/logs.rs
//   crates/kairo-core/src/automations.rs
//   crates/kairo-core/src/health/repair.rs

export type VoiceMode =
  | "idle"
  | "listening"
  | "thinking"
  | "speaking"
  | "muted"
  | "error";

export type ComponentStatus = "healthy" | "degrading" | "error" | "unknown";

export type RecentActionKind =
  | "triage"
  | "wake"
  | "worker"
  | "voice"
  | "repair";

export interface PerceptionState {
  last_frame_id: string | null;
  last_frame_ts: string | null;
  last_description: string;
  last_foreground_app: string;
  last_screenshot_path: string | null;
  last_salience: number;
  has_error_visible: boolean;
  frames_today: number;
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
}

export interface MemoryState {
  raw_log_rows: number;
  raw_log_bytes: number;
  episodic_count: number;
  semantic_count: number;
  last_distill_ts: string | null;
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

export interface KairoState {
  perception: PerceptionState;
  triage: TriageState;
  orchestrator: OrchestratorState;
  workers: WorkersState;
  voice: VoiceState;
  memory: MemoryState;
  health: HealthState;
  system: SystemState;
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

export interface SemanticFact {
  key: string;
  value: string;
  source: string;
  confidence: number;
  namespace: string;
}

export type RepairEvent =
  | { kind: "started"; ts: string }
  | { kind: "context_written"; path: string }
  | { kind: "assistant_delta"; text: string }
  | { kind: "tool_call"; name: string }
  | { kind: "tool_result"; name: string; summary: string }
  | { kind: "stderr"; line: string }
  | { kind: "finished"; ts: string; success: boolean; cost_usd: number | null }
  | { kind: "error"; message: string };

export interface KairoConfig {
  vision: {
    name: string;
    model_path: string;
    gpu_enabled: boolean;
    input_width: number;
    input_height: number;
  };
  screen: {
    interval_secs: number;
    capture_width: number;
    capture_height: number;
    save_screenshots: boolean;
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
  context: { poll_interval_secs: number };
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
  };
  tts: {
    enabled: boolean;
    engine: string;
    espeak_data_dir: string;
    voices: Record<
      string,
      { model_path: string; config_path: string; speaker_id: number | null }
    >;
    primary: string;
    length_scale: number | null;
    elevenlabs: {
      api_key: string;
      voice_id: string;
      model_id: string;
      stability: number;
      similarity_boost: number;
    };
  };
}
