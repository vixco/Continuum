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
} from "lucide-react";

import { bootstrapStore, useStore } from "@/lib/store";
import { kairo, listen } from "@/lib/tauri";
import { StatusOrb } from "@/components/ui/primitives";
import { HomeTab } from "@/components/tabs/HomeTab";
import { BrainTab } from "@/components/tabs/BrainTab";
import { MemoryTab } from "@/components/tabs/MemoryTab";
import { ToolsTab } from "@/components/tabs/ToolsTab";
import { VoiceTab } from "@/components/tabs/VoiceTab";
import { AutomationsTab } from "@/components/tabs/AutomationsTab";
import { LogsTab } from "@/components/tabs/LogsTab";
import { HealthTab } from "@/components/tabs/HealthTab";

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
  const voice = useStore((s) => s.state.voice);
  const system = useStore((s) => s.state.system);
  const orchestrator = useStore((s) => s.state.orchestrator);
  const paused = system.paused;

  useEffect(() => {
    void bootstrapStore();
  }, []);

  useEffect(() => {
    const id = window.setInterval(() => setNow(new Date()), 1000);
    return () => window.clearInterval(id);
  }, []);

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

  return (
    <div className="flex h-screen w-screen overflow-hidden">
      <Sidebar active={tab} onSelect={setTab} />
      <div className="flex flex-1 flex-col overflow-hidden">
        <Topbar
          now={now}
          mode={effectiveMode}
          paused={paused}
          voiceMuted={voice.muted}
          onTogglePause={() => kairo.setPaused(!paused)}
          onToggleVoice={() => kairo.setVoiceMuted(!voice.muted)}
        />
        <main className="flex-1 overflow-y-auto p-6">
          {tab === "home" && <HomeTab />}
          {tab === "brain" && <BrainTab />}
          {tab === "memory" && <MemoryTab />}
          {tab === "tools" && <ToolsTab />}
          {tab === "voice" && <VoiceTab />}
          {tab === "automations" && <AutomationsTab />}
          {tab === "logs" && <LogsTab />}
          {tab === "health" && <HealthTab />}
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
        "flex flex-col border-r border-bg-border bg-bg-surface transition-[width]",
        expanded ? "w-48" : "w-14",
      )}
      onMouseEnter={() => setExpanded(true)}
      onMouseLeave={() => setExpanded(false)}
    >
      <div className="flex h-12 items-center justify-center border-b border-bg-border">
        <span className="font-semibold tracking-tight">
          K
          <span className="text-accent-purple">AI</span>
          {expanded && <span>ro</span>}
        </span>
      </div>
      <div className="flex-1 py-2">
        {TABS.map(({ id, label, icon: Icon }) => (
          <button
            key={id}
            onClick={() => onSelect(id)}
            className={clsx(
              "flex w-full items-center gap-3 px-4 py-2 text-sm transition-colors",
              "hover:bg-bg-hover",
              active === id
                ? "bg-bg-hover text-ink border-l-2 border-accent-purple"
                : "border-l-2 border-transparent text-ink-muted",
            )}
          >
            <Icon size={16} strokeWidth={2} />
            {expanded && <span>{label}</span>}
          </button>
        ))}
      </div>
      <div className="border-t border-bg-border p-2">
        <button
          className="flex w-full items-center gap-3 px-2 py-1.5 text-xs text-ink-dim hover:text-ink"
          onClick={() => kairo.quit()}
        >
          <Power size={14} />
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
  onTogglePause,
  onToggleVoice,
}: {
  now: Date;
  mode: import("@/lib/types").VoiceMode;
  paused: boolean;
  voiceMuted: boolean;
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
    <header className="flex h-12 items-center justify-between border-b border-bg-border bg-bg-surface px-4">
      <div className="flex items-center gap-3">
        <StatusOrb mode={paused ? "idle" : mode} size="sm" />
        <span className="text-sm text-ink">{statusText}</span>
      </div>
      <div className="flex items-center gap-2">
        <span className="kairo-subtle font-mono">
          {now.toLocaleTimeString(undefined, { hour12: false })}
        </span>
        <button
          onClick={onTogglePause}
          className="flex items-center gap-1.5 rounded-md border border-bg-border bg-bg-elevated px-2.5 py-1 text-xs hover:bg-bg-hover"
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
          className="flex items-center gap-1.5 rounded-md border border-bg-border bg-bg-elevated px-2.5 py-1 text-xs hover:bg-bg-hover"
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
