"use client";

import { invoke } from "@tauri-apps/api/core";
import { useEffect, useMemo, useState } from "react";
import { clsx } from "clsx";
import { CheckCircle2, ChevronDown, Loader2, Plus, Server, Trash2 } from "lucide-react";

import { Button, Card, Modal, Select, Toggle } from "@/components/ui/primitives";
import { continuum } from "@/lib/tauri";
import type {
  InstallMcpServerInput,
  McpServerRegistration,
  McpTool,
  SaveSkillInput,
  Skill,
} from "@/lib/types";

const PERMISSION_PRESETS: Array<{ value: Permission; label: string }> = [
  { value: "auto", label: "Auto" },
  { value: "session", label: "Session" },
  { value: "confirm", label: "Confirm" },
  { value: "blocked", label: "Blocked" },
];

type Permission = "auto" | "session" | "confirm" | "blocked";

interface ToolPermissionView {
  tool: string;
  permission: Permission;
  source: "bundled_default" | "user_override";
}

function permissionMap(items: ToolPermissionView[]): Record<string, Permission> {
  return Object.fromEntries(items.map((item) => [item.tool, item.permission]));
}

export function ToolsTab() {
  const [skills, setSkills] = useState<Skill[]>([]);
  const [editing, setEditing] = useState<Skill | null>(null);
  const [installUrl, setInstallUrl] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [mcpTools, setMcpTools] = useState<McpTool[]>([]);
  const [mcpServers, setMcpServers] = useState<McpServerRegistration[]>([]);
  const [mcpLoading, setMcpLoading] = useState(false);
  const [mcpError, setMcpError] = useState<string | null>(null);
  const [mcpNotice, setMcpNotice] = useState<string | null>(null);
  const [showServerInstaller, setShowServerInstaller] = useState(false);
  const [toolPermissions, setToolPermissions] = useState<Record<string, Permission>>({});
  const [savingPermissions, setSavingPermissions] = useState<Set<string>>(new Set());

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
      const [tools, servers, permissions] = await Promise.all([
        continuum.listMcpTools(),
        continuum.listInstalledMcpServers(),
        invoke<ToolPermissionView[]>("list_tool_permissions"),
      ]);
      setMcpTools(tools);
      setMcpServers(servers);
      setToolPermissions(permissionMap(permissions));
      setMcpError(null);
    } catch (e) {
      setMcpError(`Failed to load MCP tools and permissions: ${formatError(e)}`);
    } finally {
      setMcpLoading(false);
    }
  }

  useEffect(() => {
    void refresh();
    void refreshMcpTools();
  }, []);

  const mcpByNamespace = useMemo(() => {
    const out: Array<{ namespace: string; tools: McpTool[] }> = [];
    const index = new Map<string, number>();
    for (const tool of mcpTools) {
      let groupIndex = index.get(tool.namespace);
      if (groupIndex === undefined) {
        groupIndex = out.length;
        index.set(tool.namespace, groupIndex);
        out.push({ namespace: tool.namespace, tools: [] });
      }
      out[groupIndex].tools.push(tool);
    }
    return out;
  }, [mcpTools]);

  async function handlePermissionChange(name: string, value: Permission) {
    const previous = toolPermissions[name];
    setToolPermissions((current) => ({ ...current, [name]: value }));
    setSavingPermissions((current) => new Set(current).add(name));
    setMcpError(null);
    try {
      const permissions = await invoke<ToolPermissionView[]>("set_tool_permission", {
        tool: name,
        permission: value,
      });
      setToolPermissions(permissionMap(permissions));
      setMcpNotice(
        `${name} is now ${value}. The enforced policy is used by the next MCP/agent process.`
      );
    } catch (permissionError) {
      setToolPermissions((current) => ({ ...current, [name]: previous ?? "blocked" }));
      setMcpError(`Permission update failed: ${formatError(permissionError)}`);
    } finally {
      setSavingPermissions((current) => {
        const next = new Set(current);
        next.delete(name);
        return next;
      });
    }
  }

  async function handleInstallServer(input: InstallMcpServerInput) {
    const server = await continuum.installMcpServer(input);
    setShowServerInstaller(false);
    setMcpNotice(`${server.name} is registered and will be connected on the next agent run.`);
    await refreshMcpTools();
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

  return (
    <div className="mx-auto max-w-6xl space-y-6">
      <Card
        title="MCP tools"
        subtitle={`${mcpTools.length} tools across ${mcpByNamespace.length} namespaces — enforced by continuum-mcp`}
        actions={
          <Button
            size="sm"
            variant="default"
            onClick={() => {
              setMcpError(null);
              setMcpNotice(null);
              setShowServerInstaller(true);
            }}
          >
            <Plus size={12} /> Install server
          </Button>
        }
      >
        <div
          className="mb-3 rounded-md border border-accent-blue/30 bg-accent-blue/10 px-3 py-2 text-xs leading-relaxed text-accent-blue"
          role="status"
        >
          These permissions are live. Continuum writes changes atomically to the local permission
          policy, and the broker enforces them before a tool body runs. Unknown tools require
          confirmation by default.
        </div>

        {mcpError && (
          <div
            className="mb-3 rounded-md border border-state-error/40 bg-state-error/10 px-3 py-2 text-sm text-state-error"
            role="alert"
          >
            {mcpError}
          </div>
        )}

        {mcpNotice && (
          <div
            className="mb-3 flex items-center gap-2 rounded-md border border-state-healthy/30 bg-state-healthy/10 px-3 py-2 text-sm text-state-healthy"
            role="status"
          >
            <CheckCircle2 size={14} />
            {mcpNotice}
          </div>
        )}

        {mcpServers.length > 0 && (
          <div className="mb-4 rounded-md border border-bg-border bg-bg-elevated">
            <div className="border-b border-bg-border px-3 py-2 text-[11px] uppercase tracking-wide text-ink-dim">
              Installed servers
            </div>
            <ul className="divide-y divide-bg-border">
              {mcpServers.map((server) => (
                <li key={server.name} className="flex items-start gap-3 px-3 py-2.5">
                  <Server className="mt-0.5 shrink-0 text-accent-amber" size={14} />
                  <div className="min-w-0 flex-1">
                    <div className="font-mono text-xs text-ink">{server.name}</div>
                    <div
                      className="truncate font-mono text-[11px] text-ink-dim"
                      title={server.command}
                    >
                      {server.command}
                      {server.args.length > 0 ? ` ${server.args.join(" ")}` : ""}
                    </div>
                  </div>
                  <span className="shrink-0 text-[11px] text-ink-dim">Next agent run</span>
                </li>
              ))}
            </ul>
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
            {mcpByNamespace.map((namespace) => (
              <McpNamespace
                key={namespace.namespace}
                ns={namespace}
                permissions={toolPermissions}
                saving={savingPermissions}
                onPermissionChange={handlePermissionChange}
              />
            ))}
          </div>
        )}
      </Card>

      <McpServerInstaller
        open={showServerInstaller}
        onClose={() => setShowServerInstaller(false)}
        onInstall={handleInstallServer}
      />

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
            {skills.map((skill) => (
              <li
                key={skill.name}
                className="flex flex-col gap-2 py-3 text-sm sm:flex-row sm:items-center"
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2 text-ink">
                    <span>{skill.name}</span>
                    {skill.source && <SourceBadge source={skill.source} />}
                  </div>
                  <div className="truncate text-xs text-ink-muted">{skill.description}</div>
                  <div className="mt-0.5 truncate text-[11px] text-ink-dim">
                    triggers: {skill.triggers.join(", ") || "-"}
                  </div>
                </div>
                <div className="flex items-center gap-2">
                  <Toggle checked={skill.enabled} onChange={() => handleToggle(skill)} />
                  <Button size="sm" variant="ghost" onClick={() => setEditing(skill)}>
                    Edit
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={() => handleDelete(skill)}
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

function McpServerInstaller({
  open,
  onClose,
  onInstall,
}: {
  open: boolean;
  onClose: () => void;
  onInstall: (input: InstallMcpServerInput) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const [argsText, setArgsText] = useState("[]");
  const [installing, setInstalling] = useState(false);
  const [installError, setInstallError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setName("");
    setCommand("");
    setArgsText("[]");
    setInstalling(false);
    setInstallError(null);
  }, [open]);

  async function submit() {
    setInstallError(null);
    let args: unknown;
    try {
      args = JSON.parse(argsText);
    } catch {
      setInstallError('Arguments must be a JSON array, for example ["--stdio"].');
      return;
    }
    if (!Array.isArray(args) || !args.every((argument) => typeof argument === "string")) {
      setInstallError("Every argument must be a string inside a JSON array.");
      return;
    }

    setInstalling(true);
    try {
      await onInstall({ name: name.trim(), command: command.trim(), args });
    } catch (installErrorValue) {
      setInstallError(formatError(installErrorValue));
    } finally {
      setInstalling(false);
    }
  }

  return (
    <Modal
      open={open}
      onClose={() => {
        if (!installing) onClose();
      }}
      title="Install MCP server"
      width="md"
      footer={
        <>
          <Button size="sm" variant="ghost" onClick={onClose} disabled={installing}>
            Cancel
          </Button>
          <Button
            size="sm"
            variant="primary"
            onClick={submit}
            disabled={installing || !name.trim() || !command.trim()}
          >
            {installing ? <Loader2 className="animate-spin" size={13} /> : <Plus size={13} />}
            {installing ? "Checking executable…" : "Install server"}
          </Button>
        </>
      }
    >
      <div className="space-y-4 text-sm">
        <div className="rounded-md border border-bg-border bg-bg-elevated px-3 py-2 text-xs leading-relaxed text-ink-muted">
          Register an MCP server that is already installed on this computer. Continuum checks the
          executable and saves the registration locally; it does not download packages or run the
          server during installation. The server connects on the next agent run. Only register
          software you trust, because its process runs with your operating-system account access.
        </div>

        <Field label="Server name">
          <input
            type="text"
            value={name}
            onChange={(event) => setName(event.target.value.toLowerCase())}
            placeholder="example-server"
            autoComplete="off"
            disabled={installing}
            className="w-full rounded-md border border-bg-border bg-bg-elevated px-3 py-2 font-mono text-sm text-ink placeholder:text-ink-dim focus:border-accent-amber focus:outline-none focus:ring-2 focus:ring-accent-amber/20 disabled:opacity-60"
          />
          <div className="mt-1 text-[11px] text-ink-dim">
            Lowercase letters, numbers, hyphens, and underscores only.
          </div>
        </Field>

        <Field label="Executable">
          <input
            type="text"
            value={command}
            onChange={(event) => setCommand(event.target.value)}
            placeholder="C:\\Tools\\my-mcp-server.exe"
            autoComplete="off"
            spellCheck={false}
            disabled={installing}
            className="w-full rounded-md border border-bg-border bg-bg-elevated px-3 py-2 font-mono text-sm text-ink placeholder:text-ink-dim focus:border-accent-amber focus:outline-none focus:ring-2 focus:ring-accent-amber/20 disabled:opacity-60"
          />
          <div className="mt-1 text-[11px] text-ink-dim">
            Use a full path, or a command already available on PATH. Do not put tokens or passwords
            in the command or arguments.
          </div>
        </Field>

        <Field label="Arguments (JSON array)">
          <input
            type="text"
            value={argsText}
            onChange={(event) => setArgsText(event.target.value)}
            placeholder='["--stdio"]'
            autoComplete="off"
            spellCheck={false}
            disabled={installing}
            className="w-full rounded-md border border-bg-border bg-bg-elevated px-3 py-2 font-mono text-sm text-ink placeholder:text-ink-dim focus:border-accent-amber focus:outline-none focus:ring-2 focus:ring-accent-amber/20 disabled:opacity-60"
          />
        </Field>

        {installError && (
          <div
            className="rounded-md border border-state-error/40 bg-state-error/10 px-3 py-2 text-sm text-state-error"
            role="alert"
          >
            <div className="font-medium">Server was not installed</div>
            <div className="mt-1 text-xs leading-relaxed">{installError}</div>
          </div>
        )}
      </div>
    </Modal>
  );
}

function formatError(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return "An unknown error occurred.";
  }
}

function McpNamespace({
  ns,
  permissions,
  saving,
  onPermissionChange,
}: {
  ns: { namespace: string; tools: McpTool[] };
  permissions: Record<string, Permission>;
  saving: Set<string>;
  onPermissionChange: (name: string, value: Permission) => Promise<void>;
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
              <div className="flex items-center gap-2">
                {saving.has(tool.name) && <Loader2 size={12} className="animate-spin text-ink-dim" />}
                <Select
                  value={permissions[tool.name] ?? "blocked"}
                  options={PERMISSION_PRESETS}
                  onChange={(value) => void onPermissionChange(tool.name, value as Permission)}
                  disabled={saving.has(tool.name)}
                  className="w-28"
                  title="Enforced by continuum-mcp. Changes apply to the next MCP process."
                />
              </div>
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
  onSave: (skill: SaveSkillInput) => void;
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
                  .map((trigger) => trigger.trim())
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
