"use client";

// Docked right-hand panel for a single memory note: metadata viewer/editor +
// rendered body + backlinks. Full editing (title/body/type) happens in
// NoteEditorOverlay, reached via the Expand button. See task-13-brief.md.

import { useCallback, useEffect, useRef, useState } from "react";
import type { MouseEvent as ReactMouseEvent, PropsWithChildren } from "react";
import { clsx } from "clsx";
import { Maximize2, Plus, Trash2, X } from "lucide-react";
import ReactMarkdown from "react-markdown";

import { continuum } from "@/lib/tauri";
import { NODE_COLORS, NODE_TYPE_LABELS } from "@/lib/memoryTheme";
import { Button, Modal, Select, Slider, TextInput } from "@/components/ui/primitives";
import type { MemoryFrontmatter, MemoryNodeStatus, MemoryNote, MemoryRelation } from "@/lib/types";

// Same handful-of-elements markdown styling as ChatTab - no
// @tailwindcss/typography plugin in this repo.
const MARKDOWN_CLASSES =
  "break-words text-sm leading-relaxed " +
  "[&_p:not(:last-child)]:mb-2 " +
  "[&_ul]:my-1 [&_ul]:list-disc [&_ul]:pl-5 [&_ol]:my-1 [&_ol]:list-decimal [&_ol]:pl-5 [&_li]:mb-0.5 " +
  "[&_a]:text-accent-purple [&_a]:underline [&_strong]:font-semibold " +
  "[&_pre]:overflow-x-auto [&_pre]:rounded [&_pre]:bg-black/40 [&_pre]:p-2 [&_code]:text-xs";

const WIDTH_STORAGE_KEY = "continuum-memory-panel-w";
const MIN_WIDTH = 320;
const MAX_WIDTH_FRACTION = 0.7;
const DEFAULT_WIDTH = 420;

export const STATUS_LABELS: Record<MemoryNodeStatus, string> = {
  candidate: "Candidate",
  confirmed: "Confirmed",
  rejected: "Rejected",
  superseded: "Superseded",
  archived: "Archived",
};

export const STATUS_DOT_CLASSES: Record<MemoryNodeStatus, string> = {
  candidate: "bg-state-warn",
  confirmed: "bg-state-healthy",
  rejected: "bg-state-error",
  superseded: "bg-ink-dim",
  archived: "bg-ink-subtle",
};

/** Shared `<Select>` options for the status field, in `STATUS_LABELS`
 * order. Exported so NoteEditorOverlay's status editor always offers the
 * exact same choices as NotePanel's. */
export const STATUS_OPTIONS: Array<{ value: MemoryNodeStatus; label: string }> = (
  Object.entries(STATUS_LABELS) as [MemoryNodeStatus, string][]
).map(([value, label]) => ({ value, label }));

function clampWidth(w: number, viewportWidth: number): number {
  const max = Math.max(MIN_WIDTH, viewportWidth * MAX_WIDTH_FRACTION);
  return Math.min(Math.max(w, MIN_WIDTH), max);
}

function formatDate(iso: string | null | undefined): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString();
}

function parseTags(input: string): string[] {
  return input
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean);
}

export function MetaChip({ children }: PropsWithChildren) {
  return (
    <span className="flex items-center gap-1.5 rounded-md border border-bg-border bg-bg-elevated px-2 py-1 text-[11px] text-ink-muted">
      {children}
    </span>
  );
}

interface NotePanelProps {
  noteId: string;
  onClose: () => void;
  onExpand: () => void;
  onChanged: () => void;
  onNavigate: (id: string) => void;
}

export function NotePanel({ noteId, onClose, onExpand, onChanged, onNavigate }: NotePanelProps) {
  const [note, setNote] = useState<MemoryNote | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [width, setWidth] = useState(DEFAULT_WIDTH);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);

  // Draft fields mirrored from `note` on load, edited locally, and
  // committed to the backend on blur/release (not on every keystroke/tick).
  const [confidence, setConfidence] = useState(0);
  const [importance, setImportance] = useState(0);
  const [tagsDraft, setTagsDraft] = useState("");
  const [projectDraft, setProjectDraft] = useState("");
  const [relations, setRelations] = useState<MemoryRelation[]>([]);

  const rootRef = useRef<HTMLDivElement>(null);
  const dragRightEdgeRef = useRef<number | null>(null);

  useEffect(() => {
    const raw = window.localStorage.getItem(WIDTH_STORAGE_KEY);
    const n = raw ? Number(raw) : NaN;
    if (Number.isFinite(n)) setWidth(clampWidth(n, window.innerWidth));
  }, []);

  const load = useCallback(async () => {
    // Clear stale content up front - before the await - so the panel can
    // never keep showing (and, via a slider release/blur in the gap,
    // saving against) the *previous* note while a new noteId's fetch is
    // still in flight. Render is gated on `loading` alone below, mirroring
    // NoteEditorOverlay, so none of this stale state is ever visible.
    setLoading(true);
    setError(null);
    setNote(null);
    setConfidence(0);
    setImportance(0);
    setTagsDraft("");
    setProjectDraft("");
    setRelations([]);
    try {
      const n = await continuum.memoryGetNote(noteId);
      setNote(n);
      setConfidence(n.frontmatter.confidence);
      setImportance(n.frontmatter.importance);
      setTagsDraft((n.frontmatter.tags ?? []).join(", "));
      setProjectDraft(n.frontmatter.project ?? "");
      setRelations(n.frontmatter.relations ?? []);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setNote(null);
    } finally {
      setLoading(false);
    }
  }, [noteId]);

  useEffect(() => {
    void load();
  }, [load]);

  const persist = useCallback(
    async (patch: Partial<MemoryFrontmatter>) => {
      // Belt-and-braces alongside the stale-clear in `load`: never let a
      // save target anything but the note currently on screen.
      if (!note || note.frontmatter.id !== noteId) return;
      const previous = note;
      const updated: MemoryNote = { ...note, frontmatter: { ...note.frontmatter, ...patch } };
      setNote(updated);
      setSaveError(null);
      try {
        await continuum.memorySaveNote(updated);
        onChanged();
      } catch (e) {
        // Roll back the optimistic update - a failed save must not leave
        // the UI showing an unsaved value as though it persisted. Guarded
        // by identity: if a concurrent persist() already moved state past
        // `updated` (i.e. a later save succeeded), that newer state wins -
        // this rollback only undoes its own optimistic write.
        setNote((cur) => (cur === updated ? previous : cur));
        setSaveError(e instanceof Error ? e.message : String(e));
      }
    },
    [note, noteId, onChanged]
  );

  // Resize: drag the left-edge handle. `dragRightEdgeRef` is the panel's
  // fixed right edge (it's docked `right-0`), captured once at drag start,
  // so width is just that minus the live cursor x - no need to track a
  // running delta.
  useEffect(() => {
    function onMove(e: MouseEvent) {
      const rightEdge = dragRightEdgeRef.current;
      if (rightEdge == null) return;
      setWidth(clampWidth(rightEdge - e.clientX, window.innerWidth));
    }
    function onUp() {
      if (dragRightEdgeRef.current == null) return;
      dragRightEdgeRef.current = null;
      setWidth((w) => {
        window.localStorage.setItem(WIDTH_STORAGE_KEY, String(w));
        return w;
      });
    }
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, []);

  function startDrag(e: ReactMouseEvent) {
    const rect = rootRef.current?.getBoundingClientRect();
    if (!rect) return;
    dragRightEdgeRef.current = rect.right;
    e.preventDefault();
  }

  function commitStatus(status: MemoryNodeStatus) {
    void persist({ status });
  }
  function commitConfidence() {
    void persist({ confidence });
  }
  function commitImportance() {
    void persist({ importance });
  }
  function commitTags() {
    void persist({ tags: parseTags(tagsDraft) });
  }
  function commitProject() {
    void persist({ project: projectDraft.trim() || null });
  }
  function commitRelations() {
    void persist({ relations });
  }

  function updateRelation(idx: number, patch: Partial<MemoryRelation>) {
    setRelations((rs) => rs.map((r, i) => (i === idx ? { ...r, ...patch } : r)));
  }

  function addRelation() {
    const next = [...relations, { to: "", rel: "related_to", confidence: 1 }];
    setRelations(next);
    void persist({ relations: next });
  }

  function removeRelation(idx: number) {
    const next = relations.filter((_, i) => i !== idx);
    setRelations(next);
    void persist({ relations: next });
  }

  async function handleDelete() {
    setDeleting(true);
    try {
      await continuum.memoryDeleteNote(noteId);
      setConfirmDelete(false);
      onChanged();
      onClose();
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    } finally {
      setDeleting(false);
    }
  }

  return (
    <div
      ref={rootRef}
      className="absolute right-0 top-0 z-10 flex h-full flex-col border-l border-bg-border bg-bg-surface shadow-xl"
      style={{ width }}
    >
      <div
        onMouseDown={startDrag}
        className="group absolute left-0 top-0 h-full w-1.5 cursor-col-resize"
      >
        <div className="h-full w-px bg-bg-border group-hover:bg-accent-purple/50" />
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4 pl-5">
        {loading ? (
          <div className="py-8 text-center text-xs text-ink-dim">Loading…</div>
        ) : error ? (
          <div className="space-y-2 py-8 text-center">
            <div className="text-xs text-state-error">{error}</div>
            <Button size="sm" onClick={() => void load()}>
              Retry
            </Button>
          </div>
        ) : note ? (
          <>
            <h2 className="mb-3 break-words text-base font-semibold text-ink">
              {note.frontmatter.title}
            </h2>

            <div className="mb-4 flex flex-wrap gap-1.5">
              <MetaChip>
                <span
                  className="h-2 w-2 rounded-full"
                  style={{ backgroundColor: NODE_COLORS[note.frontmatter.type] }}
                />
                {NODE_TYPE_LABELS[note.frontmatter.type]}
              </MetaChip>
              <MetaChip>
                <span
                  className={clsx(
                    "h-1.5 w-1.5 rounded-full",
                    STATUS_DOT_CLASSES[note.frontmatter.status]
                  )}
                />
                {STATUS_LABELS[note.frontmatter.status]}
              </MetaChip>
              <MetaChip>{note.frontmatter.source}</MetaChip>
              <MetaChip>{note.frontmatter.sensitivity}</MetaChip>
            </div>

            <div className="mb-4 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-[11px] text-ink-dim">
              <span>Created</span>
              <span className="text-ink-muted">{formatDate(note.frontmatter.created)}</span>
              <span>Updated</span>
              <span className="text-ink-muted">{formatDate(note.frontmatter.updated)}</span>
            </div>

            <div className="mb-4">
              <Select
                label="Status"
                value={note.frontmatter.status}
                options={STATUS_OPTIONS}
                onChange={commitStatus}
              />
            </div>

            <div className="mb-4 space-y-3">
              <div
                onMouseUp={commitConfidence}
                onTouchEnd={commitConfidence}
                onBlur={commitConfidence}
              >
                <Slider
                  label="Confidence"
                  value={confidence}
                  onChange={setConfidence}
                  format={(v) => `${Math.round(v * 100)}%`}
                />
              </div>
              <div
                onMouseUp={commitImportance}
                onTouchEnd={commitImportance}
                onBlur={commitImportance}
              >
                <Slider
                  label="Importance"
                  value={importance}
                  onChange={setImportance}
                  format={(v) => `${Math.round(v * 100)}%`}
                />
              </div>
            </div>

            <label className="mb-4 block">
              <span className="mb-1 block text-xs text-ink-muted">Tags (comma-separated)</span>
              <TextInput
                value={tagsDraft}
                onChange={setTagsDraft}
                onBlur={commitTags}
                placeholder="tag-a, tag-b"
              />
            </label>

            <label className="mb-4 block">
              <span className="mb-1 block text-xs text-ink-muted">Project</span>
              <TextInput
                value={projectDraft}
                onChange={setProjectDraft}
                onBlur={commitProject}
                placeholder="none"
              />
            </label>

            <div className="mb-4">
              <div className="mb-1.5 flex items-center justify-between">
                <span className="text-xs text-ink-muted">Relations</span>
                <Button size="sm" variant="ghost" onClick={addRelation}>
                  <Plus size={12} /> Add relation
                </Button>
              </div>
              <div className="space-y-1.5">
                {relations.length === 0 && (
                  <div className="text-xs text-ink-dim">No relations yet.</div>
                )}
                {relations.map((r, idx) => (
                  <div key={idx} className="flex items-center gap-1.5">
                    <TextInput
                      value={r.to}
                      onChange={(v) => updateRelation(idx, { to: v })}
                      onBlur={commitRelations}
                      placeholder="target id / title"
                      className="min-w-0 flex-1"
                    />
                    <TextInput
                      value={r.rel}
                      onChange={(v) => updateRelation(idx, { rel: v })}
                      onBlur={commitRelations}
                      placeholder="relation"
                      className="w-24 shrink-0"
                    />
                    <div
                      className="w-16 shrink-0"
                      onMouseUp={commitRelations}
                      onTouchEnd={commitRelations}
                      onBlur={commitRelations}
                    >
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

            {saveError && <div className="mb-3 text-xs text-state-error">{saveError}</div>}

            <div className="border-t border-bg-border pt-3">
              <div className={MARKDOWN_CLASSES}>
                <ReactMarkdown>{note.body}</ReactMarkdown>
              </div>
            </div>

            {note.backlinks.length > 0 && (
              <div className="mt-4 border-t border-bg-border pt-3">
                <div className="mb-1.5 text-xs text-ink-muted">Backlinks</div>
                <ul className="space-y-0.5">
                  {note.backlinks.map((b) => (
                    <li key={b.id}>
                      <button
                        type="button"
                        onClick={() => onNavigate(b.id)}
                        className="flex w-full items-center gap-1.5 rounded px-1.5 py-1 text-left text-xs text-ink-muted transition-colors hover:bg-bg-hover hover:text-ink"
                      >
                        <span
                          className="h-1.5 w-1.5 shrink-0 rounded-full"
                          style={{ backgroundColor: NODE_COLORS[b.type] }}
                        />
                        <span className="truncate">{b.title}</span>
                      </button>
                    </li>
                  ))}
                </ul>
              </div>
            )}
          </>
        ) : null}
      </div>

      <div className="flex items-center justify-end gap-2 border-t border-bg-border px-3 py-2">
        <Button size="sm" variant="ghost" onClick={onExpand} disabled={!note}>
          <Maximize2 size={13} /> Expand
        </Button>
        <Button size="sm" variant="danger" onClick={() => setConfirmDelete(true)} disabled={!note}>
          <Trash2 size={13} /> Delete
        </Button>
        <Button size="sm" onClick={onClose}>
          <X size={13} /> Close
        </Button>
      </div>

      <Modal
        open={confirmDelete}
        onClose={() => setConfirmDelete(false)}
        title="Delete this memory?"
        footer={
          <>
            <Button variant="ghost" onClick={() => setConfirmDelete(false)}>
              Cancel
            </Button>
            <Button variant="danger" onClick={() => void handleDelete()} disabled={deleting}>
              {deleting ? "Deleting…" : "Delete"}
            </Button>
          </>
        }
      >
        <p className="text-sm text-ink-muted">
          This permanently deletes &ldquo;{note?.frontmatter.title}&rdquo; from the vault. This
          can&apos;t be undone.
        </p>
      </Modal>
    </div>
  );
}
