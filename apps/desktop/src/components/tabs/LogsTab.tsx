"use client";

import { useMemo, useState } from "react";
import { clsx } from "clsx";
import { Download } from "lucide-react";

import { useStore } from "@/lib/store";
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

const LAYERS = [
  { value: "", label: "All layers" },
  { value: "senses", label: "senses" },
  { value: "triage", label: "triage" },
  { value: "orchestrator", label: "orchestrator" },
  { value: "workers", label: "workers" },
  { value: "voice", label: "voice" },
  { value: "memory", label: "memory" },
  { value: "health", label: "health" },
  { value: "system", label: "system" },
  { value: "dashboard", label: "dashboard" },
];

const LEVEL_STYLE: Record<string, string> = {
  error: "text-state-error",
  warn: "text-state-warn",
  info: "text-ink",
  debug: "text-ink-muted",
  trace: "text-ink-dim",
};

export function LogsTab() {
  const logs = useStore((s) => s.logs);
  const logLimit = useStore((s) => s.logLimit);
  const [level, setLevel] = useState("");
  const [layer, setLayer] = useState("");
  const [text, setText] = useState("");
  const [autoScroll, setAutoScroll] = useState(true);

  const filtered = useMemo(() => {
    return logs.filter((e) => {
      if (level && e.level !== level) return false;
      if (layer && e.layer !== layer) return false;
      if (text) {
        const needle = text.toLowerCase();
        if (
          !e.message.toLowerCase().includes(needle) &&
          !(e.component ?? "").toLowerCase().includes(needle) &&
          !(e.target ?? "").toLowerCase().includes(needle)
        )
          return false;
      }
      return true;
    });
  }, [logs, level, layer, text]);

  function exportLogs() {
    const ndjson = filtered.map((e) => JSON.stringify(e)).join("\n");
    const blob = new Blob([ndjson], { type: "application/x-ndjson" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `kairo-logs-${new Date().toISOString()}.ndjson`;
    link.click();
    URL.revokeObjectURL(url);
  }

  return (
    <div className="mx-auto max-w-6xl">
      <Card
        title="Live logs"
        subtitle={`${filtered.length} / ${logs.length} entries (buffer cap ${logLimit.toLocaleString()})`}
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
          className={clsx(
            "h-[520px] overflow-y-auto rounded-md border border-bg-border bg-black/40 p-3 font-mono text-[12px] leading-[1.5]"
          )}
        >
          {filtered.length === 0 ? (
            <div className="flex h-full items-center justify-center text-ink-dim">
              No matching log entries yet.
            </div>
          ) : (
            (autoScroll ? filtered : [...filtered].reverse()).map((e) => (
              <LogRow key={e.id} entry={e} />
            ))
          )}
        </div>
      </Card>
    </div>
  );
}

function LogRow({ entry }: { entry: LogEntry }) {
  return (
    <div className="flex gap-2 py-0.5">
      <span className="shrink-0 text-ink-dim">
        {new Date(entry.ts).toLocaleTimeString(undefined, { hour12: false })}
      </span>
      <span className={clsx("shrink-0 uppercase", LEVEL_STYLE[entry.level])}>
        {entry.level.padEnd(5)}
      </span>
      <span className="shrink-0 text-ink-muted">
        {entry.layer ?? "-"}/{entry.component ?? "-"}
      </span>
      <span className="text-ink">{entry.message}</span>
      {entry.fields.length > 0 && (
        <span className="text-ink-dim">{entry.fields.map(([k, v]) => `${k}=${v}`).join(" ")}</span>
      )}
    </div>
  );
}
