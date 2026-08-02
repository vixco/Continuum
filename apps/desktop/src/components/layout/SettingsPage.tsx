"use client";

import { useState } from "react";
import { RefreshCw, RotateCcw } from "lucide-react";

import { IntegrationsPanel } from "@/components/continuum/IntegrationsPanel";
import { ResourcePanel } from "@/components/continuum/ResourcePanel";
import { Button, Card, Modal } from "@/components/ui/primitives";
import { useStore } from "@/lib/store";
import { continuum } from "@/lib/tauri";
import type { UpdateInfo } from "@/lib/tauri";

interface UpdateState {
  phase: string;
  update: UpdateInfo | null;
  message: string | null;
  progress: number | null;
}

export function SettingsPage({
  autoUpdateEnabled,
  onAutoUpdateChange,
  updateState,
  onCheckForUpdates,
  onInstallUpdate,
  onResetEverything,
}: {
  autoUpdateEnabled: boolean;
  onAutoUpdateChange: (enabled: boolean) => void;
  updateState: UpdateState;
  onCheckForUpdates: () => void;
  onInstallUpdate: () => void;
  onResetEverything: () => void;
}) {
  const version = useStore((s) => s.state.system.version);
  const startedAt = useStore((s) => s.state.system.started_at);
  const statePath = useStore((s) => s.state.health);
  const [confirmReset, setConfirmReset] = useState(false);
  const [resetting, setResetting] = useState(false);

  return (
    <div className="mx-auto max-w-4xl space-y-6">
      <header>
        <h1 className="text-xl font-semibold tracking-tight text-ink">Settings</h1>
        <p className="mt-1 text-sm text-ink-muted">
          Resources, updates, and runtime info. Changes to models and resources apply on the next
          runtime start.
        </p>
      </header>

      <IntegrationsPanel />

      <ResourcePanel />

      <Card
        title="Continuum updates"
        subtitle="Checked securely at startup from signed release artifacts."
      >
        <div className="flex flex-wrap items-center gap-4">
          <label className="flex cursor-pointer items-center gap-2 text-sm text-ink">
            <input
              type="checkbox"
              checked={autoUpdateEnabled}
              onChange={(e) => onAutoUpdateChange(e.target.checked)}
              className="h-4 w-4 accent-amber-500"
            />
            Install updates automatically
          </label>
          <Button size="sm" variant="default" onClick={onCheckForUpdates}>
            <RefreshCw
              size={13}
              className={updateState.phase === "checking" ? "animate-spin" : ""}
            />
            Check for updates
          </Button>
        </div>
        <div className="mt-3 border-t border-bg-border pt-3 text-xs text-ink-muted">
          {updateState.phase === "checking" && "Checking for updates…"}
          {updateState.phase === "current" && "You are up to date."}
          {updateState.phase === "available" && updateState.update && (
            <span className="flex flex-wrap items-center gap-3 text-amber-300">
              Update v{updateState.update.version} is available.
              <Button size="sm" variant="primary" onClick={onInstallUpdate}>
                Install now
              </Button>
            </span>
          )}
          {updateState.phase === "downloading" && (
            <>
              Installing update
              {updateState.progress !== null ? ` (${updateState.progress}%)` : ""}…
            </>
          )}
          {updateState.phase === "error" && (
            <span className="text-red-300">{updateState.message ?? "Update check failed."}</span>
          )}
        </div>
      </Card>

      <Card title="About">
        <dl className="grid grid-cols-1 gap-x-8 gap-y-2 text-sm sm:grid-cols-2">
          <Row label="Version" value={version} />
          <Row label="Started" value={startedAt ? new Date(startedAt).toLocaleString() : "-"} />
          <Row label="Backups retained" value={String(statePath.backups_retained)} />
          <Row
            label="Last backup"
            value={
              statePath.last_backup_ts ? new Date(statePath.last_backup_ts).toLocaleString() : "-"
            }
          />
        </dl>
      </Card>

      <Card
        title="Danger zone"
        subtitle="Re-runs the first-run wizard. Your models and memory are kept; only the onboarding marker is cleared."
      >
        <div className="flex items-center justify-between gap-4">
          <p className="text-sm text-ink-muted">Reset everything &amp; onboard again</p>
          <Button
            size="sm"
            variant="danger"
            onClick={() => setConfirmReset(true)}
            disabled={resetting}
          >
            <RotateCcw size={13} />
            Reset &amp; onboard again
          </Button>
        </div>
      </Card>

      <Modal
        open={confirmReset}
        onClose={() => !resetting && setConfirmReset(false)}
        title="Reset everything and onboard again?"
        footer={
          <>
            <Button
              size="sm"
              variant="ghost"
              onClick={() => setConfirmReset(false)}
              disabled={resetting}
            >
              Cancel
            </Button>
            <Button
              size="sm"
              variant="danger"
              onClick={async () => {
                setResetting(true);
                try {
                  await continuum.resetOnboarding();
                } catch (err) {
                  console.warn("reset_onboarding failed, proceeding to wizard", err);
                }
                setResetting(false);
                setConfirmReset(false);
                onResetEverything();
              }}
              disabled={resetting}
            >
              {resetting ? "Resetting…" : "Yes, reset"}
            </Button>
          </>
        }
      >
        <p className="text-sm leading-relaxed text-ink-muted">
          This clears the onboarding marker and relaunches the setup wizard. Your downloaded models,
          memory, and config are kept — you will just walk through the wizard again. Continue?
        </p>
      </Modal>
    </div>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex justify-between gap-3 border-b border-bg-border py-1.5">
      <dt className="text-ink-muted">{label}</dt>
      <dd className="font-mono text-ink">{value}</dd>
    </div>
  );
}
