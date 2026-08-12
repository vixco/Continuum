"use client";

import { useMemo, useState } from "react";
import { AlertTriangle, CheckCircle2, Wrench } from "lucide-react";

import { Button, StatusBadge } from "@/components/ui/primitives";
import { continuum } from "@/lib/tauri";
import { useStore } from "@/lib/store";

export function HealthStatusMenu() {
  const components = useStore((state) => state.components);
  const setComponents = useStore((state) => state.setComponents);
  const clearRepair = useStore((state) => state.clearRepair);
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const issues = useMemo(
    () => components.filter((component) => component.status !== "healthy"),
    [components]
  );

  if (issues.length === 0) {
    return (
      <span className="no-drag hidden items-center gap-1.5 text-[11px] text-state-healthy lg:inline-flex">
        <CheckCircle2 size={12} strokeWidth={1.5} /> Healthy
      </span>
    );
  }

  const runSafeFix = async () => {
    setBusy(true);
    setMessage(null);
    try {
      const preview = await continuum.previewRepair();
      if (!preview.issues.some((issue) => issue.actionable)) {
        setMessage("These issues need diagnosis; no safe automatic action is available.");
        return;
      }
      clearRepair();
      await continuum.triggerRepair(preview.id, "Started from the global health indicator");
      setComponents(await continuum.getHealth());
      setMessage("Safe repair started. Open Health for live output.");
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="no-drag relative"
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
    >
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen(true)}
        onFocus={() => setOpen(true)}
        className="flex items-center gap-1.5 rounded-md border border-state-error/35 bg-state-error/10 px-2 py-1 text-[11px] font-medium text-state-error"
      >
        <AlertTriangle size={12} strokeWidth={1.5} /> Unhealthy ({issues.length})
      </button>
      {open && (
        <div className="absolute left-0 top-full z-[var(--z-dropdown)] w-[min(24rem,calc(100vw-2rem))] overflow-hidden rounded-lg border border-bg-border bg-bg-elevated shadow-xl">
          <div className="border-b border-bg-border px-3 py-2 text-xs font-semibold text-ink">
            Needs attention
          </div>
          <div className="max-h-72 divide-y divide-bg-border overflow-y-auto">
            {issues.map((issue) => (
              <div key={issue.name} className="grid grid-cols-[1fr_auto] gap-2 px-3 py-2.5">
                <div className="min-w-0">
                  <div className="text-xs font-medium capitalize text-ink">
                    {issue.name.replaceAll("_", " ")}
                  </div>
                  <div className="mt-0.5 break-words text-[11px] leading-4 text-ink-muted">
                    {issue.last_error ?? issue.recovery_note ?? "Health probe is not healthy."}
                  </div>
                </div>
                <StatusBadge status={issue.status} />
              </div>
            ))}
          </div>
          <div className="border-t border-bg-border p-2.5">
            {message && <div className="mb-2 text-[11px] leading-4 text-ink-muted">{message}</div>}
            <div className="flex items-center justify-between gap-2">
              <button
                type="button"
                className="text-[11px] text-ink-muted underline-offset-2 hover:text-ink hover:underline"
                onClick={() =>
                  window.dispatchEvent(new CustomEvent("continuum:navigate", { detail: "health" }))
                }
              >
                Open Health
              </button>
              <Button size="sm" variant="primary" disabled={busy} onClick={() => void runSafeFix()}>
                <Wrench size={12} strokeWidth={1.5} /> {busy ? "Checking…" : "Fix safely"}
              </Button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
