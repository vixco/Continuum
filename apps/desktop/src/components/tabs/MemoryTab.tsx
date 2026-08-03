"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { FolderOpen, Plus, RefreshCw, Search } from "lucide-react";

import { continuum, onMemoryEvent } from "@/lib/tauri";
import { NODE_COLORS, NODE_TYPE_LABELS } from "@/lib/memoryTheme";
import { MemoryGraph } from "@/components/memory/MemoryGraph";
import { NotePanel } from "@/components/memory/NotePanel";
import { NoteEditorOverlay, type OverlayMode } from "@/components/memory/NoteEditorOverlay";
import { Button, Card, EmptyState, SearchInput } from "@/components/ui/primitives";
import type {
  MemoryGraphData,
  MemoryGraphFilter,
  MemoryNodeType,
  MemoryVaultInfo,
} from "@/lib/types";

const EMPTY_GRAPH: MemoryGraphData = { nodes: [], edges: [], ghosts: [], truncated: false };
const ALL_TYPES = Object.keys(NODE_TYPE_LABELS) as MemoryNodeType[];

export function MemoryTab() {
  const [filter, setFilter] = useState<MemoryGraphFilter>({});
  const [graph, setGraph] = useState<MemoryGraphData>(EMPTY_GRAPH);
  const [info, setInfo] = useState<MemoryVaultInfo | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [overlayMode, setOverlayMode] = useState<OverlayMode | null>(null);
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const refreshTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const refresh = useCallback(async () => {
    // allSettled, not all: a failed vault-info fetch must not discard a
    // successful graph fetch (or vice versa) and blank out an otherwise
    // valid, already-rendered graph behind the full-bleed error card.
    const [graphResult, infoResult] = await Promise.allSettled([
      continuum.memoryGraph(filter),
      continuum.memoryVaultInfo(),
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
            placeholder="Zoek in geheugen…"
            className="pl-8"
          />
        </div>
        <Button size="sm" variant="primary" onClick={submitSearch}>
          Zoek
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
                  ? "border-accent-purple/60 bg-accent-purple/15 text-ink"
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
              ? "border-accent-purple/60 text-ink"
              : "border-bg-border text-ink-muted hover:text-ink")
          }
        >
          Toon verborgen
        </button>
        <div className="flex-1" />
        {(info?.quarantined.length ?? 0) > 0 && (
          <span className="rounded-md border border-state-warn/40 bg-state-warn/10 px-2 py-1 text-xs text-state-warn">
            {info?.quarantined.length} bestand(en) in quarantaine
          </span>
        )}
        {graph.truncated && (
          <span className="text-xs text-ink-dim">graph afgekapt — verfijn je filters</span>
        )}
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
      </div>

      {/* Graph body */}
      <div className="relative min-h-0 flex-1">
        <MemoryGraph
          data={graph}
          selectedId={selectedId}
          dimIds={null}
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
        {loadError && (
          <div className="absolute inset-0 flex items-center justify-center">
            <Card title="Memory niet beschikbaar" className="max-w-md">
              <p className="text-sm text-ink-muted">{loadError}</p>
              <div className="mt-3">
                <Button variant="primary" onClick={() => void refresh()}>
                  Opnieuw proberen
                </Button>
              </div>
            </Card>
          </div>
        )}
        {!loading && !loadError && graph.nodes.length === 0 && (
          <div className="absolute inset-0 flex items-center justify-center">
            <Card title="Nog geen memories" className="max-w-md">
              <EmptyState
                title="Je vault is leeg"
                description="Continuum bewaart hier alles wat het over je werk leert — als gewone markdown-bestanden die je zelf mag bewerken."
              />
              <div className="mt-2 flex justify-center gap-2">
                <Button onClick={() => void continuum.memoryOpenVault()}>
                  <FolderOpen size={13} /> Open vault-map
                </Button>
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
    </div>
  );
}
