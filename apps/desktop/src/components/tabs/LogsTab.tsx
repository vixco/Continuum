"use client";

import { useMemo, useState } from "react";
import { clsx } from "clsx";
import { ChevronDown, Download } from "lucide-react";

import { useStore } from "@/lib/store";
import { groupAdjacentLogs, logSeverity, type LogDisplayGroup } from "@/lib/log-view";
import { Button, Card, SearchInput, Select, Toggle } from "@/components/ui/primitives";
import type { LogEntry } from "@/lib/types";

const LEVELS = [
  { value: "", label: "All levels" },
  { value: "error", label: "error" },
  { value: "warn", label: "warn" },
  { value: "info", label: "info" },
  { value: "debug", label: "debug" },
  { value: "trace", label: "trace" },
];

// Layer filter — every entry below maps to a `tracing` `layer=` value that
// the runtime + desktop + mcp processes actually emit. Adding a new layer
// here is cheap; adding a `#[tracing::instrument(layer = "...")]` somewhere
// the dashboard never filters against is a foot-gun. Keep these in lockstep
// with `crate::logs::*` constants.
const LAYERS = [
  { value: "", label: "All layers" },
  { value: "config", label: "config" },
  { value: "dashboard", label: "dashboard" },
  { value: "desktop", label: "desktop" },
  { value: "hardware", label: "hardware" },
  { value: "health", label: "health" },
  { value: "mcp", label: "mcp" },
  { value: "memory", label: "memory" },
  { value: "orchestrator", label: "orchestrator" },
  { value: "senses", label: "senses" },
  { value: "skills", label: "skills" },
  { value: "system", label: "system" },
  { value: "triage", label: "triage" },
  { value: "vision", label: "vision" },
  { value: "voice", label: "voice" },
  { value: "workers", label: "workers" },
];

const SEVERITY_STYLE = {
  error: "border-state-error/40 bg-state-error/[0.09] text-state-error",
  warn: "border-state-warn/40 bg-state-warn/[0.08] text-state-warn",
  info: "border-transparent text-ink",
  debug: "border-transparent text-ink-muted",
  trace: "border-transparent text-ink-dim",
  other: "border-accent-blue/30 bg-accent-blue/[0.07] text-accent-blue",
};

export function LogsTab() {
  const logs = useStore((s) => s.logs);
  const logLimit = useStore((s) => s.logLimit);
  const [level, setLevel] = useState("");
  const [layer, setLayer] = useState("");
  const [text, setText] = useState("");
  const [autoScroll, setAutoScroll] = useState(true);

  const filtered = useMemo(() => {
    return logs.filter((entry) => matchesLogFilter(entry, level, layer, text));
  }, [logs, level, layer, text]);
  const displayGroups = useMemo(
    () =>
      groupAdjacentLogs(logs).filter((group) =>
        matchesLogFilter(group.entries[0], level, layer, text)
      ),
    [logs, level, layer, text]
  );

  function exportLogs() {
    const ndjson = filtered.map((e) => JSON.stringify(e)).join("\n");
    const blob = new Blob([ndjson], { type: "application/x-ndjson" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `continuum-logs-${new Date().toISOString()}.ndjson`;
    link.click();
    URL.revokeObjectURL(url);
  }

  return (
    <div className="mx-auto max-w-6xl">
      <Card
        title="Live logs"
        subtitle={`${filtered.length} / ${logs.length} entries in ${displayGroups.length} visible rows (buffer cap ${logLimit.toLocaleString()})`}
        actions={
          <>
            <Toggle checked={autoScroll} onChange={setAutoScroll} label="Auto-scroll" />
            <Button size="sm" variant="ghost" onClick={exportLogs}>
              <Download size={12} /> Export
            </Button>
          </>
        }
      >
        <div className="mb-3 grid grid-cols-1 gap-2 md:grid-cols-[auto_auto_1fr]">
          <Select value={level} options={LEVELS} onChange={setLevel} />
          <Select value={layer} options={LAYERS} onChange={setLayer} />
          <SearchInput
            value={text}
            onChange={setText}
            placeholder="Search message, component, target…"
          />
        </div>
        <div
          role="log"
          aria-label="Continuum live logs"
          className={clsx(
            "h-[520px] overflow-y-auto rounded-md border border-bg-border bg-black/40 p-3 font-mono text-[12px] leading-[1.5]"
          )}
        >
          {filtered.length === 0 ? (
            <div className="flex h-full items-center justify-center text-ink-dim">
              No matching log entries yet.
            </div>
          ) : (
            (autoScroll ? displayGroups : [...displayGroups].reverse()).map((group) => (
              <LogGroup key={group.key} group={group} reverseEntries={!autoScroll} />
            ))
          )}
        </div>
      </Card>
    </div>
  );
}

function LogGroup({ group, reverseEntries }: { group: LogDisplayGroup; reverseEntries: boolean }) {
  const [expanded, setExpanded] = useState(false);
  const entries = reverseEntries ? [...group.entries].reverse() : group.entries;
  const first = entries[0];

  if (entries.length === 1) return <LogRow entry={first} />;

  const last = entries.at(-1) ?? first;
  const timeRange = `${formatTime(first.ts)} to ${formatTime(last.ts)}`;

  return (
    <div className="py-0.5">
      <div className="flex items-start gap-2">
        <button
          type="button"
          className="press mt-px inline-flex shrink-0 items-center gap-1 rounded border border-bg-border bg-bg-elevated px-1.5 py-0.5 text-[10px] font-semibold tabular-nums text-ink-muted hover:border-bg-hover hover:text-ink focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent-amber"
          aria-expanded={expanded}
          aria-label={`${expanded ? "Collapse" : "Expand"} ${entries.length} repeated log events from ${timeRange}`}
          onClick={() => setExpanded((value) => !value)}
        >
          <ChevronDown
            size={10}
            aria-hidden="true"
            className={clsx("transition-transform", expanded && "rotate-180")}
          />
          x{entries.length}
        </button>
        <div className="min-w-0 flex-1">
          <LogRow entry={first} timeLabel={timeRange} />
          {expanded && (
            <div className="ml-2 border-l border-bg-border pl-2">
              {entries.map((entry, index) => (
                <LogRow key={`${entry.id}:${index}`} entry={entry} />
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function LogRow({ entry, timeLabel }: { entry: LogEntry; timeLabel?: string }) {
  const severity = logSeverity(entry.level);

  return (
    <div
      className={clsx(
        "flex min-w-0 gap-2 rounded border-l-2 px-1 py-0.5",
        SEVERITY_STYLE[severity]
      )}
      data-severity={severity}
    >
      <span className="shrink-0 text-ink-dim">{timeLabel ?? formatTime(entry.ts)}</span>
      <span className="shrink-0 font-semibold uppercase" aria-label={`Severity: ${severity}`}>
        {severity.padEnd(5)}
      </span>
      <span className="shrink-0 text-ink-muted">
        {entry.layer ?? "-"}/{entry.component ?? "-"}
      </span>
      <span>{entry.message}</span>
      {entry.fields.length > 0 && (
        <span className="text-ink-dim">{entry.fields.map(([k, v]) => `${k}=${v}`).join(" ")}</span>
      )}
    </div>
  );
}

function formatTime(timestamp: string) {
  return new Date(timestamp).toLocaleTimeString(undefined, { hour12: false });
}

function matchesLogFilter(entry: LogEntry, level: string, layer: string, text: string) {
  if (level && entry.level !== level) return false;
  if (layer && entry.layer !== layer) return false;
  if (!text) return true;

  const needle = text.toLowerCase();
  return (
    entry.message.toLowerCase().includes(needle) ||
    (entry.component ?? "").toLowerCase().includes(needle) ||
    (entry.target ?? "").toLowerCase().includes(needle)
  );
}
