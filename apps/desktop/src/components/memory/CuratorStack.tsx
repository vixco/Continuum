"use client";

// Floating top-right stack of pending-candidate proposal cards. Purely a
// session-local UI concern: "later" just hides a card behind a counter
// badge for the rest of this session (a `Set<string>` of ids kept in
// component state) - it does not touch backend state. Confirm/reject calls
// straight through to the parent via `onResolve`, which owns the API call
// and the subsequent `refresh()`. See task-14-brief.md.

import { useState } from "react";
import { Check, X } from "lucide-react";

import { NODE_COLORS, NODE_TYPE_LABELS } from "@/lib/memoryTheme";
import { Button, Card } from "@/components/ui/primitives";
import type { MemoryNodeSummary, MemoryResolution } from "@/lib/types";

const VISIBLE_LIMIT = 3;

interface CuratorStackProps {
  pending: MemoryNodeSummary[];
  onResolve: (id: string, resolution: MemoryResolution) => void;
  onOpen: (id: string) => void;
}

export function CuratorStack({ pending, onResolve, onOpen }: CuratorStackProps) {
  const [snoozed, setSnoozed] = useState<Set<string>>(new Set());
  const [expanded, setExpanded] = useState(false);

  const active = pending.filter((p) => !snoozed.has(p.id));
  if (active.length === 0) return null;

  const shown = expanded ? active : active.slice(0, VISIBLE_LIMIT);
  const overflowCount = active.length - shown.length;

  function snooze(id: string) {
    setSnoozed((s) => new Set(s).add(id));
  }

  return (
    <div className="absolute right-3 top-3 z-20 flex w-full max-w-sm flex-col gap-2">
      {shown.map((p) => (
        <Card key={p.id} dense className="shadow-lg">
          <div className="flex items-start justify-between gap-2">
            <button
              type="button"
              onClick={() => onOpen(p.id)}
              className="min-w-0 flex-1 truncate text-left text-sm font-medium text-ink hover:text-accent-amber"
            >
              {p.title}
            </button>
            <span className="flex shrink-0 items-center gap-1.5 rounded-md border border-bg-border bg-bg-elevated px-1.5 py-0.5 text-[10px] text-ink-muted">
              <span
                className="h-1.5 w-1.5 rounded-full"
                style={{ backgroundColor: NODE_COLORS[p.type] }}
              />
              {NODE_TYPE_LABELS[p.type]}
            </span>
          </div>
          {p.snippet && <p className="mt-1.5 line-clamp-2 text-xs text-ink-dim">{p.snippet}</p>}
          <div className="mt-2 flex items-center gap-2 text-[11px] text-ink-dim">
            <span>{Math.round(p.confidence * 100)}% confidence</span>
            <span>&middot;</span>
            <span>{p.source}</span>
          </div>
          <div className="mt-2.5 flex items-center gap-1.5">
            <Button
              size="sm"
              variant="primary"
              className="flex-1"
              onClick={() => onResolve(p.id, { action: "confirm" })}
            >
              <Check size={12} /> Confirm
            </Button>
            <Button
              size="sm"
              variant="danger"
              className="flex-1"
              onClick={() => onResolve(p.id, { action: "reject" })}
            >
              <X size={12} /> Reject
            </Button>
            <Button size="sm" variant="ghost" onClick={() => snooze(p.id)}>
              Later
            </Button>
          </div>
        </Card>
      ))}
      {(overflowCount > 0 || snoozed.size > 0) && (
        <div className="flex items-center gap-2">
          {overflowCount > 0 && (
            <button
              type="button"
              onClick={() => setExpanded(true)}
              className="rounded-md border border-bg-border bg-bg-surface px-2 py-1 text-xs text-ink-muted transition-colors hover:text-ink"
            >
              +{overflowCount} more
            </button>
          )}
          {snoozed.size > 0 && (
            <span className="rounded-md border border-bg-border bg-bg-surface px-2 py-1 text-xs text-ink-dim">
              +{snoozed.size} snoozed
            </span>
          )}
        </div>
      )}
    </div>
  );
}
