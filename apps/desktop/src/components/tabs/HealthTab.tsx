"use client";

import { useEffect, useState } from "react";
import { clsx } from "clsx";
import { Archive, RefreshCcw, Wrench } from "lucide-react";

import { useStore } from "@/lib/store";
import { kairo } from "@/lib/tauri";
import {
  Button,
  Card,
  Modal,
  StatusBadge,
} from "@/components/ui/primitives";
import type { ComponentHealth, RepairEvent } from "@/lib/types";

export function HealthTab() {
  const components = useStore((s) => s.components);
  const setComponents = useStore((s) => s.setComponents);
  const health = useStore((s) => s.state.health);
  const repairEvents = useStore((s) => s.repairEvents);
  const clearRepair = useStore((s) => s.clearRepair);
  const [selected, setSelected] = useState<ComponentHealth | null>(null);

  async function refresh() {
    setComponents(await kairo.getHealth());
  }

  useEffect(() => {
    void refresh();
  }, []);

  return (
    <div className="mx-auto max-w-6xl space-y-6">
      <div className="grid grid-cols-1 gap-4 md:grid-cols-[2fr_1fr]">
        <Card
          title="Components"
          subtitle={
            health.last_check_ts
              ? `last checked ${new Date(health.last_check_ts).toLocaleTimeString()}`
              : "awaiting first probe"
          }
          actions={
            <Button size="sm" variant="ghost" onClick={refresh}>
              <RefreshCcw size={12} /> Refresh
            </Button>
          }
        >
          {components.length === 0 ? (
            <div className="py-6 text-center text-sm text-ink-dim">
              No components registered yet.
            </div>
          ) : (
            <div className="grid grid-cols-1 gap-2 md:grid-cols-2">
              {components.map((c) => (
                <button
                  key={c.name}
                  onClick={() => setSelected(c)}
                  className={clsx(
                    "rounded-md border border-bg-border bg-bg-elevated p-3 text-left transition-colors",
                    "hover:border-accent-purple hover:bg-bg-hover",
                  )}
                >
                  <div className="flex items-center justify-between">
                    <div className="font-medium capitalize text-ink">
                      {c.name.replaceAll("_", " ")}
                    </div>
                    <StatusBadge status={c.status} />
                  </div>
                  <div className="mt-1 text-xs text-ink-muted">
                    avg {c.avg_response_ms ?? 0} ms · errors (24h){" "}
                    {c.error_count_24h}
                  </div>
                  {c.last_error && (
                    <div className="mt-1 truncate text-xs text-state-error">
                      {c.last_error}
                    </div>
                  )}
                </button>
              ))}
            </div>
          )}
        </Card>

        <Card
          title="Self-heal"
          subtitle={
            health.repair_running
              ? "repair agent running…"
              : "run the repair agent if something looks off"
          }
        >
          <Button
            variant="primary"
            onClick={async () => {
              clearRepair();
              await kairo.triggerRepair(undefined);
            }}
            disabled={health.repair_running}
            className="w-full"
          >
            <Wrench size={13} />
            {health.repair_running ? "Running…" : "Fix issues"}
          </Button>
          <div className="mt-4">
            <div className="text-[11px] uppercase tracking-wider text-ink-dim">
              Backup
            </div>
            <div className="mt-1 text-sm">
              {health.last_backup_ts
                ? new Date(health.last_backup_ts).toLocaleString()
                : "no backup yet"}
            </div>
            <div className="text-xs text-ink-muted">
              {health.backups_retained} retained
            </div>
            <Button
              size="sm"
              variant="ghost"
              onClick={async () => {
                await kairo.runBackupNow();
                await refresh();
              }}
              className="mt-2"
            >
              <Archive size={12} /> Backup now
            </Button>
          </div>
        </Card>
      </div>

      <Card title="Repair agent output" subtitle="live stream from Opus">
        <RepairStream events={repairEvents} />
      </Card>

      <Modal
        open={!!selected}
        onClose={() => setSelected(null)}
        title={selected?.name ?? ""}
      >
        {selected && (
          <div className="space-y-3 text-sm">
            <div className="flex items-center gap-2">
              <StatusBadge status={selected.status} />
              <span className="text-xs text-ink-muted">
                last probe{" "}
                {selected.last_check_ts
                  ? new Date(selected.last_check_ts).toLocaleTimeString()
                  : "never"}
              </span>
            </div>
            <div>
              <div className="kairo-label">Average response</div>
              <div>{selected.avg_response_ms ?? 0} ms</div>
            </div>
            <div>
              <div className="kairo-label">Errors (24h)</div>
              <div>{selected.error_count_24h}</div>
            </div>
            {selected.last_error && (
              <div>
                <div className="kairo-label">Last error</div>
                <div className="text-state-error">{selected.last_error}</div>
              </div>
            )}
            {selected.log_path && (
              <div>
                <div className="kairo-label">Log path</div>
                <div className="font-mono text-xs text-ink-muted">
                  {selected.log_path}
                </div>
              </div>
            )}
            {selected.recovery_note && (
              <div>
                <div className="kairo-label">Recovery</div>
                <div>{selected.recovery_note}</div>
              </div>
            )}
            <Button
              size="sm"
              variant="default"
              onClick={async () => {
                await kairo.restartComponent(selected.name);
                await refresh();
                setSelected(null);
              }}
            >
              Re-probe
            </Button>
          </div>
        )}
      </Modal>
    </div>
  );
}

function RepairStream({ events }: { events: RepairEvent[] }) {
  if (events.length === 0) {
    return (
      <div className="h-40 flex items-center justify-center text-sm text-ink-dim">
        No repair events. Click <span className="mx-1 text-ink">Fix issues</span>{" "}
        to start.
      </div>
    );
  }
  return (
    <div className="h-64 overflow-y-auto rounded-md border border-bg-border bg-black/40 p-3 font-mono text-[12px] leading-[1.5]">
      {events.map((e, idx) => (
        <div key={idx} className="flex gap-2">
          <span className="shrink-0 text-ink-dim">{labelFor(e)}</span>
          <span className={clsx("text-ink", styleFor(e))}>{bodyFor(e)}</span>
        </div>
      ))}
    </div>
  );
}

function labelFor(e: RepairEvent) {
  return e.kind.padEnd(15);
}

function styleFor(e: RepairEvent) {
  if (e.kind === "error") return "text-state-error";
  if (e.kind === "stderr") return "text-state-warn";
  if (e.kind === "tool_call" || e.kind === "tool_result") return "text-accent-blue";
  return "";
}

function bodyFor(e: RepairEvent): string {
  switch (e.kind) {
    case "started":
      return `repair started at ${e.ts}`;
    case "finished":
      return `done (success=${e.success})`;
    case "context_written":
      return `context at ${e.path}`;
    case "assistant_delta":
      return e.text;
    case "tool_call":
      return e.name;
    case "tool_result":
      return `${e.name}: ${e.summary}`;
    case "stderr":
      return e.line;
    case "error":
      return e.message;
  }
}
