"use client";

import { useEffect, useMemo, useState, type ReactNode } from "react";
import { clsx } from "clsx";
import { Activity, Bot, Cpu, Eye, Gauge, Network, RotateCcw, Users } from "lucide-react";

import { useStore } from "@/lib/store";
import { continuum, type AgentRuntimeInfo } from "@/lib/tauri";
import type { ProviderConnection } from "@/lib/types";
import { Button, Card, Select, Slider, TextInput, Toggle } from "@/components/ui/primitives";

export function BrainTab() {
  const config = useStore((state) => state.config);
  const setConfig = useStore((state) => state.setConfig);
  const state = useStore((store) => store.state);
  const [providers, setProviders] = useState<ProviderConnection[]>([]);
  const [agents, setAgents] = useState<AgentRuntimeInfo[]>([]);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void Promise.all([continuum.providersList(), continuum.listAgentRuntimes()]).then(
      ([nextProviders, nextAgents]) => {
        setProviders(nextProviders);
        setAgents(nextAgents);
      }
    );
  }, []);

  const agentOptions = useMemo(
    () =>
      ["claude", "codex", "hermes"].map((id) => {
        const runtime = agents.find((item) => item.id === id);
        const label = runtime?.label ?? agentLabel(id);
        return {
          value: id,
          label: runtime?.available === false ? `${label} (not installed)` : label,
          disabled: runtime?.available === false,
        };
      }),
    [agents]
  );

  const save = async (update: Parameters<typeof continuum.updateBrainConfig>[0]) => {
    setError(null);
    try {
      setConfig(await continuum.updateBrainConfig(update));
      setNotice("Saved. The selected runtime and model apply on the next Continuum start.");
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    }
  };

  const triageOptions = localTriageOptions(config.triage.model_path);
  const orchestratorModels = modelOptions(
    config.orchestrator.model_id,
    config.orchestrator.agent,
    providers
  );
  const workerModels = modelOptions(config.workers.power_model, config.workers.agent, providers);
  const triageCalls = Object.values(state.triage.decision_counts_today).reduce(
    (sum, value) => sum + value,
    0
  );

  return (
    <div className="mx-auto max-w-5xl space-y-5">
      <header className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold tracking-tight text-ink">Brain</h1>
          <p className="mt-1 max-w-2xl text-sm text-ink-muted">
            One pipeline, four jobs. Senses observe, triage decides what matters, the orchestrator
            plans, and workers execute bounded tasks.
          </p>
        </div>
        <div className="flex items-center gap-2 text-xs text-ink-muted">
          <RotateCcw size={13} strokeWidth={1.6} /> Model changes apply after restart
        </div>
      </header>

      {(notice || error) && (
        <div
          role={error ? "alert" : "status"}
          className={clsx(
            "rounded-md border px-3 py-2 text-xs",
            error
              ? "border-state-error/40 bg-state-error/10 text-state-error"
              : "border-accent-blue/30 bg-accent-blue/10 text-ink-muted"
          )}
        >
          {error ?? notice}
        </div>
      )}

      <LayerCard
        number="01"
        icon={Eye}
        title="Vision"
        description="Turns changed screen frames into a short local description. No screenshot is sent to a cloud model."
        ready={state.system.vision_model_loaded}
        model={config.vision.name}
        recommendation="SmolVLM-256M ONNX · light default"
        usage={`${state.perception.frames_today.toLocaleString()} frames today · ${state.perception.dropped_capture_events} dropped`}
      >
        <div className="grid min-w-0 grid-cols-1 gap-4 lg:grid-cols-3">
          <Select
            label="Provider"
            value="onnx-local"
            onChange={() => undefined}
            options={[{ value: "onnx-local", label: "Local ONNX Runtime" }]}
          />
          <Select
            label="Model"
            value={config.vision.name}
            onChange={(vision_name) => void save({ vision_name })}
            options={[{ value: "SmolVLM-256M", label: "SmolVLM-256M Instruct" }]}
          />
          <Slider
            label="Vision cadence"
            value={config.screen.capture_interval_ms}
            onChange={async (capture_interval_ms) => {
              setConfig(
                await continuum.updateLiveContextConfig({
                  capture_interval_ms: Math.round(capture_interval_ms),
                  all_monitors: true,
                })
              );
            }}
            min={200}
            max={3000}
            step={100}
            format={(value) => `${Math.round(value)} ms`}
          />
        </div>
        <div className="mt-3 grid grid-cols-1 gap-3 lg:grid-cols-[1fr_2fr]">
          <Toggle
            checked={config.screen.enabled}
            onChange={async (enabled) =>
              setConfig(await continuum.updateLiveContextConfig({ enabled }))
            }
            label="Continuous local vision"
          />
          <TextInput
            aria-label="Vision model directory"
            value={config.vision.model_path}
            onChange={(vision_model_path) =>
              setConfig({ ...config, vision: { ...config.vision, model_path: vision_model_path } })
            }
            onBlur={(event) => void save({ vision_model_path: event.currentTarget.value })}
          />
        </div>
      </LayerCard>

      <LayerCard
        number="02"
        icon={Gauge}
        title="Triage"
        description="A small local llama.cpp model filters routine context before an expensive agent is woken."
        ready={state.system.triage_model_loaded}
        model={fileName(config.triage.model_path) || "Qwen 3 8B"}
        recommendation="Qwen 3 4B for speed · 8B for best classification"
        usage={`${triageCalls.toLocaleString()} decisions today · ${state.triage.last_latency_ms ?? "—"} ms last`}
      >
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-3">
          <Select
            label="Provider"
            value="llama-cpp-local"
            onChange={() => undefined}
            options={[{ value: "llama-cpp-local", label: "Local llama.cpp" }]}
          />
          <Select
            label="Model"
            value={config.triage.model_path}
            onChange={(triage_model_path) => void save({ triage_model_path })}
            options={triageOptions}
          />
          <Slider
            label="Wake threshold"
            value={config.frame.salience_threshold}
            onChange={async (value) => setConfig(await continuum.updateTriageThreshold(value))}
            min={0}
            max={1}
            step={0.05}
            format={(value) => value.toFixed(2)}
          />
        </div>
      </LayerCard>

      <LayerCard
        number="03"
        icon={Bot}
        title="Orchestrator"
        description="The selected agent plans the response to a wake. Claude Code, Codex and Hermes use their own authenticated CLI."
        ready={state.system.orchestrator_ready}
        model={`${agentLabel(config.orchestrator.agent)} · ${config.orchestrator.model_id}`}
        recommendation="Use your strongest reliable reasoning model"
        usage={`${state.orchestrator.wakes_today.toLocaleString()} wakes today · $${state.orchestrator.cost_usd_today.toFixed(3)} reported`}
      >
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-3">
          <Select
            label="Agent runtime"
            value={config.orchestrator.agent}
            onChange={(orchestrator_agent) => void save({ orchestrator_agent })}
            options={agentOptions}
          />
          <Select
            label="Provider"
            value={config.orchestrator.provider || "agent-default"}
            disabled={config.orchestrator.agent !== "hermes"}
            title={
              config.orchestrator.agent === "hermes"
                ? "Select the Hermes model provider"
                : "Claude Code and Codex use the provider authenticated by their own CLI"
            }
            onChange={(value) =>
              void save({ orchestrator_provider: value === "agent-default" ? "" : value })
            }
            options={providerOptions(providers)}
          />
          <Select
            label="Model"
            value={config.orchestrator.model_id}
            onChange={(orchestrator_model) => void save({ orchestrator_model })}
            options={orchestratorModels}
          />
        </div>
      </LayerCard>

      <LayerCard
        number="04"
        icon={Users}
        title="Workers"
        description="Short-lived execution agents. Auto chooses the cheaper or stronger model from task complexity; Budget and Power force one tier."
        ready={true}
        model={`${agentLabel(config.workers.agent)} · ${config.workers.mode}`}
        recommendation="Auto · 2–3 concurrent workers"
        usage={`${state.workers.active.length} active · ${state.workers.queue_depth} queued · ${state.workers.completed_today} completed today`}
      >
        <div className="grid grid-cols-1 gap-4 lg:grid-cols-5">
          <Select
            label="Agent runtime"
            value={config.workers.agent}
            onChange={(workers_agent) => void save({ workers_agent })}
            options={agentOptions}
          />
          <Select
            label="Provider"
            value={config.workers.provider || "agent-default"}
            disabled={config.workers.agent !== "hermes"}
            title={
              config.workers.agent === "hermes"
                ? "Select the Hermes model provider"
                : "Claude Code and Codex use the provider authenticated by their own CLI"
            }
            onChange={(value) =>
              void save({ workers_provider: value === "agent-default" ? "" : value })
            }
            options={providerOptions(providers)}
          />
          <Select
            label="Routing"
            value={config.workers.mode}
            onChange={(workers_mode) => void save({ workers_mode })}
            options={[
              { value: "auto", label: "Auto by task" },
              { value: "budget", label: "Always budget" },
              { value: "power", label: "Always power" },
            ]}
          />
          <Select
            label="Budget model"
            value={config.workers.budget_model}
            onChange={(workers_budget_model) => void save({ workers_budget_model })}
            options={modelOptions(config.workers.budget_model, config.workers.agent, providers)}
          />
          <Select
            label="Power model"
            value={config.workers.power_model}
            onChange={(workers_power_model) => void save({ workers_power_model })}
            options={workerModels}
          />
        </div>
        <div className="mt-4 max-w-xs">
          <Slider
            label="Concurrent worker limit"
            value={config.workers.max_concurrent}
            onChange={(workers_max_concurrent) =>
              void save({ workers_max_concurrent: Math.round(workers_max_concurrent) })
            }
            min={1}
            max={10}
            step={1}
            format={(value) => `${Math.round(value)}`}
          />
        </div>
        <div className="mt-3 flex flex-wrap items-center gap-3 text-xs text-ink-muted">
          <Network size={13} strokeWidth={1.6} />
          Provider: {config.workers.provider || "agent default"}
          <span className="text-ink-dim">·</span>
          Maximum {config.workers.max_concurrent} concurrent
          <Button
            size="sm"
            variant="ghost"
            onClick={() =>
              window.dispatchEvent(new CustomEvent("continuum:navigate", { detail: "home" }))
            }
          >
            View worker runs
          </Button>
        </div>
      </LayerCard>
    </div>
  );
}

function LayerCard({
  number,
  icon: Icon,
  title,
  description,
  ready,
  model,
  recommendation,
  usage,
  children,
}: {
  number: string;
  icon: typeof Activity;
  title: string;
  description: string;
  ready: boolean;
  model: string;
  recommendation: string;
  usage: string;
  children: ReactNode;
}) {
  return (
    <Card>
      <div className="grid min-w-0 grid-cols-1 gap-5 xl:grid-cols-[13rem_minmax(0,1fr)]">
        <div className="border-b border-bg-border pb-4 xl:border-b-0 xl:border-r xl:pb-0 xl:pr-5">
          <div className="flex items-center gap-2 text-[11px] uppercase tracking-[0.14em] text-ink-dim">
            <span className="font-mono">{number}</span>
            <Icon size={14} strokeWidth={1.5} />
            {title}
          </div>
          <p className="mt-2 text-sm leading-5 text-ink-muted">{description}</p>
          <div className="mt-4 flex items-center gap-2 text-xs">
            <span
              className={clsx(
                "h-1.5 w-1.5 rounded-full",
                ready ? "bg-state-healthy" : "bg-state-error"
              )}
            />
            <span className={ready ? "text-state-healthy" : "text-state-error"}>
              {ready ? "Ready" : "Unavailable"}
            </span>
          </div>
        </div>
        <div className="min-w-0">
          {children}
          <div className="mt-4 grid grid-cols-1 gap-2 border-t border-bg-border pt-3 text-xs md:grid-cols-3">
            <Meta icon={Cpu} label="Using" value={model} />
            <Meta icon={Activity} label="Usage" value={usage} />
            <Meta icon={Gauge} label="Recommended" value={recommendation} />
          </div>
        </div>
      </div>
    </Card>
  );
}

function Meta({
  icon: Icon,
  label,
  value,
}: {
  icon: typeof Activity;
  label: string;
  value: string;
}) {
  return (
    <div className="min-w-0">
      <div className="flex items-center gap-1.5 text-[10px] uppercase tracking-wider text-ink-dim">
        <Icon size={11} strokeWidth={1.5} /> {label}
      </div>
      <div className="mt-1 break-words font-mono text-[11px] leading-4 text-ink-muted">{value}</div>
    </div>
  );
}

function localTriageOptions(current: string) {
  const normalized = current.replaceAll("\\", "/");
  const directory = normalized.includes("/")
    ? normalized.slice(0, normalized.lastIndexOf("/"))
    : "";
  const join = (name: string) => (directory ? `${directory}/${name}` : name);
  const values = [
    current || join("qwen3-8b-q4_k_m.gguf"),
    join("qwen3-8b-q4_k_m.gguf"),
    join("qwen3-4b-q4_k_m.gguf"),
  ];
  return [...new Set(values)].map((value) => ({ value, label: fileName(value) || value }));
}

function providerOptions(providers: ProviderConnection[]) {
  return [
    { value: "agent-default", label: "Agent default" },
    ...providers.map((provider) => ({
      value: provider.catalog_id || provider.id,
      label: provider.display_name,
    })),
  ];
}

function modelOptions(current: string, agent: string, providers: ProviderConnection[]) {
  const suggested =
    agent === "claude"
      ? ["claude-sonnet-4-6", "claude-opus-4-6"]
      : agent === "codex"
        ? ["gpt-5.6-terra", "gpt-5.6-sol"]
        : [];
  const values = [
    current,
    ...suggested,
    ...providers.flatMap((provider) => provider.models),
  ].filter(Boolean);
  return [...new Set(values)].slice(0, 500).map((value) => ({ value, label: value }));
}

function agentLabel(agent: string) {
  if (agent === "codex") return "Codex";
  if (agent === "hermes") return "Hermes Agent";
  return "Claude Code";
}

function fileName(path: string) {
  return path.split(/[\\/]/).pop() ?? path;
}
