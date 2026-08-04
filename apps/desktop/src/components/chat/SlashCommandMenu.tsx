"use client";

// SlashCommandMenu — pop-up of /skills, /tools, /commands triggered by
// typing `/` in the composer. Filtering is fuzzy-substring on
// `searchHaystack`; arrow keys / enter / click select. ESC closes. The
// selected item's `insert` is dropped into the composer with a trailing
// space (for skills) or replaced whole (for commands like /clear).

import { useEffect, useMemo, useRef, useState } from "react";
import { clsx } from "clsx";
import { Command, Loader2, Terminal, Sparkles } from "lucide-react";

import type { SlashCommand } from "./types";

interface SlashCommandMenuProps {
  open: boolean;
  query: string;
  commands: SlashCommand[];
  onPick: (cmd: SlashCommand) => void;
  onClose: () => void;
}

export function SlashCommandMenu({
  open,
  query,
  commands,
  onPick,
  onClose,
}: SlashCommandMenuProps) {
  const [active, setActive] = useState(0);
  const list = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return commands.slice(0, 8);
    return commands.filter((c) => c.searchHaystack.toLowerCase().includes(q)).slice(0, 8);
  }, [commands, query]);
  const ref = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (open) setActive(0);
  }, [open, query]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setActive((a) => Math.min(list.length - 1, a + 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setActive((a) => Math.max(0, a - 1));
      } else if (e.key === "Enter" && list[active]) {
        e.preventDefault();
        onPick(list[active]);
      } else if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, list, active, onPick, onClose]);

  if (!open) return null;
  if (list.length === 0) {
    return (
      <div
        ref={ref}
        className="absolute bottom-full left-0 right-0 mb-1 rounded-md border border-bg-border bg-bg-surface px-3 py-2.5 text-[11px] text-ink-dim shadow-md"
      >
        No matches. Press{" "}
        <kbd className="rounded border border-bg-border bg-bg-elevated px-1 font-mono text-[10px]">
          esc
        </kbd>{" "}
        to close.
      </div>
    );
  }

  return (
    <div
      ref={ref}
      className="absolute bottom-full left-0 right-0 mb-1 max-h-72 overflow-y-auto rounded-md border border-bg-border bg-bg-surface py-1 shadow-md"
      role="listbox"
    >
      {list.map((c, i) => (
        <button
          key={c.id}
          type="button"
          onClick={() => onPick(c)}
          onMouseEnter={() => setActive(i)}
          className={clsx(
            "flex w-full items-center gap-2.5 px-3 py-1.5 text-left text-[12px] transition-colors",
            i === active ? "bg-amber-500/[0.08] text-ink" : "text-ink-muted hover:bg-bg-elevated"
          )}
          role="option"
          aria-selected={i === active}
        >
          <KindIcon kind={c.kind} />
          <span className="min-w-0 flex-1 truncate font-mono">{c.insert}</span>
          <span className="truncate text-[10.5px] text-ink-dim">{c.hint}</span>
        </button>
      ))}
    </div>
  );
}

function KindIcon({ kind }: { kind: SlashCommand["kind"] }) {
  if (kind === "skill") return <Sparkles size={11} className="text-amber-400/80" />;
  if (kind === "tool") return <Terminal size={11} className="text-ink-dim" />;
  return <Command size={11} className="text-ink-dim" />;
}

// --- Built-in slash commands. Skills and tools are appended by the tab. ---

export const BUILTIN_COMMANDS: SlashCommand[] = [
  {
    id: "cmd_clear",
    label: "/clear",
    hint: "Reset the composer and skill chips",
    kind: "command",
    insert: "/clear",
    searchHaystack: "clear reset composer",
  },
  {
    id: "cmd_help",
    label: "/help",
    hint: "Show keyboard shortcuts and available skills",
    kind: "command",
    insert: "/help",
    searchHaystack: "help shortcuts",
  },
  {
    id: "cmd_voice",
    label: "/voice",
    hint: "Begin a voice-capture session",
    kind: "command",
    insert: "/voice",
    searchHaystack: "voice listen microphone",
  },
  {
    id: "cmd_cancel",
    label: "/stop",
    hint: "Cancel the in-flight response",
    kind: "command",
    insert: "/stop",
    searchHaystack: "stop cancel abort",
  },
];

// Loader2 referenced for future async state; kept import-clean.
export const _SlashCommandMenuTypes = { Loader2 };
