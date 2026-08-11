"use client";

import { useEffect, useState } from "react";
import { clsx } from "clsx";
import { Check, Download, FolderOpen, RefreshCw, RotateCcw } from "lucide-react";
import { open } from "@tauri-apps/plugin-dialog";

import { IntegrationsPanel } from "@/components/continuum/IntegrationsPanel";
import { ResourcePanel } from "@/components/continuum/ResourcePanel";
import { Button, Card, Modal } from "@/components/ui/primitives";
import { useStore } from "@/lib/store";
import { useTheme, type Theme } from "@/lib/theme";
import { continuum } from "@/lib/tauri";
import type { ModelsDirectoryInfo, UpdateInfo } from "@/lib/tauri";

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
  onRestartToApplyUpdate,
  onResetEverything,
}: {
  autoUpdateEnabled: boolean;
  onAutoUpdateChange: (enabled: boolean) => void;
  updateState: UpdateState;
  onCheckForUpdates: () => void;
  onInstallUpdate: () => void;
  onRestartToApplyUpdate: () => void;
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

      <AppearanceCard />

      <IntegrationsPanel />

      <ResourcePanel />

      <ModelDirectoryCard />

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
            Download updates automatically
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
              {updateState.message ?? `Update v${updateState.update.version} is available.`}
              <Button size="sm" variant="primary" onClick={onInstallUpdate}>
                Download now
              </Button>
            </span>
          )}
          {updateState.phase === "downloading" && (
            <>
              Downloading update
              {updateState.progress !== null ? ` (${updateState.progress}%)` : ""}…
            </>
          )}
          {updateState.phase === "ready" && (
            <span className="flex flex-wrap items-center gap-3 text-green-300">
              {updateState.message ?? "Update ready to apply."}
              <Button size="sm" variant="primary" onClick={onRestartToApplyUpdate}>
                Restart to update
              </Button>
            </span>
          )}
          {updateState.phase === "error" && (
            <span className="flex flex-wrap items-center gap-3 text-red-300">
              {updateState.message ?? "Update check failed."}
              {updateState.update && (
                <Button size="sm" variant="danger" onClick={onInstallUpdate}>
                  Retry install
                </Button>
              )}
            </span>
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

function ModelDirectoryCard() {
  const setConfig = useStore((state) => state.setConfig);
  const [info, setInfo] = useState<ModelsDirectoryInfo | null>(null);
  const [busy, setBusy] = useState<"saving" | "downloading" | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = async () => {
    const next = await continuum.getModelsDirectory();
    setInfo(next);
  };

  useEffect(() => {
    void refresh().catch((reason: unknown) => setError(String(reason)));
  }, []);

  const chooseDirectory = async () => {
    setError(null);
    setMessage(null);
    const selected = await open({
      directory: true,
      multiple: false,
      defaultPath: info?.path || undefined,
      title: "Choose Continuum models directory",
    });
    if (typeof selected !== "string") return;

    setBusy("saving");
    try {
      const result = await continuum.updateModelsDirectory(selected);
      setConfig(result.config);
      setInfo(result.info);
      setMessage(
        result.restart_required
          ? "Saved. Continuum will use this directory on its next automatic start."
          : "Saved. Continuum will use this directory when the runtime starts."
      );
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(null);
    }
  };

  const downloadModels = async () => {
    if (!info?.path) return;
    setBusy("downloading");
    setError(null);
    setMessage(null);
    try {
      await continuum.downloadModels(info.path);
      await refresh();
      setMessage("Models downloaded. They load on the next automatic runtime start.");
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(null);
    }
  };

  return (
    <Card
      title="Local model storage"
      subtitle="Choose where Continuum reads and downloads Whisper, vision, triage, and TTS models."
    >
      <div className="flex flex-col gap-3">
        <div className="flex flex-col gap-2 sm:flex-row">
          <input
            type="text"
            readOnly
            value={info?.path ?? "Loading…"}
            aria-label="Models directory"
            className="min-w-0 flex-1 rounded-md border border-bg-border bg-bg-elevated px-3 py-2 font-mono text-xs text-ink"
          />
          <Button size="sm" variant="default" onClick={chooseDirectory} disabled={busy !== null}>
            {busy === "saving" ? (
              <RefreshCw size={13} className="animate-spin" />
            ) : (
              <FolderOpen size={13} />
            )}
            Choose directory
          </Button>
          <Button size="sm" variant="primary" onClick={downloadModels} disabled={busy !== null}>
            {busy === "downloading" ? (
              <RefreshCw size={13} className="animate-spin" />
            ) : (
              <Download size={13} />
            )}
            {busy === "downloading" ? "Downloading…" : "Download missing models"}
          </Button>
        </div>
        {info && (
          <p className={clsx("text-xs", info.whisper_present ? "text-green-300" : "text-red-300")}>
            Whisper: {info.whisper_present ? "ready" : `missing at ${info.whisper_model_path}`}
          </p>
        )}
        {message && <p className="text-xs text-amber-200">{message}</p>}
        {error && (
          <p className="text-xs text-red-300" role="alert">
            {error}
          </p>
        )}
      </div>
    </Card>
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

function AppearanceCard() {
  const [theme, setTheme] = useTheme();
  return (
    <Card title="Appearance" subtitle="How Continuum looks. Applied instantly across the app.">
      <div className="grid grid-cols-2 gap-3 sm:max-w-md">
        <ThemeOption
          id="light"
          label="Light"
          hint="Cool paper · day"
          active={theme === "light"}
          onSelect={setTheme}
        />
        <ThemeOption
          id="dark"
          label="Dark"
          hint="Slate console · night"
          active={theme === "dark"}
          onSelect={setTheme}
        />
      </div>
    </Card>
  );
}

function ThemeOption({
  id,
  label,
  hint,
  active,
  onSelect,
}: {
  id: Theme;
  label: string;
  hint: string;
  active: boolean;
  onSelect: (t: Theme) => void;
}) {
  return (
    <button
      type="button"
      onClick={() => onSelect(id)}
      className={clsx(
        "press group relative flex flex-col gap-2 rounded-xl border p-3 text-left transition-colors",
        active
          ? "border-accent-amber/60 bg-accent-amber/10"
          : "border-bg-border bg-bg-elevated hover:border-bg-hover"
      )}
    >
      <div
        data-theme={id}
        className="flex h-16 overflow-hidden rounded-md border border-bg-border bg-bg"
      >
        <div className="flex w-1/4 flex-col gap-1 bg-bg-surface p-1.5">
          <span className="h-1 w-3/4 rounded-full bg-accent-amber/70" />
          <span className="h-1 w-2/3 rounded-full bg-ink/20" />
          <span className="h-1 w-1/2 rounded-full bg-ink/15" />
        </div>
        <div className="flex flex-1 flex-col gap-1 p-1.5">
          <span className="h-1.5 w-1/2 rounded-full bg-ink/40" />
          <span className="h-1 w-3/4 rounded-full bg-ink/15" />
          <span className="mt-auto h-2 w-2/3 rounded bg-accent-amber/25" />
        </div>
      </div>
      <div className="flex items-center justify-between">
        <span className="text-[13px] font-medium text-ink">{label}</span>
        {active && <Check size={14} className="text-accent-amber" />}
      </div>
      <span className="-mt-1 text-[11px] text-ink-dim">{hint}</span>
    </button>
  );
}
