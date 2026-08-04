// Thin wrappers around Tauri's invoke/listen. `isTauri()` lets components
// degrade gracefully in the Next.js dev server (pnpm dev) where the Tauri
// globals aren't injected - each command returns sensible empty defaults
// so the UI still renders for design iteration.

"use client";

import type {
  Automation,
  AutomationInput,
  CatalogEntry,
  ChatEventPayload,
  ComponentHealth,
  ConnectionTestReport,
  ContinuumConfig,
  ContinuumState,
  Conversation,
  ConversationSummary,
  LogEntry,
  InstallMcpServerInput,
  McpServerRegistration,
  McpTool,
  MemoryEvent,
  MemoryEventPayload,
  MemoryEventRange,
  MemoryGraphData,
  MemoryGraphFilter,
  MemoryIndexStats,
  MemoryMigrationReport,
  MemoryNodeSummary,
  MemoryNote,
  MemoryNoteDraft,
  MemoryResolution,
  MemoryVaultInfo,
  ProviderAddInput,
  ProviderConnection,
  RepairEvent,
  RepairPreview,
  ResourceConfig,
  ResourceProfile,
  ResourceProfileUpdate,
  ResourceProfileUpdateResult,
  SaveSkillInput,
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

// --- Window controls (frameless titlebar) ---
// No-ops outside Tauri (pnpm dev) so the buttons don't throw.

export const windowControls = {
  async minimize() {
    if (!(await isTauri())) return;
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().minimize();
  },
  async toggleMaximize() {
    if (!(await isTauri())) return;
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().toggleMaximize();
  },
  async hide() {
    if (!(await isTauri())) return;
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    await getCurrentWindow().hide();
  },
  async isMaximized(): Promise<boolean> {
    if (!(await isTauri())) return false;
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    return await getCurrentWindow().isMaximized();
  },
  async onMaximizeChange(cb: (maximized: boolean) => void): Promise<() => void> {
    if (!(await isTauri())) return () => {};
    const { getCurrentWindow } = await import("@tauri-apps/api/window");
    const win = getCurrentWindow();
    const unlisten = await win.onResized(async () => {
      cb(await win.isMaximized());
    });
    return unlisten;
  },
};

async function invoke<T>(cmd: string, args?: Record<string, unknown>, fallback?: T): Promise<T> {
  const api = await loadApi();
  if (!api) {
    if (fallback !== undefined) return fallback;
    throw new Error(`Tauri not available for command ${cmd}`);
  }
  return api.invoke<T>(cmd, args);
}

export async function listen<T>(event: string, handler: (payload: T) => void): Promise<() => void> {
  const api = await loadApi();
  if (!api) return () => {};
  return api.listen<T>(event, (e) => handler(e.payload));
}

export interface UpdateInfo {
  version: string;
  notes: string | null;
  date: string | null;
}

interface UpdateDownloadEvent {
  event: string;
  data?: {
    contentLength?: number | null;
    chunkLength?: number;
  };
}

interface PendingUpdate {
  version: string;
  body?: string | null;
  date?: string | null;
  downloadAndInstall: (onEvent: (event: UpdateDownloadEvent) => void) => Promise<void>;
}

let pendingUpdate: PendingUpdate | null = null;

export async function checkForUpdate(): Promise<UpdateInfo | null> {
  if (!(await isTauri())) {
    pendingUpdate = null;
    return null;
  }

  const { check } = await import("@tauri-apps/plugin-updater");
  const update = await check();
  if (!update) {
    pendingUpdate = null;
    return null;
  }

  pendingUpdate = update as unknown as PendingUpdate;
  return {
    version: update.version,
    notes: update.body ?? null,
    date: update.date ?? null,
  };
}

export async function installPendingUpdate(
  onProgress?: (downloaded: number, total: number | null) => void
): Promise<void> {
  if (!pendingUpdate) {
    throw new Error("No update is waiting to be installed");
  }

  let downloaded = 0;
  let total: number | null = null;
  await pendingUpdate.downloadAndInstall((event) => {
    if (event.event === "Started") {
      total = event.data?.contentLength ?? null;
    } else if (event.event === "Progress") {
      downloaded += event.data?.chunkLength ?? 0;
    }
    onProgress?.(downloaded, total);
  });

  pendingUpdate = null;
  const { relaunch } = await import("@tauri-apps/plugin-process");
  await relaunch();
}

// --- Commands ---

export const continuum = {
  checkForUpdate,
  installPendingUpdate,
  getState: () => invoke<ContinuumState>("get_state", undefined, DEFAULT_STATE),
  getConfig: () => invoke<ContinuumConfig>("get_config", undefined, DEFAULT_CONFIG),
  getResourceProfile: () =>
    invoke<ResourceProfile>("get_resource_profile", undefined, {
      specs: {
        physical_cores: 0,
        logical_cores: 0,
        total_ram_mb: 0,
        cpu_brand: "",
        has_cuda: false,
        vram_mb: null,
        on_battery: false,
        is_laptop: false,
      },
      plan: {
        triage_threads: 2,
        triage_gpu_layers: 0,
        vision_enabled: false,
        vision_gpu: false,
        whisper_threads: 2,
        workers_max_concurrent: 1,
        screen_interval_secs: 3,
        context_interval_secs: 1,
      },
      config: DEFAULT_RESOURCE_CONFIG,
      applied: false,
    }),
  updateResourceProfile: (update: ResourceProfileUpdate) =>
    invoke<ResourceProfileUpdateResult>("update_resource_profile", { update }),
  updateVoiceVolume: (volume: number) =>
    invoke<ContinuumConfig>("update_voice_volume", { volume }, DEFAULT_CONFIG),
  updateVoiceFlag: (flag: string, value: boolean) =>
    invoke<ContinuumConfig>("update_voice_flag", { flag, value }, DEFAULT_CONFIG),
  updateScreenInterval: (seconds: number) =>
    invoke<ContinuumConfig>("update_screen_interval", { seconds }, DEFAULT_CONFIG),
  updateLiveContextConfig: (update: {
    enabled?: boolean;
    capture_interval_ms?: number;
    all_monitors?: boolean;
    save_screenshots?: boolean;
  }) => invoke<ContinuumConfig>("update_live_context_config", { update }, DEFAULT_CONFIG),
  updateTriageThreshold: (threshold: number) =>
    invoke<ContinuumConfig>("update_triage_threshold", { threshold }, DEFAULT_CONFIG),
  getLogs: (query?: {
    level?: string;
    layer?: string;
    component?: string;
    text?: string;
    limit?: number;
  }) => invoke<LogEntry[]>("get_logs", { query }, []),
  wipeMemory: (confirm: string) => invoke<void>("wipe_memory", { confirm }),
  memoryGraph: (filter: MemoryGraphFilter) =>
    invoke<MemoryGraphData>(
      "memory_graph",
      { filter },
      {
        nodes: [],
        edges: [],
        ghosts: [],
        truncated: false,
      }
    ),
  memorySearch: (query: string, limit?: number) =>
    invoke<MemoryNodeSummary[]>("memory_search", { query, limit }, []),
  memoryGetNote: (id: string) => invoke<MemoryNote>("memory_get_note", { id }),
  memoryCreateNote: (draft: MemoryNoteDraft) => invoke<MemoryNote>("memory_create_note", { draft }),
  memorySaveNote: (note: MemoryNote) => invoke<void>("memory_save_note", { note }),
  memoryDeleteNote: (id: string) => invoke<void>("memory_delete_note", { id }),
  memoryResolveCandidate: (id: string, resolution: MemoryResolution) =>
    invoke<void>("memory_resolve_candidate", { id, resolution }),
  memoryPending: () => invoke<MemoryNodeSummary[]>("memory_pending", undefined, []),
  memoryEvents: (range: MemoryEventRange) => invoke<MemoryEvent[]>("memory_events", { range }, []),
  memoryVaultInfo: () =>
    invoke<MemoryVaultInfo>("memory_vault_info", undefined, {
      path: "",
      note_count: 0,
      counts_by_status: {},
      quarantined: [],
      last_full_index_at: null,
      legacy_semantic_present: false,
    }),
  memoryMigrateLegacy: () => invoke<MemoryMigrationReport>("memory_migrate_legacy"),
  memoryRebuildIndex: () => invoke<MemoryIndexStats>("memory_rebuild_index"),
  memoryOpenVault: () => invoke<void>("memory_open_vault"),
  listAutomations: () => invoke<Automation[]>("list_automations", undefined, []),
  createAutomation: (input: AutomationInput) => invoke<Automation>("create_automation", { input }),
  updateAutomation: (id: string, input: AutomationInput) =>
    invoke<Automation>("update_automation", { id, input }),
  deleteAutomation: (id: string) => invoke<void>("delete_automation", { id }),
  toggleAutomation: (id: string, enabled: boolean) =>
    invoke<void>("toggle_automation", { id, enabled }),
  getHealth: () => invoke<ComponentHealth[]>("get_health", undefined, []),
  previewRepair: () => invoke<RepairPreview>("preview_repair"),
  triggerRepair: (previewId: string, reason?: string) =>
    invoke<void>("trigger_repair", { previewId, reason }),
  restartComponent: (name: string) =>
    invoke<ComponentHealth | null>("restart_component", { name }, null),
  runBackupNow: () => invoke<string>("run_backup_now"),
  rollbackConfig: (date: string) => invoke<void>("rollback_config", { date }),
  setPaused: (paused: boolean) => invoke<void>("set_paused", { paused }),
  setVoiceMuted: (muted: boolean) => invoke<void>("set_voice_muted", { muted }),
  talkNow: () => invoke<void>("talk_now"),
  updateWakeSensitivity: (value: number) =>
    invoke<ContinuumConfig>("update_wake_sensitivity", { value }, DEFAULT_CONFIG),
  updateTtsLengthScale: (value: number) =>
    invoke<ContinuumConfig>("update_tts_length_scale", { value }, DEFAULT_CONFIG),
  updateTtsEngine: (engine: string) =>
    invoke<ContinuumConfig>("update_tts_engine", { engine }, DEFAULT_CONFIG),
  updateTtsPrimaryVoice: (voice: string) =>
    invoke<ContinuumConfig>("update_tts_primary_voice", { voice }, DEFAULT_CONFIG),
  quit: () => invoke<void>("quit_app"),
  listSkills: () => invoke<Skill[]>("list_skills", undefined, []),
  saveSkill: (input: SaveSkillInput) => invoke<Skill>("save_skill", { input }),
  deleteSkill: (name: string) => invoke<void>("delete_skill", { name }),
  toggleSkill: (name: string, enabled: boolean) =>
    invoke<ContinuumConfig>("toggle_skill", { name, enabled }),
  installSkillFromUrl: (url: string) => invoke<Skill>("install_skill_from_url", { url }),
  listMcpTools: () => invoke<McpTool[]>("list_mcp_tools", undefined, []),
  listInstalledMcpServers: () =>
    invoke<McpServerRegistration[]>("list_installed_mcp_servers", undefined, []),
  installMcpServer: (input: InstallMcpServerInput) =>
    invoke<McpServerRegistration>("install_mcp_server", { input }),
  listWorkers: (limit?: number) => invoke<WorkerSnapshot[]>("list_workers", { limit }, []),
  getWorker: (id: string) => invoke<WorkerSnapshot | null>("get_worker", { id }, null),
  cancelWorker: (id: string) => invoke<void>("cancel_worker", { id }),
  dismissWorker: (id: string) => invoke<void>("dismiss_worker", { id }),
  getRuntimeStatus: () =>
    invoke<RuntimeStatus>("get_runtime_status", undefined, {
      alive: false,
      state_path: "",
      binary_path: null,
    }),
  startRuntime: () => invoke<void>("start_runtime"),
  isOnboardingComplete: () => invoke<boolean>("is_onboarding_complete", undefined, true),
  resetOnboarding: () => invoke<boolean>("reset_onboarding", undefined, false),
  catalogList: () => invoke<CatalogEntry[]>("catalog_list", undefined, []),
  providersList: () => invoke<ProviderConnection[]>("providers_list", undefined, []),
  providerAdd: (input: ProviderAddInput) => invoke<ProviderConnection>("provider_add", { input }),
  providerTest: (id: string) => invoke<ConnectionTestReport>("provider_test", { id }),
  providerRefreshModels: (id: string) => invoke<string[]>("provider_refresh_models", { id }),
  providerRemove: (id: string) => invoke<void>("provider_remove", { id }),
  providerSetDefaultModel: (id: string, model: string) =>
    invoke<void>("provider_set_default_model", { id, model }),
  chatListConversations: () =>
    invoke<ConversationSummary[]>("chat_list_conversations", undefined, []),
  chatGetConversation: (id: string) =>
    invoke<Conversation | null>("chat_get_conversation", { conversationId: id }, null),
  chatCreateConversation: (providerId: string, model: string) =>
    invoke<Conversation>("chat_create_conversation", { providerId, model }),
  chatDeleteConversation: (id: string) =>
    invoke<void>("chat_delete_conversation", { conversationId: id }),
  chatRenameConversation: (id: string, title: string) =>
    invoke<void>("chat_rename_conversation", { conversationId: id, title }),
  chatSetConversationModel: (conversationId: string, providerId: string, model: string) =>
    invoke<void>("chat_set_conversation_model", { conversationId, providerId, model }),
  chatSendMessage: (conversationId: string, text: string) =>
    invoke<void>("chat_send_message", { conversationId, text }),
  chatCancel: (conversationId: string) => invoke<void>("chat_cancel", { conversationId }),
  onChatEvent,
  onMemoryEvent,
};

export interface RuntimeStatus {
  alive: boolean;
  state_path: string;
  binary_path: string | null;
}

export async function subscribeState(handler: (s: ContinuumState) => void) {
  return listen<ContinuumState>("continuum:state", handler);
}

export async function subscribeLogs(handler: (e: LogEntry) => void) {
  return listen<LogEntry>("continuum:log", handler);
}

export async function subscribeRepair(handler: (e: RepairEvent) => void) {
  return listen<RepairEvent>("continuum:repair", handler);
}

/** Subscribes to streamed chat deltas/done/error events for all conversations. */
export async function onChatEvent(
  handler: (payload: ChatEventPayload) => void
): Promise<() => void> {
  return listen<ChatEventPayload>("continuum:chat", handler);
}

/** Subscribes to live vault-change pushes (watcher-driven reindex/rebuild). */
export async function onMemoryEvent(
  handler: (payload: MemoryEventPayload) => void
): Promise<() => void> {
  return listen<MemoryEventPayload>("continuum:memory", handler);
}

// --- Defaults (used when the dashboard renders outside Tauri, e.g. `pnpm dev`) ---

export const DEFAULT_STATE: ContinuumState = {
  perception: {
    last_frame_id: null,
    last_frame_ts: null,
    last_description: "",
    last_foreground_app: "",
    last_screenshot_path: null,
    last_salience: 0,
    has_error_visible: false,
    frames_today: 0,
    monitor_count: 0,
    capture_events: 0,
    dropped_capture_events: 0,
    last_capture_at: null,
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
    curator: null,
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
    version: "0.1.0-alpha.10",
  },
  recent_actions: [],
};

export const DEFAULT_RESOURCE_CONFIG: ResourceConfig = {
  profile: "barely_notice",
  cpu_core_fraction: 0.3,
  cpu_min_threads: 2,
  cpu_max_threads: 8,
  ram_reserve_fraction: 0.5,
  gpu_enabled: null,
  gpu_min_vram_mb: 3000,
  battery_throttle: true,
  battery_core_fraction: 0.2,
  vision_min_ram_mb: 6144,
  vision_enabled: null,
  workers_max_concurrent: null,
  screen_interval_secs: null,
  context_interval_secs: null,
};

export const DEFAULT_CONFIG: ContinuumConfig = {
  health: {
    repair_timeout_secs: 600,
    runtime_start_timeout_secs: 90,
    repair_session_ttl_secs: 900,
    backup_retention: 7,
  },
  chat: {
    max_tokens: 8192,
    temperature: null,
    connect_timeout_secs: 10,
    stream_idle_timeout_secs: 60,
    cli_timeout_secs: 120,
    model_refresh_interval_secs: 300,
    system_prompt_path: null,
  },
  vision: {
    name: "SmolVLM-256M",
    model_path: "",
    gpu_enabled: false,
    input_width: 384,
    input_height: 384,
  },
  screen: {
    enabled: true,
    interval_secs: 3,
    capture_interval_ms: 200,
    capture_width: 1280,
    capture_height: 720,
    save_screenshots: false,
    all_monitors: true,
    excluded_monitor_ids: [],
    buffer_capacity: 64,
    meaningful_change_threshold: 0.025,
    vision_min_interval_ms: 2000,
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
  context: {
    poll_interval_secs: 1,
    redact_sensitive_titles: true,
    sensitive_process_names: [
      "1password.exe",
      "bitwarden.exe",
      "keepass.exe",
      "keepassxc.exe",
      "credentialuibroker.exe",
    ],
    sensitive_title_keywords: [
      "password",
      "passkey",
      "two-factor",
      "2fa",
      "private key",
      "seed phrase",
    ],
    terminal_process_names: [
      "windowsterminal.exe",
      "powershell.exe",
      "pwsh.exe",
      "cmd.exe",
      "bash.exe",
      "wsl.exe",
    ],
  },
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
    wake_keyword: "hey continuum",
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
  resources: DEFAULT_RESOURCE_CONFIG,
};
