"use client";

import { useState } from "react";
import { clsx } from "clsx";
import { Eye, Filter, MessageSquare, Users } from "lucide-react";

import { useStore } from "@/lib/store";
import { kairo } from "@/lib/tauri";
import { Button, Card, Select, Slider, Toggle } from "@/components/ui/primitives";

export function BrainTab() {
  const config = useStore((s) => s.config);
  const setConfig = useStore((s) => s.setConfig);
  const system = useStore((s) => s.state.system);
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
        title="Layer 1 — Vision"
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
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
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
            label="Capture interval (s)"
            value={config.screen.interval_secs}
            onChange={async (v) => {
              const cfg = await kairo.updateScreenInterval(Math.round(v));
              setConfig(cfg);
            }}
            min={1}
            max={10}
            step={1}
            format={(v) => `${Math.round(v)}s`}
          />
        </div>
        <ResourceRow loaded={system.vision_model_loaded} label="Vision model" />
      </Card>

      <Card
        title="Layer 2 — Triage"
        subtitle="Local LLM gatekeeper"
        actions={
          <Button size="sm" onClick={() => testLayer("triage")} disabled={testing === "triage"}>
            <Filter size={12} />
            {testing === "triage" ? "Running…" : "Test triage"}
          </Button>
        }
      >
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
          <Select
            label="Model"
            value="qwen3-8b"
            onChange={() => {}}
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
              const cfg = await kairo.updateTriageThreshold(v);
              setConfig(cfg);
            }}
            min={0}
            max={1}
            step={0.05}
            format={(v) => v.toFixed(2)}
          />
        </div>
        <ResourceRow loaded={system.triage_model_loaded} label="Triage model" />
      </Card>

      <Card
        title="Layer 3 — Orchestrator"
        subtitle="Claude Opus via headless CLI"
        actions={
          <Button size="sm" onClick={() => testLayer("orchestrator")}>
            <MessageSquare size={12} /> Dry-run
          </Button>
        }
      >
        <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
          <Select
            label="Model"
            value="claude-opus-4-6"
            onChange={() => {}}
            options={[
              { value: "claude-opus-4-6", label: "claude-opus-4-6 (default)" },
              { value: "claude-sonnet-4-6", label: "claude-sonnet-4-6" },
            ]}
          />
          <Slider
            label="Token budget"
            value={4000}
            onChange={() => {}}
            min={1000}
            max={16000}
            step={500}
            format={(v) => `${Math.round(v)} tokens`}
          />
        </div>
        <ResourceRow loaded={system.orchestrator_ready} label="Claude CLI reachable" />
      </Card>

      <Card
        title="Layer 4 — Workers"
        subtitle="Headless Claude Code sessions spawned by the orchestrator"
        actions={
          <Button size="sm" disabled>
            <Users size={12} /> Phase 8
          </Button>
        }
      >
        <div className="grid grid-cols-1 gap-4 md:grid-cols-3">
          <Select
            label="Mode"
            value="auto"
            onChange={() => {}}
            options={[
              { value: "auto", label: "Auto" },
              { value: "budget", label: "Budget (Sonnet)" },
              { value: "power", label: "Power (Opus)" },
            ]}
          />
          <Slider
            label="Max concurrent"
            value={3}
            onChange={() => {}}
            min={1}
            max={10}
            step={1}
            format={(v) => `${Math.round(v)}`}
          />
          <Select
            label="Default worker model"
            value="claude-sonnet-4-6"
            onChange={() => {}}
            options={[
              { value: "claude-sonnet-4-6", label: "claude-sonnet-4-6" },
              { value: "claude-opus-4-6", label: "claude-opus-4-6" },
              { value: "claude-haiku-4-5", label: "claude-haiku-4-5" },
            ]}
          />
        </div>
      </Card>
    </div>
  );
}

function LayerDiagram() {
  const items = [
    { label: "Senses", colour: "from-accent-blue to-accent-blue-dim" },
    { label: "Triage", colour: "from-accent-purple to-accent-purple-dim" },
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

function ResourceRow({ loaded, label }: { loaded: boolean; label: string }) {
  return (
    <div className="mt-4 flex items-center gap-4 border-t border-bg-border pt-3 text-xs text-ink-muted">
      <Toggle checked={loaded} onChange={() => {}} label={label} disabled />
      <span className="text-ink-dim">RAM: –</span>
      <span className="text-ink-dim">CPU: –</span>
      <span className="text-ink-dim">GPU: –</span>
    </div>
  );
}
