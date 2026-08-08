"use client";

import { useEffect, useMemo, useState } from "react";
import { clsx } from "clsx";
import { CheckCircle2, ChevronDown, Loader2, Plus, Server, Trash2 } from "lucide-react";

import { Button, Card, Modal, Select, Toggle } from "@/components/ui/primitives";
import { continuum } from "@/lib/tauri";
import type {
  InstallMcpServerInput,
  McpServerRegistration,
  McpTool,
  PermissionGrant,
  PermissionRequest,
  PermissionTier,
  SaveSkillInput,
  Skill,
} from "@/lib/types";

/** Permission presets persisted and enforced by the shared gateway. */
const PERMISSION_PRESETS: Array<{ value: PermissionTier; label: string }> = [
  { value: "auto", label: "Auto" },
  { value: "session-approved", label: "Session" },
  { value: "always-confirm", label: "Confirm" },
  { value: "blocked", label: "Blocked" },
];

export function ToolsTab() {
  const [skills, setSkills] = useState<Skill[]>([]);
  const [editing, setEditing] = useState<Skill | null>(null);
  const [installUrl, setInstallUrl] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // --- MCP tools (live, sourced from continuum-mcp static manifest) ---
  const [mcpTools, setMcpTools] = useState<McpTool[]>([]);
  const [mcpServers, setMcpServers] = useState<McpServerRegistration[]>([]);
  const [mcpLoading, setMcpLoading] = useState(false);
  const [mcpError, setMcpError] = useState<string | null>(null);
  const [mcpNotice, setMcpNotice] = useState<string | null>(null);
  const [showServerInstaller, setShowServerInstaller] = useState(false);
  const [toolPermissions, setToolPermissions] = useState<Record<string, PermissionTier>>({});
  const [permissionRequests, setPermissionRequests] = useState<PermissionRequest[]>([]);
  const [permissionGrants, setPermissionGrants] = useState<PermissionGrant[]>([]);

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
      const [tools, servers, policies, requests, grants] = await Promise.all([
        continuum.listMcpTools(),
        continuum.listInstalledMcpServers(),
        continuum.listToolPermissions(),
        continuum.listPermissionRequests(),
        continuum.listPermissionGrants(),
      ]);
      setMcpTools(tools);
      setMcpServers(servers);
      setToolPermissions(Object.fromEntries(policies.map((row) => [row.action, row.tier])));
      setPermissionRequests(requests);
      setPermissionGrants(grants);
      setMcpError(null);
    } catch (e) {
      setMcpError(`Failed to load MCP tools: ${e}`);
    } finally {
      setMcpLoading(false);
    }
  }

  async function refreshPermissionActivity() {
    try {
      const [requests, grants] = await Promise.all([
        continuum.listPermissionRequests(),
        continuum.listPermissionGrants(),
      ]);
      setPermissionRequests(requests);
      setPermissionGrants(grants);
    } catch (cause) {
      setMcpError(`Failed to refresh permission activity: ${cause}`);
    }
  }

  useEffect(() => {
    refresh();
    refreshMcpTools();
    const timer = window.setInterval(refreshPermissionActivity, 2_000);
    return () => window.clearInterval(timer);
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

  async function setToolPermission(name: string, value: PermissionTier) {
    const previous = toolPermissions[name];
    setToolPermissions((current) => ({ ...current, [name]: value }));
    try {
      await continuum.setToolPermission(name, value);
      setMcpNotice(`${name} is now ${value}.`);
    } catch (cause) {
      setToolPermissions((current) => ({ ...current, [name]: previous ?? "blocked" }));
      setMcpError(`Failed to save ${name}: ${cause}`);
    }
  }

  async function decideRequest(requestId: string, decision: "once" | "session" | "deny") {
    try {
      if (decision === "deny") await continuum.denyPermissionRequest(requestId);
      else await continuum.approvePermissionRequest(requestId, decision);
      await refreshMcpTools();
    } catch (cause) {
      setMcpError(`Permission decision failed: ${cause}`);
    }
  }

  async function revokeGrant(grantId: string) {
    try {
      await continuum.revokePermissionGrant(grantId);
      await refreshMcpTools();
    } catch (cause) {
      setMcpError(`Failed to revoke grant: ${cause}`);
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
        subtitle={`${mcpTools.length} tools across ${mcpByNamespace.length} namespaces — exposed to the orchestrator via continuum-mcp`}
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
        {permissionRequests.length > 0 && (
          <div className="mb-4 rounded-md border border-state-warn/40 bg-state-warn/10 p-3">
            <div className="mb-2 text-xs font-medium uppercase tracking-wide text-state-warn">
              Waiting for approval
            </div>
            <ul className="space-y-2">
              {permissionRequests.map((request) => (
                <li key={request.id} className="rounded border border-bg-border bg-bg-elevated p-2">
                  <div className="font-mono text-xs text-ink">{request.action}</div>
                  <div className="mt-0.5 text-xs text-ink-muted">{request.summary}</div>
                  {request.resource && (
                    <div className="mt-0.5 truncate font-mono text-[11px] text-ink-dim">
                      {request.resource}
                    </div>
                  )}
                  <div className="mt-2 flex gap-2">
                    <Button size="sm" onClick={() => decideRequest(request.id, "once")}>
                      Allow once
                    </Button>
                    {request.tier === "session-approved" && (
                      <Button
                        size="sm"
                        variant="default"
                        onClick={() => decideRequest(request.id, "session")}
                      >
                        Allow session
                      </Button>
                    )}
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() => decideRequest(request.id, "deny")}
                    >
                      Deny
                    </Button>
                  </div>
                </li>
              ))}
            </ul>
          </div>
        )}

        {permissionGrants.length > 0 && (
          <div className="mb-4 rounded-md border border-bg-border bg-bg-elevated p-3">
            <div className="mb-2 text-[11px] uppercase tracking-wide text-ink-dim">
              Active grants
            </div>
            <ul className="space-y-1">
              {permissionGrants.map((grant) => (
                <li key={grant.id} className="flex items-center justify-between gap-3 text-xs">
                  <span className="min-w-0 truncate font-mono text-ink-muted">
                    {grant.action} · {grant.scope}
                    {grant.resource ? ` · ${grant.resource}` : ""}
                  </span>
                  <Button size="sm" variant="ghost" onClick={() => revokeGrant(grant.id)}>
                    Revoke
                  </Button>
                </li>
              ))}
            </ul>
          </div>
        )}

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
    } catch (error) {
      setInstallError(formatError(error));
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
          software you trust, because its process runs with your Windows account access.
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
    return "An unknown error occurred while installing the server.";
  }
}

function McpNamespace({
  ns,
  permissions,
  onPermissionChange,
}: {
  ns: { namespace: string; tools: McpTool[] };
  permissions: Record<string, PermissionTier>;
  onPermissionChange: (name: string, value: PermissionTier) => void;
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
                onChange={(v) => onPermissionChange(tool.name, v as PermissionTier)}
                className="w-28"
                title="Saved immediately and enforced by continuum-mcp."
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
