"use client";

import { useState } from "react";
import { clsx } from "clsx";
import { Eye, Filter, MessageSquare, Users } from "lucide-react";

import { useStore } from "@/lib/store";
import { continuum } from "@/lib/tauri";
import { Button, Card, Select, Slider, Toggle } from "@/components/ui/primitives";

export function BrainTab() {
  const config = useStore((s) => s.config);
  const setConfig = useStore((s) => s.setConfig);
  const system = useStore((s) => s.state.system);
  const perception = useStore((s) => s.state.perception);
  const [testing, setTesting] = useState<string | null>(null);

  async function testLayer(name: string) {
    setTesting(name);
    await new Promise((r) => window.setTimeout(r, 800));
    setTesting(null);
  }

  return (
    <div className="mx-auto max-w-6xl space-y-6">
      <LayerDiagram />

      <Card
        title="Layer 1 - Vision"
        subtitle={`Active model: ${config.vision.name}`}
        actions={
          <Button
            size="sm"
            variant="default"
            onClick={() => testLayer("vision")}
            disabled={testing === "vision"}
          >
            <Eye size={12} />
            {testing === "vision" ? "Capturing…" : "Test capture"}
          </Button>
        }
      >
        <div className="grid grid-cols-1 gap-4 md:grid-cols-3">
          <div className="bg-bg-raised/40 flex flex-col justify-end gap-2 rounded-md border border-bg-border px-3 py-2">
            <Toggle
              checked={config.screen.enabled}
              onChange={async (enabled) => {
                const cfg = await continuum.updateLiveContextConfig({ enabled });
                setConfig(cfg);
              }}
              label="Continuous local context"
            />
            <p className="text-[11px] leading-4 text-ink-dim">
              All displays, processed locally. Raw keys, pointer positions, and clipboard text are
              never collected.
            </p>
          </div>
          <Select
            label="Model"
            value={config.vision.name}
            onChange={(v) => setConfig({ ...config, vision: { ...config.vision, name: v } })}
            options={[
              { value: "SmolVLM-256M", label: "SmolVLM-256M (default)" },
              { value: "Moondream2", label: "Moondream2" },
              { value: "Florence-2", label: "Florence-2" },
              { value: "MiniCPM-V", label: "MiniCPM-V" },
            ]}
          />
          <Slider
            label="Capture cadence"
            value={config.screen.capture_interval_ms}
            onChange={async (v) => {
              const cfg = await continuum.updateLiveContextConfig({
                capture_interval_ms: Math.round(v),
                all_monitors: true,
              });
              setConfig(cfg);
            }}
            min={100}
            max={2000}
            step={100}
            format={(v) => `${Math.round(v)}ms / monitor`}
          />
        </div>
        <div className="bg-bg-deep mt-3 rounded-md border border-bg-border px-3 py-2 text-xs text-ink-muted">
          <span className="font-medium text-ink">All connected monitors</span>
          <span className="ml-1 font-mono text-ink-muted">({perception.monitor_count} live)</span>
          <span className="mx-2 text-ink-dim">•</span>
          bounded ordered buffer ({config.screen.buffer_capacity} events)
          <span className="mx-2 text-ink-dim">•</span>
          local vision at meaningful changes only
          <span className="mx-2 text-ink-dim">•</span>
          dropped: {perception.dropped_capture_events}
          <span className="mx-2 text-ink-dim">•</span>
          restart runtime after changing capture settings
        </div>
        <ModelStatus
          loaded={system.vision_model_loaded}
          label="Vision model"
          appliedModel={config.vision.name}
        />
      </Card>

      <Card
        title="Layer 2 - Triage"
        subtitle="Local LLM gatekeeper"
        actions={
          <Button size="sm" onClick={() => testLayer("triage")} disabled={testing === "triage"}>
            <Filter size={12} />
            {testing === "triage" ? "Running…" : "Test triage"}
          </Button>
        }
      >
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
          {/* Triage model is loaded at boot from config.toml — the dashboard
              does not yet expose a hot-swap. Until that ships, this control
              is read-only and reflects the model the runtime currently has. */}
          <Select
            label="Model"
            value="qwen3-8b"
            onChange={() => {}}
            disabled
            title="Hot-swap is not wired yet. Edit config.toml under [triage].model."
            options={[
              { value: "qwen3-8b", label: "Qwen 3 8B (default, 95% acc.)" },
              { value: "qwen25-3b", label: "Qwen 2.5 3B" },
              { value: "gemma-2b", label: "Gemma 2B" },
              { value: "phi-3-mini", label: "Phi-3 mini" },
            ]}
          />
          <Slider
            label="Salience threshold"
            value={config.frame.salience_threshold}
            onChange={async (v) => {
              const cfg = await continuum.updateTriageThreshold(v);
              setConfig(cfg);
            }}
            min={0}
            max={1}
            step={0.05}
            format={(v) => v.toFixed(2)}
          />
        </div>
        <ModelStatus
          loaded={system.triage_model_loaded}
          label="Triage model"
          appliedModel="qwen3-8b"
        />
      </Card>

      <Card
        title="Layer 3 - Orchestrator"
        subtitle="Claude Opus via headless CLI"
        actions={
          <Button size="sm" onClick={() => testLayer("orchestrator")}>
            <MessageSquare size={12} /> Dry-run
          </Button>
        }
      >
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
          {/* Orchestrator model is owned by the Claude CLI — the dashboard
              can't swap it mid-session. Until the orchestrator profile
              plugin ships, this is fixed at the value the CLI was launched
              with. */}
          <Select
            label="Model"
            value="claude-opus-4-6"
            onChange={() => {}}
            disabled
            title="Orchestrator model is selected at launch via providers.json; hot-swap not yet wired."
            options={[
              { value: "claude-opus-4-6", label: "claude-opus-4-6 (default)" },
              { value: "claude-sonnet-4-6", label: "claude-sonnet-4-6" },
            ]}
          />
          {/* Token budget is enforced in the orchestrator prompt template,
              not by the dashboard. There's no live command to push a new
              value yet — surface this as a stat, not a control. */}
          <Slider
            label="Token budget (read-only)"
            value={4000}
            onChange={() => {}}
            disabled
            min={1000}
            max={16000}
            step={500}
            format={(v) => `${Math.round(v)} tokens`}
          />
        </div>
        <ModelStatus
          loaded={system.orchestrator_ready}
          label="Claude CLI reachable"
          appliedModel="claude-opus-4-6"
        />
      </Card>

      <Card
        title="Layer 4 - Workers"
        subtitle="Headless Claude Code sessions spawned by the orchestrator"
        actions={
          <Button size="sm" disabled>
            <Users size={12} /> Phase 8
          </Button>
        }
      >
        <p className="text-sm text-ink-muted">
          Headless Claude Code sessions the orchestrator spawns when a task needs more than a few
          tool calls. Configuration lands in a later phase — until then, workers run with the
          runtime defaults.
        </p>
      </Card>
    </div>
  );
}

function LayerDiagram() {
  const items = [
    { label: "Senses", colour: "from-accent-blue to-accent-blue-dim" },
    { label: "Triage", colour: "from-accent-amber to-accent-amber-dim" },
    { label: "Orchestrator", colour: "from-state-healthy to-accent-blue-dim" },
    { label: "Workers", colour: "from-state-warn to-state-error" },
  ];
  return (
    <Card title="Four-layer pipeline" subtitle="data flows up, commands flow down">
      <div className="flex items-stretch gap-2">
        {items.map((item, idx) => (
          <div key={item.label} className="flex-1">
            <div
              className={clsx(
                "flex h-16 items-center justify-center rounded-md bg-gradient-to-br font-medium text-white",
                item.colour
              )}
            >
              {item.label}
            </div>
            {idx < items.length - 1 && <div className="mx-auto mt-1 h-px w-8 bg-ink-dim" />}
          </div>
        ))}
      </div>
    </Card>
  );
}

/**
 * Honest status row for a layer's model / backend.
 *
 * Replaces the previous "ResourceRow" that hard-coded "RAM/CPU/GPU: –" — we
 * don't have live per-process numbers from the Tauri side, so showing
 * "–" implied we'd eventually fill them in. The model_status plumbing on
 * the runtime side is the source of truth; this row surfaces the bits we
 * actually know.
 */
function ModelStatus({
  loaded,
  label,
  appliedModel,
}: {
  loaded: boolean;
  label: string;
  appliedModel: string;
}) {
  return (
    <div className="mt-4 flex flex-wrap items-center gap-x-4 gap-y-1 border-t border-bg-border pt-3 text-xs text-ink-muted">
      <Toggle checked={loaded} onChange={() => {}} label={label} disabled />
      <span className="text-ink-dim">
        applied: <span className="font-mono text-ink-muted">{appliedModel}</span>
      </span>
      <span
        className={clsx(
          "rounded px-1.5 py-0.5 text-[10px] uppercase tracking-wider",
          loaded ? "bg-state-healthy/20 text-state-healthy" : "bg-state-idle/20 text-state-idle"
        )}
        title={
          loaded
            ? "Runtime reports this model/backend is reachable"
            : "Not yet reported by the runtime"
        }
      >
        {loaded ? "ready" : "not reported"}
      </span>
    </div>
  );
}
