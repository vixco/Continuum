"use client";

// ToolGroup — collapsible header for a parallel fan-out of tool calls.
// When the model fires several tools in the same tick (e.g. "look up X, Y,
// and Z" or "search past events, then read 3 of them"), we collapse them
// into a single card so the conversation doesn't explode with rows. Tap to
// expand into the individual ToolInvocationCards.

import { useState } from "react";
import { clsx } from "clsx";
import { ChevronDown, Layers } from "lucide-react";

import { ToolInvocationCard } from "./ToolInvocationCard";
import type { ContentPart, ToolInvocation } from "./types";

interface ToolGroupProps {
  group: Extract<ContentPart, { kind: "tool_group" }>;
}

export function ToolGroup({ group }: ToolGroupProps) {
  const [open, setOpen] = useState(false);
  const running = group.invocations.filter((i) => i.status === "running").length;
  const errors = group.invocations.filter((i) => i.status === "error").length;
  return (
    <div className="overflow-hidden rounded-md border border-bg-border/70 bg-bg-elevated/40">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-[11px] text-ink-muted transition-colors hover:bg-bg-elevated/70 active:scale-[0.997]"
        aria-expanded={open}
      >
        <Layers size={11} strokeWidth={1.8} className="shrink-0 text-amber-400/80" />
        <span className="font-medium tracking-wide">
          {group.invocations.length} tools
        </span>
        {running > 0 && (
          <span className="font-mono tabular-nums text-amber-300">· {running} running</span>
        )}
        {errors > 0 && (
          <span className="font-mono tabular-nums text-state-error">· {errors} errored</span>
        )}
        <span className="flex-1" />
        <ChevronDown
          size={11}
          strokeWidth={2}
          className={clsx("text-ink-dim transition-transform duration-150", open && "rotate-180")}
        />
      </button>
      {open && (
        <div className="space-y-1.5 border-t border-bg-border/60 px-2 py-2">
          {group.invocations.map((inv: ToolInvocation, i) => (
            <ToolInvocationCard key={inv.id ?? i} invocation={inv} />
          ))}
        </div>
      )}
    </div>
  );
}
