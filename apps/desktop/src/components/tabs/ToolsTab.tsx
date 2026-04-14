"use client";

import { useState } from "react";
import { clsx } from "clsx";
import { ChevronDown, Plus } from "lucide-react";

import { Button, Card, Select, Toggle } from "@/components/ui/primitives";

const MCP_NAMESPACES: Array<{
  namespace: string;
  tools: Array<{
    name: string;
    description: string;
    permission: "auto" | "session" | "confirm" | "blocked";
  }>;
}> = [
  {
    namespace: "memory",
    tools: [
      { name: "memory_query_episodic", description: "Vector search past events", permission: "auto" },
      { name: "memory_list_facts", description: "List semantic facts", permission: "auto" },
      { name: "memory_get_fact", description: "Fetch a single fact", permission: "auto" },
      { name: "memory_set_fact", description: "Write/update a fact", permission: "auto" },
    ],
  },
  {
    namespace: "system",
    tools: [
      { name: "system_current_time", description: "Local time", permission: "auto" },
      { name: "system_active_window", description: "Foreground window info", permission: "auto" },
      { name: "system_clipboard_get", description: "Read clipboard", permission: "confirm" },
      { name: "system_notification", description: "Show toast", permission: "session" },
    ],
  },
  {
    namespace: "fs",
    tools: [
      { name: "fs_read_file", description: "Read file (100 KB cap)", permission: "session" },
      { name: "fs_list_dir", description: "Directory listing", permission: "session" },
    ],
  },
  {
    namespace: "web",
    tools: [
      { name: "web_fetch", description: "HTTP GET, 50 KB cap, public IPs only", permission: "session" },
    ],
  },
];

const SKILLS = [
  {
    id: "simcharts-dev",
    enabled: true,
    description: "SimCharts-specific dev workflow",
  },
];

export function ToolsTab() {
  return (
    <div className="mx-auto max-w-6xl space-y-6">
      <Card
        title="MCP tools"
        subtitle="Exposed to the orchestrator via kairo-mcp"
        actions={
          <Button size="sm" variant="default" disabled>
            <Plus size={12} /> Install server
          </Button>
        }
      >
        <div className="space-y-2">
          {MCP_NAMESPACES.map((ns) => (
            <Namespace key={ns.namespace} ns={ns} />
          ))}
        </div>
      </Card>

      <Card
        title="Skills"
        subtitle="SKILL.md files loaded by the orchestrator"
        actions={
          <Button size="sm" disabled>
            <Plus size={12} /> Install skill
          </Button>
        }
      >
        {SKILLS.length === 0 ? (
          <div className="py-6 text-center text-sm text-ink-dim">
            No skills installed.
          </div>
        ) : (
          <ul className="divide-y divide-bg-border">
            {SKILLS.map((s) => (
              <li
                key={s.id}
                className="flex items-center justify-between py-3 text-sm"
              >
                <div>
                  <div className="text-ink">{s.id}</div>
                  <div className="text-xs text-ink-muted">{s.description}</div>
                </div>
                <Toggle checked={s.enabled} onChange={() => {}} />
              </li>
            ))}
          </ul>
        )}
      </Card>
    </div>
  );
}

function Namespace({
  ns,
}: {
  ns: {
    namespace: string;
    tools: Array<{ name: string; description: string; permission: string }>;
  };
}) {
  const [open, setOpen] = useState(true);
  return (
    <div className="rounded-md border border-bg-border bg-bg-elevated">
      <button
        onClick={() => setOpen(!open)}
        className="flex w-full items-center justify-between px-3 py-2 text-left"
      >
        <span className="font-mono text-sm text-ink">{ns.namespace}</span>
        <span className="flex items-center gap-2 text-xs text-ink-muted">
          {ns.tools.length} tools
          <ChevronDown
            size={14}
            className={clsx("transition-transform", open && "rotate-180")}
          />
        </span>
      </button>
      {open && (
        <ul className="divide-y divide-bg-border border-t border-bg-border">
          {ns.tools.map((tool) => (
            <li
              key={tool.name}
              className="flex items-center justify-between gap-4 px-3 py-2 text-sm"
            >
              <div className="min-w-0">
                <div className="truncate font-mono text-xs text-ink">
                  {tool.name}
                </div>
                <div className="truncate text-xs text-ink-muted">
                  {tool.description}
                </div>
              </div>
              <Select
                value={tool.permission}
                options={[
                  { value: "auto", label: "Auto" },
                  { value: "session", label: "Session" },
                  { value: "confirm", label: "Confirm" },
                  { value: "blocked", label: "Blocked" },
                ]}
                onChange={() => {}}
                className="w-28"
              />
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
