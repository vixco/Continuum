import type {
  ComponentHealthSummary,
  ContinuumConfig,
  ContinuumState,
  ContextEngineSnapshot,
  ObservationPauseStatus,
  ObservationTogglesView,
} from "./types";

export type ObservationStatusKind =
  | "observing"
  | "paused"
  | "permission_required"
  | "vision_unavailable"
  | "degraded"
  | "processing"
  | "historical_context_off"
  | "off"
  | "unavailable";

export type ObservationSourceState =
  "active" | "idle" | "last_known" | "off" | "unavailable" | "degraded";
export type PrivacyImpact = "higher" | "moderate" | "lower";

export interface ObservationSourceView {
  id: "screen" | "files" | "git" | "microphone" | "processes" | "triage" | "history";
  label: string;
  state: ObservationSourceState;
  enabled: boolean;
  canToggle: boolean;
  toggleName: keyof ObservationTogglesView | null;
  reason: string;
  privacy: string;
  privacyImpact: PrivacyImpact;
}

export interface CurrentActivityView {
  title: string;
  project: string | null;
  confidence: number | null;
  evidence: string;
  updatedAt: string | null;
  known: boolean;
}

export interface ObservationSummary {
  kind: ObservationStatusKind;
  label: string;
  reason: string;
  sources: ObservationSourceView[];
  activeCount: number;
  currentActivity: CurrentActivityView;
  lastUpdatedAt: string | null;
}

export interface ObservationSummaryInput {
  state: ContinuumState;
  config: ContinuumConfig;
  pauseStatus?: ObservationPauseStatus | null;
  runtimeAvailable: boolean;
  runtimeStarting?: boolean;
}

function normalizedDetail(summary: ComponentHealthSummary | null | undefined): string | null {
  const detail = summary?.detail?.trim();
  return detail ? detail : null;
}

function isPermissionReason(reason: string): boolean {
  return /permission|access denied|screen recording|consent/i.test(reason);
}

function watcherState(
  enabled: boolean,
  summary: ComponentHealthSummary | null | undefined,
  idle: boolean,
  unavailableReason: string
): Pick<ObservationSourceView, "state" | "reason"> {
  if (!enabled) return { state: "off", reason: "Off by choice." };
  if (!summary) return { state: "unavailable", reason: unavailableReason };
  if (!summary.enabled) {
    return {
      state: "unavailable",
      reason: normalizedDetail(summary) ?? "This capability is unavailable in the running runtime.",
    };
  }
  if (!summary.healthy || summary.should_restart) {
    return {
      state: "degraded",
      reason: normalizedDetail(summary) ?? "The runtime reported a problem with this capability.",
    };
  }
  if (idle) {
    return {
      state: "idle",
      reason:
        normalizedDetail(summary) ??
        "Enabled, but slowed down because there is no recent activity.",
    };
  }
  return { state: "active", reason: normalizedDetail(summary) ?? "Active now." };
}

function toggleState(
  toggles: ObservationTogglesView | null,
  name: keyof ObservationTogglesView,
  summary: ComponentHealthSummary | null | undefined,
  idle: boolean
): Pick<ObservationSourceView, "state" | "reason" | "enabled" | "canToggle"> {
  if (!toggles) {
    return {
      state: "unavailable",
      reason: "Waiting for the runtime to publish live source state.",
      enabled: false,
      canToggle: false,
    };
  }
  const enabled = toggles[name];
  return {
    enabled,
    canToggle: true,
    ...watcherState(enabled, summary, idle, "Waiting for this watcher to publish health."),
  };
}

function currentActivity(state: ContinuumState): CurrentActivityView {
  const session = state.context.session;
  if (!session) {
    return {
      title: "Not enough evidence yet",
      project: null,
      confidence: null,
      evidence: "The runtime has not published a current session.",
      updatedAt: null,
      known: false,
    };
  }

  const appLine = [session.active_app, session.window_title].filter(Boolean).join(" — ");
  const inferred = session.local_only
    ? "Working in a private context"
    : session.current_task || session.current_goal;
  const title = inferred || appLine || "Not enough evidence yet";
  const known = Boolean(inferred || appLine);

  let evidence = "No supporting evidence was published.";
  if (session.local_only) {
    evidence = "The active window is in a private zone; detailed goal and task text stay local.";
  } else if (session.active_project && session.active_app) {
    evidence = `Resolved from ${session.active_app} in ${session.active_project}.`;
  } else if (session.active_app) {
    evidence = `Based on the active application ${session.active_app}.`;
  } else if (session.active_project) {
    evidence = `Resolved to project ${session.active_project}.`;
  }

  return {
    title,
    project: session.active_project,
    confidence: inferred && !session.local_only ? session.confidence : null,
    evidence,
    updatedAt: session.updated,
    known,
  };
}

function historicalContextSource(
  state: ContinuumState,
  config: ContinuumConfig
): Pick<ObservationSourceView, "enabled" | "state" | "reason"> {
  const retentionDays = config.storage.retention_days;
  if (retentionDays <= 0) {
    return {
      enabled: false,
      state: "off",
      reason: "Historical retention is off by choice.",
    };
  }

  const page = state.context.page;
  const writer = state.context.engine?.events_writer;
  const latestEventAt = page?.recent_events.reduce<string | null>(
    (latest, event) => (!latest || event.ts > latest ? event.ts : latest),
    null
  );
  const retentionLabel = `${retentionDays} day${retentionDays === 1 ? "" : "s"}`;
  const latestLabel = latestEventAt
    ? ` Latest retained event: ${new Date(latestEventAt).toLocaleString()}.`
    : " No retained events have been published yet.";

  if (!writer) {
    if (page) {
      return {
        enabled: true,
        state: "last_known",
        reason: `Showing last-known historical context; writer health is not available.${latestLabel}`,
      };
    }
    return {
      enabled: true,
      state: "unavailable",
      reason:
        "Historical retention is configured, but neither writer health nor a history projection is available.",
    };
  }

  if (!writer.enabled) {
    return {
      enabled: true,
      state: "unavailable",
      reason:
        normalizedDetail(writer) ??
        "Historical retention is configured, but the runtime history writer is unavailable.",
    };
  }

  if (!writer.healthy || writer.should_restart) {
    return {
      enabled: true,
      state: "degraded",
      reason: `${
        normalizedDetail(writer) ?? "The runtime history writer reported a problem."
      }${latestEventAt ? latestLabel : ""}`,
    };
  }

  if (!page) {
    return {
      enabled: true,
      state: "unavailable",
      reason:
        "The history writer is healthy, but the desktop has not received a historical-context projection yet.",
    };
  }

  return {
    enabled: true,
    state: "active",
    reason: `History writer is healthy; source-attributed activity is retained for ${retentionLabel}.${latestLabel}`,
  };
}

function sourceViews(state: ContinuumState, config: ContinuumConfig): ObservationSourceView[] {
  const engine: ContextEngineSnapshot | null = state.context.engine;
  const toggles = state.context.page?.toggles ?? null;
  const idle = engine?.idle ?? false;

  const screen = toggleState(toggles, "screen", engine?.live_context, idle);
  if (screen.enabled && config.resources.vision_enabled === false) {
    screen.state = "unavailable";
    screen.reason = "Vision is disabled by the selected resource profile.";
  } else if (screen.enabled && !state.system.vision_model_loaded) {
    screen.state = "unavailable";
    screen.reason =
      normalizedDetail(engine?.live_context) ?? "The local vision model is not loaded.";
  }

  const files = toggleState(toggles, "files", engine?.file_watcher, idle);
  const git = toggleState(toggles, "git", engine?.git_watcher, idle);

  // Audio health is published outside ContextEngineSnapshot today. The live
  // toggle remains authoritative; model availability explains degradation.
  const microphone = toggleState(toggles, "mic", undefined, idle);
  if (microphone.enabled) {
    if (!state.system.stt_loaded) {
      microphone.state = "unavailable";
      microphone.reason = "Local speech recognition is not loaded.";
    } else {
      microphone.state = idle ? "idle" : "active";
      microphone.reason = idle ? "Enabled, but waiting for speech." : "Listening locally.";
    }
  }

  const processSummary = engine?.process_watcher;
  const processesEnabled = processSummary?.enabled ?? false;
  const processes = processSummary
    ? watcherState(
        processesEnabled,
        processSummary,
        idle,
        "The runtime has not published background-process watcher state."
      )
    : {
        state: "unavailable" as const,
        reason: "The runtime has not published background-process watcher state.",
      };
  const triageSummary = engine?.triage;
  const triageEnabled = triageSummary?.enabled ?? false;
  const triage = triageSummary
    ? watcherState(
        triageEnabled,
        triageSummary,
        idle,
        "The runtime has not published triage state."
      )
    : {
        state: "unavailable" as const,
        reason: "The runtime has not published triage state.",
      };

  const history = historicalContextSource(state, config);

  return [
    {
      id: "screen",
      label: "Screen & vision",
      ...screen,
      toggleName: "screen",
      privacyImpact: "higher",
      privacy: config.screen.save_screenshots
        ? "Screen descriptions stay local; raw screenshots are also stored on this device."
        : "Screen descriptions stay local; raw screenshots are not stored.",
    },
    {
      id: "files",
      label: "File activity",
      ...files,
      toggleName: "files",
      privacyImpact: "moderate",
      privacy: "Records change events only inside confirmed project folders.",
    },
    {
      id: "git",
      label: "Git activity",
      ...git,
      toggleName: "git",
      privacyImpact: "lower",
      privacy: "Records repository activity for confirmed projects.",
    },
    {
      id: "microphone",
      label: "Microphone",
      ...microphone,
      toggleName: "mic",
      privacyImpact: "higher",
      privacy: "Speech is transcribed locally when enabled.",
    },
    {
      id: "processes",
      label: "Background activity",
      enabled: processesEnabled,
      canToggle: false,
      toggleName: null,
      ...processes,
      privacyImpact: "moderate",
      privacy:
        "Publishes bounded process lifecycle and resource pressure, not command lines or environment variables.",
    },
    {
      id: "triage",
      label: "Relevance triage",
      enabled: triageEnabled,
      canToggle: false,
      toggleName: null,
      ...triage,
      privacyImpact: "lower",
      privacy: "Evaluates already-collected local events to decide what deserves attention.",
    },
    {
      id: "history",
      label: "Historical context",
      enabled: history.enabled,
      canToggle: false,
      toggleName: null,
      state: history.state,
      reason: history.reason,
      privacyImpact: "moderate",
      privacy:
        "Keeps source-attributed activity on this device until retention removes it or you delete it.",
    },
  ];
}

export function deriveObservationSummary(input: ObservationSummaryInput): ObservationSummary {
  const { state, config } = input;
  const sources = sourceViews(state, config);
  const activeCount = sources.filter(
    (source) =>
      source.id !== "history" &&
      source.id !== "triage" &&
      (source.state === "active" || source.state === "idle")
  ).length;
  const lastUpdatedAt =
    state.context.session?.updated ??
    state.perception.last_capture_at ??
    state.health.last_check_ts ??
    null;

  // Pause signals come from three live boundaries (durable lease, context
  // projection and the general runtime state). Treat any positive signal as
  // authoritative so a temporarily stale false value cannot make the UI claim
  // observation resumed before every boundary confirms it.
  const paused = Boolean(
    input.pauseStatus?.paused || state.context.page?.toggles.pause_all || state.system.paused
  );

  const base = {
    sources,
    activeCount,
    currentActivity: currentActivity(state),
    lastUpdatedAt,
  };

  if (input.runtimeStarting) {
    return {
      ...base,
      kind: "processing",
      label: "Starting observation",
      reason: "The local runtime is starting and has not published source state yet.",
    };
  }
  if (!input.runtimeAvailable) {
    return {
      ...base,
      kind: "unavailable",
      label: "Observation unavailable",
      reason: "The local runtime is not publishing a heartbeat.",
    };
  }
  if (paused) {
    const until = input.pauseStatus?.until;
    return {
      ...base,
      kind: "paused",
      label: "Paused",
      reason: until
        ? `Observation resumes automatically at ${new Date(until).toLocaleString()}.`
        : "No source is observing right now.",
    };
  }

  const screen = sources.find((source) => source.id === "screen");
  if (screen?.enabled && screen.state === "unavailable" && isPermissionReason(screen.reason)) {
    return {
      ...base,
      kind: "permission_required",
      label: "Permission required",
      reason: screen.reason,
    };
  }
  if (screen?.enabled && screen.state === "unavailable") {
    return {
      ...base,
      kind: "vision_unavailable",
      label: "Vision unavailable",
      reason: screen.reason,
    };
  }

  const degraded = sources.find((source) => source.enabled && source.state === "degraded");
  if (degraded) {
    return {
      ...base,
      kind: "degraded",
      label: "Degraded",
      reason: `${degraded.label}: ${degraded.reason}`,
    };
  }
  if (activeCount === 0) {
    return {
      ...base,
      kind: "off",
      label: "Observation off",
      reason: "Every available observation source is off.",
    };
  }

  const history = sources.find((source) => source.id === "history");
  if (history?.state === "off") {
    return {
      ...base,
      kind: "historical_context_off",
      label: "Historical context off",
      reason: "Live sources are active, but their history is not retained.",
    };
  }

  return {
    ...base,
    kind: "observing",
    label: state.context.engine?.idle ? "Observing · idle" : "Observing",
    reason: state.context.engine?.idle
      ? "Sources are enabled and have slowed down until activity resumes."
      : `${activeCount} local capabilities are active.`,
  };
}

export interface ObservationToggleResult {
  ok: boolean;
  message: string;
}

export async function requestObservationToggle(
  writeIntent: (intent: {
    kind: "set_toggle";
    name: keyof ObservationTogglesView;
    value: boolean;
  }) => Promise<void>,
  name: keyof ObservationTogglesView,
  value: boolean,
  label: string
): Promise<ObservationToggleResult> {
  try {
    await writeIntent({ kind: "set_toggle", name, value });
    return {
      ok: true,
      message: `${label} change queued. Waiting for the runtime to confirm it.`,
    };
  } catch (error) {
    return {
      ok: false,
      message: `${label} could not be changed: ${error instanceof Error ? error.message : String(error)}`,
    };
  }
}
