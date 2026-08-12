"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { clsx } from "clsx";
import {
  AlertTriangle,
  Download,
  FolderOpen,
  ListFilter,
  MoreHorizontal,
  Plus,
  RefreshCw,
  RotateCw,
  Search,
  Trash2,
} from "lucide-react";

import { continuum, onMemoryEvent } from "@/lib/tauri";
import { useStore } from "@/lib/store";
import { NODE_COLORS, NODE_TYPE_LABELS } from "@/lib/memoryTheme";
import { useMemoryViews, type SavedView } from "@/lib/memoryViews";
import { MemoryGraph } from "@/components/memory/MemoryGraph";
import { NotePanel } from "@/components/memory/NotePanel";
import { NoteEditorOverlay, type OverlayMode } from "@/components/memory/NoteEditorOverlay";
import { CuratorStack } from "@/components/memory/CuratorStack";
import { TimelineStrip, todayStartIso } from "@/components/memory/TimelineStrip";
import {
  Button,
  Card,
  EmptyState,
  Modal,
  SearchInput,
  TextInput,
} from "@/components/ui/primitives";
import type {
  MemoryEvent,
  MemoryGraphData,
  MemoryGraphFilter,
  MemoryIndexStats,
  MemoryMigrationReport,
  MemoryNodeSummary,
  MemoryNodeType,
  MemoryResolution,
  MemoryVaultInfo,
} from "@/lib/types";

const EMPTY_GRAPH: MemoryGraphData = { nodes: [], edges: [], ghosts: [], truncated: false };
const ALL_TYPES = Object.keys(NODE_TYPE_LABELS) as MemoryNodeType[];

/** Inline feedback for the "…" vault-actions menu - stands in for a toast
 * since this app has no toast infrastructure yet. */
type ActionBanner =
  | { kind: "migrate"; report: MemoryMigrationReport }
  | { kind: "rebuild"; stats: MemoryIndexStats }
  | { kind: "error"; message: string };

export function MemoryTab() {
  const curator = useStore((state) => state.state.memory.curator);
  const [filter, setFilter] = useState<MemoryGraphFilter>({});
  const [graph, setGraph] = useState<MemoryGraphData>(EMPTY_GRAPH);
  const [info, setInfo] = useState<MemoryVaultInfo | null>(null);
  const [pending, setPending] = useState<MemoryNodeSummary[]>([]);
  const [events, setEvents] = useState<MemoryEvent[]>([]);
  const [scrubWindow, setScrubWindow] = useState<{ since: string; until: string } | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [overlayMode, setOverlayMode] = useState<OverlayMode | null>(null);
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const refreshTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const { views, addView, removeView } = useMemoryViews();
  const [viewsOpen, setViewsOpen] = useState(false);
  const [saveViewOpen, setSaveViewOpen] = useState(false);
  const [viewName, setViewName] = useState("");

  const [menuOpen, setMenuOpen] = useState(false);
  const [banner, setBanner] = useState<ActionBanner | null>(null);
  const [migrating, setMigrating] = useState(false);
  const [rebuilding, setRebuilding] = useState(false);
  const [wipeOpen, setWipeOpen] = useState(false);
  const [wipeInput, setWipeInput] = useState("");
  const [wiping, setWiping] = useState(false);
  const [wipeError, setWipeError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    // allSettled, not all: a failed vault-info fetch must not discard a
    // successful graph fetch (or vice versa) and blank out an otherwise
    // valid, already-rendered graph behind the full-bleed error card.
    const [graphResult, infoResult, pendingResult, eventsResult] = await Promise.allSettled([
      continuum.memoryGraph(filter),
      continuum.memoryVaultInfo(),
      continuum.memoryPending(),
      continuum.memoryEvents({ since: todayStartIso(), limit: 2000 }),
    ]);
    if (graphResult.status === "fulfilled") {
      setGraph(graphResult.value);
      setLoadError(null);
    } else {
      setLoadError(String(graphResult.reason));
    }
    if (infoResult.status === "fulfilled") {
      setInfo(infoResult.value);
    } else {
      // Non-critical: the quarantine chip/vault path just goes stale until
      // the next successful refresh, rather than hiding a healthy graph.
      console.warn("memoryVaultInfo failed:", infoResult.reason);
    }
    if (pendingResult.status === "fulfilled") {
      setPending(pendingResult.value);
    } else {
      console.warn("memoryPending failed:", pendingResult.reason);
    }
    if (eventsResult.status === "fulfilled") {
      setEvents(eventsResult.value);
    } else {
      console.warn("memoryEvents failed:", eventsResult.reason);
    }
    setLoading(false);
  }, [filter]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let disposed = false;
    void onMemoryEvent(() => {
      if (refreshTimer.current) clearTimeout(refreshTimer.current);
      refreshTimer.current = setTimeout(() => void refresh(), 300);
    }).then((u) => {
      if (disposed) u();
      else unlisten = u;
    });
    return () => {
      disposed = true;
      unlisten?.();
      if (refreshTimer.current) clearTimeout(refreshTimer.current);
    };
  }, [refresh]);

  // Escape closes whichever of the two topbar popovers is open, consistent
  // with the command palette (Shell.tsx) and NoteEditorOverlay.
  useEffect(() => {
    if (!viewsOpen && !menuOpen) return;
    function onKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") {
        setViewsOpen(false);
        setMenuOpen(false);
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [viewsOpen, menuOpen]);

  // Ids of nodes whose created/updated timestamp falls inside the scrubbed
  // timeline window - passed to MemoryGraph as the set to keep highlighted
  // (everything else dims). Computed client-side per the brief: the graph
  // fetch already has its own filter pipeline, so the timeline scrub is a
  // pure view-layer overlay on whatever graph is already loaded.
  //
  // `null` vs. an empty `Set` are deliberately distinct states, and
  // MemoryGraph's dim guard (`dims != null && !dims.has(node.id)`) depends
  // on that distinction: `null` = no active window, live view, nothing
  // dims. A `Set` - even an empty one, meaning the scrubbed window matched
  // zero nodes - means a window IS active, so everything not in it (i.e.
  // everything, when it's empty) dims. Only return `null` when there is no
  // scrub window at all; once one is active, always return a Set.
  const dimIds = useMemo(() => {
    if (!scrubWindow) return null;
    const since = new Date(scrubWindow.since).getTime();
    const until = new Date(scrubWindow.until).getTime();
    const ids = new Set<string>();
    for (const n of graph.nodes) {
      const created = new Date(n.created).getTime();
      const updated = new Date(n.updated).getTime();
      if ((created >= since && created < until) || (updated >= since && updated < until)) {
        ids.add(n.id);
      }
    }
    return ids;
  }, [scrubWindow, graph.nodes]);

  function toggleType(t: MemoryNodeType) {
    setFilter((f) => {
      const cur = f.types ?? null;
      if (!cur) return { ...f, types: [t] };
      const next = cur.includes(t) ? cur.filter((x) => x !== t) : [...cur, t];
      return { ...f, types: next.length === 0 ? null : next };
    });
  }

  function submitSearch() {
    setFilter((f) => ({ ...f, query: query.trim() || null }));
  }

  const showHidden = (filter.statuses?.length ?? 0) > 0;
  function toggleHidden() {
    setFilter((f) => ({
      ...f,
      statuses: showHidden
        ? null
        : ["confirmed", "candidate", "rejected", "superseded", "archived"],
    }));
  }

  const handleResolve = useCallback(
    async (id: string, resolution: MemoryResolution) => {
      try {
        await continuum.memoryResolveCandidate(id, resolution);
        await refresh();
      } catch (e) {
        setBanner({ kind: "error", message: e instanceof Error ? e.message : String(e) });
      }
    },
    [refresh]
  );

  async function handleMigrate() {
    setMigrating(true);
    try {
      const report = await continuum.memoryMigrateLegacy();
      setBanner({ kind: "migrate", report });
      await refresh();
    } catch (e) {
      setBanner({ kind: "error", message: e instanceof Error ? e.message : String(e) });
    } finally {
      setMigrating(false);
    }
  }

  async function handleRebuild() {
    setRebuilding(true);
    try {
      const stats = await continuum.memoryRebuildIndex();
      setBanner({ kind: "rebuild", stats });
      await refresh();
    } catch (e) {
      setBanner({ kind: "error", message: e instanceof Error ? e.message : String(e) });
    } finally {
      setRebuilding(false);
    }
  }

  async function handleWipe() {
    setWiping(true);
    setWipeError(null);
    try {
      await continuum.wipeMemory(wipeInput);
      setWipeOpen(false);
      setWipeInput("");
      await refresh();
    } catch (e) {
      setWipeError(e instanceof Error ? e.message : String(e));
    } finally {
      setWiping(false);
    }
  }

  function applyView(v: SavedView) {
    setFilter(v.filter);
    setViewsOpen(false);
  }

  function saveCurrentView() {
    const name = viewName.trim();
    if (!name) return;
    addView(name, filter);
    setViewName("");
    setSaveViewOpen(false);
  }

  return (
    <div className="relative flex h-full min-h-0 flex-col">
      {/* Topbar */}
      <div className="flex flex-wrap items-center gap-2 border-b border-bg-border px-3 py-2">
        <div className="relative w-64">
          <Search size={14} className="pointer-events-none absolute left-2.5 top-2 text-ink-dim" />
          <SearchInput
            value={query}
            onChange={setQuery}
            onKeyDown={(e) => {
              if (e.key === "Enter") submitSearch();
            }}
            placeholder="Search memory…"
            className="pl-8"
          />
        </div>
        <Button size="sm" variant="primary" onClick={submitSearch}>
          Search
        </Button>
        {ALL_TYPES.map((t) => {
          const active = filter.types?.includes(t) ?? false;
          return (
            <button
              key={t}
              onClick={() => toggleType(t)}
              className={
                "flex items-center gap-1.5 rounded-md border px-2 py-1 text-xs transition-colors " +
                (active
                  ? "border-accent-amber/60 bg-accent-amber/15 text-ink"
                  : "border-bg-border bg-bg-elevated text-ink-muted hover:text-ink")
              }
            >
              <span className="h-2 w-2 rounded-full" style={{ backgroundColor: NODE_COLORS[t] }} />
              {NODE_TYPE_LABELS[t]}
            </button>
          );
        })}
        <button
          onClick={toggleHidden}
          className={
            "rounded-md border px-2 py-1 text-xs " +
            (showHidden
              ? "border-accent-amber/60 text-ink"
              : "border-bg-border text-ink-muted hover:text-ink")
          }
        >
          Show hidden
        </button>
        <div className="flex-1" />
        {(info?.quarantined.length ?? 0) > 0 && (
          <span className="rounded-md border border-state-warn/40 bg-state-warn/10 px-2 py-1 text-xs text-state-warn">
            {info?.quarantined.length} file(s) quarantined
          </span>
        )}
        {graph.truncated && (
          <span className="text-xs text-ink-dim">graph truncated — refine your filters</span>
        )}

        {/* Saved views */}
        <div className="relative">
          <Button
            size="sm"
            onClick={() => {
              setMenuOpen(false);
              setViewsOpen((o) => !o);
            }}
          >
            <ListFilter size={13} /> Views{views.length > 0 ? ` (${views.length})` : ""}
          </Button>
          {viewsOpen && (
            <>
              <div className="fixed inset-0 z-30" onClick={() => setViewsOpen(false)} />
              <Card dense className="absolute right-0 top-full z-40 mt-1 w-56">
                {views.length === 0 ? (
                  <div className="px-1.5 py-2 text-center text-xs text-ink-dim">
                    No saved views yet.
                  </div>
                ) : (
                  <ul className="space-y-0.5">
                    {views.map((v) => (
                      <li
                        key={v.name}
                        className="flex items-center gap-1 rounded hover:bg-bg-hover"
                      >
                        <button
                          type="button"
                          onClick={() => applyView(v)}
                          className="min-w-0 flex-1 truncate px-1.5 py-1 text-left text-xs text-ink"
                        >
                          {v.name}
                        </button>
                        <button
                          type="button"
                          aria-label={`Delete view "${v.name}"`}
                          onClick={() => removeView(v.name)}
                          className="shrink-0 px-1.5 text-ink-dim hover:text-state-error"
                        >
                          <Trash2 size={12} />
                        </button>
                      </li>
                    ))}
                  </ul>
                )}
                <div className="mt-1 border-t border-bg-border pt-1">
                  <button
                    type="button"
                    onClick={() => {
                      setViewsOpen(false);
                      setViewName("");
                      setSaveViewOpen(true);
                    }}
                    className="flex w-full items-center gap-1.5 rounded px-1.5 py-1 text-left text-xs text-accent-amber hover:bg-bg-hover"
                  >
                    <Plus size={12} /> Save current view
                  </button>
                </div>
              </Card>
            </>
          )}
        </div>

        <Button size="sm" variant="ghost" onClick={() => void refresh()}>
          <RefreshCw size={13} />
        </Button>
        <Button
          size="sm"
          variant="primary"
          onClick={() => setOverlayMode({ kind: "create", draft: { type: "note", title: "" } })}
        >
          <Plus size={13} /> New memory
        </Button>
        <Button size="sm" onClick={() => void continuum.memoryOpenVault()}>
          <FolderOpen size={13} /> Open vault
        </Button>

        {/* "..." vault-actions menu: rebuild index, legacy import, danger row */}
        <div className="relative">
          <Button
            size="sm"
            variant="ghost"
            aria-label="Vault actions"
            onClick={() => {
              setViewsOpen(false);
              setMenuOpen((o) => !o);
            }}
          >
            <MoreHorizontal size={14} />
          </Button>
          {menuOpen && (
            <>
              <div className="fixed inset-0 z-30" onClick={() => setMenuOpen(false)} />
              <Card dense className="absolute right-0 top-full z-40 mt-1 w-64">
                <div className="space-y-0.5">
                  {info?.legacy_semantic_present && (
                    <button
                      type="button"
                      disabled={migrating}
                      onClick={() => void handleMigrate()}
                      className="flex w-full items-center gap-2 rounded px-1.5 py-1.5 text-left text-xs text-ink hover:bg-bg-hover disabled:opacity-50"
                    >
                      <Download size={13} /> {migrating ? "Importing…" : "Import legacy memory"}
                    </button>
                  )}
                  <button
                    type="button"
                    disabled={rebuilding}
                    onClick={() => void handleRebuild()}
                    className="flex w-full items-center gap-2 rounded px-1.5 py-1.5 text-left text-xs text-ink hover:bg-bg-hover disabled:opacity-50"
                  >
                    <RotateCw size={13} /> {rebuilding ? "Rebuilding…" : "Rebuild index"}
                  </button>
                  <div className="my-1 border-t border-bg-border" />
                  <button
                    type="button"
                    onClick={() => {
                      setMenuOpen(false);
                      setWipeInput("");
                      setWipeError(null);
                      setWipeOpen(true);
                    }}
                    className="flex w-full items-center gap-2 rounded px-1.5 py-1.5 text-left text-xs text-state-error hover:bg-state-error/10"
                  >
                    <AlertTriangle size={13} /> Wipe derived data
                  </button>
                </div>
                <p className="mt-2 border-t border-bg-border pt-2 text-[10px] text-ink-dim">
                  Vault markdown is never deleted.
                </p>
              </Card>
            </>
          )}
        </div>
      </div>

      {/* Inline result banner for migrate/rebuild - stands in for a toast */}
      {banner && (
        <div
          className={clsx(
            "flex items-center justify-between gap-2 border-b px-3 py-1.5 text-xs",
            banner.kind === "error"
              ? "border-state-error/30 bg-state-error/10 text-state-error"
              : "border-bg-border bg-bg-elevated text-ink-muted"
          )}
        >
          <span>
            {banner.kind === "migrate" &&
              `Imported ${banner.report.migrated} note(s), skipped ${banner.report.skipped}` +
                (banner.report.errors.length > 0
                  ? `, ${banner.report.errors.length} error(s).`
                  : ".")}
            {banner.kind === "rebuild" &&
              `Rebuilt index: ${banner.stats.indexed} indexed, ${banner.stats.quarantined} quarantined, ${banner.stats.removed} removed.`}
            {banner.kind === "error" && banner.message}
          </span>
          <button
            type="button"
            onClick={() => setBanner(null)}
            className="shrink-0 text-ink-dim hover:text-ink"
            aria-label="Dismiss"
          >
            &times;
          </button>
        </div>
      )}

      {/* Graph body */}
      <div className="relative min-h-0 flex-1">
        <MemoryGraph
          data={graph}
          selectedId={selectedId}
          dimIds={dimIds}
          onSelect={setSelectedId}
          onExpand={(id) => setOverlayMode({ kind: "edit", id })}
          onGhostClick={(target) =>
            setOverlayMode({ kind: "create", draft: { type: "note", title: target } })
          }
        />
        {/* Legend */}
        <div className="absolute bottom-3 left-3 flex flex-wrap gap-x-3 gap-y-1 rounded-md border border-bg-border bg-bg-surface/90 px-3 py-2 text-[11px] text-ink-muted">
          {ALL_TYPES.map((t) => (
            <span key={t} className="flex items-center gap-1">
              <span className="h-2 w-2 rounded-full" style={{ backgroundColor: NODE_COLORS[t] }} />
              {NODE_TYPE_LABELS[t]}
            </span>
          ))}
        </div>
        <CuratorStack
          pending={pending}
          onResolve={(id, resolution) => void handleResolve(id, resolution)}
          onOpen={setSelectedId}
        />
        {loadError && (
          <div className="absolute inset-0 flex items-center justify-center">
            <Card title="Memory unavailable" className="max-w-md">
              <p className="text-sm text-ink-muted">{loadError}</p>
              <div className="mt-3">
                <Button variant="primary" onClick={() => void refresh()}>
                  Try again
                </Button>
              </div>
            </Card>
          </div>
        )}
        {!loading && !loadError && graph.nodes.length === 0 && (
          <div className="absolute inset-0 flex items-center justify-center">
            <Card title="No memories yet" className="max-w-md">
              <EmptyState
                title={
                  curator?.enabled ? "Local curator is organizing context" : "Your vault is empty"
                }
                description="Continuum stores everything it learns about your work here — as plain markdown files you own and can edit."
              />
              {curator?.enabled && (
                <div className="mx-auto mt-2 max-w-sm text-center text-[11px] leading-4 text-ink-dim">
                  {curator.consecutive_failures > 0
                    ? "The small local LLM could not finish its latest pass. It now receives a fair turn and retries automatically without sending activity off-device."
                    : curator.last_pass_at
                      ? `Last local pass ${new Date(curator.last_pass_at).toLocaleTimeString()} · ${curator.candidates_written_total} written`
                      : "Recent context is waiting for its first private local curation pass."}
                </div>
              )}
              <div className="mt-2 flex justify-center gap-2">
                <Button onClick={() => void continuum.memoryOpenVault()}>
                  <FolderOpen size={13} /> Open vault folder
                </Button>
                {info?.legacy_semantic_present && (
                  <Button
                    variant="primary"
                    disabled={migrating}
                    onClick={() => void handleMigrate()}
                  >
                    <Download size={13} /> {migrating ? "Importing…" : "Import legacy memory"}
                  </Button>
                )}
              </div>
            </Card>
          </div>
        )}
        {selectedId && (
          <NotePanel
            noteId={selectedId}
            onClose={() => setSelectedId(null)}
            onExpand={() => setOverlayMode({ kind: "edit", id: selectedId })}
            onChanged={() => void refresh()}
            onNavigate={setSelectedId}
          />
        )}
      </div>

      <TimelineStrip events={events} window={scrubWindow} onScrub={setScrubWindow} />

      {overlayMode && (
        <NoteEditorOverlay
          mode={overlayMode}
          onClose={() => setOverlayMode(null)}
          onSaved={(id) => {
            setOverlayMode(null);
            setSelectedId(id);
            void refresh();
          }}
        />
      )}

      <Modal
        open={saveViewOpen}
        onClose={() => setSaveViewOpen(false)}
        title="Save current view"
        footer={
          <>
            <Button variant="ghost" onClick={() => setSaveViewOpen(false)}>
              Cancel
            </Button>
            <Button variant="primary" disabled={!viewName.trim()} onClick={saveCurrentView}>
              Save
            </Button>
          </>
        }
      >
        <label className="block">
          <span className="mb-1 block text-xs text-ink-muted">Name</span>
          <TextInput
            value={viewName}
            onChange={setViewName}
            placeholder="e.g. Active project"
            onKeyDown={(e) => {
              if (e.key === "Enter") saveCurrentView();
            }}
          />
        </label>
      </Modal>

      <Modal
        open={wipeOpen}
        onClose={() => {
          setWipeOpen(false);
          setWipeInput("");
          setWipeError(null);
        }}
        title="Wipe derived memory data"
        footer={
          <>
            <Button
              variant="ghost"
              onClick={() => {
                setWipeOpen(false);
                setWipeInput("");
                setWipeError(null);
              }}
            >
              Cancel
            </Button>
            <Button
              variant="danger"
              disabled={wipeInput !== "DELETE" || wiping}
              onClick={() => void handleWipe()}
            >
              {wiping ? "Wiping…" : "Confirm wipe"}
            </Button>
          </>
        }
      >
        <p className="text-sm text-ink-muted">
          Timeline events and the derived index are wiped immediately. Raw log and episodic memory
          are wiped when the runtime processes the request — at its next start or its next daily
          hygiene tick. Vault markdown is never deleted.
        </p>
        <p className="mt-2 text-sm">
          Type <span className="font-mono text-state-error">DELETE</span> to confirm. This action is
          irreversible.
        </p>
        <div className="mt-3">
          <TextInput value={wipeInput} onChange={setWipeInput} placeholder="DELETE" />
        </div>
        {wipeError && <p className="mt-2 text-xs text-state-error">{wipeError}</p>}
      </Modal>
    </div>
  );
}
