"use client";

import Image from "next/image";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { clsx } from "clsx";
import {
  BrainCircuit,
  CalendarClock,
  Check,
  Compass,
  Database,
  Home,
  MessagesSquare,
  Mic,
  Minus,
  RefreshCw,
  Search,
  Settings as SettingsIcon,
  Stethoscope,
  Terminal,
  X,
} from "lucide-react";

import { AutomationsTab } from "@/components/tabs/AutomationsTab";
import { BrainTab } from "@/components/tabs/BrainTab";
import { ChatTab } from "@/components/tabs/ChatTab";
import { ContextTab } from "@/components/tabs/ContextTab";
import { HealthTab } from "@/components/tabs/HealthTab";
import { HomeTab } from "@/components/tabs/HomeTab";
import { LogsTab } from "@/components/tabs/LogsTab";
import { MemoryTab } from "@/components/tabs/MemoryTab";
import { ToolsTab } from "@/components/tabs/ToolsTab";
import { VoiceTab } from "@/components/tabs/VoiceTab";
import { OnboardingWizard } from "@/components/onboarding/OnboardingWizard";
import { ProviderRefreshCoordinator } from "@/components/providers/ProviderRefreshCoordinator";
import { ObservationStatusControl } from "@/components/observation/ObservationStatusControl";
import { HealthStatusMenu } from "@/components/health/HealthStatusMenu";
import { SettingsPage } from "@/components/layout/SettingsPage";
import { StatusOrb } from "@/components/ui/primitives";
import { bootstrapStore, teardownStore, useStore } from "@/lib/store";
import { continuum, type RuntimeStatus, type UpdateInfo, windowControls } from "@/lib/tauri";
import type { VoiceMode } from "@/lib/types";

type TabId =
  | "home"
  | "chat"
  | "voice"
  | "context"
  | "brain"
  | "memory"
  | "tools"
  | "automations"
  | "health"
  | "logs"
  | "settings";

interface NavEntry {
  id: TabId;
  label: string;
  icon: typeof Home;
}

const NAV_GROUPS: Array<{ label: string; items: NavEntry[] }> = [
  {
    label: "Daily",
    items: [
      { id: "home", label: "Home", icon: Home },
      { id: "chat", label: "Chat", icon: MessagesSquare },
      { id: "voice", label: "Voice", icon: Mic },
      { id: "context", label: "Context", icon: Compass },
      { id: "memory", label: "Memory", icon: Database },
    ],
  },
  {
    label: "Configure",
    items: [
      { id: "brain", label: "Brain", icon: BrainCircuit },
      { id: "tools", label: "Tools & Skills", icon: Terminal },
      { id: "automations", label: "Automations", icon: CalendarClock },
    ],
  },
  {
    label: "Advanced",
    items: [
      { id: "health", label: "Health", icon: Stethoscope },
      { id: "logs", label: "Logs", icon: Terminal },
    ],
  },
];

const FLAT_NAV: NavEntry[] = NAV_GROUPS.flatMap((g) => g.items);
const COMMAND_ENTRIES: NavEntry[] = [
  ...FLAT_NAV,
  { id: "settings", label: "Settings", icon: SettingsIcon },
];

type UpdatePhase =
  "idle" | "checking" | "current" | "available" | "downloading" | "ready" | "error";

interface UpdateState {
  phase: UpdatePhase;
  update: UpdateInfo | null;
  message: string | null;
  progress: number | null;
}

const AUTO_UPDATE_STORAGE_KEY = "continuum.auto-updates";
const UPDATE_ATTEMPT_STORAGE_KEY = "continuum.update-attempted-version";

function attemptedUpdateVersion(): string | null {
  try {
    return window.localStorage.getItem(UPDATE_ATTEMPT_STORAGE_KEY);
  } catch {
    return null;
  }
}

function rememberUpdateAttempt(version: string): void {
  try {
    window.localStorage.setItem(UPDATE_ATTEMPT_STORAGE_KEY, version);
  } catch (error) {
    console.warn("Could not persist the updater attempt guard", error);
  }
}

function clearUpdateAttempt(): void {
  try {
    window.localStorage.removeItem(UPDATE_ATTEMPT_STORAGE_KEY);
  } catch (error) {
    console.warn("Could not clear the updater attempt guard", error);
  }
}

function updateErrorMessage(error: unknown, action: "check" | "install"): string {
  const detail = error instanceof Error ? error.message : String(error || "Unknown error");
  const nextStep =
    action === "install"
      ? "Retry from Settings. If it still fails, install the latest GitHub release manually."
      : "Check your connection, then try again from Settings.";
  return `${action === "install" ? "Update installation" : "Update check"} failed: ${detail}. ${nextStep}`;
}

function useUpdates() {
  const [autoUpdateEnabled, setAutoUpdateEnabled] = useState(true);
  const [preferencesReady, setPreferencesReady] = useState(false);
  const pendingVersionRef = useRef<string | null>(null);
  const [state, setState] = useState<UpdateState>({
    phase: "idle",
    update: null,
    message: null,
    progress: null,
  });

  useEffect(() => {
    const stored = window.localStorage.getItem(AUTO_UPDATE_STORAGE_KEY);
    setAutoUpdateEnabled(stored !== "false");
    setPreferencesReady(true);
  }, []);

  const downloadUpdate = useCallback(async () => {
    const pendingVersion = pendingVersionRef.current;
    if (pendingVersion) {
      rememberUpdateAttempt(pendingVersion);
    }
    setState((current) => ({ ...current, phase: "downloading", message: null, progress: 0 }));
    try {
      await continuum.downloadAndInstallPendingUpdate((downloaded, total) => {
        setState((current) => ({
          ...current,
          progress: total ? Math.round((downloaded / total) * 100) : null,
        }));
      });
      setState((current) => ({
        ...current,
        phase: "ready",
        message: `Update v${pendingVersion ?? ""} is ready. Restart when it suits you to apply it.`,
        progress: 100,
      }));
    } catch (error) {
      setState((current) => ({
        ...current,
        phase: "error",
        message: updateErrorMessage(error, "install"),
      }));
    }
  }, []);

  const restartToApplyUpdate = useCallback(async () => {
    try {
      await continuum.restartToApplyUpdate();
    } catch (error) {
      setState((current) => ({
        ...current,
        phase: "error",
        message: updateErrorMessage(error, "install"),
      }));
    }
  }, []);

  const checkForUpdates = useCallback(
    async (automatic = false) => {
      setState({ phase: "checking", update: null, message: null, progress: null });
      try {
        const update = await continuum.checkForUpdate();
        if (!update) {
          pendingVersionRef.current = null;
          clearUpdateAttempt();
          setState({ phase: "current", update: null, message: null, progress: null });
          return;
        }
        pendingVersionRef.current = update.version;
        const attemptedVersion = attemptedUpdateVersion();
        const previousAttemptDidNotFinish = attemptedVersion === update.version;
        setState({
          phase: "available",
          update,
          message: previousAttemptDidNotFinish
            ? `Update v${update.version} is still available because the previous automatic install did not finish. Retry it manually when ready.`
            : null,
          progress: null,
        });
        if (automatic && autoUpdateEnabled && !previousAttemptDidNotFinish) await downloadUpdate();
      } catch (error) {
        setState({
          phase: "error",
          update: null,
          message: updateErrorMessage(error, "check"),
          progress: null,
        });
      }
    },
    [autoUpdateEnabled, downloadUpdate]
  );

  useEffect(() => {
    if (preferencesReady) void checkForUpdates(true);
  }, [checkForUpdates, preferencesReady]);

  const setAutoUpdate = (enabled: boolean) => {
    setAutoUpdateEnabled(enabled);
    window.localStorage.setItem(AUTO_UPDATE_STORAGE_KEY, String(enabled));
  };

  return {
    autoUpdateEnabled,
    setAutoUpdate,
    state,
    checkForUpdates,
    downloadUpdate,
    restartToApplyUpdate,
  };
}

export function Shell() {
  const [tab, setTab] = useState<TabId>("home");
  const [commandOpen, setCommandOpen] = useState(false);
  const [onboarding, setOnboarding] = useState<boolean | null>(null);
  const updates = useUpdates();

  // Hydrate the live-data store once, then keep it subscribed to Tauri events.
  useEffect(() => {
    void bootstrapStore();
    return () => teardownStore();
  }, []);

  // First-run gate. The wizard self-persists completion; on done we re-render.
  useEffect(() => {
    void (async () => {
      try {
        const done = await continuum.isOnboardingComplete();
        setOnboarding(!done);
      } catch {
        setOnboarding(false);
      }
    })();
  }, []);

  const navigate = useCallback((next: string) => {
    if (FLAT_NAV.some((item) => item.id === next) || next === "settings") {
      setTab(next as TabId);
    }
  }, []);

  // Cross-component navigation without prop-drilling the tab setter: any
  // component can dispatch `continuum:navigate` with a tab id as detail
  // (e.g. the Home tab's Memories stat card jumping to the Memory tab).
  useEffect(() => {
    const onNavigate = (event: Event) => {
      const detail = (event as CustomEvent<unknown>).detail;
      if (typeof detail === "string") navigate(detail);
    };
    window.addEventListener("continuum:navigate", onNavigate);
    return () => window.removeEventListener("continuum:navigate", onNavigate);
  }, [navigate]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setCommandOpen((open) => !open);
      }
      if (event.key === "Escape") setCommandOpen(false);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  if (onboarding) {
    return <OnboardingWizard onComplete={() => setOnboarding(false)} />;
  }

  return (
    <div className="app-window" data-active-tab={tab}>
      <ProviderRefreshCoordinator />
      <TitleBar onCommand={() => setCommandOpen(true)} />
      <div className="app-body">
        <Sidebar active={tab} onSelect={setTab} />
        <main className="main">
          <UpdateBanner
            state={updates.state}
            onDownload={updates.downloadUpdate}
            onRestart={updates.restartToApplyUpdate}
          />
          <div className={clsx("main-scroll", (tab === "chat" || tab === "memory") && "is-flush")}>
            <div className={tab === "home" ? "contents" : "hidden"} aria-hidden={tab !== "home"}>
              <HomeTab />
            </div>
            {tab === "chat" && <ChatTab />}
            {tab === "voice" && <VoiceTab />}
            {tab === "context" && <ContextTab />}
            {tab === "brain" && <BrainTab />}
            {tab === "memory" && <MemoryTab />}
            {tab === "tools" && <ToolsTab />}
            {tab === "automations" && <AutomationsTab />}
            {tab === "health" && <HealthTab />}
            {tab === "logs" && <LogsTab />}
            {tab === "settings" && (
              <SettingsPage
                autoUpdateEnabled={updates.autoUpdateEnabled}
                onAutoUpdateChange={updates.setAutoUpdate}
                updateState={updates.state}
                onCheckForUpdates={() => void updates.checkForUpdates()}
                onInstallUpdate={() => void updates.downloadUpdate()}
                onRestartToApplyUpdate={() => void updates.restartToApplyUpdate()}
                onResetEverything={() => setOnboarding(true)}
              />
            )}
          </div>
        </main>
      </div>
      {commandOpen && (
        <CommandPalette onClose={() => setCommandOpen(false)} onNavigate={navigate} />
      )}
    </div>
  );
}

function TitleBar({ onCommand }: { onCommand: () => void }) {
  const voice = useStore((s) => s.state.voice);
  const orchestrator = useStore((s) => s.state.orchestrator);
  const version = useStore((s) => s.state.system.version);

  const mode: VoiceMode = voice.muted ? "muted" : orchestrator.active ? "thinking" : voice.mode;

  const runtime = useRuntime();
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    let unsub: (() => void) | undefined;
    void windowControls.onMaximizeChange(setMaximized).then((u) => {
      unsub = u;
    });
    return () => {
      unsub?.();
    };
  }, []);

  return (
    <div className="titlebar" role="banner" data-tauri-drag-region>
      <div className="tb-brand" data-tauri-drag-region>
        <span className="tb-mark">
          <Image
            src="/continuum-mark.png"
            alt=""
            width={18}
            height={18}
            draggable={false}
            unoptimized
            priority
          />
        </span>
        Continuum
        <HealthStatusMenu />
        <span className="tb-status no-drag">
          <StatusOrb mode={mode} size="sm" />
          <span className="capitalize">{mode}</span>
        </span>
      </div>

      <div className="tb-spacer" data-tauri-drag-region />

      <div className="tb-actions no-drag">
        <button
          type="button"
          onClick={onCommand}
          className="press flex items-center gap-2 rounded-md border border-bg-border bg-bg-elevated px-2.5 py-1.5 text-[11px] text-ink-muted hover:border-bg-hover hover:text-ink"
          title="Ask Continuum (Ctrl/⌘ K)"
        >
          <Search size={12} />
          <span className="hidden sm:inline">Ask</span>
          <kbd className="rounded border border-bg-border bg-bg-elevated px-1.5 font-mono text-[10px] text-ink-dim">
            Ctrl/⌘ K
          </kbd>
        </button>

        {!runtime.alive && runtime.starting && (
          <span className="tb-runtime offline" title="Continuum is starting automatically">
            <RefreshCw size={11} className="animate-spin" /> Starting runtime
          </span>
        )}
        {!runtime.alive && !runtime.starting && (
          <span
            className="tb-runtime offline"
            title={runtime.error ?? "The Continuum runtime is not publishing a heartbeat."}
            role="status"
          >
            Runtime unavailable
          </span>
        )}

        <ObservationStatusControl />

        <span className="mx-1 hidden text-[10px] text-ink-dim md:inline">{version}</span>

        <button
          type="button"
          aria-label="Minimize"
          className="win-btn"
          onClick={() => void windowControls.minimize()}
        >
          <Minus size={15} />
        </button>
        <button
          type="button"
          aria-label={maximized ? "Restore" : "Maximize"}
          className="win-btn relative"
          onClick={() => void windowControls.toggleMaximize()}
        >
          <span className="windows-caption-glyph" aria-hidden="true">
            {maximized ? "\uE923" : "\uE922"}
          </span>
        </button>
        <button
          type="button"
          aria-label="Close"
          className="win-btn close"
          onClick={() => void windowControls.hide()}
        >
          <X size={15} />
        </button>
      </div>
    </div>
  );
}

function useRuntime() {
  const [runtime, setRuntime] = useState<RuntimeStatus>({
    alive: false,
    starting: true,
    error: null,
    state_path: "",
    binary_path: null,
  });

  const refresh = useCallback(async () => {
    try {
      const status = await continuum.getRuntimeStatus();
      setRuntime(status);
    } catch {
      setRuntime((runtime) => ({
        ...runtime,
        alive: false,
        starting: false,
        error: "Could not read the runtime startup status.",
      }));
    }
  }, []);

  useEffect(() => {
    void refresh();
    const t = setInterval(() => void refresh(), runtime.starting ? 500 : 3000);
    return () => clearInterval(t);
  }, [refresh, runtime.starting]);

  return runtime;
}

function UpdateBanner({
  state,
  onDownload,
  onRestart,
}: {
  state: UpdateState;
  onDownload: () => void;
  onRestart: () => void;
}) {
  if (state.phase === "idle" || state.phase === "current" || state.phase === "checking")
    return null;

  const updateLabel = state.update ? `v${state.update.version}` : "update";
  return (
    <div className="update-banner">
      {state.phase === "error" ? (
        <>
          <span className="flex-1 text-red-300">{state.message}</span>
          {state.update && (
            <button
              onClick={onDownload}
              className="press rounded-md border border-red-300/50 px-3 py-1 text-[10px] font-medium text-red-200 hover:bg-red-300/10"
            >
              Retry install
            </button>
          )}
        </>
      ) : state.phase === "downloading" ? (
        <>
          <RefreshCw size={14} className="animate-spin text-amber-400" />
          <span>
            Installing {updateLabel}
            {state.progress !== null ? ` (${state.progress}%)` : ""}…
          </span>
        </>
      ) : state.phase === "ready" ? (
        <>
          <Check size={14} className="text-green-300" />
          <span className="flex-1">{state.message ?? `Update ready: ${updateLabel}`}</span>
          <button
            onClick={onRestart}
            className="press rounded-md border border-green-300/50 px-3 py-1 text-[10px] font-medium text-green-200 hover:bg-green-300/10"
          >
            Restart to update
          </button>
        </>
      ) : (
        <>
          <Check size={14} className="text-amber-400" />
          <span className="flex-1">{state.message ?? `Update available: ${updateLabel}`}</span>
          <button
            onClick={onDownload}
            className="press rounded-md border border-amber-400/50 px-3 py-1 text-[10px] font-medium text-amber-300 hover:bg-amber-400/10"
          >
            Download update
          </button>
        </>
      )}
    </div>
  );
}

function Sidebar({ active, onSelect }: { active: TabId; onSelect: (tab: TabId) => void }) {
  return (
    <aside className="sidebar" aria-label="Main navigation">
      {NAV_GROUPS.map((group) => (
        <div key={group.label} className="nav-group" aria-label={group.label}>
          <div className="nav-group-label" aria-hidden="true">
            {group.label}
          </div>
          {group.items.map(({ id, label, icon: Icon }) => (
            <button
              key={id}
              onClick={() => onSelect(id)}
              aria-current={active === id ? "page" : undefined}
              className={clsx("nav-item", active === id && "is-active")}
            >
              <Icon size={17} strokeWidth={1.8} />
              <span>{label}</span>
            </button>
          ))}
        </div>
      ))}

      <div className="sidebar-foot">
        <button
          onClick={() => onSelect("settings")}
          aria-current={active === "settings" ? "page" : undefined}
          className={clsx("nav-item", active === "settings" && "is-active")}
        >
          <SettingsIcon size={17} strokeWidth={1.8} />
          <span>Settings</span>
        </button>
      </div>
    </aside>
  );
}

// Command palette — fuzzy filter over the nav + a quick recent-actions
// index (filled in later when we have more than the nav). Keyboard:
// up/down to move, enter to commit, esc to dismiss. Auto-focuses the
// search field. Keeps a clean rest of <header /> kbd hint visible.
function CommandPalette({
  onClose,
  onNavigate,
}: {
  onClose: () => void;
  onNavigate: (tab: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return COMMAND_ENTRIES;
    return COMMAND_ENTRIES.filter(
      (entry) => entry.label.toLowerCase().includes(q) || entry.id.includes(q)
    );
  }, [query]);

  // Reset highlight when the filter changes so the user never lands on a
  // hidden row. Clamp on the way down too — if `filtered` shrinks below
  // the current `active` index we keep the highlight inside the list.
  useEffect(
    () => setActive((i) => Math.min(i, Math.max(0, filtered.length - 1))),
    [query, filtered.length]
  );

  // Hold the latest values in refs so the global keyboard handler can read
  // them without re-binding the listener on every keystroke. Without this
  // `useEffect` would churn the `keydown` listener on each character.
  const filteredRef = useRef(filtered);
  const activeRef = useRef(active);
  const onNavigateRef = useRef(onNavigate);
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    filteredRef.current = filtered;
  }, [filtered]);
  useEffect(() => {
    activeRef.current = active;
  }, [active]);
  useEffect(() => {
    onNavigateRef.current = onNavigate;
  }, [onNavigate]);
  useEffect(() => {
    onCloseRef.current = onClose;
  }, [onClose]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const list = filteredRef.current;
      if (list.length === 0) {
        if (e.key === "Escape") {
          e.preventDefault();
          onCloseRef.current();
        }
        return;
      }
      if (e.key === "ArrowDown") {
        e.preventDefault();
        const next = Math.min(list.length - 1, activeRef.current + 1);
        activeRef.current = next;
        setActive(next);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        const next = Math.max(0, activeRef.current - 1);
        activeRef.current = next;
        setActive(next);
      } else if (e.key === "Enter") {
        e.preventDefault();
        const idx = activeRef.current;
        const target = list[idx];
        if (target) {
          onNavigateRef.current(target.id);
          onCloseRef.current();
        }
      } else if (e.key === "Escape") {
        e.preventDefault();
        onCloseRef.current();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div
      className="command-scrim"
      role="dialog"
      aria-modal="true"
      aria-label="Ask Continuum"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="command-palette">
        <div className="flex items-center gap-3 border-b border-bg-border px-4">
          <Search size={18} className="text-amber-400/80" />
          <input
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Jump to a page…"
            className="h-14 flex-1 bg-transparent text-sm text-ink outline-none placeholder:text-ink-dim"
          />
          <kbd>Esc</kbd>
        </div>
        <div className="max-h-80 overflow-y-auto p-1">
          {filtered.length === 0 ? (
            <div className="px-3 py-6 text-center text-[11px] text-ink-dim">No matching pages.</div>
          ) : (
            filtered.map(({ id, label, icon: Icon }, i) => (
              <button
                key={id}
                onMouseEnter={() => setActive(i)}
                onClick={() => {
                  onNavigate(id);
                  onClose();
                }}
                className={clsx(
                  "press flex min-h-10 w-full items-center gap-3 rounded-md px-3 text-[12.5px] transition-colors",
                  i === active
                    ? "bg-amber-500/[0.08] text-ink"
                    : "text-ink-muted hover:bg-bg-elevated"
                )}
              >
                <Icon size={15} className="text-amber-400/80" />
                <span>{label}</span>
                {i === active && (
                  <span className="ml-auto font-mono text-[10px] text-ink-dim">⏎</span>
                )}
              </button>
            ))
          )}
        </div>
        <div className="border-t border-bg-border px-3 py-1.5 text-[10px] text-ink-dim">
          ↑↓ navigate · ⏎ open · Esc close
        </div>
      </div>
    </div>
  );
}
