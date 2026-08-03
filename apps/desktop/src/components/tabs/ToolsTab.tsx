"use client";

import { useEffect, useMemo, useState } from "react";
import { clsx } from "clsx";
import { ChevronDown, Plus, Trash2 } from "lucide-react";

import { Button, Card, Select, Toggle } from "@/components/ui/primitives";
import { continuum } from "@/lib/tauri";
import type { McpTool, SaveSkillInput, Skill } from "@/lib/types";

/**
 * Permission presets surfaced in the per-tool dropdown. The wire-up against
 * `default-permissions.toml` is intentionally NOT live yet — toggling
 * here updates component state only, with a small banner telling the user
 * that the runtime is unaware of the change. That avoids a silent "save
 * but nothing happens" footgun while the in-memory state exists.
 */
const PERMISSION_PRESETS: Array<{ value: Permission; label: string }> = [
  { value: "auto", label: "Auto" },
  { value: "session", label: "Session" },
  { value: "confirm", label: "Confirm" },
  { value: "blocked", label: "Blocked" },
];

type Permission = "auto" | "session" | "confirm" | "blocked";

export function ToolsTab() {
  const [skills, setSkills] = useState<Skill[]>([]);
  const [editing, setEditing] = useState<Skill | null>(null);
  const [installUrl, setInstallUrl] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // --- MCP tools (live, sourced from continuum-mcp static manifest) ---
  const [mcpTools, setMcpTools] = useState<McpTool[]>([]);
  const [mcpLoading, setMcpLoading] = useState(false);
  const [mcpError, setMcpError] = useState<string | null>(null);
  const [toolPermissions, setToolPermissions] = useState<Record<string, Permission>>({});

  async function refresh() {
    setLoading(true);
    try {
      setSkills(await continuum.listSkills());
      setError(null);
    } catch (e) {
      setError(`Failed to load skills: ${e}`);
    } finally {
      setLoading(false);
    }
  }

  async function refreshMcpTools() {
    setMcpLoading(true);
    try {
      setMcpTools(await continuum.listMcpTools());
      setMcpError(null);
    } catch (e) {
      setMcpError(`Failed to load MCP tools: ${e}`);
    } finally {
      setMcpLoading(false);
    }
  }

  useEffect(() => {
    refresh();
    refreshMcpTools();
  }, []);

  // Group MCP tools by namespace, preserve the order returned by the backend.
  const mcpByNamespace = useMemo(() => {
    const out: Array<{ namespace: string; tools: McpTool[] }> = [];
    const index = new Map<string, number>();
    for (const t of mcpTools) {
      let i = index.get(t.namespace);
      if (i === undefined) {
        i = out.length;
        index.set(t.namespace, i);
        out.push({ namespace: t.namespace, tools: [] });
      }
      out[i].tools.push(t);
    }
    return out;
  }, [mcpTools]);

  function setToolPermission(name: string, value: Permission) {
    setToolPermissions((prev) => ({ ...prev, [name]: value }));
  }

  async function handleToggle(skill: Skill) {
    try {
      await continuum.toggleSkill(skill.name, !skill.enabled);
      await refresh();
    } catch (e) {
      setError(`Toggle failed: ${e}`);
    }
  }

  async function handleDelete(skill: Skill) {
    if (!confirm(`Delete skill '${skill.name}'? This removes the directory on disk.`)) return;
    try {
      await continuum.deleteSkill(skill.name);
      await refresh();
    } catch (e) {
      setError(`Delete failed: ${e}`);
    }
  }

  async function handleSave(input: SaveSkillInput) {
    try {
      await continuum.saveSkill(input);
      setEditing(null);
      await refresh();
    } catch (e) {
      setError(`Save failed: ${e}`);
    }
  }

  async function handleInstall() {
    if (!installUrl.trim()) return;
    try {
      await continuum.installSkillFromUrl(installUrl.trim());
      setInstallUrl("");
      await refresh();
    } catch (e) {
      setError(`Install failed: ${e}`);
    }
  }

  const dirtyPermissionCount = useMemo(
    () => Object.keys(toolPermissions).length,
    [toolPermissions]
  );

  return (
    <div className="mx-auto max-w-6xl space-y-6">
      <Card
        title="MCP tools"
        subtitle={`${mcpTools.length} tools across ${mcpByNamespace.length} namespaces — exposed to the orchestrator via continuum-mcp`}
        actions={
          <Button
            size="sm"
            variant="default"
            disabled
            title="Adding new MCP servers is not yet supported from the UI. Edit config/default-permissions.toml to add namespaces."
          >
            <Plus size={12} /> Install server
          </Button>
        }
      >
        {dirtyPermissionCount > 0 && (
          <div
            className="mb-3 rounded-md border border-state-warn/40 bg-state-warn/10 px-3 py-2 text-xs text-state-warn"
            role="status"
          >
            {dirtyPermissionCount} permission change
            {dirtyPermissionCount === 1 ? "" : "s"} held in memory only. The runtime does not yet
            read the dashboard&apos;s per-tool permissions — this UI exists so the
            orchestrator&apos;s eventual permission policy is visible and reviewable.
          </div>
        )}

        {mcpError && (
          <div className="mb-3 rounded-md border border-state-error/40 bg-state-error/10 px-3 py-2 text-sm text-state-error">
            {mcpError}
          </div>
        )}

        {mcpLoading && mcpTools.length === 0 ? (
          <div className="py-6 text-center text-sm text-ink-dim">Loading MCP tool list…</div>
        ) : mcpTools.length === 0 ? (
          <div className="py-6 text-center text-sm text-ink-dim">
            No MCP tools registered. This is unexpected — check that the dashboard was built with
            the latest continuum-mcp manifest.
          </div>
        ) : (
          <div className="space-y-2">
            {mcpByNamespace.map((ns) => (
              <McpNamespace
                key={ns.namespace}
                ns={ns}
                permissions={toolPermissions}
                onPermissionChange={setToolPermission}
              />
            ))}
          </div>
        )}
      </Card>

      <Card
        title="Skills"
        subtitle={`${skills.length} loaded · hot-reload watches skills/`}
        actions={
          <Button
            size="sm"
            onClick={() =>
              setEditing({
                name: "",
                description: "",
                triggers: [],
                source: "user",
                manual_only: false,
                enabled: true,
                body: "",
                path: "",
              })
            }
          >
            <Plus size={12} /> New skill
          </Button>
        }
      >
        <div className="mb-3 flex items-center gap-2">
          <input
            type="text"
            value={installUrl}
            onChange={(e) => setInstallUrl(e.target.value)}
            placeholder="Install from git URL (https://…)"
            className="flex-1 rounded-md border border-bg-border bg-bg-elevated px-3 py-1.5 text-sm text-ink placeholder:text-ink-dim"
          />
          <Button size="sm" onClick={handleInstall} disabled={!installUrl.trim()}>
            Install
          </Button>
        </div>

        {error && (
          <div className="mb-3 rounded-md border border-state-error/40 bg-state-error/10 px-3 py-2 text-sm text-state-error">
            {error}
          </div>
        )}

        {loading && skills.length === 0 ? (
          <div className="py-6 text-center text-sm text-ink-dim">Loading skills…</div>
        ) : skills.length === 0 ? (
          <div className="py-6 text-center text-sm text-ink-dim">
            No skills installed yet. Drop a `&lt;name&gt;/SKILL.md` under `skills/` or click New
            skill.
          </div>
        ) : (
          <ul className="divide-y divide-bg-border">
            {skills.map((s) => (
              <li
                key={s.name}
                className="flex flex-col gap-2 py-3 text-sm sm:flex-row sm:items-center"
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2 text-ink">
                    <span>{s.name}</span>
                    {s.source && <SourceBadge source={s.source} />}
                  </div>
                  <div className="truncate text-xs text-ink-muted">{s.description}</div>
                  <div className="mt-0.5 truncate text-[11px] text-ink-dim">
                    triggers: {s.triggers.join(", ") || "-"}
                  </div>
                </div>
                <div className="flex items-center gap-2">
                  <Toggle checked={s.enabled} onChange={() => handleToggle(s)} />
                  <Button size="sm" variant="ghost" onClick={() => setEditing(s)}>
                    Edit
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={() => handleDelete(s)}
                    title="Delete skill"
                  >
                    <Trash2 size={12} />
                  </Button>
                </div>
              </li>
            ))}
          </ul>
        )}
      </Card>

      {editing && (
        <SkillEditor initial={editing} onCancel={() => setEditing(null)} onSave={handleSave} />
      )}
    </div>
  );
}

function McpNamespace({
  ns,
  permissions,
  onPermissionChange,
}: {
  ns: { namespace: string; tools: McpTool[] };
  permissions: Record<string, Permission>;
  onPermissionChange: (name: string, value: Permission) => void;
}) {
  const [open, setOpen] = useState(true);
  return (
    <div className="rounded-md border border-bg-border bg-bg-elevated">
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className="flex w-full items-center justify-between px-3 py-2 text-left"
      >
        <span className="font-mono text-sm text-ink">{ns.namespace}</span>
        <span className="flex items-center gap-2 text-xs text-ink-muted">
          {ns.tools.length} tools
          <ChevronDown size={14} className={clsx("transition-transform", open && "rotate-180")} />
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
                <div className="truncate font-mono text-xs text-ink">{tool.name}</div>
                <div className="truncate text-xs text-ink-muted">{tool.description}</div>
              </div>
              <Select
                value={permissions[tool.name] ?? "auto"}
                options={PERMISSION_PRESETS}
                onChange={(v) => onPermissionChange(tool.name, v as Permission)}
                className="w-28"
                title="Per-tool permission is in-memory only — the runtime reads default-permissions.toml on startup."
              />
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function SkillEditor({
  initial,
  onSave,
  onCancel,
}: {
  initial: Skill;
  onSave: (s: SaveSkillInput) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(initial.name);
  const [description, setDescription] = useState(initial.description);
  const [triggers, setTriggers] = useState(initial.triggers.join(", "));
  const [body, setBody] = useState(initial.body);
  return (
    <Card
      title={initial.name ? `Edit: ${initial.name}` : "New skill"}
      subtitle={initial.path || "unsaved"}
      actions={
        <div className="flex gap-2">
          <Button size="sm" variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
          <Button
            size="sm"
            onClick={() =>
              onSave({
                name: name.trim(),
                description: description.trim(),
                triggers: triggers
                  .split(",")
                  .map((t) => t.trim())
                  .filter(Boolean),
                body,
                source: initial.source ?? "user",
                manual_only: initial.manual_only,
              })
            }
            disabled={!name.trim() || !description.trim()}
          >
            Save
          </Button>
        </div>
      }
    >
      <div className="space-y-3 text-sm">
        <Field label="Name">
          <input
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            disabled={Boolean(initial.name)}
            className="w-full rounded-md border border-bg-border bg-bg-elevated px-3 py-1.5 text-ink disabled:opacity-60"
          />
        </Field>
        <Field label="Description">
          <input
            type="text"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            className="w-full rounded-md border border-bg-border bg-bg-elevated px-3 py-1.5 text-ink"
          />
        </Field>
        <Field label="Triggers (comma-separated)">
          <input
            type="text"
            value={triggers}
            onChange={(e) => setTriggers(e.target.value)}
            className="w-full rounded-md border border-bg-border bg-bg-elevated px-3 py-1.5 text-ink"
          />
        </Field>
        <Field label="Body (Markdown)">
          <textarea
            value={body}
            onChange={(e) => setBody(e.target.value)}
            rows={12}
            className="w-full rounded-md border border-bg-border bg-bg-elevated px-3 py-2 font-mono text-xs text-ink"
          />
        </Field>
      </div>
    </Card>
  );
}

function SourceBadge({ source }: { source: string }) {
  const color =
    source === "bundled"
      ? "bg-accent-blue/20 text-accent-blue"
      : source === "third-party"
        ? "bg-state-warn/20 text-state-warn"
        : "bg-bg-elevated text-ink-muted";
  return <span className={clsx("rounded px-1.5 py-0.5 text-[10px]", color)}>{source}</span>;
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="block">
      <div className="mb-1 text-xs uppercase tracking-wider text-ink-dim">{label}</div>
      {children}
    </label>
  );
}
