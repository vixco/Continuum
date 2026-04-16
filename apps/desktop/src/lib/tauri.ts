// Thin wrappers around Tauri's invoke/listen. `isTauri()` lets components
// degrade gracefully in the Next.js dev server (pnpm dev) where the Tauri
// globals aren't injected — each command returns sensible empty defaults
// so the UI still renders for design iteration.

"use client";

import type {
  Automation,
  AutomationInput,
  ComponentHealth,
  KairoConfig,
  KairoState,
  LogEntry,
  RepairEvent,
  SaveSkillInput,
  SemanticFact,
  Skill,
  WorkerSnapshot,
} from "./types";

type InvokeFn = <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;
type ListenFn<T> = (event: string, handler: (e: { payload: T }) => void) => Promise<() => void>;

interface TauriApi {
  invoke: InvokeFn;
  listen: <T>(event: string, handler: (e: { payload: T }) => void) => Promise<() => void>;
}

let cachedApi: Promise<TauriApi | null> | null = null;

function loadApi(): Promise<TauriApi | null> {
  if (cachedApi) return cachedApi;
  cachedApi = (async () => {
    if (typeof window === "undefined") return null;
    // Tauri v2 exposes __TAURI_INTERNALS__ when running inside the app.
    if (!(window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__) {
      return null;
    }
    try {
      const core = await import("@tauri-apps/api/core");
      const event = await import("@tauri-apps/api/event");
      return {
        invoke: core.invoke as InvokeFn,
        listen: event.listen as ListenFn<unknown> as TauriApi["listen"],
      };
    } catch (err) {
      console.warn("Tauri API load failed", err);
      return null;
    }
  })();
  return cachedApi;
}

export async function isTauri(): Promise<boolean> {
  return (await loadApi()) !== null;
}

async function invoke<T>(cmd: string, args?: Record<string, unknown>, fallback?: T): Promise<T> {
  const api = await loadApi();
  if (!api) {
    if (fallback !== undefined) return fallback;
    throw new Error(`Tauri not available for command ${cmd}`);
  }
  return api.invoke<T>(cmd, args);
}

export async function listen<T>(
  event: string,
  handler: (payload: T) => void,
): Promise<() => void> {
  const api = await loadApi();
  if (!api) return () => {};
  return api.listen<T>(event, (e) => handler(e.payload));
}

// --- Commands ---

export const kairo = {
  getState: () => invoke<KairoState>("get_state", undefined, DEFAULT_STATE),
  getConfig: () => invoke<KairoConfig>("get_config", undefined, DEFAULT_CONFIG),
  updateVoiceVolume: (volume: number) =>
    invoke<KairoConfig>("update_voice_volume", { volume }, DEFAULT_CONFIG),
  updateVoiceFlag: (flag: string, value: boolean) =>
    invoke<KairoConfig>("update_voice_flag", { flag, value }, DEFAULT_CONFIG),
  updateScreenInterval: (seconds: number) =>
    invoke<KairoConfig>("update_screen_interval", { seconds }, DEFAULT_CONFIG),
  updateTriageThreshold: (threshold: number) =>
    invoke<KairoConfig>("update_triage_threshold", { threshold }, DEFAULT_CONFIG),
  getLogs: (query?: {
    level?: string;
    layer?: string;
    component?: string;
    text?: string;
    limit?: number;
  }) => invoke<LogEntry[]>("get_logs", { query }, []),
  searchEpisodic: (query: string, limit?: number) =>
    invoke<unknown[]>("search_episodic", { query, limit }, []),
  deleteEpisodic: (id: string) => invoke<void>("delete_episodic", { id }),
  listSemantic: () => invoke<SemanticFact[]>("list_semantic", undefined, []),
  setSemantic: (key: string, value: string) =>
    invoke<void>("set_semantic", { key, value }),
  deleteSemantic: (key: string) => invoke<void>("delete_semantic", { key }),
  wipeMemory: (confirm: string) => invoke<void>("wipe_memory", { confirm }),
  listAutomations: () => invoke<Automation[]>("list_automations", undefined, []),
  createAutomation: (input: AutomationInput) =>
    invoke<Automation>("create_automation", { input }),
  updateAutomation: (id: string, input: AutomationInput) =>
    invoke<Automation>("update_automation", { id, input }),
  deleteAutomation: (id: string) =>
    invoke<void>("delete_automation", { id }),
  toggleAutomation: (id: string, enabled: boolean) =>
    invoke<void>("toggle_automation", { id, enabled }),
  getHealth: () => invoke<ComponentHealth[]>("get_health", undefined, []),
  triggerRepair: (reason?: string) =>
    invoke<void>("trigger_repair", { reason }),
  restartComponent: (name: string) =>
    invoke<ComponentHealth | null>("restart_component", { name }, null),
  runBackupNow: () => invoke<string>("run_backup_now"),
  rollbackConfig: (date: string) =>
    invoke<void>("rollback_config", { date }),
  setPaused: (paused: boolean) => invoke<void>("set_paused", { paused }),
  setVoiceMuted: (muted: boolean) => invoke<void>("set_voice_muted", { muted }),
  talkNow: () => invoke<void>("talk_now"),
  updateWakeSensitivity: (value: number) =>
    invoke<KairoConfig>("update_wake_sensitivity", { value }, DEFAULT_CONFIG),
  updateTtsLengthScale: (value: number) =>
    invoke<KairoConfig>("update_tts_length_scale", { value }, DEFAULT_CONFIG),
  updateTtsEngine: (engine: string) =>
    invoke<KairoConfig>("update_tts_engine", { engine }, DEFAULT_CONFIG),
  updateTtsPrimaryVoice: (voice: string) =>
    invoke<KairoConfig>("update_tts_primary_voice", { voice }, DEFAULT_CONFIG),
  quit: () => invoke<void>("quit_app"),
  listSkills: () => invoke<Skill[]>("list_skills", undefined, []),
  saveSkill: (input: SaveSkillInput) =>
    invoke<Skill>("save_skill", { input }),
  deleteSkill: (name: string) => invoke<void>("delete_skill", { name }),
  toggleSkill: (name: string, enabled: boolean) =>
    invoke<KairoConfig>("toggle_skill", { name, enabled }),
  installSkillFromUrl: (url: string) =>
    invoke<Skill>("install_skill_from_url", { url }),
  listWorkers: (limit?: number) =>
    invoke<WorkerSnapshot[]>("list_workers", { limit }, []),
  getWorker: (id: string) =>
    invoke<WorkerSnapshot | null>("get_worker", { id }, null),
  cancelWorker: (id: string) => invoke<void>("cancel_worker", { id }),
  dismissWorker: (id: string) => invoke<void>("dismiss_worker", { id }),
};

export async function subscribeState(handler: (s: KairoState) => void) {
  return listen<KairoState>("kairo:state", handler);
}

export async function subscribeLogs(handler: (e: LogEntry) => void) {
  return listen<LogEntry>("kairo:log", handler);
}

export async function subscribeRepair(handler: (e: RepairEvent) => void) {
  return listen<RepairEvent>("kairo:repair", handler);
}

// --- Defaults (used when the dashboard renders outside Tauri, e.g. `pnpm dev`) ---

export const DEFAULT_STATE: KairoState = {
  perception: {
    last_frame_id: null,
    last_frame_ts: null,
    last_description: "",
    last_foreground_app: "",
    last_screenshot_path: null,
    last_salience: 0,
    has_error_visible: false,
    frames_today: 0,
  },
  triage: {
    last_decision: null,
    last_decision_ts: null,
    last_latency_ms: null,
    decision_counts_today: {},
  },
  orchestrator: {
    active: false,
    current_session_id: null,
    last_wake_reason: null,
    last_wake_ts: null,
    last_duration_ms: null,
    cost_usd_today: 0,
    wakes_today: 0,
  },
  workers: { active: [], queue_depth: 0, completed_today: 0 },
  voice: {
    mode: "idle",
    partial_transcript: "",
    tts_queue_len: 0,
    volume: 0.8,
    muted: false,
    ambient_mute_active: false,
    detected_call_app: null,
    wake_word_enabled: true,
    last_heard_at: null,
  },
  memory: {
    raw_log_rows: 0,
    raw_log_bytes: 0,
    episodic_count: 0,
    semantic_count: 0,
    last_distill_ts: null,
  },
  health: {
    components: [],
    last_check_ts: null,
    error_count_24h: 0,
    repair_running: false,
    last_repair_ts: null,
    last_backup_ts: null,
    backups_retained: 0,
  },
  system: {
    started_at: null,
    uptime_secs: 0,
    cpu_percent: 0,
    ram_used_mb: 0,
    ram_total_mb: 0,
    gpu_percent: null,
    triage_model_loaded: false,
    vision_model_loaded: false,
    tts_loaded: false,
    stt_loaded: false,
    orchestrator_ready: false,
    paused: false,
    version: "0.1.0-alpha.1",
  },
  recent_actions: [],
};

export const DEFAULT_CONFIG: KairoConfig = {
  vision: {
    name: "SmolVLM-256M",
    model_path: "",
    gpu_enabled: false,
    input_width: 384,
    input_height: 384,
  },
  screen: {
    interval_secs: 3,
    capture_width: 1280,
    capture_height: 720,
    save_screenshots: true,
  },
  audio: {
    enabled: true,
    whisper_model_path: "",
    whisper_language: "auto",
    vad_threshold: 0.005,
    silence_duration_ms: 800,
    max_segment_secs: 8,
    device_name: "",
    device_index: null,
  },
  context: { poll_interval_secs: 1 },
  frame: { interval_secs: 3, salience_threshold: 0.1 },
  storage: { db_path: "", screenshots_dir: "", retention_days: 30 },
  memory: {
    distillation_enabled: true,
    distillation_interval_minutes: 15,
    distillation_lookback_minutes: 20,
    distillation_min_salience: 0.35,
    distillation_batch_size: 100,
  },
  voice: {
    enabled: true,
    wake_word_enabled: true,
    wake_keyword: "hey kairo",
    wake_sensitivity: 0.5,
    custom_keyword_path: "",
    listen_timeout_ms: 12000,
    endpoint_silence_ms: 700,
    min_utterance_chars: 3,
    barge_in_enabled: true,
    ambient_mute_enabled: true,
    language_detection_enabled: false,
    default_language: "en",
    volume: 0.8,
    feedback_sounds: true,
    hotkey: "Ctrl+Shift+K",
    conversation_followup_seconds: 5,
  },
  tts: {
    enabled: true,
    engine: "piper",
    espeak_data_dir: "",
    voices: {},
    primary: "en",
    length_scale: null,
    elevenlabs: {
      api_key: "",
      voice_id: "",
      model_id: "eleven_turbo_v2_5",
      stability: 0.5,
      similarity_boost: 0.75,
    },
  },
  workers: {
    mode: "auto",
    budget_model: "claude-sonnet-4-6",
    power_model: "claude-opus-4-6",
    max_concurrent: 3,
    default_timeout_secs: 1800,
    default_allowed_tools: "",
    status_refresh_ms: 500,
    failure_streak_limit: 3,
    failure_window_secs: 600,
  },
  skills: {
    enabled: true,
    dir: "skills",
    hot_reload: true,
    token_budget: 2000,
    disabled: [],
  },
};
