"use client";

import { useEffect, useMemo, useState } from "react";
import { clsx } from "clsx";
import { AlertTriangle, Download, Search, Trash2 } from "lucide-react";

import { useStore } from "@/lib/store";
import { kairo } from "@/lib/tauri";
import {
  Button,
  Card,
  EmptyState,
  Modal,
  SearchInput,
  TextInput,
} from "@/components/ui/primitives";
import type { SemanticFact } from "@/lib/types";

type SubTab = "raw" | "episodic" | "semantic";

export function MemoryTab() {
  const [sub, setSub] = useState<SubTab>("raw");
  const mem = useStore((s) => s.state.memory);

  return (
    <div className="mx-auto max-w-6xl space-y-6">
      <Card title="Memory overview">
        <div className="grid grid-cols-3 gap-4 text-center">
          <Stat label="Raw log rows" value={mem.raw_log_rows} />
          <Stat label="Episodic memories" value={mem.episodic_count} />
          <Stat label="Semantic facts" value={mem.semantic_count} />
        </div>
      </Card>

      <div className="flex items-center gap-2 border-b border-bg-border">
        {(["raw", "episodic", "semantic"] as SubTab[]).map((t) => (
          <button
            key={t}
            onClick={() => setSub(t)}
            className={clsx(
              "border-b-2 px-3 py-2 text-sm capitalize transition-colors",
              sub === t
                ? "border-accent-purple text-ink"
                : "border-transparent text-ink-muted hover:text-ink"
            )}
          >
            {t === "raw" ? "Raw log" : t}
          </button>
        ))}
      </div>

      {sub === "raw" && <RawLogPanel />}
      {sub === "episodic" && <EpisodicPanel />}
      {sub === "semantic" && <SemanticPanel />}
    </div>
  );
}

function Stat({ label, value }: { label: string; value: number }) {
  return (
    <div>
      <div className="text-[11px] uppercase tracking-wider text-ink-dim">{label}</div>
      <div className="mt-1 font-mono text-2xl text-ink">{value.toLocaleString()}</div>
    </div>
  );
}

function RawLogPanel() {
  return (
    <Card
      title="Raw perception log"
      subtitle="one row per frame — screen, audio, context, triage"
      actions={
        <Button size="sm" variant="ghost">
          <Download size={12} /> Export NDJSON
        </Button>
      }
    >
      <EmptyState
        title="Raw log browsing is limited"
        description="Full timeline view requires the kairo runtime to be running. It ships with Phase 6.5."
      />
    </Card>
  );
}

function EpisodicPanel() {
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<unknown[]>([]);
  const [loading, setLoading] = useState(false);

  async function search() {
    setLoading(true);
    try {
      const r = await kairo.searchEpisodic(query, 20);
      setResults(r);
    } finally {
      setLoading(false);
    }
  }

  return (
    <Card title="Episodic memories" subtitle="vector-searchable summaries of past events">
      <div className="mb-3 flex items-center gap-2">
        <div className="relative flex-1">
          <Search size={14} className="pointer-events-none absolute left-2.5 top-2 text-ink-dim" />
          <SearchInput
            value={query}
            onChange={setQuery}
            placeholder="Search memories…"
            className="pl-8"
          />
        </div>
        <Button size="sm" variant="primary" onClick={search} disabled={loading}>
          Search
        </Button>
      </div>
      {results.length === 0 ? (
        <EmptyState
          title="No results"
          description="Episodic search is wired through kairo-mcp. Run the main runtime to populate it."
        />
      ) : (
        <ul className="space-y-2">
          {results.map((r, idx) => (
            <li key={idx} className="rounded-md border border-bg-border bg-bg-elevated p-3 text-sm">
              {JSON.stringify(r)}
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

function SemanticPanel() {
  const [facts, setFacts] = useState<SemanticFact[]>([]);
  const [newKey, setNewKey] = useState("");
  const [newValue, setNewValue] = useState("");
  const [confirmWipe, setConfirmWipe] = useState(false);
  const [wipeInput, setWipeInput] = useState("");

  useEffect(() => {
    void kairo.listSemantic().then(setFacts);
  }, []);

  const grouped = useMemo(() => {
    const g = new Map<string, SemanticFact[]>();
    for (const f of facts) {
      const list = g.get(f.namespace) ?? [];
      list.push(f);
      g.set(f.namespace, list);
    }
    return Array.from(g.entries());
  }, [facts]);

  async function addFact() {
    if (!newKey.trim() || !newValue.trim()) return;
    await kairo.setSemantic(newKey.trim(), newValue.trim());
    setNewKey("");
    setNewValue("");
    const fresh = await kairo.listSemantic();
    setFacts(fresh);
  }

  return (
    <>
      <Card title="Semantic facts" subtitle="long-term key/value memory">
        <div className="mb-4 grid grid-cols-[1fr,2fr,auto] gap-2">
          <TextInput value={newKey} onChange={setNewKey} placeholder="namespace.key" />
          <TextInput value={newValue} onChange={setNewValue} placeholder="value" />
          <Button onClick={addFact} variant="primary">
            Add
          </Button>
        </div>

        {grouped.length === 0 ? (
          <EmptyState
            title="No semantic facts"
            description="Run the kairo runtime and let Kairo observe for a while, or add facts manually above."
          />
        ) : (
          <div className="space-y-4">
            {grouped.map(([ns, items]) => (
              <div key={ns}>
                <div className="mb-1 text-[11px] font-medium uppercase tracking-wider text-ink-dim">
                  {ns}
                </div>
                <table className="w-full text-sm">
                  <tbody>
                    {items.map((f) => (
                      <tr key={f.key} className="border-b border-bg-border last:border-none">
                        <td className="py-1.5 pr-3 font-mono text-xs text-ink-muted">{f.key}</td>
                        <td className="py-1.5 pr-3 text-ink">{f.value}</td>
                        <td className="w-20 py-1.5 text-right">
                          <button
                            className="text-state-error hover:opacity-80"
                            onClick={async () => {
                              await kairo.deleteSemantic(f.key);
                              setFacts(await kairo.listSemantic());
                            }}
                          >
                            <Trash2 size={13} />
                          </button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            ))}
          </div>
        )}
      </Card>

      <Card title="Danger zone" subtitle="deletes every raw, episodic and semantic memory">
        <div className="flex items-center justify-between rounded-md border border-state-error/30 bg-state-error/5 p-3">
          <div className="flex items-start gap-2 text-sm">
            <AlertTriangle size={14} className="mt-0.5 shrink-0 text-state-error" />
            <div>
              <div className="font-medium">Wipe all memory</div>
              <div className="text-xs text-ink-muted">
                This cannot be undone. Semantic facts you rely on (routine, user, project) will be
                gone.
              </div>
            </div>
          </div>
          <Button variant="danger" onClick={() => setConfirmWipe(true)}>
            Wipe
          </Button>
        </div>
      </Card>

      <Modal
        open={confirmWipe}
        onClose={() => {
          setConfirmWipe(false);
          setWipeInput("");
        }}
        title="Wipe all memory"
        footer={
          <>
            <Button variant="ghost" onClick={() => setConfirmWipe(false)}>
              Cancel
            </Button>
            <Button
              variant="danger"
              disabled={wipeInput !== "DELETE"}
              onClick={async () => {
                await kairo.wipeMemory(wipeInput);
                setConfirmWipe(false);
                setWipeInput("");
                setFacts(await kairo.listSemantic());
              }}
            >
              Confirm wipe
            </Button>
          </>
        }
      >
        <p className="text-sm">
          Type <span className="font-mono text-state-error">DELETE</span> to confirm the wipe. This
          action is irreversible.
        </p>
        <div className="mt-3">
          <TextInput value={wipeInput} onChange={setWipeInput} placeholder="DELETE" />
        </div>
      </Modal>
    </>
  );
}
