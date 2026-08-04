"use client";

import { useEffect, useMemo, useState } from "react";
import { clsx } from "clsx";
import { AlertCircle, MessagesSquare, Sparkles, Users, Wallet, X } from "lucide-react";

import { useStore } from "@/lib/store";
import { continuum } from "@/lib/tauri";
import { Button, Card, StatusBadge, StatusOrb } from "@/components/ui/primitives";
import { PushToTalkButton } from "@/components/PushToTalkButton";
import type { CuratorSnapshot, RecentAction, VoiceMode, WorkerSnapshot } from "@/lib/types";

const ACTIVE_STATUSES = new Set(["queued", "starting", "running", "pending"]);

export function HomeTab() {
  const state = useStore((s) => s.state);
  const voiceMode: VoiceMode = state.voice.muted
    ? "muted"
    : state.orchestrator.active
      ? "thinking"
      : state.voice.mode;

  const [workers, setWorkers] = useState<WorkerSnapshot[]>([]);
  const [selected, setSelected] = useState<WorkerSnapshot | null>(null);
  const selectedId = selected?.id;

  useEffect(() => {
    let cancelled = false;
    async function poll() {
      try {
        const list = await continuum.listWorkers(50);
        if (!cancelled) {
          setWorkers(list);
          if (selectedId) {
            const match = list.find((w) => w.id === selectedId) ?? null;
            setSelected(match);
          }
        }
      } catch {
        /* dev server or permissions: ignore */
      }
    }
    poll();
    const t = setInterval(poll, 750);
    return () => {
      cancelled = true;
      clearInterval(t);
    };
  }, [selectedId]);

  const active = workers.filter((w) => ACTIVE_STATUSES.has(w.status));
  const completed = workers.filter((w) => !ACTIVE_STATUSES.has(w.status)).slice(0, 5);
  const totalCost = workers.map((w) => w.cost_usd ?? 0).reduce((a, b) => a + b, 0);

  return (
    <div className="mx-auto grid max-w-6xl grid-cols-12 gap-6">
      <section className="col-span-12 flex items-center gap-6 rounded-lg border border-bg-border bg-bg-surface p-6">
        <StatusOrb mode={voiceMode} size="lg" />
        <div className="flex-1">
          <h1 className="text-xl font-medium">
            {statusHeadline(voiceMode, state.orchestrator.last_wake_reason)}
          </h1>
          <p className="mt-1 text-sm text-ink-muted">
            {state.perception.last_description || "Waiting for the first perception frame…"}
          </p>
          <p className="mt-1 text-xs text-ink-dim">
            App: {state.perception.last_foreground_app || "–"} · salience{" "}
            {state.perception.last_salience.toFixed(2)}
            {state.perception.has_error_visible && (
              <span className="ml-2 inline-flex items-center gap-1 text-state-error">
                <AlertCircle size={12} /> error visible
              </span>
            )}
          </p>
        </div>
        <PushToTalkButton mode={voiceMode} />
        <ScreenshotThumb path={state.perception.last_screenshot_path} />
      </section>

      <Stats
        wakesToday={state.orchestrator.wakes_today}
        costToday={state.orchestrator.cost_usd_today + totalCost}
        episodic={state.memory.episodic_count}
        uptime={state.system.uptime_secs}
      />

      <CuratorRow curator={state.memory.curator} />

      <section className="col-span-12 md:col-span-7">
        <Card title="Workers" subtitle={`${active.length} running · ${completed.length} recent`}>
          {active.length === 0 && completed.length === 0 ? (
            <div className="py-6 text-center text-sm text-ink-dim">
              No workers yet. The orchestrator will spawn them when a task needs more than a few
              tool calls.
            </div>
          ) : (
            <ul className="space-y-3">
              {active.map((w) => (
                <WorkerRow
                  key={w.id}
                  worker={w}
                  onSelect={() => setSelected(w)}
                  onCancel={() => continuum.cancelWorker(w.id)}
                />
              ))}
              {completed.map((w) => (
                <WorkerRow
                  key={w.id}
                  worker={w}
                  onSelect={() => setSelected(w)}
                  onCancel={() => continuum.dismissWorker(w.id)}
                  dismiss
                />
              ))}
            </ul>
          )}
        </Card>
      </section>

      <section className="col-span-12 md:col-span-5">
        <Card
          title="Voice"
          subtitle={
            state.voice.partial_transcript
              ? "hearing…"
              : state.voice.ambient_mute_active
                ? `muted during ${state.voice.detected_call_app ?? "call"}`
                : "listening in the background"
          }
        >
          <Waveform listening={voiceMode === "listening"} />
          <div className="mt-2 min-h-[2rem] text-sm text-ink">
            {state.voice.partial_transcript ? (
              <span>"{state.voice.partial_transcript}"</span>
            ) : (
              <span className="text-ink-dim">–</span>
            )}
          </div>
        </Card>
      </section>

      <section className="col-span-12">
        <RecentTimeline actions={state.recent_actions} />
      </section>

      {selected && <WorkerDetail worker={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}

function WorkerRow({
  worker,
  onSelect,
  onCancel,
  dismiss,
}: {
  worker: WorkerSnapshot;
  onSelect: () => void;
  onCancel: () => void;
  dismiss?: boolean;
}) {
  const progressPct = Math.round(Math.min(1, worker.progress) * 100);
  return (
    <li>
      <div
        className="flex cursor-pointer items-center justify-between gap-2 text-sm"
        onClick={onSelect}
      >
        <span className="flex items-center gap-2">
          <StatusDot status={worker.status} />
          <span className="truncate">{worker.task}</span>
        </span>
        <span className="flex items-center gap-2 text-xs text-ink-muted">
          <span>{worker.model.replace("claude-", "")}</span>
          <span>
            {worker.elapsed_ms < 1000
              ? `${worker.elapsed_ms}ms`
              : `${Math.floor(worker.elapsed_ms / 1000)}s`}
          </span>
          <Button
            size="sm"
            variant="ghost"
            onClick={(e) => {
              e.stopPropagation();
              onCancel();
            }}
          >
            {dismiss ? "Dismiss" : "Cancel"}
          </Button>
        </span>
      </div>
      <div className="mt-1 h-1.5 overflow-hidden rounded-full bg-bg-elevated">
        <div className="h-full bg-accent-purple" style={{ width: `${progressPct}%` }} />
      </div>
      <div className="mt-1 flex items-center justify-between text-[11px] text-ink-dim">
        <span className="truncate">{worker.last_line || worker.status}</span>
        {worker.cost_usd != null && <span>${worker.cost_usd.toFixed(4)}</span>}
      </div>
    </li>
  );
}

function WorkerDetail({ worker, onClose }: { worker: WorkerSnapshot; onClose: () => void }) {
  return (
    <div className="fixed inset-0 z-40 flex items-center justify-center bg-black/60 p-6">
      <div className="max-h-[80vh] w-full max-w-3xl overflow-y-auto rounded-lg border border-bg-border bg-bg-surface p-6 shadow-xl">
        <div className="mb-4 flex items-start justify-between gap-4">
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2 text-xs text-ink-dim">
              <StatusDot status={worker.status} />
              <span>{worker.status}</span>
              <span>·</span>
              <span>{worker.model}</span>
              <span>·</span>
              <span>{worker.priority}</span>
            </div>
            <h2 className="mt-2 text-base font-medium text-ink">{worker.task}</h2>
            <p className="mt-1 font-mono text-xs text-ink-dim">{worker.cwd}</p>
          </div>
          <button onClick={onClose} className="rounded p-1 text-ink-dim hover:text-ink">
            <X size={16} />
          </button>
        </div>

        <dl className="mb-4 grid grid-cols-2 gap-x-4 gap-y-2 text-xs sm:grid-cols-4">
          <Stat label="Elapsed" value={`${Math.floor(worker.elapsed_ms / 100) / 10}s`} />
          <Stat
            label="Cost"
            value={worker.cost_usd != null ? `$${worker.cost_usd.toFixed(4)}` : "-"}
          />
          <Stat label="Tool calls" value={worker.tool_calls.toString()} />
          <Stat label="Session" value={worker.session_id?.slice(0, 8) ?? "-"} />
        </dl>

        {worker.skills.length > 0 && (
          <div className="mb-4">
            <div className="text-xs uppercase tracking-wider text-ink-dim">Active skills</div>
            <div className="mt-1 flex flex-wrap gap-1">
              {worker.skills.map((s) => (
                <span key={s} className="rounded bg-bg-elevated px-2 py-0.5 text-xs text-ink">
                  {s}
                </span>
              ))}
            </div>
          </div>
        )}

        <div className="mb-2 text-xs uppercase tracking-wider text-ink-dim">Live output</div>
        <pre className="max-h-96 overflow-y-auto rounded-md border border-bg-border bg-bg-elevated p-3 font-mono text-xs text-ink">
          {worker.result || worker.last_line || "(no output yet)"}
        </pre>

        {worker.error && (
          <div className="mt-3 rounded-md border border-state-error/50 bg-state-error/10 p-3 text-xs text-state-error">
            <div className="font-medium">Error</div>
            {worker.error}
          </div>
        )}

        <div className="mt-4 flex items-center justify-between text-xs text-ink-dim">
          <span>Model chose because: {worker.model_reason}</span>
          {ACTIVE_STATUSES.has(worker.status) ? (
            <Button size="sm" variant="ghost" onClick={() => continuum.cancelWorker(worker.id)}>
              Cancel worker
            </Button>
          ) : (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                continuum.dismissWorker(worker.id);
                onClose();
              }}
            >
              Dismiss
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-[10px] uppercase tracking-wider text-ink-dim">{label}</div>
      <div className="font-mono text-ink">{value}</div>
    </div>
  );
}

function StatusDot({ status }: { status: string }) {
  const color =
    status === "running" || status === "starting"
      ? "bg-accent-purple"
      : status === "queued" || status === "pending"
        ? "bg-accent-blue"
        : status === "completed"
          ? "bg-state-healthy"
          : status === "failed" || status === "timed_out"
            ? "bg-state-error"
            : "bg-ink-dim";
  return <span className={clsx("h-2 w-2 shrink-0 rounded-full", color)} />;
}

function statusHeadline(mode: VoiceMode, lastReason: string | null): string {
  switch (mode) {
    case "thinking":
      return lastReason ? `Thinking about: ${lastReason}` : "Thinking…";
    case "speaking":
      return "Speaking…";
    case "listening":
      return "Listening…";
    case "muted":
      return "Voice muted";
    case "error":
      return "Voice error";
    default:
      return "Idle";
  }
}

function Stats({
  wakesToday,
  costToday,
  episodic,
  uptime,
}: {
  wakesToday: number;
  costToday: number;
  episodic: number;
  uptime: number;
}) {
  const items = [
    { label: "Opus wakes", value: wakesToday.toLocaleString(), icon: Sparkles },
    { label: "Cost today", value: `$${costToday.toFixed(3)}`, icon: Wallet },
    { label: "Memories", value: episodic.toLocaleString(), icon: MessagesSquare },
    { label: "Uptime", value: humanDuration(uptime), icon: Users },
  ];
  return (
    <section className="col-span-12 grid grid-cols-2 gap-4 md:grid-cols-4">
      {items.map(({ label, value, icon: Icon }) => (
        <div key={label} className="rounded-lg border border-bg-border bg-bg-surface p-4">
          <div className="flex items-center gap-2 text-[11px] uppercase tracking-wider text-ink-dim">
            <Icon size={12} />
            {label}
          </div>
          <div className="mt-2 font-mono text-2xl text-ink">{value}</div>
        </div>
      ))}
    </section>
  );
}

/** Compact curator (Plan B memory-vault) health strip, beside the memory
 * stats above. `curator` is `undefined`/`null` until the dashboard's
 * runtime bridge has read a state.json at least once, or when running
 * outside Tauri (see DEFAULT_STATE) — treated the same as "off" here since
 * there is nothing meaningful to report either way. */
function CuratorRow({ curator }: { curator: CuratorSnapshot | null | undefined }) {
  if (!curator || !curator.enabled) {
    return (
      <section className="col-span-12 flex items-center gap-2 rounded-lg border border-bg-border bg-bg-surface px-4 py-2 text-xs text-ink-dim">
        <StatusBadge status="unknown" label="Curator: off" />
      </section>
    );
  }

  const failing = curator.consecutive_failures >= 3;
  const lastPass = curator.last_pass_at
    ? new Date(curator.last_pass_at).toLocaleTimeString(undefined, {
        hour: "2-digit",
        minute: "2-digit",
        hour12: false,
      })
    : "never";

  return (
    <section className="col-span-12 flex flex-wrap items-center gap-3 rounded-lg border border-bg-border bg-bg-surface px-4 py-2 text-xs text-ink-muted">
      <StatusBadge status={failing ? "degrading" : "healthy"} label="Curator" />
      <span>last pass {lastPass}</span>
      <span className="text-ink-dim">·</span>
      <span>{curator.pending_count.toLocaleString()} pending</span>
      <span className="text-ink-dim">·</span>
      <span>{curator.candidates_written_total.toLocaleString()} written</span>
      {failing && (
        <span className="ml-auto rounded-md border border-state-warn/30 bg-state-warn/15 px-2 py-0.5 font-medium text-state-warn">
          {curator.consecutive_failures} consecutive failures
        </span>
      )}
    </section>
  );
}

function ScreenshotThumb({ path }: { path: string | null }) {
  if (!path) {
    return (
      <div className="flex h-20 w-32 items-center justify-center rounded-md border border-dashed border-bg-border text-xs text-ink-dim">
        no screenshot
      </div>
    );
  }
  return (
    <div
      className="flex h-20 w-32 items-center justify-center rounded-md border border-bg-border bg-bg-elevated p-2 text-center text-[10px] text-ink-dim"
      title={path}
    >
      {path.split(/[\\/]/).pop()}
    </div>
  );
}

function Waveform({ listening }: { listening: boolean }) {
  const bars = useMemo(() => Array.from({ length: 24 }, (_, i) => i), []);
  return (
    <div className="flex h-10 items-center gap-0.5">
      {bars.map((i) => {
        const h = listening
          ? 20 + Math.round(Math.abs(Math.sin(i * 0.6 + Date.now() / 400)) * 60)
          : 10 + (i % 3) * 4;
        return (
          <span
            key={i}
            className={clsx(
              "w-1 rounded-sm transition-[height] duration-300",
              listening ? "bg-accent-blue" : "bg-bg-elevated"
            )}
            style={{ height: `${h}%` }}
          />
        );
      })}
    </div>
  );
}

function RecentTimeline({ actions }: { actions: RecentAction[] }) {
  if (actions.length === 0) {
    return (
      <Card title="Recent actions">
        <div className="py-6 text-center text-sm text-ink-dim">
          Nothing yet - perception frames will appear here as the triage layer evaluates them.
        </div>
      </Card>
    );
  }
  return (
    <Card title="Recent actions" subtitle={`last ${actions.length} events`}>
      <ul className="max-h-72 space-y-1.5 overflow-y-auto">
        {actions.map((a, idx) => (
          <li key={`${a.ts}-${idx}`} className="flex items-start justify-between gap-4 text-sm">
            <div className="flex min-w-0 items-start gap-2">
              <KindDot kind={a.kind} />
              <span className="truncate text-ink">{a.summary}</span>
            </div>
            <span className="shrink-0 font-mono text-[11px] text-ink-dim">
              {new Date(a.ts).toLocaleTimeString(undefined, { hour12: false })}
            </span>
          </li>
        ))}
      </ul>
    </Card>
  );
}

function KindDot({ kind }: { kind: RecentAction["kind"] }) {
  const color =
    kind === "wake"
      ? "bg-accent-purple"
      : kind === "triage"
        ? "bg-accent-blue"
        : kind === "repair"
          ? "bg-state-warn"
          : kind === "voice"
            ? "bg-state-healthy"
            : "bg-ink-dim";
  return <span className={clsx("mt-1.5 h-1.5 w-1.5 shrink-0 rounded-full", color)} />;
}

function humanDuration(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86400)}d`;
}
