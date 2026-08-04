"use client";

// ThinkingBlock — collapsible, muted region for extended-thinking content.
// Pattern: one summary line ("Thinking · 1.4s") that expands to the full
// reasoning text. Reduced-motion users get the same content; the height
// transition is the only animated part and is cheap to skip.
//
// Matches the design-system "no decorative animation" rule: the only
// states are collapsed/expanded, and the marker is a typographic cue, not
// a sparkle.

import { useState } from "react";
import { clsx } from "clsx";
import { ChevronDown, Brain } from "lucide-react";

import type { ThinkingBlock as ThinkingBlockData } from "./types";

interface ThinkingBlockProps {
  block: ThinkingBlockData;
  defaultOpen?: boolean;
}

export function ThinkingBlock({ block, defaultOpen = false }: ThinkingBlockProps) {
  const [open, setOpen] = useState(defaultOpen);
  const duration = computeDuration(block);
  return (
    <div className="overflow-hidden rounded-md border border-bg-border/60 bg-bg-elevated/40">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-[11px] text-ink-muted transition-colors hover:bg-bg-elevated/70",
          "active:scale-[0.997]"
        )}
        aria-expanded={open}
      >
        <Brain size={11} strokeWidth={1.8} className="shrink-0 text-ink-dim/80" />
        <span className="font-medium tracking-wide">Thinking</span>
        {duration != null && (
          <span className="font-mono tabular-nums text-ink-dim/70">· {duration}</span>
        )}
        {!block.finishedAt && (
          <span className="ml-1 inline-flex h-1 w-1 animate-pulse rounded-full bg-amber-400" />
        )}
        <span className="flex-1" />
        <ChevronDown
          size={11}
          strokeWidth={2}
          className={clsx("text-ink-dim transition-transform duration-150", open && "rotate-180")}
        />
      </button>
      {open && (
        <div className="border-t border-bg-border/60 px-3 py-2 text-[12px] leading-relaxed text-ink-muted">
          <pre className="whitespace-pre-wrap font-sans">{block.text}</pre>
        </div>
      )}
    </div>
  );
}

function computeDuration(block: ThinkingBlockData): string | null {
  if (!block.finishedAt) return null;
  try {
    const start = new Date(block.startedAt).getTime();
    const end = new Date(block.finishedAt).getTime();
    const ms = Math.max(0, end - start);
    if (ms < 1000) return `${ms}ms`;
    return `${(ms / 1000).toFixed(1)}s`;
  } catch {
    return null;
  }
}
