"use client";

// Context page (Task C5, spec §4.13). Everything on this page is read from
// the Zustand store slice `state.context`, which the runtime bridge refills
// from the runtime's `state.json` roughly every two seconds. Every action is
// a fire-and-forget intent file drained by the runtime's main loop, so the
// UI never mutates the store optimistically — it reports "queued" and waits
// for the next publish.

import { useCallback, useEffect, useMemo, useState } from "react";
import { clsx } from "clsx";
import {
  AlertTriangle,
  Check,
  Eye,
  FileText,
  FolderPlus,
  GitBranch,
  Lock,
  Mic,
  Monitor,
  Pin,
  PinOff,
  Plus,
  ShieldAlert,
  Trash2,
} from "lucide-react";

import { useStore } from "@/lib/store";
import { continuum } from "@/lib/tauri";
import {
  Button,
  Card,
  EmptyState,
  Modal,
  Select,
  StatusBadge,
  TextInput,
  Toggle,
} from "@/components/ui/primitives";
import type {
  ComponentHealthSummary,
  ComponentStatus,
  ContextEngineSnapshot,
  ContextEventView,
  ContextIntentInput,
  ContinuationCandidateView,
  ObservationTogglesView,
  OverrideRuleView,
  ProjectStatus,
  ProjectSummaryView,
  SessionPinView,
  SessionState,
  StampedText,
} from "@/lib/types";

// --- Small shared helpers -------------------------------------------------

type CorrectField = "project" | "goal" | "task";
type ToggleName = keyof ObservationTogglesView;

const CORRECT_FIELDS: Array<{ value: CorrectField; label: string }> = [
  { value: "project", label: "Project" },
  { value: "goal", label: "Goal" },
  { value: "task", label: "Task" },
];

const PIN_FIELDS: readonly CorrectField[] = ["project", "goal", "task"];

const INTENT_LABELS: Record<ContextIntentInput["kind"], string> = {
  add_project: "Add project",
  confirm_project: "Confirm project",
  correct: "Correction",
  not_this_project: "Exclusion rule",
  pin: "Pin",
  forget: "Forget",
  delete_range: "Range deletion",
  set_toggle: "Observation toggle",
  set_runtime_service: "Runtime service",
};

/** Narrows a persisted pin's free-form `field` back to the intent union. */
function asCorrectField(field: string): CorrectField | null {
  return PIN_FIELDS.includes(field as CorrectField) ? (field as CorrectField) : null;
}

function formatTs(ts: string | null): string {
  if (!ts) return "—";
  const parsed = new Date(ts);
  return Number.isNaN(parsed.getTime()) ? ts : parsed.toLocaleString();
}

function formatClock(ts: string): string {
  const parsed = new Date(ts);
  return Number.isNaN(parsed.getTime()) ? ts : parsed.toLocaleTimeString();
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

// --- Intent plumbing ------------------------------------------------------

interface Notice {
  kind: "queued" | "error";
  message: string;
}

/** Writes context intents and surfaces a transient queued/rejected notice. */
function useContextIntent() {
  const [notice, setNotice] = useState<Notice | null>(null);
  const [sending, setSending] = useState(false);

  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(null), 6000);
    return () => window.clearTimeout(timer);
  }, [notice]);

  const send = useCallback(async (intent: ContextIntentInput): Promise<boolean> => {
    const label = INTENT_LABELS[intent.kind];
    setSending(true);
    try {
      await continuum.contextWriteIntent(intent);
      setNotice({
        kind: "queued",
        message: `${label} queued — the runtime applies this within a second.`,
      });
      return true;
    } catch (error) {
      setNotice({ kind: "error", message: `${label} was rejected: ${errorText(error)}` });
      return false;
    } finally {
      setSending(false);
    }
  }, []);

  const dismiss = useCallback(() => setNotice(null), []);

  return { notice, sending, send, dismiss };
}

// --- Presentational primitives -------------------------------------------

function Field({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div>
      <div className="continuum-label">{label}</div>
      <div className={clsx("text-sm text-ink", mono && "font-mono text-xs")}>{value}</div>
    </div>
  );
}

function ConfidenceMeter({ label, value }: { label: string; value: number }) {
  const pct = Math.max(0, Math.min(100, Math.round((Number.isFinite(value) ? value : 0) * 100)));
  return (
    <div>
      <div className="mb-1.5 flex items-center justify-between text-[11px] text-ink-muted">
        <span>{label}</span>
        <span className="font-mono tabular-nums text-ink">{pct}%</span>
      </div>
      <div
        role="meter"
        aria-label={label}
        aria-valuenow={pct}
        aria-valuemin={0}
        aria-valuemax={100}
        className="h-1.5 w-full overflow-hidden rounded-full border border-bg-border bg-bg-elevated"
      >
        <div
          className={clsx(
            "h-full rounded-full transition-[width] duration-300",
            pct >= 60 ? "bg-state-healthy" : pct >= 30 ? "bg-state-warn" : "bg-state-idle"
          )}
          style={{ width: `${pct}%` }}
        />
      </div>
    </div>
  );
}

function PrivateBadge() {
  return (
    <span className="inline-flex items-center gap-1.5 rounded-md border border-accent-blue/30 bg-accent-blue/15 px-2 py-0.5 text-[11px] font-medium text-accent-blue">
      <Lock size={11} /> Private context
    </span>
  );
}

function StampedField({
  label,
  stamped,
  tone,
  emptyText,
}: {
  label: string;
  stamped: StampedText | null;
  tone: "error" | "healthy" | "neutral";
  emptyText: string;
}) {
  return (
    <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
      <div className="continuum-label">{label}</div>
      {stamped ? (
        <>
          <div
            className={clsx(
              "text-sm leading-5",
              tone === "error" && "text-state-error",
              tone === "healthy" && "text-state-healthy",
              tone === "neutral" && "text-ink"
            )}
          >
            {stamped.text}
          </div>
          <div className="mt-1 text-[11px] text-ink-dim">{formatTs(stamped.at)}</div>
        </>
      ) : (
        <div className="text-sm text-ink-dim">{emptyText}</div>
      )}
    </div>
  );
}

// --- 1. Session state -----------------------------------------------------

/** Inferred fields hide behind a privacy/confidence gate before rendering. */
function inferredValue(session: SessionState, raw: string | null): string {
  if (session.local_only) return "working in a private context";
  if (!raw || session.confidence <= 0) return "unknown";
  return raw;
}

function activityLine(session: SessionState): string {
  const app = session.active_app?.trim() || "";
  const title = session.window_title?.trim() || "";
  if (app && title) return `${app} — ${title}`;
  return app || title || "unknown";
}

/** Plain-English statement of where the current belief actually comes from. */
function beliefLine(session: SessionState): string {
  const parts: string[] = [];

  if (session.active_project) {
    parts.push(
      session.active_app
        ? `The project is resolved from the active window (${session.active_app})`
        : "The project is resolved from the active window"
    );
  } else {
    parts.push("No project could be resolved from the active window");
  }

  if (session.local_only) {
    parts.push(
      "the window falls in a private zone, so the goal and task are kept local and are not described here"
    );
  } else if (session.confidence <= 0 || (!session.current_goal && !session.current_task)) {
    parts.push("the local model has not inferred a goal or task yet, so both read as unknown");
  } else {
    parts.push(
      `the goal and task are inferred by the local model at ${Math.round(session.confidence * 100)}% confidence`
    );
  }

  return `${parts.join("; ")}.`;
}

/**
 * Appends the pin/correction state to a field label (spec §4.13). A pinned
 * field is frozen at what the user asserted; a corrected one moved once
 * because the user said so but is still free to move again.
 */
function fieldLabel(label: string, session: SessionState, field: string): string {
  if (session.pinned?.includes(field)) return `${label} · pinned`;
  if (session.user_confirmed?.includes(field)) return `${label} · you told me this`;
  return label;
}

function SessionPanel({ session }: { session: SessionState | null }) {
  if (!session) {
    return (
      <Card title="Session state" subtitle="What Continuum believes you are working on">
        <EmptyState
          title="Nothing published yet"
          description="The background runtime publishes session state roughly every two seconds. Once it is running, the active project, goal, task and current activity appear here."
        />
      </Card>
    );
  }

  return (
    <Card
      title="Session state"
      subtitle={`Tracking since ${formatTs(session.since)} · last updated ${formatTs(session.updated)}`}
      actions={session.local_only ? <PrivateBadge /> : undefined}
    >
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
        <div className="space-y-3">
          <Field
            label={fieldLabel("Active project", session, "project")}
            value={session.active_project ?? "unknown"}
          />
          <Field
            label={fieldLabel("Current goal", session, "goal")}
            value={inferredValue(session, session.current_goal)}
          />
          <Field
            label={fieldLabel("Current task", session, "task")}
            value={inferredValue(session, session.current_task)}
          />
          <Field label="Activity" value={activityLine(session)} />
        </div>
        <div className="space-y-4">
          <ConfidenceMeter label="Goal confidence" value={session.confidence} />
          <ConfidenceMeter label="Task confidence" value={session.confidence} />
          <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
            <div className="continuum-label">Source of belief</div>
            <p className="mt-1 text-[13px] leading-5 text-ink-muted">{beliefLine(session)}</p>
          </div>
        </div>
      </div>

      <div className="mt-4 grid grid-cols-1 gap-3 md:grid-cols-2">
        <StampedField
          label="Blocker (last error)"
          stamped={session.last_error}
          tone="error"
          emptyText="Nothing is blocking you right now."
        />
        <StampedField
          label="Last success"
          stamped={session.last_success}
          tone="healthy"
          emptyText="No success recorded yet."
        />
      </div>

      <div className="mt-3">
        <StampedField
          label="Last user command"
          stamped={session.last_user_command}
          tone="neutral"
          emptyText="No spoken or typed command recorded yet."
        />
      </div>

      {session.open_files.length > 0 && (
        <div className="mt-3 rounded-md border border-bg-border bg-bg-elevated p-3">
          <div className="continuum-label">Open files</div>
          <ul className="mt-1 space-y-0.5">
            {session.open_files.map((path) => (
              <li key={path} className="truncate font-mono text-xs text-ink-muted" title={path}>
                {path}
              </li>
            ))}
          </ul>
        </div>
      )}
    </Card>
  );
}

// --- 2. Per-source health + privacy toggles -------------------------------

type SourceKey = keyof Omit<ContextEngineSnapshot, "idle">;

const SOURCE_ROWS: Array<{ key: SourceKey; label: string; description: string }> = [
  {
    key: "context_watcher",
    label: "Window & context watcher",
    description: "Foreground process, window title and monitor geometry.",
  },
  {
    key: "live_context",
    label: "Screen capture & captions",
    description: "Local vision captions for meaningful screen changes.",
  },
  {
    key: "git_watcher",
    label: "Git activity",
    description: "Branch, dirty state and commits for the active confirmed project.",
  },
  {
    key: "file_watcher",
    label: "File activity",
    description: "File changes under confirmed project roots. Opt-in, off by default.",
  },
  {
    key: "process_watcher",
    label: "Background activity",
    description: "Meaningful process starts, stops and sustained CPU or memory pressure. Opt-in.",
  },
  {
    key: "events_writer",
    label: "Event log writer",
    description: "Writes the deduplicated context event log to the local database.",
  },
  {
    key: "triage",
    label: "Triage evaluation",
    description: "The local model that decides which frames deserve attention.",
  },
];

const TOGGLE_ROWS: Array<{
  name: Exclude<ToggleName, "pause_all">;
  label: string;
  description: string;
  icon: typeof Mic;
}> = [
  {
    name: "mic",
    label: "Microphone",
    description: "Local transcription of what is said near the machine.",
    icon: Mic,
  },
  {
    name: "screen",
    label: "Screen",
    description: "Screen captures and the local captions derived from them.",
    icon: Monitor,
  },
  {
    name: "files",
    label: "Files",
    description: "File create/modify/delete events under confirmed project roots.",
    icon: FileText,
  },
  {
    name: "git",
    label: "Git",
    description: "Commits, branch switches and working-tree state.",
    icon: GitBranch,
  },
];

interface SourceStatusView {
  status: ComponentStatus;
  label: string;
  alarm: boolean;
}

/** `healthy: true, enabled: false` is a healthy state — only `should_restart`
 *  is an alarm. Everything else is informational. */
function sourceStatus(summary: ComponentHealthSummary | null): SourceStatusView {
  if (!summary) return { status: "unknown", label: "No report", alarm: false };
  if (summary.should_restart) return { status: "error", label: "Needs restart", alarm: true };
  if (!summary.healthy) return { status: "degrading", label: "Degraded", alarm: false };
  if (!summary.enabled) return { status: "unknown", label: "Off", alarm: false };
  return { status: "healthy", label: "Running", alarm: false };
}

function SourceHealthRow({
  label,
  description,
  summary,
}: {
  label: string;
  description: string;
  summary: ComponentHealthSummary | null;
}) {
  const view = sourceStatus(summary);
  return (
    <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-sm font-medium text-ink">{label}</div>
          <div className="mt-0.5 text-xs leading-4 text-ink-dim">{description}</div>
        </div>
        <StatusBadge status={view.status} label={view.label} />
      </div>
      {summary?.detail && (
        <div
          className={clsx(
            "mt-2 text-xs leading-4",
            view.alarm ? "text-state-error" : "text-ink-muted"
          )}
        >
          {view.alarm ? "" : "Reason: "}
          {summary.detail}
        </div>
      )}
      {!summary && (
        <div className="mt-2 text-xs text-ink-dim">
          The runtime has not published a health report for this source yet.
        </div>
      )}
    </div>
  );
}

function SourcesPanel({
  engine,
  toggles,
  sending,
  onToggle,
}: {
  engine: ContextEngineSnapshot | null;
  toggles: ObservationTogglesView | null;
  sending: boolean;
  onToggle: (name: ToggleName, value: boolean) => void;
}) {
  return (
    <Card
      title="Sources & privacy"
      subtitle="What each collector is doing, and what you allow it to observe"
      actions={
        engine ? (
          <StatusBadge
            status={engine.idle ? "unknown" : "healthy"}
            label={engine.idle ? "Idle" : "Active"}
          />
        ) : undefined
      }
    >
      {engine?.idle && (
        <div className="mb-4 rounded-md border border-bg-border bg-bg-elevated p-3 text-xs leading-5 text-ink-muted">
          The engine is idle: with no recent input, capture and captioning slow down. Error
          detection stays alive, and normal cadence returns as soon as you touch the machine, speak
          a wake word, or press the hotkey.
        </div>
      )}

      {toggles?.pause_all && (
        <div className="mb-4 flex items-start gap-2 rounded-md border border-state-warn/40 bg-state-warn/10 p-3 text-xs leading-5 text-state-warn">
          <ShieldAlert size={14} className="mt-0.5 shrink-0" />
          <span>
            Everything is paused. No source is observing anything right now, whatever the individual
            switches below say. Turn off "Pause everything" to resume the sources you have enabled.
          </span>
        </div>
      )}

      <div className="grid grid-cols-1 gap-2 md:grid-cols-2">
        {SOURCE_ROWS.map((row) => (
          <SourceHealthRow
            key={row.key}
            label={row.label}
            description={row.description}
            summary={engine ? engine[row.key] : null}
          />
        ))}
      </div>

      <div className="mt-5">
        <div className="continuum-label">Observation toggles</div>
        {toggles ? (
          <>
            <div className="mt-2 grid grid-cols-1 gap-2 md:grid-cols-2">
              {TOGGLE_ROWS.map(({ name, label, description, icon: Icon }) => (
                <div
                  key={name}
                  className="flex items-start justify-between gap-3 rounded-md border border-bg-border bg-bg-elevated p-3"
                >
                  <div className="min-w-0">
                    <div className="flex items-center gap-2 text-sm font-medium text-ink">
                      <Icon size={14} className="text-accent-amber" /> {label}
                    </div>
                    <div className="mt-0.5 text-xs leading-4 text-ink-dim">{description}</div>
                  </div>
                  <Toggle
                    checked={toggles[name]}
                    disabled={sending}
                    onChange={(next) => onToggle(name, next)}
                  />
                </div>
              ))}
            </div>
            <div className="mt-2 flex items-start justify-between gap-3 rounded-md border border-bg-border bg-bg-elevated p-3">
              <div className="min-w-0">
                <div className="text-sm font-medium text-ink">Pause everything</div>
                <div className="mt-0.5 text-xs leading-4 text-ink-dim">
                  A master switch over the frame loop. The individual switches keep their own
                  values, so nothing is silently re-enabled when you unpause.
                </div>
              </div>
              <Toggle
                checked={toggles.pause_all}
                disabled={sending}
                onChange={(next) => onToggle("pause_all", next)}
              />
            </div>
          </>
        ) : (
          <div className="mt-2 rounded-md border border-bg-border bg-bg-elevated p-3 text-xs text-ink-dim">
            The runtime has not published the live toggle values yet, so they cannot be shown or
            changed from here.
          </div>
        )}
      </div>

      <p className="mt-4 border-t border-bg-border pt-3 text-[11px] leading-5 text-ink-dim">
        Known limitation: Zones are matched on the foreground window plus a per-monitor
        visible-window sweep. A private window that is fully hidden behind others on a monitor is
        not seen by the sweep — exclude the whole monitor if you need a hard guarantee.
      </p>
    </Card>
  );
}

// --- 3. Recent events strip ----------------------------------------------

function EventsPanel({
  events,
  sending,
  onForget,
}: {
  events: ContextEventView[];
  sending: boolean;
  onForget: (event: ContextEventView) => Promise<boolean>;
}) {
  const [pending, setPending] = useState<ContextEventView | null>(null);

  const ordered = useMemo(
    () => [...events].sort((a, b) => new Date(b.ts).getTime() - new Date(a.ts).getTime()),
    [events]
  );

  const confirmForget = useCallback(async () => {
    if (!pending) return;
    const ok = await onForget(pending);
    if (ok) setPending(null);
  }, [onForget, pending]);

  return (
    <Card title="Recent events" subtitle="Deduplicated, newest first — already privacy filtered">
      {ordered.length === 0 ? (
        <EmptyState
          title="No events recorded"
          description="Continuum logs one row per meaningful change: a window switch, a commit, a failing build. Repeats collapse into a single row with a count."
        />
      ) : (
        <ul className="space-y-1.5">
          {ordered.map((event) => (
            <li
              key={event.id}
              className="flex items-start gap-3 rounded-md border border-bg-border bg-bg-elevated p-2.5"
            >
              <span className="w-16 shrink-0 pt-0.5 font-mono text-[11px] tabular-nums text-ink-dim">
                {formatClock(event.ts)}
              </span>
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-1.5">
                  <span className="rounded border border-bg-border px-1.5 py-px text-[10px] uppercase tracking-wider text-ink-dim">
                    {event.source}
                  </span>
                  <span className="text-xs font-medium text-ink">
                    {event.event_type.replaceAll("_", " ")}
                  </span>
                  {event.application && (
                    <span className="truncate text-xs text-ink-muted">· {event.application}</span>
                  )}
                  {event.count > 1 && (
                    <span className="rounded border border-accent-amber/40 bg-accent-amber/15 px-1.5 py-px text-[10px] font-semibold text-accent-amber">
                      ×{event.count}
                    </span>
                  )}
                </div>
                <div className="mt-0.5 text-[13px] leading-5 text-ink-muted">{event.summary}</div>
              </div>
              <Button
                size="sm"
                variant="ghost"
                disabled={sending}
                onClick={() => setPending(event)}
                aria-label={`Forget event ${event.id}`}
              >
                <Trash2 size={12} /> Forget
              </Button>
            </li>
          ))}
        </ul>
      )}

      <Modal
        open={pending !== null}
        onClose={() => setPending(null)}
        title="Forget this event"
        footer={
          <>
            <Button variant="ghost" onClick={() => setPending(null)}>
              Cancel
            </Button>
            <Button variant="danger" disabled={sending} onClick={() => void confirmForget()}>
              <Trash2 size={13} /> Forget it
            </Button>
          </>
        }
      >
        {pending && (
          <div className="space-y-3 text-sm">
            <p className="text-ink-muted">
              Forgetting cascades. Continuum deletes the event row and its search-index entry, the
              perception frame it points to together with that frame's screenshot file, any episodic
              memories derived from that frame, and any unconfirmed memory candidate created from
              it.
            </p>
            <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
              <div className="continuum-label">Event</div>
              <div className="text-[13px] leading-5 text-ink">{pending.summary}</div>
              <div className="mt-1 text-[11px] text-ink-dim">
                {pending.source} · {pending.event_type} · {formatTs(pending.ts)}
                {pending.count > 1 ? ` · ${pending.count} occurrences` : ""}
              </div>
              {!pending.raw_reference && (
                <div className="mt-2 text-[11px] text-state-warn">
                  This event has no raw reference, so only the event row itself is removed.
                </div>
              )}
            </div>
            <p className="text-xs text-state-error">This cannot be undone.</p>
          </div>
        )}
      </Modal>
    </Card>
  );
}

// --- 4/5. Projects, add-project form, empty state -------------------------

const STATUS_RANK: Record<ProjectStatus, number> = {
  configured: 0,
  confirmed: 0,
  discovered: 1,
};

const STATUS_LABEL: Record<ProjectStatus, string> = {
  configured: "From config",
  confirmed: "Confirmed",
  discovered: "Candidate",
};

function AddProjectForm({
  sending,
  onSubmit,
}: {
  sending: boolean;
  onSubmit: (name: string, rootPath: string) => Promise<boolean>;
}) {
  const [name, setName] = useState("");
  const [rootPath, setRootPath] = useState("");

  const canSubmit = name.trim().length > 0 && rootPath.trim().length > 0 && !sending;

  const submit = useCallback(async () => {
    if (name.trim().length === 0 || rootPath.trim().length === 0) return;
    const ok = await onSubmit(name.trim(), rootPath.trim());
    if (ok) {
      setName("");
      setRootPath("");
    }
  }, [name, onSubmit, rootPath]);

  return (
    <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
      <div className="continuum-label">Add a project</div>
      <div className="mt-2 grid grid-cols-1 gap-2 md:grid-cols-[1fr_2fr_auto]">
        <label className="block">
          <span className="mb-1.5 block text-xs text-ink-muted">Name</span>
          <TextInput value={name} onChange={setName} placeholder="e.g. Continuum" />
        </label>
        <label className="block">
          <span className="mb-1.5 block text-xs text-ink-muted">Folder path</span>
          <TextInput
            value={rootPath}
            onChange={setRootPath}
            placeholder="e.g. D:\Continuum\Continuum-main"
          />
        </label>
        <div className="flex items-end">
          <Button variant="primary" disabled={!canSubmit} onClick={() => void submit()}>
            <FolderPlus size={13} /> Add project
          </Button>
        </div>
      </div>
    </div>
  );
}

function ProjectRow({
  project,
  sending,
  onConfirm,
}: {
  project: ProjectSummaryView;
  sending: boolean;
  onConfirm: (project: ProjectSummaryView) => void;
}) {
  const discovered = project.status === "discovered";
  return (
    <li
      className={clsx(
        "rounded-md border p-3",
        project.active ? "border-accent-amber/50 bg-accent-amber/[.06]" : "border-bg-border",
        !project.active && "bg-bg-elevated"
      )}
    >
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-sm font-medium text-ink">{project.name}</span>
            {project.active && (
              <span className="rounded border border-accent-amber/40 bg-accent-amber/15 px-1.5 py-px text-[10px] font-semibold uppercase tracking-wider text-accent-amber">
                Active
              </span>
            )}
            <span
              className={clsx(
                "rounded border px-1.5 py-px text-[10px] uppercase tracking-wider",
                discovered
                  ? "border-state-warn/40 bg-state-warn/10 text-state-warn"
                  : "border-bg-border text-ink-dim"
              )}
            >
              {STATUS_LABEL[project.status]}
            </span>
          </div>
          <div className="mt-1 text-[11px] text-ink-dim">
            id <span className="font-mono">{project.id}</span> · {project.frames_count} frame
            {project.frames_count === 1 ? "" : "s"} · last active {formatTs(project.last_active)}
          </div>
          {project.root_paths.length > 0 && (
            <ul className="mt-1 space-y-0.5">
              {project.root_paths.map((path) => (
                <li
                  key={path}
                  className="truncate font-mono text-[11px] text-ink-muted"
                  title={path}
                >
                  {path}
                </li>
              ))}
            </ul>
          )}
        </div>
        {discovered && (
          <Button size="sm" variant="primary" disabled={sending} onClick={() => onConfirm(project)}>
            <Check size={13} /> Confirm
          </Button>
        )}
      </div>
      {discovered && (
        <p className="mt-2 border-t border-bg-border pt-2 text-[11px] leading-5 text-ink-dim">
          Continuum spotted this folder in a window title but collects nothing from it: no git
          polling, no file watching, no per-project statistics. Confirm it to start.
        </p>
      )}
    </li>
  );
}

function ProjectsPanel({
  projects,
  sending,
  onConfirm,
  onAddProject,
}: {
  projects: ProjectSummaryView[];
  sending: boolean;
  onConfirm: (project: ProjectSummaryView) => void;
  onAddProject: (name: string, rootPath: string) => Promise<boolean>;
}) {
  const [addOpen, setAddOpen] = useState(false);

  const ordered = useMemo(
    () =>
      [...projects].sort((a, b) => {
        if (a.active !== b.active) return a.active ? -1 : 1;
        const rank = STATUS_RANK[a.status] - STATUS_RANK[b.status];
        if (rank !== 0) return rank;
        return a.name.localeCompare(b.name);
      }),
    [projects]
  );

  return (
    <Card
      title="Projects"
      subtitle="What Continuum is allowed to attribute your work to"
      actions={
        <Button size="sm" onClick={() => setAddOpen((open) => !open)}>
          <Plus size={13} /> {addOpen ? "Close" : "Add project"}
        </Button>
      }
    >
      {addOpen && (
        <div className="mb-3">
          <AddProjectForm sending={sending} onSubmit={onAddProject} />
        </div>
      )}
      <ul className="space-y-2">
        {ordered.map((project) => (
          <ProjectRow key={project.id} project={project} sending={sending} onConfirm={onConfirm} />
        ))}
      </ul>
    </Card>
  );
}

function ContextEmptyState({
  runtimeOffline,
  sending,
  onAddProject,
}: {
  runtimeOffline: boolean;
  sending: boolean;
  onAddProject: (name: string, rootPath: string) => Promise<boolean>;
}) {
  return (
    <Card title="No projects yet" subtitle="Nothing is being attributed to a project">
      <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
        <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
          <div className="flex items-center gap-2 text-sm font-medium text-ink">
            <Eye size={14} className="text-accent-amber" /> What Continuum observes
          </div>
          <ul className="mt-2 space-y-1 text-[13px] leading-5 text-ink-muted">
            <li>• One-sentence captions of your screen, produced by a local vision model.</li>
            <li>• The foreground application and its window title.</li>
            <li>• Git and file activity — only inside project roots you have confirmed.</li>
            <li>• Speech near the machine, transcribed locally, when the microphone is on.</li>
          </ul>
        </div>
        <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
          <div className="flex items-center gap-2 text-sm font-medium text-ink">
            <Lock size={14} className="text-accent-blue" /> What it never does
          </div>
          <ul className="mt-2 space-y-1 text-[13px] leading-5 text-ink-muted">
            <li>• Never captures keystrokes. There is no keylogger, in any mode.</li>
            <li>• Never collects from unconfirmed folders or projects you have not added.</li>
            <li>• Never sends anything from a private zone to a cloud model.</li>
            <li>
              • Everything stays local first: captures, captions and events live on this machine.
            </li>
          </ul>
        </div>
      </div>

      <p className="mt-4 text-[13px] leading-5 text-ink-muted">
        Add the folder you are working in and Continuum can start attributing events to it.
        {runtimeOffline
          ? " The runtime is not running, so this is written to disk and picked up the next time it starts."
          : ""}
      </p>
      <div className="mt-3">
        <AddProjectForm sending={sending} onSubmit={onAddProject} />
      </div>
    </Card>
  );
}

// --- 6. Correction controls ----------------------------------------------

function CorrectForm({
  sending,
  onSubmit,
}: {
  sending: boolean;
  onSubmit: (intent: ContextIntentInput) => Promise<boolean>;
}) {
  const [field, setField] = useState<CorrectField>("project");
  const [value, setValue] = useState("");
  const [matchProcess, setMatchProcess] = useState("");
  const [matchTitle, setMatchTitle] = useState("");

  const submit = useCallback(async () => {
    const trimmed = value.trim();
    if (trimmed.length === 0) return;
    const ok = await onSubmit({
      kind: "correct",
      field,
      value: trimmed,
      match_process: field === "project" ? matchProcess.trim() || null : null,
      match_title_substring: field === "project" ? matchTitle.trim() || null : null,
    });
    if (ok) {
      setValue("");
      setMatchProcess("");
      setMatchTitle("");
    }
  }, [field, matchProcess, matchTitle, onSubmit, value]);

  return (
    <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
      <div className="continuum-label">Correct project, goal or task</div>
      <div className="mt-2 grid grid-cols-1 gap-2 md:grid-cols-[8rem_1fr_auto]">
        <Select label="Field" value={field} options={CORRECT_FIELDS} onChange={setField} />
        <label className="block">
          <span className="mb-1.5 block text-xs text-ink-muted">Correct value</span>
          <TextInput
            value={value}
            onChange={setValue}
            placeholder={field === "project" ? "e.g. continuum" : "What you are actually doing"}
          />
        </label>
        <div className="flex items-end">
          <Button
            variant="primary"
            disabled={sending || !value.trim()}
            onClick={() => void submit()}
          >
            Apply correction
          </Button>
        </div>
      </div>

      {field === "project" && (
        <>
          <div className="mt-2 grid grid-cols-1 gap-2 md:grid-cols-2">
            <label className="block">
              <span className="mb-1.5 block text-xs text-ink-muted">
                Scope to process (optional)
              </span>
              <TextInput
                value={matchProcess}
                onChange={setMatchProcess}
                placeholder="e.g. Code.exe"
              />
            </label>
            <label className="block">
              <span className="mb-1.5 block text-xs text-ink-muted">
                Scope to window-title text (optional)
              </span>
              <TextInput
                value={matchTitle}
                onChange={setMatchTitle}
                placeholder="e.g. Continuum-main"
              />
            </label>
          </div>
          <p className="mt-2 text-[11px] leading-5 text-ink-dim">
            Correcting the project also writes a permanent override rule, so the same window
            resolves the same way next time. Leaving both scope fields empty makes the rule as broad
            as the current window allows — filling one in keeps it narrow.
          </p>
        </>
      )}
    </div>
  );
}

function NotThisProjectForm({
  projects,
  sending,
  onSubmit,
}: {
  projects: ProjectSummaryView[];
  sending: boolean;
  onSubmit: (intent: ContextIntentInput) => Promise<boolean>;
}) {
  const [projectId, setProjectId] = useState("");
  const [matchProcess, setMatchProcess] = useState("");
  const [matchTitle, setMatchTitle] = useState("");

  const options = useMemo(
    () => projects.map((p) => ({ value: p.id, label: `${p.name} (${p.id})` })),
    [projects]
  );
  const selected = projectId || options[0]?.value || "";

  const submit = useCallback(async () => {
    if (!selected) return;
    const ok = await onSubmit({
      kind: "not_this_project",
      project_id: selected,
      match_process: matchProcess.trim() || null,
      match_title_substring: matchTitle.trim() || null,
    });
    if (ok) {
      setMatchProcess("");
      setMatchTitle("");
    }
  }, [matchProcess, matchTitle, onSubmit, selected]);

  if (options.length === 0) {
    return (
      <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
        <div className="continuum-label">Not this project</div>
        <p className="text-xs text-ink-dim">Add at least one project before you can exclude one.</p>
      </div>
    );
  }

  return (
    <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
      <div className="continuum-label">Not this project</div>
      <div className="mt-2 grid grid-cols-1 gap-2 md:grid-cols-3">
        <Select label="Project" value={selected} options={options} onChange={setProjectId} />
        <label className="block">
          <span className="mb-1.5 block text-xs text-ink-muted">Scope to process (optional)</span>
          <TextInput
            value={matchProcess}
            onChange={setMatchProcess}
            placeholder="e.g. chrome.exe"
          />
        </label>
        <label className="block">
          <span className="mb-1.5 block text-xs text-ink-muted">
            Scope to window-title text (optional)
          </span>
          <TextInput value={matchTitle} onChange={setMatchTitle} placeholder="e.g. Jira" />
        </label>
      </div>
      <div className="mt-2 flex items-center justify-between gap-3">
        <p className="text-[11px] leading-5 text-ink-dim">
          Writes a permanent exclusion rule: matching windows will never resolve to this project
          again.
        </p>
        <Button disabled={sending} onClick={() => void submit()}>
          Exclude
        </Button>
      </div>
    </div>
  );
}

function PinForm({
  sending,
  onSubmit,
}: {
  sending: boolean;
  onSubmit: (intent: ContextIntentInput) => Promise<boolean>;
}) {
  const [field, setField] = useState<CorrectField>("project");
  const [value, setValue] = useState("");

  const submit = useCallback(async () => {
    const trimmed = value.trim();
    if (trimmed.length === 0) return;
    const ok = await onSubmit({ kind: "pin", field, value: trimmed });
    if (ok) setValue("");
  }, [field, onSubmit, value]);

  return (
    <div className="rounded-md border border-bg-border bg-bg-elevated p-3">
      <div className="continuum-label">Pin a field</div>
      <div className="mt-2 grid grid-cols-1 gap-2 md:grid-cols-[8rem_1fr_auto]">
        <Select label="Field" value={field} options={CORRECT_FIELDS} onChange={setField} />
        <label className="block">
          <span className="mb-1.5 block text-xs text-ink-muted">Pinned value</span>
          <TextInput value={value} onChange={setValue} placeholder="Value to hold in place" />
        </label>
        <div className="flex items-end">
          <Button disabled={sending || !value.trim()} onClick={() => void submit()}>
            <Pin size={13} /> Pin
          </Button>
        </div>
      </div>
      <p className="mt-2 text-[11px] leading-5 text-ink-dim">
        A pin stops inference from overwriting that session-state field. It never blocks resolution:
        Continuum keeps working out which project a window belongs to, it just stops rewriting what
        you pinned.
      </p>
    </div>
  );
}

function DeleteRangeForm({
  sending,
  onSubmit,
}: {
  sending: boolean;
  onSubmit: (intent: ContextIntentInput) => Promise<boolean>;
}) {
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [confirmOpen, setConfirmOpen] = useState(false);

  const fromMs = from ? new Date(from).getTime() : Number.NaN;
  const toMs = to ? new Date(to).getTime() : Number.NaN;
  const invalid =
    !from || !to || Number.isNaN(fromMs) || Number.isNaN(toMs)
      ? "range"
      : fromMs >= toMs
        ? "order"
        : null;

  const submit = useCallback(async () => {
    if (Number.isNaN(fromMs) || Number.isNaN(toMs) || fromMs >= toMs) return;
    const ok = await onSubmit({
      kind: "delete_range",
      from: new Date(fromMs).toISOString(),
      to: new Date(toMs).toISOString(),
    });
    if (ok) {
      setFrom("");
      setTo("");
      setConfirmOpen(false);
    }
  }, [fromMs, onSubmit, toMs]);

  return (
    <div className="rounded-md border border-state-error/30 bg-state-error/[.06] p-3">
      <div className="continuum-label">Delete a time range</div>
      <div className="mt-2 grid grid-cols-1 gap-2 md:grid-cols-[1fr_1fr_auto]">
        <label className="block">
          <span className="mb-1.5 block text-xs text-ink-muted">From</span>
          <TextInput type="datetime-local" value={from} onChange={setFrom} />
        </label>
        <label className="block">
          <span className="mb-1.5 block text-xs text-ink-muted">To</span>
          <TextInput type="datetime-local" value={to} onChange={setTo} />
        </label>
        <div className="flex items-end">
          <Button
            variant="danger"
            disabled={sending || invalid !== null}
            onClick={() => setConfirmOpen(true)}
          >
            <Trash2 size={13} /> Delete range
          </Button>
        </div>
      </div>
      {invalid === "order" && (
        <p className="mt-2 text-xs text-state-error">
          The end of the range must be later than its start.
        </p>
      )}
      {invalid === "range" && (
        <p className="mt-2 text-[11px] text-ink-dim">
          Pick both a start and an end before deleting.
        </p>
      )}

      <Modal
        open={confirmOpen}
        onClose={() => setConfirmOpen(false)}
        title="Delete everything in this range"
        footer={
          <>
            <Button variant="ghost" onClick={() => setConfirmOpen(false)}>
              Cancel
            </Button>
            <Button
              variant="danger"
              disabled={sending || invalid !== null}
              onClick={() => void submit()}
            >
              <Trash2 size={13} /> Delete permanently
            </Button>
          </>
        }
      >
        <div className="space-y-3 text-sm">
          <p className="text-ink-muted">
            Everything Continuum recorded between{" "}
            <span className="text-ink">{from ? new Date(from).toLocaleString() : "—"}</span> and{" "}
            <span className="text-ink">{to ? new Date(to).toLocaleString() : "—"}</span> is purged:
            perception frames and their screenshot files, context events, and the episodic memories
            derived from them inside that window.
          </p>
          <p className="text-xs text-state-error">This cannot be undone.</p>
        </div>
      </Modal>
    </div>
  );
}

function CorrectionsPanel({
  projects,
  sending,
  onSubmit,
}: {
  projects: ProjectSummaryView[];
  sending: boolean;
  onSubmit: (intent: ContextIntentInput) => Promise<boolean>;
}) {
  return (
    <Card
      title="Corrections"
      subtitle="Tell Continuum when it has the wrong idea — corrections persist across restarts"
    >
      <div className="space-y-3">
        <CorrectForm sending={sending} onSubmit={onSubmit} />
        <NotThisProjectForm projects={projects} sending={sending} onSubmit={onSubmit} />
        <PinForm sending={sending} onSubmit={onSubmit} />
        <DeleteRangeForm sending={sending} onSubmit={onSubmit} />
      </div>
    </Card>
  );
}

// --- 7. Overrides + pins --------------------------------------------------

function OverridesPanel({
  rules,
  pins,
  sending,
  onClearPin,
}: {
  rules: OverrideRuleView[];
  pins: SessionPinView[];
  sending: boolean;
  onClearPin: (field: CorrectField) => void;
}) {
  return (
    <Card title="Overrides & pins" subtitle="Everything you have corrected, still in force">
      <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
        <div>
          <div className="continuum-label">Resolver rules</div>
          {rules.length === 0 ? (
            <p className="mt-1 text-xs text-ink-dim">No override rules yet.</p>
          ) : (
            <ul className="mt-2 space-y-1.5">
              {rules.map((rule, index) => (
                <li
                  key={`${rule.action}-${rule.project_id}-${rule.match_process ?? ""}-${rule.match_title_substring ?? ""}-${index}`}
                  className="rounded-md border border-bg-border bg-bg-elevated p-2.5"
                >
                  <div className="flex items-center gap-2">
                    <span
                      className={clsx(
                        "rounded border px-1.5 py-px text-[10px] uppercase tracking-wider",
                        rule.action === "force_project"
                          ? "border-accent-amber/40 bg-accent-amber/15 text-accent-amber"
                          : "border-state-error/40 bg-state-error/10 text-state-error"
                      )}
                    >
                      {rule.action === "force_project" ? "Always" : "Never"}
                    </span>
                    <span className="truncate font-mono text-xs text-ink">{rule.project_id}</span>
                  </div>
                  <div className="mt-1 text-[11px] text-ink-dim">
                    {rule.match_process || rule.match_title_substring
                      ? [
                          rule.match_process ? `process ${rule.match_process}` : null,
                          rule.match_title_substring
                            ? `title contains "${rule.match_title_substring}"`
                            : null,
                        ]
                          .filter((part): part is string => part !== null)
                          .join(" · ")
                      : "matches any window"}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div>
          <div className="continuum-label">Pinned fields</div>
          {pins.length === 0 ? (
            <p className="mt-1 text-xs text-ink-dim">Nothing is pinned.</p>
          ) : (
            <ul className="mt-2 space-y-1.5">
              {pins.map((pin) => {
                const field = asCorrectField(pin.field);
                return (
                  <li
                    key={pin.field}
                    className="flex items-center justify-between gap-2 rounded-md border border-bg-border bg-bg-elevated p-2.5"
                  >
                    <div className="min-w-0">
                      <div className="flex items-center gap-1.5 text-xs font-medium capitalize text-ink">
                        <Pin size={12} className="text-accent-amber" />
                        {pin.field}
                      </div>
                      <div className="mt-0.5 truncate text-[13px] text-ink-muted">
                        {pin.value ?? "—"}
                      </div>
                    </div>
                    <Button
                      size="sm"
                      variant="ghost"
                      disabled={sending || field === null}
                      title={
                        field === null
                          ? "This pin's field is not one this page can clear"
                          : undefined
                      }
                      onClick={() => field && onClearPin(field)}
                    >
                      <PinOff size={12} /> Clear pin
                    </Button>
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      </div>

      <p className="mt-4 border-t border-bg-border pt-3 text-[11px] leading-5 text-ink-dim">
        Removing an override rule is not yet available from this page. Pins can be cleared here; to
        drop a rule, edit the projects table from the runtime side.
      </p>
    </Card>
  );
}

// --- 8. Continuation candidates ------------------------------------------

function ContinuationPanel({ candidates }: { candidates: ContinuationCandidateView[] }) {
  return (
    <Card
      title="Where you left off"
      subtitle='Ranked answers to "continue" — highest confidence first'
    >
      <ol className="space-y-1.5">
        {candidates.map((candidate, index) => (
          <li
            key={`${candidate.kind}-${index}`}
            className="flex items-start gap-3 rounded-md border border-bg-border bg-bg-elevated p-2.5"
          >
            <span className="w-5 shrink-0 pt-0.5 font-mono text-xs tabular-nums text-ink-dim">
              {index + 1}
            </span>
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <span className="rounded border border-bg-border px-1.5 py-px text-[10px] uppercase tracking-wider text-ink-dim">
                  {candidate.kind.replaceAll("_", " ")}
                </span>
                <span className="text-xs font-medium text-ink">{candidate.label}</span>
              </div>
              <div className="mt-0.5 text-[13px] leading-5 text-ink-muted">{candidate.text}</div>
            </div>
            <span className="shrink-0 font-mono text-xs tabular-nums text-ink-dim">
              {Math.round(candidate.confidence * 100)}%
            </span>
          </li>
        ))}
      </ol>
    </Card>
  );
}

// --- Tab ------------------------------------------------------------------

export function ContextTab() {
  const context = useStore((s) => s.state.context);
  const { notice, sending, send, dismiss } = useContextIntent();

  const runtimeOffline =
    context.session === null && context.engine === null && context.page === null;
  const page = context.page;
  const projects = useMemo(() => page?.projects ?? [], [page]);

  const handleToggle = useCallback(
    (name: ToggleName, value: boolean) => {
      void send({ kind: "set_toggle", name, value });
    },
    [send]
  );

  const handleForget = useCallback(
    (event: ContextEventView) =>
      send({ kind: "forget", event_id: event.id, raw_reference: event.raw_reference }),
    [send]
  );

  const handleConfirmProject = useCallback(
    (project: ProjectSummaryView) => {
      void send({ kind: "confirm_project", project_id: project.id });
    },
    [send]
  );

  const handleAddProject = useCallback(
    (name: string, rootPath: string) => send({ kind: "add_project", name, root_path: rootPath }),
    [send]
  );

  const handleClearPin = useCallback(
    (field: CorrectField) => {
      void send({ kind: "pin", field, value: null });
    },
    [send]
  );

  return (
    <div className="mx-auto max-w-6xl space-y-6">
      {notice && (
        <div
          role="status"
          className={clsx(
            "flex items-start justify-between gap-3 rounded-md border p-3 text-sm",
            notice.kind === "error"
              ? "border-state-error/40 bg-state-error/10 text-state-error"
              : "border-accent-amber/40 bg-accent-amber/10 text-ink"
          )}
        >
          <span className="flex items-start gap-2">
            {notice.kind === "error" ? (
              <AlertTriangle size={15} className="mt-0.5 shrink-0" />
            ) : (
              <Check size={15} className="mt-0.5 shrink-0 text-accent-amber" />
            )}
            {notice.message}
          </span>
          <button
            type="button"
            aria-label="Dismiss"
            onClick={dismiss}
            className="shrink-0 text-ink-dim hover:text-ink"
          >
            &times;
          </button>
        </div>
      )}

      {runtimeOffline && (
        <div className="flex items-start gap-2 rounded-md border border-state-warn/40 bg-state-warn/10 p-3 text-sm leading-5 text-state-warn">
          <AlertTriangle size={15} className="mt-0.5 shrink-0" />
          <span>
            The background runtime is not running, so nothing is being observed and this page has no
            live state to show. You can still add a project or queue a correction below — the
            request is written to disk and drained the next time the runtime starts.
          </span>
        </div>
      )}

      <SessionPanel session={context.session} />

      <SourcesPanel
        engine={context.engine}
        toggles={page?.toggles ?? null}
        sending={sending}
        onToggle={handleToggle}
      />

      {projects.length === 0 ? (
        <ContextEmptyState
          runtimeOffline={runtimeOffline}
          sending={sending}
          onAddProject={handleAddProject}
        />
      ) : (
        <ProjectsPanel
          projects={projects}
          sending={sending}
          onConfirm={handleConfirmProject}
          onAddProject={handleAddProject}
        />
      )}

      <EventsPanel events={page?.recent_events ?? []} sending={sending} onForget={handleForget} />

      <CorrectionsPanel projects={projects} sending={sending} onSubmit={send} />

      <OverridesPanel
        rules={page?.rules ?? []}
        pins={page?.pins ?? []}
        sending={sending}
        onClearPin={handleClearPin}
      />

      {(page?.continuation.length ?? 0) > 0 && (
        <ContinuationPanel candidates={page?.continuation ?? []} />
      )}
    </div>
  );
}
