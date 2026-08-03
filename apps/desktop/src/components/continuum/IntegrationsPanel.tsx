"use client";

import { useCallback, useEffect, useState } from "react";
import { clsx } from "clsx";
import { Plug, Plus, RefreshCcw, Trash2, Zap } from "lucide-react";

import { continuum } from "@/lib/tauri";
import type {
  CatalogEntry,
  ConnectionTestReport,
  ProviderConnection,
  ProviderKind,
} from "@/lib/types";
import { Button, Card, EmptyState, Modal, Select, TextInput } from "@/components/ui/primitives";

const KIND_LABEL: Record<ProviderKind, string> = {
  open_ai_compat: "OpenAI-compatible",
  anthropic: "Anthropic API",
  claude_cli: "Claude Code CLI",
};

/** Feedback shown inline under a single provider row after Test/Refresh/Remove. */
type RowMessage = { kind: "ok" | "error"; text: string };

const EMPTY_FORM = { name: "", baseUrl: "", apiKey: "" };

export function IntegrationsPanel() {
  const [providers, setProviders] = useState<ProviderConnection[]>([]);
  const [catalog, setCatalog] = useState<CatalogEntry[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<Record<string, boolean>>({});
  const [rowMessage, setRowMessage] = useState<Record<string, RowMessage>>({});
  const [confirmRemoveId, setConfirmRemoveId] = useState<string | null>(null);

  // "Add provider" modal state.
  const [modalOpen, setModalOpen] = useState(false);
  const [selectedPreset, setSelectedPreset] = useState<CatalogEntry | null>(null);
  const [form, setForm] = useState(EMPTY_FORM);
  const [testFailed, setTestFailed] = useState(false);
  const [adding, setAdding] = useState(false);
  const [modalError, setModalError] = useState<string | null>(null);

  const loadProviders = useCallback(async () => {
    try {
      setProviders(await continuum.providersList());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void (async () => {
      try {
        const [cat, provs] = await Promise.all([
          continuum.catalogList(),
          continuum.providersList(),
        ]);
        setCatalog(cat);
        setProviders(provs);
        setError(null);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setLoaded(true);
      }
    })();
  }, []);

  const resetModal = useCallback(() => {
    setModalOpen(false);
    setSelectedPreset(null);
    setForm(EMPTY_FORM);
    setTestFailed(false);
    setModalError(null);
    setAdding(false);
  }, []);

  function openModal() {
    resetModal();
    setModalOpen(true);
  }

  function selectPreset(entry: CatalogEntry) {
    setSelectedPreset(entry);
    setForm({ name: entry.label, baseUrl: entry.default_base_url ?? "", apiKey: "" });
    setTestFailed(false);
    setModalError(null);
  }

  const showBaseUrl = selectedPreset?.kind !== "claude_cli";
  const isCustomPreset = selectedPreset?.id === "custom";
  const showApiKey = isCustomPreset || Boolean(selectedPreset?.needs_key);
  const canSubmit =
    selectedPreset !== null &&
    form.name.trim().length > 0 &&
    (!showBaseUrl || form.baseUrl.trim().length > 0);

  async function submit(saveAnyway: boolean) {
    if (!selectedPreset || !canSubmit) {
      setModalError("Fill in the required fields first.");
      return;
    }
    setAdding(true);
    setModalError(null);
    try {
      await continuum.providerAdd({
        catalog_id: selectedPreset.id,
        display_name: form.name.trim(),
        base_url: showBaseUrl ? form.baseUrl.trim() || null : null,
        api_key: form.apiKey.trim() || null,
        save_anyway: saveAnyway,
      });
      resetModal();
      await loadProviders();
    } catch (e) {
      setModalError(e instanceof Error ? e.message : String(e));
      setTestFailed(true);
    } finally {
      setAdding(false);
    }
  }

  async function handleTest(id: string) {
    setBusy((b) => ({ ...b, [`${id}:test`]: true }));
    try {
      const report: ConnectionTestReport = await continuum.providerTest(id);
      setRowMessage((m) => ({
        ...m,
        [id]: {
          kind: report.ok ? "ok" : "error",
          text: `${report.detail} (${report.latency_ms}ms)`,
        },
      }));
    } catch (e) {
      setRowMessage((m) => ({
        ...m,
        [id]: { kind: "error", text: e instanceof Error ? e.message : String(e) },
      }));
    } finally {
      setBusy((b) => ({ ...b, [`${id}:test`]: false }));
      await loadProviders();
    }
  }

  async function handleRefresh(id: string) {
    setBusy((b) => ({ ...b, [`${id}:refresh`]: true }));
    try {
      const models = await continuum.providerRefreshModels(id);
      setRowMessage((m) => ({
        ...m,
        [id]: {
          kind: "ok",
          text: `Refreshed ${models.length} model${models.length === 1 ? "" : "s"}.`,
        },
      }));
    } catch (e) {
      setRowMessage((m) => ({
        ...m,
        [id]: { kind: "error", text: e instanceof Error ? e.message : String(e) },
      }));
    } finally {
      setBusy((b) => ({ ...b, [`${id}:refresh`]: false }));
      await loadProviders();
    }
  }

  function handleRemoveClick(id: string) {
    if (confirmRemoveId === id) {
      void doRemove(id);
    } else {
      setConfirmRemoveId(id);
    }
  }

  async function doRemove(id: string) {
    setBusy((b) => ({ ...b, [`${id}:remove`]: true }));
    try {
      await continuum.providerRemove(id);
    } catch (e) {
      setRowMessage((m) => ({
        ...m,
        [id]: { kind: "error", text: e instanceof Error ? e.message : String(e) },
      }));
    } finally {
      setConfirmRemoveId(null);
      setBusy((b) => ({ ...b, [`${id}:remove`]: false }));
      await loadProviders();
    }
  }

  async function handleSetDefaultModel(id: string, model: string) {
    setBusy((b) => ({ ...b, [`${id}:model`]: true }));
    try {
      await continuum.providerSetDefaultModel(id, model);
    } catch (e) {
      setRowMessage((m) => ({
        ...m,
        [id]: { kind: "error", text: e instanceof Error ? e.message : String(e) },
      }));
    } finally {
      setBusy((b) => ({ ...b, [`${id}:model`]: false }));
      await loadProviders();
    }
  }

  return (
    <>
      <Card
        title="AI providers"
        subtitle="Connect local or cloud models for Chat."
        actions={
          <Button size="sm" variant="primary" onClick={openModal}>
            <Plus size={12} /> Add provider
          </Button>
        }
      >
        {error && (
          <div className="mb-3 rounded-md border border-state-error/30 bg-state-error/10 px-3 py-2 text-xs text-state-error">
            {error}
          </div>
        )}

        {!loaded ? (
          <div className="py-6 text-center text-sm text-ink-dim">Loading providers…</div>
        ) : providers.length === 0 ? (
          <EmptyState
            title="No providers connected"
            description="Add LM Studio, Ollama, Claude Code, or an API key to start chatting."
          />
        ) : (
          <ul className="divide-y divide-bg-border">
            {providers.map((conn) => (
              <ProviderRow
                key={conn.id}
                conn={conn}
                testBusy={Boolean(busy[`${conn.id}:test`])}
                refreshBusy={Boolean(busy[`${conn.id}:refresh`])}
                removeBusy={Boolean(busy[`${conn.id}:remove`])}
                modelBusy={Boolean(busy[`${conn.id}:model`])}
                message={rowMessage[conn.id]}
                confirming={confirmRemoveId === conn.id}
                onTest={() => void handleTest(conn.id)}
                onRefresh={() => void handleRefresh(conn.id)}
                onRemoveClick={() => handleRemoveClick(conn.id)}
                onSetDefaultModel={(model) => void handleSetDefaultModel(conn.id, model)}
              />
            ))}
          </ul>
        )}
      </Card>

      <Modal
        open={modalOpen}
        onClose={() => !adding && resetModal()}
        title="Add provider"
        width="lg"
        footer={
          <>
            <Button size="sm" variant="ghost" onClick={resetModal} disabled={adding}>
              Cancel
            </Button>
            <Button
              size="sm"
              variant="primary"
              onClick={() => void submit(false)}
              disabled={adding || !canSubmit}
            >
              {adding ? "Testing…" : "Test & save"}
            </Button>
            {testFailed && (
              <Button
                size="sm"
                variant="danger"
                onClick={() => void submit(true)}
                disabled={adding || !canSubmit}
              >
                Save anyway
              </Button>
            )}
          </>
        }
      >
        <div className="space-y-4">
          <div>
            <div className="mb-1.5 text-xs text-ink-muted">Provider</div>
            <div className="grid grid-cols-2 gap-1.5 sm:grid-cols-3">
              {catalog.map((entry) => (
                <CatalogTile
                  key={entry.id}
                  entry={entry}
                  active={selectedPreset?.id === entry.id}
                  onClick={() => selectPreset(entry)}
                />
              ))}
            </div>
          </div>

          {selectedPreset && (
            <div className="space-y-3 border-t border-bg-border pt-3">
              <LabeledField label="Name">
                <TextInput
                  value={form.name}
                  onChange={(v) => setForm((f) => ({ ...f, name: v }))}
                  placeholder="Display name"
                />
              </LabeledField>
              {showBaseUrl && (
                <LabeledField label="Base URL">
                  <TextInput
                    value={form.baseUrl}
                    onChange={(v) => setForm((f) => ({ ...f, baseUrl: v }))}
                    placeholder="http://localhost:1234/v1"
                  />
                </LabeledField>
              )}
              {showApiKey && (
                <LabeledField label="API key">
                  <TextInput
                    type="password"
                    value={form.apiKey}
                    onChange={(v) => setForm((f) => ({ ...f, apiKey: v }))}
                    placeholder={selectedPreset.key_hint || "optional"}
                  />
                </LabeledField>
              )}
            </div>
          )}

          {modalError && <p className="text-xs text-state-error">{modalError}</p>}
        </div>
      </Modal>
    </>
  );
}

function ProviderRow({
  conn,
  testBusy,
  refreshBusy,
  removeBusy,
  modelBusy,
  message,
  confirming,
  onTest,
  onRefresh,
  onRemoveClick,
  onSetDefaultModel,
}: {
  conn: ProviderConnection;
  testBusy: boolean;
  refreshBusy: boolean;
  removeBusy: boolean;
  modelBusy: boolean;
  message: RowMessage | undefined;
  confirming: boolean;
  onTest: () => void;
  onRefresh: () => void;
  onRemoveClick: () => void;
  onSetDefaultModel: (model: string) => void;
}) {
  // A row's Test/Refresh/Remove are mutually exclusive - one in flight
  // should block the others so, e.g., Remove can't fire while a Test is
  // still resolving for the same connection.
  const rowBusy = testBusy || refreshBusy || removeBusy;
  return (
    <li className="flex flex-col gap-2 py-3 text-sm sm:flex-row sm:items-start sm:justify-between">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span
            className={clsx(
              "h-2 w-2 shrink-0 rounded-full",
              conn.last_test_ok === true && "bg-state-healthy",
              conn.last_test_ok === false && "bg-state-error",
              conn.last_test_ok === null && "bg-state-idle"
            )}
          />
          <span className="truncate font-medium text-ink">{conn.display_name}</span>
          <span className="shrink-0 text-xs text-ink-dim">{KIND_LABEL[conn.kind]}</span>
        </div>
        <div className="mt-0.5 truncate text-xs text-ink-muted">
          {conn.base_url ?? "local subprocess"} · {conn.models.length} model
          {conn.models.length === 1 ? "" : "s"}
        </div>
        {message && (
          <div
            className={clsx(
              "mt-1 text-xs",
              message.kind === "ok" ? "text-state-healthy" : "text-state-error"
            )}
          >
            {message.text}
          </div>
        )}
      </div>

      <div className="flex flex-wrap items-center gap-1.5">
        <ModelPicker conn={conn} busy={rowBusy || modelBusy} onSet={onSetDefaultModel} />
        <Button size="sm" variant="ghost" disabled={rowBusy} onClick={onTest}>
          <Zap size={12} /> {testBusy ? "Testing…" : "Test"}
        </Button>
        <Button size="sm" variant="ghost" disabled={rowBusy} onClick={onRefresh}>
          <RefreshCcw size={12} /> {refreshBusy ? "Refreshing…" : "Refresh"}
        </Button>
        <Button
          size="sm"
          variant={confirming ? "danger" : "ghost"}
          disabled={rowBusy}
          onClick={onRemoveClick}
        >
          <Trash2 size={12} /> {confirming ? "Really remove?" : "Remove"}
        </Button>
      </div>
    </li>
  );
}

/** Default-model editor for a row: a `<Select>` over the discovered model
 * list, or a free-text field when the provider has no discoverable models
 * (Claude Code CLI, or a freshly-added connection with an empty cache). */
function ModelPicker({
  conn,
  busy,
  onSet,
}: {
  conn: ProviderConnection;
  busy: boolean;
  onSet: (model: string) => void;
}) {
  const useFreeText = conn.kind === "claude_cli" || conn.models.length === 0;
  const [draft, setDraft] = useState(conn.default_model ?? "");

  useEffect(() => {
    setDraft(conn.default_model ?? "");
  }, [conn.default_model, conn.id]);

  function commit() {
    const trimmed = draft.trim();
    if (trimmed && trimmed !== conn.default_model) {
      onSet(trimmed);
    }
  }

  if (useFreeText) {
    return (
      <TextInput
        aria-label="Default model"
        value={draft}
        onChange={setDraft}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit();
        }}
        disabled={busy}
        placeholder="model name"
        className="w-36"
      />
    );
  }

  return (
    <div className="w-40">
      <Select
        aria-label="Default model"
        value={conn.default_model ?? conn.models[0]}
        options={conn.models.map((m) => ({ value: m, label: m }))}
        onChange={onSet}
        disabled={busy}
      />
    </div>
  );
}

function CatalogTile({
  entry,
  active,
  onClick,
}: {
  entry: CatalogEntry;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={clsx(
        "rounded-md border px-2.5 py-2 text-left text-xs transition-colors active:scale-[0.98]",
        active
          ? "border-accent-amber bg-accent-amber/15"
          : "border-bg-border bg-bg-elevated hover:border-bg-hover"
      )}
    >
      <div className="flex items-center gap-1.5 font-medium text-ink">
        <Plug size={11} className="text-ink-dim" />
        {entry.label}
      </div>
      {!entry.needs_key && <div className="mt-0.5 text-[10px] text-ink-dim">no key needed</div>}
    </button>
  );
}

function LabeledField({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="block">
      <span className="mb-1.5 block text-xs text-ink-muted">{label}</span>
      {children}
    </label>
  );
}
