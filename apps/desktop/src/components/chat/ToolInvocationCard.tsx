"use client";

// ToolInvocationCard — collapsible row showing a single tool call.
//
// Collapsed: tool name (mono) + 1-line summary + status indicator.
// Expanded: full input JSON, full output JSON, duration. Streaming
// (status === "running") keeps the card in a quiet pulsing state and
// progressively reveals the output buffer.
//
// "Summary" is a one-line heuristic on the input shape: e.g. for
// `memory_query_episodic` with input `{query: "deploy"}` we show
// `query="deploy"`. Tools that want a richer summary can return a custom
// `label` on the invocation.

import { useState } from "react";
import { clsx } from "clsx";
import { ChevronDown, Check, AlertTriangle, Loader2, Terminal, XCircle } from "lucide-react";

import type { ToolInvocation } from "./types";

interface ToolInvocationCardProps {
  invocation: ToolInvocation;
  defaultOpen?: boolean;
}

export function ToolInvocationCard({ invocation, defaultOpen = false }: ToolInvocationCardProps) {
  const [open, setOpen] = useState(defaultOpen || invocation.status === "error");
  const isRunning = invocation.status === "running";
  const summary = oneLineSummary(invocation);
  return (
    <div
      className={clsx(
        "overflow-hidden rounded-md border bg-bg-elevated/50 transition-colors",
        invocation.status === "error" ? "border-state-error/30" : "border-bg-border/70"
      )}
    >
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className="flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-[11px] transition-colors hover:bg-bg-elevated/80 active:scale-[0.997]"
        aria-expanded={open}
      >
        <StatusDot status={invocation.status} />
        <Terminal size={11} strokeWidth={1.8} className="shrink-0 text-ink-dim" />
        <span className="font-mono text-ink">{invocation.name}</span>
        {summary && <span className="min-w-0 flex-1 truncate text-ink-muted">{summary}</span>}
        {!summary && <span className="flex-1" />}
        {invocation.durationMs != null && (
          <span className="font-mono tabular-nums text-ink-dim/70">
            {formatDuration(invocation.durationMs)}
          </span>
        )}
        {isRunning && <Loader2 size={10} strokeWidth={2} className="animate-spin text-amber-400" />}
        <ChevronDown
          size={11}
          strokeWidth={2}
          className={clsx("text-ink-dim transition-transform duration-150", open && "rotate-180")}
        />
      </button>
      {open && (
        <div className="space-y-1.5 border-t border-bg-border/60 px-3 py-2 text-[11px]">
          {invocation.input != null && (
            <Section label="Input">
              <JsonBlock value={invocation.input} />
            </Section>
          )}
          {invocation.output != null && (
            <Section label="Output">
              <JsonBlock value={invocation.output} />
            </Section>
          )}
          {invocation.error && (
            <Section label="Error">
              <pre className="whitespace-pre-wrap text-state-error">{invocation.error}</pre>
            </Section>
          )}
          {invocation.status === "running" &&
            invocation.output == null &&
            invocation.input == null && (
              <div className="flex items-center gap-2 text-ink-dim">
                <Loader2 size={10} className="animate-spin" />
                <span>running…</span>
              </div>
            )}
        </div>
      )}
    </div>
  );
}

function Section({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="mb-1 text-[10px] font-semibold uppercase tracking-wider text-ink-dim/70">
        {label}
      </div>
      {children}
    </div>
  );
}

function JsonBlock({ value }: { value: unknown }) {
  let text: string;
  try {
    text = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  } catch {
    text = String(value);
  }
  return (
    <pre className="max-h-80 overflow-auto rounded border border-bg-border/60 bg-bg/60 px-2.5 py-2 font-mono text-[11px] leading-relaxed text-ink-muted">
      {text}
    </pre>
  );
}

function StatusDot({ status }: { status: ToolInvocation["status"] }) {
  if (status === "running") {
    return <Loader2 size={10} strokeWidth={2} className="animate-spin text-amber-400" />;
  }
  if (status === "ok") {
    return <Check size={10} strokeWidth={2} className="text-state-healthy" />;
  }
  if (status === "error") {
    return <AlertTriangle size={10} strokeWidth={2} className="text-state-error" />;
  }
  return <XCircle size={10} strokeWidth={2} className="text-ink-dim" />;
}

function oneLineSummary(inv: ToolInvocation): string | null {
  if (inv.label) return inv.label;
  if (!inv.input || typeof inv.input !== "object") return null;
  const entries = Object.entries(inv.input as Record<string, unknown>);
  if (entries.length === 0) return null;
  // Show the first 1-2 most diagnostic fields.
  const head = entries
    .slice(0, 2)
    .map(([k, v]) => {
      if (typeof v === "string") return `${k}="${truncate(v, 40)}"`;
      if (v == null) return null;
      return `${k}=${truncate(JSON.stringify(v), 40)}`;
    })
    .filter(Boolean)
    .join(" · ");
  return head || null;
}

function truncate(s: string, n: number): string {
  if (s.length <= n) return s;
  return s.slice(0, n - 1) + "…";
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60_000)}m ${Math.round((ms % 60_000) / 1000)}s`;
}
