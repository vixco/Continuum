"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { clsx } from "clsx";
import {
  Activity,
  AlertTriangle,
  BrainCircuit,
  CheckCircle2,
  Clock3,
  Eye,
  FileClock,
  FileText,
  GitBranch,
  History,
  Loader2,
  Mic,
  Monitor,
  Pause,
  Play,
  PowerOff,
  Settings2,
  ShieldAlert,
  ShieldCheck,
} from "lucide-react";

import { Button, Toggle } from "@/components/ui/primitives";
import {
  deriveObservationSummary,
  requestObservationToggle,
  type ObservationSourceView,
  type ObservationStatusKind,
} from "@/lib/observation";
import { useStore } from "@/lib/store";
import { continuum, type RuntimeStatus } from "@/lib/tauri";
import type { ObservationPausePreset, ObservationPauseStatus } from "@/lib/types";

const STATUS_TONE: Record<ObservationStatusKind, string> = {
  observing: "border-state-healthy/35 bg-bg-elevated text-ink",
  paused: "border-bg-border bg-bg-elevated text-ink-muted",
  permission_required: "border-state-warn/45 bg-state-warn/10 text-state-warn",
  vision_unavailable: "border-state-warn/45 bg-state-warn/10 text-state-warn",
  degraded: "border-state-error/40 bg-state-error/10 text-state-error",
  processing: "border-accent-amber/40 bg-accent-amber/10 text-ink",
  historical_context_off: "border-state-warn/40 bg-state-warn/10 text-state-warn",
  off: "border-bg-border bg-bg-elevated text-ink-muted",
  unavailable: "border-state-error/35 bg-state-error/10 text-state-error",
};

const SOURCE_STATE_LABEL: Record<ObservationSourceView["state"], string> = {
  active: "Live",
  idle: "Idle",
  last_known: "Last known",
  off: "Off",
  unavailable: "Unavailable",
  degraded: "Degraded",
};

const SOURCE_STATE_TONE: Record<ObservationSourceView["state"], string> = {
  active: "text-state-healthy",
  idle: "text-ink-muted",
  last_known: "text-ink-muted",
  off: "text-ink-dim",
  unavailable: "text-state-warn",
  degraded: "text-state-error",
};

const PRIVACY_LABEL = {
  higher: "Higher privacy impact",
  moderate: "Moderate privacy impact",
  lower: "Lower privacy impact",
} as const;

function sourceIcon(source: ObservationSourceView) {
  const props = { size: 14, strokeWidth: 1.8 };
  switch (source.id) {
    case "screen":
      return <Monitor {...props} />;
    case "files":
      return <FileText {...props} />;
    case "git":
      return <GitBranch {...props} />;
    case "microphone":
      return <Mic {...props} />;
    case "processes":
      return <Activity {...props} />;
    case "triage":
      return <BrainCircuit {...props} />;
    case "history":
      return <History {...props} />;
  }
}

function statusIcon(kind: ObservationStatusKind) {
  switch (kind) {
    case "observing":
      return <Eye size={14} />;
    case "paused":
    case "off":
      return <PowerOff size={14} />;
    case "processing":
      return <Loader2 size={14} className="animate-spin" />;
    case "permission_required":
      return <ShieldAlert size={14} />;
    case "degraded":
    case "unavailable":
      return <AlertTriangle size={14} />;
    case "vision_unavailable":
      return <Monitor size={14} />;
    case "historical_context_off":
      return <FileClock size={14} />;
  }
}

function navigate(tab: "context" | "health" | "settings") {
  window.dispatchEvent(new CustomEvent("continuum:navigate", { detail: tab }));
}

export function ObservationStatusControl() {
  const state = useStore((store) => store.state);
  const config = useStore((store) => store.config);
  const setConfig = useStore((store) => store.setConfig);
  const [runtime, setRuntime] = useState<RuntimeStatus>({
    alive: false,
    starting: true,
    error: null,
    state_path: "",
    binary_path: null,
  });
  const [pauseStatus, setPauseStatus] = useState<ObservationPauseStatus | null>(null);
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState<string | null>(null);
  const [notice, setNotice] = useState<{ ok: boolean; message: string } | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);

  const refreshRuntime = useCallback(async () => {
    const [nextRuntime, nextPause] = await Promise.allSettled([
      continuum.getRuntimeStatus(),
      continuum.getObservationPause(),
    ]);
    if (nextRuntime.status === "fulfilled") {
      setRuntime(nextRuntime.value);
    } else {
      setRuntime((current) => ({
        ...current,
        alive: false,
        starting: false,
        error: "Could not read runtime status.",
      }));
    }
    if (nextPause.status === "fulfilled") setPauseStatus(nextPause.value);
  }, []);

  useEffect(() => {
    void refreshRuntime();
    const timer = window.setInterval(() => void refreshRuntime(), 3000);
    return () => window.clearInterval(timer);
  }, [refreshRuntime]);

  useEffect(() => {
    if (!open) return;
    const close = (event: MouseEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) setOpen(false);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", escape);
    };
  }, [open]);

  const summary = useMemo(
    () =>
      deriveObservationSummary({
        state,
        config,
        pauseStatus,
        runtimeAvailable: runtime.alive,
        runtimeStarting: runtime.starting,
      }),
    [config, pauseStatus, runtime.alive, runtime.starting, state]
  );

  const toggleSource = async (source: ObservationSourceView, value: boolean) => {
    if (!source.toggleName || !source.canToggle) return;
    setPending(source.id);
    setNotice(null);
    const result = await requestObservationToggle(
      (intent) => continuum.contextWriteIntent(intent),
      source.toggleName,
      value,
      source.label
    );
    setNotice(result);
    setPending(null);
    if (result.ok) window.setTimeout(() => void refreshRuntime(), 500);
  };

  const setPause = async (preset: ObservationPausePreset) => {
    setPending("pause");
    setNotice(null);
    try {
      setPauseStatus(await continuum.pauseObservation(preset));
      setNotice({
        ok: true,
        message: "Observation is paused. Enabled source choices are preserved.",
      });
    } catch (error) {
      setNotice({
        ok: false,
        message: `Observation could not be paused: ${error instanceof Error ? error.message : String(error)}`,
      });
    } finally {
      setPending(null);
    }
  };

  const resume = async () => {
    setPending("resume");
    setNotice(null);
    try {
      setPauseStatus(await continuum.resumeObservation());
      setNotice({ ok: true, message: "Observation resumed for the sources that are enabled." });
    } catch (error) {
      setNotice({
        ok: false,
        message: `Observation could not resume: ${error instanceof Error ? error.message : String(error)}`,
      });
    } finally {
      setPending(null);
    }
  };

  const setScreenshotStorage = async (enabled: boolean) => {
    setPending("screenshots");
    setNotice(null);
    try {
      const next = await continuum.updateLiveContextConfig({ save_screenshots: enabled });
      setConfig(next);
      setNotice({
        ok: true,
        message: enabled
          ? "Raw screenshot storage is on. Captures remain on this device."
          : "Raw screenshot storage is off. New screen descriptions will not keep the image.",
      });
    } catch (error) {
      setNotice({
        ok: false,
        message: `Screenshot storage could not be changed: ${error instanceof Error ? error.message : String(error)}`,
      });
    } finally {
      setPending(null);
    }
  };

  const paused = summary.kind === "paused";
  const activity = summary.currentActivity;

  return (
    <div ref={rootRef} className="relative z-[70]">
      {open && (
        <section
          role="dialog"
          aria-label="Observation status and controls"
          className="absolute right-0 top-[calc(100%+0.5rem)] max-h-[calc(100vh-4.5rem)] w-[min(25rem,calc(100vw-2rem))] overflow-y-auto rounded-lg border border-bg-border bg-bg-surface shadow-2xl"
        >
          <header className="border-b border-bg-border p-4">
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <div className="flex items-center gap-2 text-sm font-semibold text-ink">
                  {statusIcon(summary.kind)}
                  {summary.label}
                </div>
                <p className="mt-1 text-xs leading-5 text-ink-muted">{summary.reason}</p>
              </div>
              <span className="shrink-0 rounded border border-bg-border px-2 py-0.5 text-[10px] text-ink-dim">
                {summary.activeCount} active
              </span>
            </div>

            <div className="mt-3 flex flex-wrap gap-2">
              {paused ? (
                <Button
                  size="sm"
                  variant="primary"
                  onClick={() => void resume()}
                  disabled={pending !== null || !runtime.alive}
                >
                  {pending === "resume" ? (
                    <Loader2 size={12} className="animate-spin" />
                  ) : (
                    <Play size={12} />
                  )}
                  Resume
                </Button>
              ) : (
                <>
                  <Button
                    size="sm"
                    variant="default"
                    onClick={() => void setPause("one_hour")}
                    disabled={pending !== null || !runtime.alive}
                  >
                    <Clock3 size={12} /> Pause 1 hour
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={() => void setPause("indefinite")}
                    disabled={pending !== null || !runtime.alive}
                  >
                    <Pause size={12} /> Pause until resumed
                  </Button>
                </>
              )}
            </div>
          </header>

          {notice && (
            <div
              role={notice.ok ? "status" : "alert"}
              className={clsx(
                "mx-4 mt-3 flex items-start gap-2 rounded-md border p-2.5 text-xs leading-5",
                notice.ok
                  ? "border-state-healthy/30 bg-state-healthy/10 text-state-healthy"
                  : "border-state-error/35 bg-state-error/10 text-state-error"
              )}
            >
              {notice.ok ? (
                <CheckCircle2 size={13} className="mt-0.5 shrink-0" />
              ) : (
                <AlertTriangle size={13} className="mt-0.5 shrink-0" />
              )}
              {notice.message}
            </div>
          )}

          <div className="p-4">
            <div className="mb-2 flex items-center justify-between">
              <div>
                <h2 className="text-xs font-semibold uppercase tracking-wider text-ink-muted">
                  Observation sources
                </h2>
                <p className="mt-0.5 text-[11px] text-ink-dim">
                  Live runtime state, not sample data.
                </p>
              </div>
            </div>
            <div className="space-y-2">
              {summary.sources.map((source) => (
                <div
                  key={source.id}
                  className="rounded-md border border-bg-border bg-bg-elevated p-3"
                >
                  <div className="flex items-start justify-between gap-3">
                    <div className="min-w-0 flex-1">
                      <div className="flex flex-wrap items-center gap-2 text-sm font-medium text-ink">
                        <span className="text-accent-amber">{sourceIcon(source)}</span>
                        {source.label}
                        <span
                          className={clsx(
                            "text-[10px] font-semibold",
                            SOURCE_STATE_TONE[source.state]
                          )}
                        >
                          {SOURCE_STATE_LABEL[source.state]}
                        </span>
                      </div>
                      <p className="mt-1 text-[11px] leading-4 text-ink-muted">{source.reason}</p>
                    </div>
                    {source.canToggle && source.toggleName !== "pause_all" ? (
                      <Toggle
                        checked={source.enabled}
                        disabled={pending !== null || !runtime.alive}
                        onChange={(next) => void toggleSource(source, next)}
                      />
                    ) : (
                      <span className="shrink-0 rounded border border-bg-border px-1.5 py-0.5 text-[9px] uppercase tracking-wider text-ink-dim">
                        Runtime controlled
                      </span>
                    )}
                  </div>
                  <details className="mt-2 text-[11px] text-ink-dim">
                    <summary className="cursor-pointer select-none text-ink-muted">
                      Privacy details
                    </summary>
                    <p className="mt-1 leading-4">
                      <span className="font-medium text-ink-muted">
                        {PRIVACY_LABEL[source.privacyImpact]}.
                      </span>{" "}
                      {source.privacy}
                    </p>
                  </details>
                </div>
              ))}
            </div>

            <div className="mt-4 rounded-md border border-bg-border bg-bg-elevated p-3">
              <div className="flex items-start justify-between gap-3">
                <div>
                  <div className="flex items-center gap-2 text-sm font-medium text-ink">
                    <ShieldCheck size={14} className="text-accent-blue" /> Store raw screenshots
                  </div>
                  <p className="mt-1 text-[11px] leading-4 text-ink-muted">
                    Off keeps descriptions and timestamps without saving the image. On stores images
                    locally under the configured retention policy.
                  </p>
                </div>
                <Toggle
                  checked={config.screen.save_screenshots}
                  disabled={pending !== null || !runtime.alive}
                  onChange={(next) => void setScreenshotStorage(next)}
                />
              </div>
              <div className="mt-2 flex items-center justify-between border-t border-bg-border pt-2 text-[11px] text-ink-muted">
                <span>Historical retention</span>
                <span className="font-mono text-ink">{config.storage.retention_days} days</span>
              </div>
            </div>

            <div className="mt-4 rounded-md border border-bg-border bg-bg-elevated p-3">
              <div className="text-xs font-semibold uppercase tracking-wider text-ink-muted">
                Current activity
              </div>
              <div className="mt-2 text-sm font-medium text-ink">{activity.title}</div>
              <div className="mt-1 text-[11px] leading-4 text-ink-muted">{activity.evidence}</div>
              <div className="mt-2 flex flex-wrap items-center gap-2 text-[10px] text-ink-dim">
                {activity.project && <span>Project: {activity.project}</span>}
                {activity.confidence !== null && (
                  <span>Confidence: {Math.round(activity.confidence * 100)}%</span>
                )}
                {activity.updatedAt && (
                  <span>Updated {new Date(activity.updatedAt).toLocaleTimeString()}</span>
                )}
              </div>
            </div>

            <div className="mt-4 flex flex-wrap gap-2 border-t border-bg-border pt-3">
              <Button size="sm" variant="default" onClick={() => navigate("context")}>
                <History size={12} /> Context & history
              </Button>
              {(summary.kind === "degraded" || summary.kind === "unavailable") && (
                <Button size="sm" variant="default" onClick={() => navigate("health")}>
                  <Activity size={12} /> Diagnose
                </Button>
              )}
              <Button size="sm" variant="ghost" onClick={() => navigate("settings")}>
                <Settings2 size={12} /> System settings
              </Button>
            </div>
          </div>
        </section>
      )}

      <button
        type="button"
        aria-expanded={open}
        aria-label={`${summary.label}. ${summary.reason}`}
        onClick={() => setOpen((value) => !value)}
        className={clsx(
          "press flex h-8 items-center gap-2 rounded-full border px-3 py-2 text-xs font-medium shadow-lg backdrop-blur-sm transition-colors",
          STATUS_TONE[summary.kind]
        )}
      >
        {statusIcon(summary.kind)}
        <span>{summary.label}</span>
        {summary.kind === "observing" && (
          <span className="text-[10px] text-ink-dim">{summary.activeCount}</span>
        )}
      </button>
    </div>
  );
}
