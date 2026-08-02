"use client";

import { useCallback, useEffect, useState } from "react";
import { clsx } from "clsx";
import { Cpu, HardDrive, MemoryStick, RefreshCcw, Zap } from "lucide-react";

import { continuum } from "@/lib/tauri";
import { useStore } from "@/lib/store";
import type {
  ProfileMode,
  ResourceConfig,
  ResourceProfile,
  ResourceProfileUpdate,
} from "@/lib/types";
import { Button, Card, Select, Slider, StatusBadge, Toggle } from "@/components/ui/primitives";

const PROFILE_OPTIONS: Array<{ value: ProfileMode; label: string }> = [
  { value: "auto", label: "Auto-detect" },
  { value: "barely_notice", label: "Barely-notice (default)" },
  { value: "balanced", label: "Balanced" },
  { value: "performance", label: "Performance" },
  { value: "custom", label: "Custom" },
];

const PROFILE_PRESET_FRACTIONS: Partial<Record<ProfileMode, number>> = {
  barely_notice: 0.3,
  balanced: 0.55,
  performance: 0.8,
};

export function ResourcePanel() {
  const [profile, setProfile] = useState<ResourceProfile | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  // Local editable copy of the resource config knobs.
  const [draft, setDraft] = useState<ResourceConfig | null>(null);

  const refresh = useCallback(async () => {
    const p = await continuum.getResourceProfile();
    setProfile(p);
    setDraft(p.config);
    setDirty(false);
    setError(null);
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const update = useCallback(
    async (partial: ResourceProfileUpdate) => {
      if (!draft) return;
      setSaving(true);
      setError(null);
      try {
        const result = await continuum.updateResourceProfile(partial);
        // The backend may flip profile to Custom when an individual knob is
        // set alongside a preset - adopt whatever it returns as the new
        // baseline so the UI reflects ground truth.
        setDraft(result.config);
        setProfile((prev) =>
          prev ? { ...prev, config: result.config, plan: result.plan, applied: false } : prev
        );
        setDirty(true);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setSaving(false);
      }
    },
    [draft]
  );

  if (!profile || !draft) {
    return (
      <Card title="Resources" subtitle="detecting hardware…">
        <div className="py-6 text-center text-sm text-ink-dim">Probing host…</div>
      </Card>
    );
  }

  const { specs, plan } = profile;
  const isCustom = draft.profile === "custom";

  return (
    <Card
      title="Resources"
      subtitle={
        specs.cpu_brand
          ? `${specs.cpu_brand} · ${specs.logical_cores} cores · ${Math.round(specs.total_ram_mb / 1024)} GB`
          : "adaptive resource policy"
      }
      actions={
        <Button size="sm" variant="ghost" onClick={refresh}>
          <RefreshCcw size={12} /> Refresh
        </Button>
      }
    >
      <div className="space-y-5">
        {/* Restart-to-apply banner */}
        {dirty && !profile.applied && (
          <div className="rounded-md border border-state-warn/30 bg-state-warn/10 px-3 py-2 text-xs text-state-warn">
            Changes saved to config. <b>Restart the Continuum runtime</b> to apply - the resource
            plan is resolved once at boot.
          </div>
        )}
        {profile.applied && (
          <div className="rounded-md border border-state-healthy/30 bg-state-healthy/10 px-3 py-2 text-xs text-state-healthy">
            Plan is live - the running runtime has applied this resource plan.
          </div>
        )}
        {error && (
          <div className="rounded-md border border-state-error/30 bg-state-error/10 px-3 py-2 text-xs text-state-error">
            {error}
          </div>
        )}

        {/* Detected hardware */}
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
          <SpecTile icon={<Cpu size={14} />} label="Cores" value={`${specs.logical_cores}`} />
          <SpecTile
            icon={<MemoryStick size={14} />}
            label="RAM"
            value={`${Math.round(specs.total_ram_mb / 1024)} GB`}
          />
          <SpecTile
            icon={<HardDrive size={14} />}
            label="GPU"
            value={specs.has_cuda ? `CUDA ${specs.vram_mb ?? 0} MB` : "CPU only"}
            tone={specs.has_cuda ? "ok" : "muted"}
          />
          <SpecTile
            icon={<Zap size={14} />}
            label="Power"
            value={specs.on_battery ? "Battery" : "AC"}
            tone={specs.on_battery ? "warn" : "ok"}
          />
        </div>

        {/* Resolved plan (read-only display) */}
        <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
          <div className="mb-2 text-xs font-semibold uppercase tracking-wide text-ink-muted">
            Resolved plan
          </div>
          <div className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs sm:grid-cols-3">
            <PlanRow label="Triage threads" value={`${plan.triage_threads}`} />
            <PlanRow label="Triage GPU layers" value={`${plan.triage_gpu_layers}`} />
            <PlanRow label="Vision" value={plan.vision_enabled ? "on" : "off"} />
            <PlanRow label="Whisper threads" value={`${plan.whisper_threads}`} />
            <PlanRow label="Workers" value={`${plan.workers_max_concurrent}`} />
            <PlanRow label="Screen interval" value={`${plan.screen_interval_secs}s`} />
          </div>
        </div>

        {/* Profile selector */}
        <div>
          <Select<ProfileMode>
            label="Profile"
            value={draft.profile}
            options={PROFILE_OPTIONS}
            onChange={(v) => {
              // Presets re-derive most knobs; just switch the profile and let
              // the backend resolve. For Custom we leave the knobs as-is.
              void update({ profile: v });
            }}
          />
          <p className="mt-1.5 text-xs text-ink-dim">
            {draft.profile === "auto"
              ? "Picks Barely-notice on laptops / CPU-only boxes, Balanced on a GPU desktop."
              : isCustom
                ? "Individual knobs are honoured verbatim - no preset overrides."
                : `Preset uses ~${Math.round((PROFILE_PRESET_FRACTIONS[draft.profile] ?? 0.3) * 100)}% of cores.`}
          </p>
        </div>

        {/* Custom knobs - only editable when profile is Custom. When on a
            preset, editing any knob flips the profile to Custom (handled
            server-side) so we render them disabled-but-visible to show what
            the preset resolves to. */}
        <div className={clsx("space-y-4", !isCustom && "opacity-60")}>
          <Slider
            label="CPU core fraction"
            value={draft.cpu_core_fraction}
            min={0.05}
            max={1}
            step={0.05}
            format={(v) => `${Math.round(v * 100)}%`}
            disabled={saving || !isCustom}
            onChange={(v) => void update({ cpu_core_fraction: v })}
          />

          <div className="flex items-center justify-between">
            <div className="text-sm text-ink">
              GPU offload
              <span className="ml-2 text-xs text-ink-dim">
                {draft.gpu_enabled === null
                  ? "auto-detect"
                  : draft.gpu_enabled
                    ? "force on"
                    : "force off"}
              </span>
            </div>
            <div className="flex gap-1.5">
              <TriButton
                active={draft.gpu_enabled === null}
                onClick={() => void update({ gpu_enabled: null })}
              >
                Auto
              </TriButton>
              <TriButton
                active={draft.gpu_enabled === true}
                onClick={() => void update({ gpu_enabled: true })}
              >
                On
              </TriButton>
              <TriButton
                active={draft.gpu_enabled === false}
                onClick={() => void update({ gpu_enabled: false })}
              >
                Off
              </TriButton>
            </div>
          </div>

          <div className="flex items-center justify-between">
            <div className="text-sm text-ink">
              Vision model
              <span className="ml-2 text-xs text-ink-dim">
                {draft.vision_enabled === null
                  ? "auto (RAM ≥ 6 GB)"
                  : draft.vision_enabled
                    ? "on"
                    : "text-only"}
              </span>
            </div>
            <div className="flex gap-1.5">
              <TriButton
                active={draft.vision_enabled === null}
                onClick={() => void update({ vision_enabled: null })}
              >
                Auto
              </TriButton>
              <TriButton
                active={draft.vision_enabled === true}
                onClick={() => void update({ vision_enabled: true })}
              >
                On
              </TriButton>
              <TriButton
                active={draft.vision_enabled === false}
                onClick={() => void update({ vision_enabled: false })}
              >
                Off
              </TriButton>
            </div>
          </div>

          <Slider
            label="Max workers"
            value={draft.workers_max_concurrent ?? 2}
            min={1}
            max={10}
            step={1}
            format={(v) => `${v}`}
            disabled={saving || !isCustom}
            onChange={(v) => void update({ workers_max_concurrent: v })}
          />

          <Slider
            label="Screen interval"
            value={draft.screen_interval_secs ?? 3}
            min={1}
            max={15}
            step={1}
            format={(v) => `${v}s`}
            disabled={saving || !isCustom}
            onChange={(v) => void update({ screen_interval_secs: v })}
          />

          <Slider
            label="Context poll interval"
            value={draft.context_interval_secs ?? 1}
            min={1}
            max={10}
            step={1}
            format={(v) => `${v}s`}
            disabled={saving || !isCustom}
            onChange={(v) => void update({ context_interval_secs: v })}
          />

          <div className="flex items-center justify-between">
            <span className="text-sm text-ink">Throttle on battery</span>
            <Toggle
              checked={draft.battery_throttle}
              onChange={(v) => void update({ battery_throttle: v })}
              disabled={saving && !isCustom}
            />
          </div>
        </div>

        {/* Live system-resources probe status, if present */}
        <SystemResourcesSummary />
      </div>
    </Card>
  );
}

/** Reads the `system_resources` health probe status from the store, if it has
 * run. Lives at the bottom of the panel so the user sees the live load that
 * their chosen profile is producing. */
function SystemResourcesSummary() {
  const components = useStore((s) => s.components);
  const probe = components.find((c) => c.name === "system_resources");
  if (!probe) return null;
  return (
    <div className="flex items-center justify-between rounded-md border border-bg-border bg-bg-elevated px-3 py-2">
      <div className="text-xs text-ink-muted">Live system load</div>
      <StatusBadge status={probe.status} label={probe.last_error ?? "within headroom"} />
    </div>
  );
}

function SpecTile({
  icon,
  label,
  value,
  tone = "default",
}: {
  icon: React.ReactNode;
  label: string;
  value: string;
  tone?: "default" | "ok" | "warn" | "muted";
}) {
  const toneClass =
    tone === "ok"
      ? "text-state-healthy"
      : tone === "warn"
        ? "text-state-warn"
        : tone === "muted"
          ? "text-ink-dim"
          : "text-ink";
  return (
    <div className="rounded-md border border-bg-border bg-bg-elevated p-2.5">
      <div className="flex items-center gap-1.5 text-xs text-ink-muted">
        {icon}
        {label}
      </div>
      <div className={clsx("mt-1 text-sm font-medium tabular-nums", toneClass)}>{value}</div>
    </div>
  );
}

function PlanRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex justify-between gap-2">
      <span className="text-ink-muted">{label}</span>
      <span className="font-mono tabular-nums text-ink">{value}</span>
    </div>
  );
}

function TriButton({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={clsx(
        "rounded-md border px-2.5 py-1 text-xs transition-colors active:scale-95",
        active
          ? "border-accent-purple bg-accent-purple/15 text-ink"
          : "border-bg-border bg-bg-elevated text-ink-muted hover:border-bg-hover"
      )}
    >
      {children}
    </button>
  );
}
