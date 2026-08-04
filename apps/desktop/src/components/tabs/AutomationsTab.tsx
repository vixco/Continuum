"use client";

import { useEffect, useState } from "react";
import { CalendarPlus, Trash2 } from "lucide-react";

import { continuum } from "@/lib/tauri";
import {
  Button,
  Card,
  EmptyState,
  Modal,
  Select,
  TextInput,
  Toggle,
} from "@/components/ui/primitives";
import type { Automation, AutomationInput } from "@/lib/types";

export function AutomationsTab() {
  const [items, setItems] = useState<Automation[]>([]);
  const [showForm, setShowForm] = useState(false);
  const [draft, setDraft] = useState<AutomationInput>({
    task: "",
    kind: "recurring",
    schedule: "0 8 * * *",
    enabled: true,
  });
  const [editingId, setEditingId] = useState<string | null>(null);

  useEffect(() => {
    void refresh();
  }, []);

  async function refresh() {
    setItems(await continuum.listAutomations());
  }

  function startCreate() {
    setDraft({
      task: "",
      kind: "recurring",
      schedule: "0 8 * * *",
      enabled: true,
    });
    setEditingId(null);
    setShowForm(true);
  }

  function startEdit(a: Automation) {
    setDraft({
      task: a.task,
      kind: a.kind,
      schedule: a.schedule,
      enabled: a.enabled,
    });
    setEditingId(a.id);
    setShowForm(true);
  }

  async function submit() {
    if (editingId) {
      await continuum.updateAutomation(editingId, draft);
    } else {
      await continuum.createAutomation(draft);
    }
    setShowForm(false);
    await refresh();
  }

  return (
    <div className="mx-auto max-w-5xl space-y-6">
      <Card
        title="Scheduled tasks"
        subtitle="one-shot or recurring Continuum wakes"
        actions={
          <Button size="sm" variant="primary" onClick={startCreate}>
            <CalendarPlus size={12} /> New
          </Button>
        }
      >
        {items.length === 0 ? (
          <div className="space-y-4">
            <EmptyState
              title="No automations yet"
              description="Schedule a morning briefing, a weekly digest, or a reminder."
            />
            <div className="flex flex-wrap gap-2">
              {TEMPLATES.map((t) => (
                <button
                  key={t.task}
                  onClick={() => {
                    setDraft({
                      task: t.task,
                      kind: "recurring",
                      schedule: t.schedule,
                      enabled: true,
                    });
                    setEditingId(null);
                    setShowForm(true);
                  }}
                  className="press rounded-md border border-bg-border bg-bg-elevated px-3 py-1.5 text-xs text-ink transition-colors hover:border-amber-500/40 hover:bg-bg-hover"
                >
                  {t.label}
                </button>
              ))}
            </div>
          </div>
        ) : (
          <table className="w-full text-sm">
            <thead className="text-[11px] uppercase tracking-wider text-ink-dim">
              <tr>
                <th className="pb-2 text-left">Task</th>
                <th className="pb-2 text-left">Schedule</th>
                <th className="pb-2 text-left">Last run</th>
                <th className="pb-2">Enabled</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {items.map((a) => (
                <tr key={a.id} className="border-t border-bg-border">
                  <td className="py-2 pr-3 text-ink">{a.task}</td>
                  <td className="py-2 pr-3 font-mono text-xs text-ink-muted">{a.schedule}</td>
                  <td className="py-2 pr-3 text-xs text-ink-muted">
                    {a.last_run ? new Date(a.last_run).toLocaleString() : "never"}
                  </td>
                  <td className="py-2 text-center">
                    <Toggle
                      checked={a.enabled}
                      onChange={async (v) => {
                        await continuum.toggleAutomation(a.id, v);
                        await refresh();
                      }}
                    />
                  </td>
                  <td className="py-2 text-right">
                    <button
                      className="mr-2 text-xs text-ink-muted hover:text-ink"
                      onClick={() => startEdit(a)}
                    >
                      Edit
                    </button>
                    <button
                      className="text-state-error hover:opacity-80"
                      onClick={async () => {
                        await continuum.deleteAutomation(a.id);
                        await refresh();
                      }}
                    >
                      <Trash2 size={13} />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Card>

      <Modal
        open={showForm}
        onClose={() => setShowForm(false)}
        title={editingId ? "Edit automation" : "New automation"}
        footer={
          <>
            <Button variant="ghost" onClick={() => setShowForm(false)}>
              Cancel
            </Button>
            <Button variant="primary" onClick={submit} disabled={!draft.task}>
              Save
            </Button>
          </>
        }
      >
        <div className="space-y-3">
          <label className="block">
            <span className="mb-1 block text-xs text-ink-muted">Task</span>
            <TextInput
              value={draft.task}
              onChange={(v) => setDraft({ ...draft, task: v })}
              placeholder="e.g. Give me a morning briefing"
            />
          </label>
          <Select
            label="Kind"
            value={draft.kind}
            options={[
              { value: "recurring", label: "Recurring (cron)" },
              { value: "one_shot", label: "One-shot" },
            ]}
            onChange={(v) => setDraft({ ...draft, kind: v as typeof draft.kind })}
          />
          <label className="block">
            <span className="mb-1 block text-xs text-ink-muted">
              Schedule (cron or ISO datetime)
            </span>
            <TextInput
              value={draft.schedule}
              onChange={(v) => setDraft({ ...draft, schedule: v })}
              placeholder={draft.kind === "recurring" ? "0 8 * * *" : "2026-04-14T10:00:00Z"}
            />
          </label>
          <Toggle
            checked={draft.enabled}
            onChange={(v) => setDraft({ ...draft, enabled: v })}
            label="Enabled"
          />
        </div>
      </Modal>
    </div>
  );
}

const TEMPLATES: Array<{ label: string; task: string; schedule: string }> = [
  { label: "Morning briefing", task: "Give me a morning briefing", schedule: "0 8 * * *" },
  { label: "Weekly digest", task: "Summarize my week and plan next week", schedule: "0 18 * * 5" },
  { label: "Tidy Downloads", task: "Tidy my Downloads folder", schedule: "0 11 * * 6" },
];
