"use client";

// Full-screen memory note editor: create flow + full-detail edit flow.
// Deliberately not built on the `Modal` primitive - this is a dedicated
// full-screen surface (fixed inset-0 z-50), not a dialog over other content.
// See task-13-brief.md.

import { useCallback, useEffect, useRef, useState } from "react";
import { clsx } from "clsx";
import { Plus, Trash2 } from "lucide-react";
import ReactMarkdown from "react-markdown";

import { continuum } from "@/lib/tauri";
import { NODE_TYPE_LABELS } from "@/lib/memoryTheme";
import { Button, Select, Slider, TextInput, Toggle } from "@/components/ui/primitives";
import {
  MetaChip,
  STATUS_DOT_CLASSES,
  STATUS_LABELS,
  STATUS_OPTIONS,
} from "@/components/memory/NotePanel";
import type {
  MemoryNodeType,
  MemoryNodeStatus,
  MemoryNote,
  MemoryNoteDraft,
  MemoryRelation,
  MemorySensitivity,
  MemorySource,
} from "@/lib/types";

// Same handful-of-elements markdown styling as ChatTab/NotePanel - no
// @tailwindcss/typography plugin in this repo.
const MARKDOWN_CLASSES =
  "break-words text-sm leading-relaxed " +
  "[&_p:not(:last-child)]:mb-2 " +
  "[&_ul]:my-1 [&_ul]:list-disc [&_ul]:pl-5 [&_ol]:my-1 [&_ol]:list-decimal [&_ol]:pl-5 [&_li]:mb-0.5 " +
  "[&_a]:text-accent-purple [&_a]:underline [&_strong]:font-semibold " +
  "[&_pre]:overflow-x-auto [&_pre]:rounded [&_pre]:bg-black/40 [&_pre]:p-2 [&_code]:text-xs";

export type OverlayMode = { kind: "edit"; id: string } | { kind: "create"; draft: MemoryNoteDraft };

interface NoteEditorOverlayProps {
  mode: OverlayMode;
  onClose: () => void;
  onSaved: (id: string) => void;
}

/** Local, fully-editable working copy. Status is user-editable (mirrors
 * NotePanel's status Select, same `STATUS_OPTIONS`); source/sensitivity
 * are kept here for display only - matching NotePanel, which shows them as
 * read-only chips - and are not user-editable in this UI. */
interface EditorDraft {
  type: MemoryNodeType;
  title: string;
  body: string;
  status: MemoryNodeStatus;
  project: string;
  confidence: number;
  importance: number;
  source: MemorySource;
  sensitivity: MemorySensitivity;
  tags: string;
  relations: MemoryRelation[];
}

const TYPE_OPTIONS = (Object.entries(NODE_TYPE_LABELS) as [MemoryNodeType, string][]).map(
  ([value, label]) => ({ value, label })
);

function draftFromNoteDraft(d: MemoryNoteDraft): EditorDraft {
  return {
    type: d.type,
    title: d.title,
    body: d.body ?? "",
    status: d.status ?? "confirmed",
    project: d.project ?? "",
    confidence: d.confidence ?? 1,
    importance: d.importance ?? 0.5,
    source: d.source ?? "manual",
    sensitivity: d.sensitivity ?? "internal",
    tags: (d.tags ?? []).join(", "),
    relations: d.relations ?? [],
  };
}

function draftFromNote(n: MemoryNote): EditorDraft {
  return {
    type: n.frontmatter.type,
    title: n.frontmatter.title,
    body: n.body,
    status: n.frontmatter.status,
    project: n.frontmatter.project ?? "",
    confidence: n.frontmatter.confidence,
    importance: n.frontmatter.importance,
    source: n.frontmatter.source,
    sensitivity: n.frontmatter.sensitivity,
    tags: (n.frontmatter.tags ?? []).join(", "),
    relations: n.frontmatter.relations ?? [],
  };
}

function parseTags(input: string): string[] {
  return input
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean);
}

export function NoteEditorOverlay({ mode, onClose, onSaved }: NoteEditorOverlayProps) {
  // The fetched note in edit mode, kept aside so Save can carry forward
  // fields the editor never touches (id, path, slug, backlinks, created...).
  const originalNote = useRef<MemoryNote | null>(null);
  const [draft, setDraft] = useState<EditorDraft | null>(
    mode.kind === "create" ? draftFromNoteDraft(mode.draft) : null
  );
  const [loading, setLoading] = useState(mode.kind === "edit");
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [showPreview, setShowPreview] = useState(true);

  useEffect(() => {
    if (mode.kind === "create") {
      originalNote.current = null;
      setDraft(draftFromNoteDraft(mode.draft));
      setLoading(false);
      setLoadError(null);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setLoadError(null);
    void continuum
      .memoryGetNote(mode.id)
      .then((n) => {
        if (cancelled) return;
        originalNote.current = n;
        setDraft(draftFromNote(n));
      })
      .catch((e) => {
        if (cancelled) return;
        setLoadError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [mode]);

  const handleSave = useCallback(async () => {
    if (!draft) return;
    const title = draft.title.trim();
    if (!title) {
      setSaveError("Title is required.");
      return;
    }
    setSaving(true);
    setSaveError(null);
    try {
      if (mode.kind === "create") {
        const payload: MemoryNoteDraft = {
          type: draft.type,
          title,
          body: draft.body,
          status: draft.status,
          project: draft.project.trim() || null,
          confidence: draft.confidence,
          importance: draft.importance,
          source: draft.source,
          sensitivity: draft.sensitivity,
          tags: parseTags(draft.tags),
          relations: draft.relations,
        };
        const created = await continuum.memoryCreateNote(payload);
        onSaved(created.frontmatter.id);
      } else {
        const base = originalNote.current;
        if (!base) throw new Error("Note has not finished loading yet.");
        const updated: MemoryNote = {
          ...base,
          frontmatter: {
            ...base.frontmatter,
            type: draft.type,
            title,
            status: draft.status,
            project: draft.project.trim() || null,
            confidence: draft.confidence,
            importance: draft.importance,
            source: draft.source,
            sensitivity: draft.sensitivity,
            tags: parseTags(draft.tags),
            relations: draft.relations,
          },
          body: draft.body,
        };
        await continuum.memorySaveNote(updated);
        onSaved(updated.frontmatter.id);
      }
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }, [draft, mode, onSaved]);

  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
        e.preventDefault();
        void handleSave();
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose, handleSave]);

  function updateRelation(idx: number, patch: Partial<MemoryRelation>) {
    setDraft((d) =>
      d ? { ...d, relations: d.relations.map((r, i) => (i === idx ? { ...r, ...patch } : r)) } : d
    );
  }

  function addRelation() {
    setDraft((d) =>
      d ? { ...d, relations: [...d.relations, { to: "", rel: "related_to", confidence: 1 }] } : d
    );
  }

  function removeRelation(idx: number) {
    setDraft((d) => (d ? { ...d, relations: d.relations.filter((_, i) => i !== idx) } : d));
  }

  return (
    <div className="fixed inset-0 z-50 flex flex-col bg-bg">
      <div className="flex shrink-0 items-center gap-3 border-b border-bg-border px-4 py-3">
        <div className="min-w-0 flex-1">
          <TextInput
            value={draft?.title ?? ""}
            onChange={(v) => setDraft((d) => (d ? { ...d, title: v } : d))}
            placeholder="Title"
            disabled={!draft}
            className="text-sm font-medium"
          />
        </div>
        <div className="w-40 shrink-0">
          <Select
            value={draft?.type ?? "note"}
            options={TYPE_OPTIONS}
            onChange={(v) => setDraft((d) => (d ? { ...d, type: v } : d))}
            disabled={!draft}
          />
        </div>
        {draft && (
          <span className="flex shrink-0 items-center gap-1.5 rounded-md border border-bg-border bg-bg-elevated px-2 py-1 text-[11px] text-ink-muted">
            <span className={clsx("h-1.5 w-1.5 rounded-full", STATUS_DOT_CLASSES[draft.status])} />
            {STATUS_LABELS[draft.status]}
          </span>
        )}
        <div className="flex-1" />
        {saveError && <span className="text-xs text-state-error">{saveError}</span>}
        <Button variant="ghost" onClick={onClose}>
          Cancel
        </Button>
        <Button
          variant="primary"
          onClick={() => void handleSave()}
          disabled={saving || !draft || !draft.title.trim()}
        >
          {saving ? "Saving…" : "Save"}
        </Button>
      </div>

      {loading ? (
        <div className="flex flex-1 items-center justify-center text-sm text-ink-dim">Loading…</div>
      ) : loadError ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-3">
          <div className="text-sm text-state-error">{loadError}</div>
          <Button onClick={onClose}>Close</Button>
        </div>
      ) : draft ? (
        <div className="flex min-h-0 flex-1">
          <div className="flex min-h-0 flex-1 flex-col">
            <div className="flex shrink-0 items-center justify-between border-b border-bg-border px-4 py-2">
              <span className="text-xs text-ink-muted">Body (Markdown)</span>
              <Toggle checked={showPreview} onChange={setShowPreview} label="Live preview" />
            </div>
            <div className="flex min-h-0 flex-1">
              <textarea
                value={draft.body}
                onChange={(e) => setDraft((d) => (d ? { ...d, body: e.target.value } : d))}
                placeholder="Write the body in Markdown…"
                className={clsx(
                  "min-h-0 flex-1 resize-none bg-transparent p-4 font-mono text-sm text-ink outline-none placeholder:text-ink-dim",
                  showPreview && "border-r border-bg-border"
                )}
              />
              {showPreview && (
                <div className="min-h-0 flex-1 overflow-y-auto p-4">
                  <div className={MARKDOWN_CLASSES}>
                    <ReactMarkdown>{draft.body}</ReactMarkdown>
                  </div>
                </div>
              )}
            </div>
          </div>

          <aside className="w-72 shrink-0 space-y-4 overflow-y-auto border-l border-bg-border p-4">
            <div className="flex flex-wrap gap-1.5">
              <MetaChip>{draft.source}</MetaChip>
              <MetaChip>{draft.sensitivity}</MetaChip>
            </div>

            <Select
              label="Status"
              value={draft.status}
              options={STATUS_OPTIONS}
              onChange={(v) => setDraft((d) => (d ? { ...d, status: v } : d))}
            />

            <Slider
              label="Confidence"
              value={draft.confidence}
              onChange={(v) => setDraft((d) => (d ? { ...d, confidence: v } : d))}
              format={(v) => `${Math.round(v * 100)}%`}
            />
            <Slider
              label="Importance"
              value={draft.importance}
              onChange={(v) => setDraft((d) => (d ? { ...d, importance: v } : d))}
              format={(v) => `${Math.round(v * 100)}%`}
            />

            <label className="block">
              <span className="mb-1 block text-xs text-ink-muted">Tags (comma-separated)</span>
              <TextInput
                value={draft.tags}
                onChange={(v) => setDraft((d) => (d ? { ...d, tags: v } : d))}
                placeholder="tag-a, tag-b"
              />
            </label>

            <label className="block">
              <span className="mb-1 block text-xs text-ink-muted">Project</span>
              <TextInput
                value={draft.project}
                onChange={(v) => setDraft((d) => (d ? { ...d, project: v } : d))}
                placeholder="none"
              />
            </label>

            <div>
              <div className="mb-1.5 flex items-center justify-between">
                <span className="text-xs text-ink-muted">Relations</span>
                <Button size="sm" variant="ghost" onClick={addRelation}>
                  <Plus size={12} /> Add relation
                </Button>
              </div>
              <div className="space-y-1.5">
                {draft.relations.length === 0 && (
                  <div className="text-xs text-ink-dim">No relations yet.</div>
                )}
                {draft.relations.map((r, idx) => (
                  <div key={idx} className="flex items-center gap-1.5">
                    <TextInput
                      value={r.to}
                      onChange={(v) => updateRelation(idx, { to: v })}
                      placeholder="target id / title"
                      className="min-w-0 flex-1"
                    />
                    <TextInput
                      value={r.rel}
                      onChange={(v) => updateRelation(idx, { rel: v })}
                      placeholder="relation"
                      className="w-20 shrink-0"
                    />
                    <div className="w-14 shrink-0">
                      <Slider
                        value={r.confidence}
                        onChange={(v) => updateRelation(idx, { confidence: v })}
                      />
                    </div>
                    <button
                      type="button"
                      aria-label="Remove relation"
                      onClick={() => removeRelation(idx)}
                      className="shrink-0 text-ink-dim transition-colors hover:text-state-error"
                    >
                      <Trash2 size={13} />
                    </button>
                  </div>
                ))}
              </div>
            </div>
          </aside>
        </div>
      ) : null}
    </div>
  );
}
