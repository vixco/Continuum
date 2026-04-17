"use client";

import { useEffect, useState } from "react";
import { clsx } from "clsx";
import {
  Activity,
  Brain,
  Database,
  Wrench,
  AudioLines,
  Clock,
  FileText,
  HeartPulse,
  Power,
  Pause,
  Play,
  MicOff,
  Mic,
  ZapOff,
  PlayCircle,
} from "lucide-react";

import { bootstrapStore, useStore } from "@/lib/store";
import { kairo, listen } from "@/lib/tauri";
import type { RuntimeStatus } from "@/lib/tauri";
import { StatusOrb } from "@/components/ui/primitives";
import { HomeTab } from "@/components/tabs/HomeTab";
import { BrainTab } from "@/components/tabs/BrainTab";
import { MemoryTab } from "@/components/tabs/MemoryTab";
import { ToolsTab } from "@/components/tabs/ToolsTab";
import { VoiceTab } from "@/components/tabs/VoiceTab";
import { AutomationsTab } from "@/components/tabs/AutomationsTab";
import { LogsTab } from "@/components/tabs/LogsTab";
import { HealthTab } from "@/components/tabs/HealthTab";
import { OnboardingWizard } from "@/components/onboarding/OnboardingWizard";

type TabId =
  | "home"
  | "brain"
  | "memory"
  | "tools"
  | "voice"
  | "automations"
  | "logs"
  | "health";

const TABS: Array<{ id: TabId; label: string; icon: typeof Activity }> = [
  { id: "home", label: "Home", icon: Activity },
  { id: "brain", label: "Brain", icon: Brain },
  { id: "memory", label: "Memory", icon: Database },
  { id: "tools", label: "Tools", icon: Wrench },
  { id: "voice", label: "Voice", icon: AudioLines },
  { id: "automations", label: "Automations", icon: Clock },
  { id: "logs", label: "Logs", icon: FileText },
  { id: "health", label: "Health", icon: HeartPulse },
];

export function Shell() {
  const [tab, setTab] = useState<TabId>("home");
  const [now, setNow] = useState(() => new Date());
  const [onboardingNeeded, setOnboardingNeeded] = useState<boolean | null>(null);
  const [runtimeStatus, setRuntimeStatus] = useState<RuntimeStatus | null>(null);
  const [startingRuntime, setStartingRuntime] = useState(false);
  const voice = useStore((s) => s.state.voice);
  const system = useStore((s) => s.state.system);
  const orchestrator = useStore((s) => s.state.orchestrator);
  const paused = system.paused;
  const version = system.version;

  useEffect(() => {
    void bootstrapStore();
  }, []);

  useEffect(() => {
    (async () => {
      try {
        const { invoke } = await import("@tauri-apps/api/core");
        const complete = await invoke<boolean>("is_onboarding_complete");
        setOnboardingNeeded(!complete);
      } catch {
        // Running outside Tauri or command not yet wired — skip wizard.
        setOnboardingNeeded(false);
      }
    })();
  }, []);

  useEffect(() => {
    const id = window.setInterval(() => setNow(new Date()), 1000);
    return () => window.clearInterval(id);
  }, []);

  useEffect(() => {
    let cancelled = false;
    const poll = async () => {
      try {
        const s = await kairo.getRuntimeStatus();
        if (!cancelled) setRuntimeStatus(s);
      } catch {
        /* running outside Tauri — ignore */
      }
    };
    void poll();
    const id = window.setInterval(poll, 3000);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, []);

  const onStartRuntime = async () => {
    setStartingRuntime(true);
    try {
      await kairo.startRuntime();
    } catch (e) {
      console.error("start_runtime failed", e);
      alert(`Could not start the Kairo runtime:\n${e}`);
    } finally {
      setStartingRuntime(false);
    }
  };

  useEffect(() => {
    let unsub: (() => void) | undefined;
    (async () => {
      unsub = await listen<{ action: string }>("kairo:control", (evt) => {
        switch (evt.action) {
          case "pause":
            void kairo.setPaused(true);
            break;
          case "resume":
            void kairo.setPaused(false);
            break;
          case "voice-on":
            void kairo.setVoiceMuted(false);
            break;
          case "voice-off":
            void kairo.setVoiceMuted(true);
            break;
        }
      });
    })();
    return () => unsub?.();
  }, []);

  const effectiveMode = orchestrator.active ? "thinking" : voice.mode;

  if (onboardingNeeded) {
    return <OnboardingWizard onComplete={() => setOnboardingNeeded(false)} />;
  }

  const currentTab = TABS.find((t) => t.id === tab);
  return (
    <div className="flex h-screen w-screen overflow-hidden bg-bg">
      <Sidebar active={tab} onSelect={setTab} />
      <div className="flex flex-1 flex-col overflow-hidden">
        <Topbar
          now={now}
          mode={effectiveMode}
          paused={paused}
          voiceMuted={voice.muted}
          version={version}
          onTogglePause={() => kairo.setPaused(!paused)}
          onToggleVoice={() => kairo.setVoiceMuted(!voice.muted)}
        />
        {runtimeStatus && !runtimeStatus.alive && (
          <div className="flex items-center justify-between gap-4 border-b border-amber-500/30 bg-amber-500/10 px-6 py-2.5 text-amber-200">
            <div className="flex items-center gap-2.5 text-sm">
              <ZapOff size={16} className="shrink-0" />
              <span>
                <span className="font-medium">Kairo runtime is offline.</span>{" "}
                <span className="text-amber-200/70">
                  Perception, triage, voice and the orchestrator are paused until you start it.
                </span>
              </span>
            </div>
            <button
              onClick={onStartRuntime}
              disabled={startingRuntime || !runtimeStatus.binary_path}
              className="inline-flex shrink-0 items-center gap-1.5 rounded-md border border-amber-400/40 bg-amber-500/20 px-3 py-1.5 text-xs font-medium text-amber-100 transition-colors hover:bg-amber-500/30 disabled:cursor-not-allowed disabled:opacity-50"
              title={runtimeStatus.binary_path ?? "kairo.exe not found"}
            >
              <PlayCircle size={14} />
              {startingRuntime ? "Starting\u2026" : "Start runtime"}
            </button>
          </div>
        )}
        <main className="flex-1 overflow-y-auto">
          <div className="mx-auto max-w-6xl px-8 py-6">
            {currentTab && (
              <div className="mb-6 flex items-center gap-3">
                <currentTab.icon size={20} className="text-accent-purple" />
                <h1 className="text-xl font-semibold tracking-tight text-ink">
                  {currentTab.label}
                </h1>
              </div>
            )}
            {tab === "home" && <HomeTab />}
            {tab === "brain" && <BrainTab />}
            {tab === "memory" && <MemoryTab />}
            {tab === "tools" && <ToolsTab />}
            {tab === "voice" && <VoiceTab />}
            {tab === "automations" && <AutomationsTab />}
            {tab === "logs" && <LogsTab />}
            {tab === "health" && <HealthTab />}
          </div>
        </main>
      </div>
    </div>
  );
}

function Sidebar({
  active,
  onSelect,
}: {
  active: TabId;
  onSelect: (id: TabId) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  return (
    <nav
      className={clsx(
        "flex flex-col border-r border-bg-border bg-bg-surface transition-[width] duration-200",
        expanded ? "w-52" : "w-16",
      )}
      onMouseEnter={() => setExpanded(true)}
      onMouseLeave={() => setExpanded(false)}
    >
      <div className="flex h-14 items-center justify-center border-b border-bg-border">
        <span className="font-semibold tracking-tight text-base">
          K
          <span className="text-accent-purple">AI</span>
          {expanded && <span>ro</span>}
        </span>
      </div>
      <div className="flex-1 space-y-0.5 px-2 py-3">
        {TABS.map(({ id, label, icon: Icon }) => (
          <button
            key={id}
            onClick={() => onSelect(id)}
            title={!expanded ? label : undefined}
            className={clsx(
              "group flex w-full items-center gap-3 rounded-md px-3 py-2 text-sm transition-colors",
              active === id
                ? "bg-accent-purple/15 text-ink"
                : "text-ink-muted hover:bg-bg-hover hover:text-ink",
            )}
          >
            <Icon
              size={16}
              strokeWidth={2}
              className={clsx(
                "shrink-0 transition-colors",
                active === id ? "text-accent-purple" : "text-ink-dim group-hover:text-ink-muted",
              )}
            />
            {expanded && <span className="truncate">{label}</span>}
          </button>
        ))}
      </div>
      <div className="border-t border-bg-border p-2">
        <button
          className="flex w-full items-center gap-3 rounded-md px-3 py-2 text-xs text-ink-dim transition-colors hover:bg-bg-hover hover:text-ink"
          onClick={() => kairo.quit()}
        >
          <Power size={14} className="shrink-0" />
          {expanded && <span>Quit</span>}
        </button>
      </div>
    </nav>
  );
}

function Topbar({
  now,
  mode,
  paused,
  voiceMuted,
  version,
  onTogglePause,
  onToggleVoice,
}: {
  now: Date;
  mode: import("@/lib/types").VoiceMode;
  paused: boolean;
  voiceMuted: boolean;
  version: string;
  onTogglePause: () => void;
  onToggleVoice: () => void;
}) {
  const statusText = paused
    ? "paused"
    : mode === "idle"
    ? "idle"
    : mode === "listening"
    ? "listening…"
    : mode === "thinking"
    ? "thinking…"
    : mode === "speaking"
    ? "speaking…"
    : mode === "muted"
    ? "muted"
    : mode;

  return (
    <header className="flex h-14 items-center justify-between border-b border-bg-border bg-bg-surface/60 px-6 backdrop-blur">
      <div className="flex items-center gap-2.5">
        <StatusOrb mode={paused ? "idle" : mode} size="sm" />
        <span className="text-sm font-medium text-ink">{statusText}</span>
      </div>
      <div className="flex items-center gap-3">
        <span
          className="font-mono text-xs text-ink-dim"
          title={`Kairo ${version}`}
        >
          v{version}
        </span>
        <span className="font-mono text-xs tabular-nums text-ink-muted">
          {now.toLocaleTimeString(undefined, { hour12: false })}
        </span>
        <div className="h-5 w-px bg-bg-border" />
        <button
          onClick={onTogglePause}
          className="inline-flex items-center gap-1.5 rounded-md border border-bg-border bg-bg-elevated px-2.5 py-1.5 text-xs text-ink transition-colors hover:border-bg-hover hover:bg-bg-hover"
        >
          {paused ? (
            <>
              <Play size={12} /> Resume
            </>
          ) : (
            <>
              <Pause size={12} /> Pause
            </>
          )}
        </button>
        <button
          onClick={onToggleVoice}
          className={clsx(
            "inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1.5 text-xs transition-colors",
            voiceMuted
              ? "border-state-warn/40 bg-state-warn/10 text-state-warn hover:bg-state-warn/20"
              : "border-bg-border bg-bg-elevated text-ink hover:border-bg-hover hover:bg-bg-hover",
          )}
        >
          {voiceMuted ? (
            <>
              <MicOff size={12} /> Unmute
            </>
          ) : (
            <>
              <Mic size={12} /> Mute
            </>
          )}
        </button>
      </div>
    </header>
  );
}
