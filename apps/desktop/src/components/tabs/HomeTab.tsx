"use client";

import { useMemo } from "react";
import { clsx } from "clsx";
import {
  AlertCircle,
  MessagesSquare,
  Mic,
  Sparkles,
  Users,
  Wallet,
} from "lucide-react";

import { useStore } from "@/lib/store";
import { Card, StatusOrb } from "@/components/ui/primitives";
import type { VoiceMode, RecentAction } from "@/lib/types";

export function HomeTab() {
  const state = useStore((s) => s.state);
  const voiceMode: VoiceMode = state.voice.muted
    ? "muted"
    : state.orchestrator.active
    ? "thinking"
    : state.voice.mode;

  return (
    <div className="mx-auto grid max-w-6xl grid-cols-12 gap-6">
      <section className="col-span-12 flex items-center gap-6 rounded-lg border border-bg-border bg-bg-surface p-6">
        <StatusOrb mode={voiceMode} size="lg" />
        <div className="flex-1">
          <h1 className="text-xl font-medium">
            {statusHeadline(voiceMode, state.orchestrator.last_wake_reason)}
          </h1>
          <p className="mt-1 text-sm text-ink-muted">
            {state.perception.last_description ||
              "Waiting for the first perception frame…"}
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
        <ScreenshotThumb path={state.perception.last_screenshot_path} />
      </section>

      <Stats />

      <section className="col-span-12 md:col-span-7">
        <Card title="Active workers" subtitle={`${state.workers.active.length} running`}>
          {state.workers.active.length === 0 ? (
            <div className="py-6 text-center text-sm text-ink-dim">
              No active workers.
            </div>
          ) : (
            <ul className="space-y-3">
              {state.workers.active.map((w) => (
                <li key={w.id}>
                  <div className="flex items-center justify-between text-sm">
                    <span>{w.task}</span>
                    <span className="text-xs text-ink-muted">{w.model}</span>
                  </div>
                  <div className="mt-1 h-1.5 overflow-hidden rounded-full bg-bg-elevated">
                    <div
                      className="h-full bg-accent-purple"
                      style={{ width: `${Math.min(1, w.progress) * 100}%` }}
                    />
                  </div>
                  <div className="mt-1 text-[11px] text-ink-dim">{w.status}</div>
                </li>
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
    </div>
  );
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

function Stats() {
  const state = useStore((s) => s.state);
  const items = [
    {
      label: "Opus wakes",
      value: state.orchestrator.wakes_today.toLocaleString(),
      icon: Sparkles,
    },
    {
      label: "Cost today",
      value: `$${state.orchestrator.cost_usd_today.toFixed(3)}`,
      icon: Wallet,
    },
    {
      label: "Memories",
      value: state.memory.episodic_count.toLocaleString(),
      icon: MessagesSquare,
    },
    {
      label: "Uptime",
      value: humanDuration(state.system.uptime_secs),
      icon: Users,
    },
  ];
  return (
    <section className="col-span-12 grid grid-cols-2 gap-4 md:grid-cols-4">
      {items.map(({ label, value, icon: Icon }) => (
        <div
          key={label}
          className="rounded-lg border border-bg-border bg-bg-surface p-4"
        >
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

function ScreenshotThumb({ path }: { path: string | null }) {
  if (!path) {
    return (
      <div className="flex h-20 w-32 items-center justify-center rounded-md border border-dashed border-bg-border text-xs text-ink-dim">
        no screenshot
      </div>
    );
  }
  // Tauri exposes file:// via convertFileSrc; we haven't wired the asset
  // protocol, so we just show the path in a mini card.
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
              listening ? "bg-accent-blue" : "bg-bg-elevated",
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
          Nothing yet — perception frames will appear here as the triage layer
          evaluates them.
        </div>
      </Card>
    );
  }
  return (
    <Card title="Recent actions" subtitle={`last ${actions.length} events`}>
      <ul className="max-h-72 space-y-1.5 overflow-y-auto">
        {actions.map((a, idx) => (
          <li
            key={`${a.ts}-${idx}`}
            className="flex items-start justify-between gap-4 text-sm"
          >
            <div className="flex items-start gap-2 min-w-0">
              <KindDot kind={a.kind} />
              <span className="text-ink truncate">{a.summary}</span>
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
  return (
    <span className={clsx("mt-1.5 h-1.5 w-1.5 shrink-0 rounded-full", color)} />
  );
}

function humanDuration(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86400)}d`;
}
