"use client";

import { useEffect, useState } from "react";
import { clsx } from "clsx";
import {
  Bell,
  Bot,
  Boxes,
  CircleHelp,
  Clock3,
  FolderGit2,
  Home,
  LayoutGrid,
  LockKeyhole,
  Maximize2,
  MemoryStick,
  MessageSquareShare,
  Minus,
  Search,
  Settings,
  X,
} from "lucide-react";

import {
  AgentsScreen,
  HomeScreen,
  MemoryScreen,
  PermissionsScreen,
  ProjectsScreen,
  SettingsScreen,
  TimelineScreen,
} from "@/components/continuum/screens";
import { Dot } from "@/components/continuum/ui";

type TabId = "home" | "projects" | "memory" | "agents" | "permissions" | "timeline" | "settings";

const NAV: Array<{ id: TabId; label: string; icon: typeof Home }> = [
  { id: "home", label: "Home", icon: Home },
  { id: "projects", label: "Projects", icon: FolderGit2 },
  { id: "memory", label: "Memory", icon: MemoryStick },
  { id: "agents", label: "Agents", icon: Bot },
  { id: "permissions", label: "Permissions", icon: LockKeyhole },
  { id: "timeline", label: "Timeline", icon: Clock3 },
  { id: "settings", label: "Settings", icon: Settings },
];

export function Shell() {
  const [tab, setTab] = useState<TabId>("home");
  const [agentMode, setAgentMode] = useState<"handoff" | "launch">("handoff");
  const [commandOpen, setCommandOpen] = useState(false);

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

  const navigate = (next: string) => {
    if (NAV.some((item) => item.id === next)) setTab(next as TabId);
  };

  return (
    <div className="continuum-window">
      <WindowBar />
      <div className="continuum-body">
        <Sidebar active={tab} onSelect={setTab} onCommand={() => setCommandOpen(true)} />
        <div className="relative min-w-0 flex-1 overflow-hidden">
          <GlobalBar />
          <main
            className={clsx(
              "continuum-main",
              `continuum-main-${tab}`,
              tab === "agents" && `continuum-main-agents-${agentMode}`
            )}
          >
            {tab === "home" && <HomeScreen onNavigate={navigate} />}
            {tab === "projects" && <ProjectsScreen />}
            {tab === "memory" && <MemoryScreen />}
            {tab === "agents" && <AgentsScreen mode={agentMode} setMode={setAgentMode} />}
            {tab === "permissions" && <PermissionsScreen />}
            {tab === "timeline" && <TimelineScreen />}
            {tab === "settings" && <SettingsScreen />}
          </main>
        </div>
      </div>
      <StatusBar />
      {commandOpen && (
        <CommandPalette onClose={() => setCommandOpen(false)} onNavigate={navigate} />
      )}
    </div>
  );
}

function ContinuumMark({ compact = false }: { compact?: boolean }) {
  return (
    <div className="flex items-center gap-2.5 font-medium text-white">
      <span className="continuum-mark">
        <Boxes size={17} />
      </span>
      {!compact && <span>Continuum</span>}
    </div>
  );
}

function WindowBar() {
  return (
    <div className="continuum-windowbar">
      <ContinuumMark />
      <div className="ml-auto flex h-full items-center">
        <button aria-label="Minimize" className="window-control">
          <Minus size={14} />
        </button>
        <button aria-label="Maximize" className="window-control">
          <Maximize2 size={12} />
        </button>
        <button aria-label="Close" className="window-control hover:bg-red-500/70">
          <X size={14} />
        </button>
      </div>
    </div>
  );
}

function GlobalBar() {
  return (
    <header className="continuum-globalbar">
      <div className="ml-auto flex items-center gap-1.5">
        <button aria-label="Search" className="icon-button">
          <Search size={18} />
        </button>
        <button aria-label="Notifications" className="icon-button relative">
          <Bell size={17} />
          <span className="notification-badge">3</span>
        </button>
        <button aria-label="Help" className="icon-button">
          <CircleHelp size={17} />
        </button>
        <button aria-label="Profile" className="continuum-avatar">
          TS
          <span />
        </button>
      </div>
    </header>
  );
}

function Sidebar({
  active,
  onSelect,
  onCommand,
}: {
  active: TabId;
  onSelect: (tab: TabId) => void;
  onCommand: () => void;
}) {
  return (
    <aside className="continuum-sidebar">
      <div className="flex h-16 items-center justify-between px-5">
        <span className="text-[16px] font-semibold text-white">Continuum</span>
        <LayoutGrid size={16} className="text-white/65" />
      </div>
      <nav className="space-y-1 px-3" aria-label="Main navigation">
        {NAV.map(({ id, label, icon: Icon }) => (
          <button
            key={id}
            onClick={() => onSelect(id)}
            aria-current={active === id ? "page" : undefined}
            className={clsx("continuum-nav-item", active === id && "is-active")}
          >
            <Icon size={17} strokeWidth={1.8} />
            <span>{label}</span>
          </button>
        ))}
        {active === "settings" && (
          <div className="continuum-settings-nav" aria-label="Settings sections">
            {["Integrations & Models", "General", "Security", "Billing", "About"].map(
              (label, index) => (
                <button key={label} className={clsx(index === 0 && "is-active")}>
                  {label}
                </button>
              )
            )}
          </div>
        )}
      </nav>
      <div className="mt-auto p-4">
        <button onClick={onCommand} className="continuum-command-button">
          <MessageSquareShare size={16} className="text-amber-400" />
          <span>Ask Continuum</span>
          <kbd>Ctrl + K</kbd>
        </button>
        <div className="mt-5 border-t border-white/[.06] pt-5">
          <button className="flex w-full items-center gap-3 rounded-lg p-1.5 text-left hover:bg-white/[.03]">
            <span className="continuum-avatar h-9 w-9">
              TS
              <span />
            </span>
            <span className="min-w-0 flex-1">
              <b className="block truncate text-[11px] text-white/85">Toshan Soekar</b>
              <small className="block truncate text-[9px] text-white/35">
                toshan@continuum.app
              </small>
            </span>
            <span className="text-amber-400">⌄</span>
          </button>
        </div>
      </div>
    </aside>
  );
}

function StatusBar() {
  return (
    <footer className="continuum-statusbar">
      <div className="flex items-center gap-2">
        <Dot /> <span>Local mode</span>
      </div>
      <div className="h-4 w-px bg-white/[.07]" />
      <div className="flex items-center gap-2 text-emerald-400/80">
        ✓ <span className="text-white/45">All systems operational</span>
      </div>
      <div className="ml-auto flex items-center gap-2">
        <Dot />
        <span>Context auto-saves</span>
      </div>
    </footer>
  );
}

function CommandPalette({
  onClose,
  onNavigate,
}: {
  onClose: () => void;
  onNavigate: (tab: string) => void;
}) {
  return (
    <div className="command-scrim" role="dialog" aria-modal="true" aria-label="Ask Continuum">
      <div className="command-palette">
        <div className="flex items-center gap-3 border-b border-white/[.07] px-4">
          <Search size={18} className="text-amber-400" />
          <input
            autoFocus
            placeholder="Ask Continuum or jump to a page..."
            className="h-14 flex-1 bg-transparent text-sm text-white outline-none placeholder:text-white/30"
          />
          <kbd>Esc</kbd>
        </div>
        <div className="p-2">
          {NAV.slice(0, 6).map(({ id, label, icon: Icon }) => (
            <button
              key={id}
              onClick={() => {
                onNavigate(id);
                onClose();
              }}
              className="flex min-h-11 w-full items-center gap-3 rounded-lg px-3 text-[12px] text-white/65 hover:bg-amber-500/[.08] hover:text-white"
            >
              <Icon size={15} className="text-amber-400" /> Open {label}
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}
