"use client";

// Chat tab: conversations with any connected provider (Settings -> Integrations),
// streamed token-by-token over the `continuum:chat` Tauri event. See
// apps/desktop/src-tauri/src/chat.rs for the backend half of this contract.

import { useCallback, useEffect, useRef, useState } from "react";
import { clsx } from "clsx";
import { Plus, Square, Trash2 } from "lucide-react";
import ReactMarkdown from "react-markdown";

import { continuum, isTauri, onChatEvent } from "@/lib/tauri";
import { Button, EmptyState, Select, TextInput } from "@/components/ui/primitives";
import type {
  ChatEventPayload,
  Conversation,
  ConversationSummary,
  ProviderConnection,
  StoredMessage,
} from "@/lib/types";

// No @tailwindcss/typography in this repo - style the handful of markdown
// elements assistant replies actually use by hand instead of pulling in the
// plugin for one component.
const MARKDOWN_CLASSES =
  "break-words text-sm leading-relaxed " +
  "[&_p:not(:last-child)]:mb-2 " +
  "[&_ul]:my-1 [&_ul]:list-disc [&_ul]:pl-5 [&_ol]:my-1 [&_ol]:list-decimal [&_ol]:pl-5 [&_li]:mb-0.5 " +
  "[&_a]:text-accent-amber [&_a]:underline [&_strong]:font-semibold " +
  "[&_pre]:overflow-x-auto [&_pre]:rounded [&_pre]:bg-black/40 [&_pre]:p-2 [&_code]:text-xs";

/** First provider with a saved default model, else its first discovered
 * model, else empty (a free-text model can be typed in afterward). */
function defaultProviderModel(
  providers: ProviderConnection[]
): { providerId: string; model: string } | null {
  if (providers.length === 0) return null;
  const target = providers.find((p) => p.default_model) ?? providers[0];
  return { providerId: target.id, model: target.default_model ?? target.models[0] ?? "" };
}

export function ChatTab() {
  const [tauriReady, setTauriReady] = useState<boolean | null>(null);

  useEffect(() => {
    void isTauri().then(setTauriReady);
  }, []);

  if (tauriReady === null) return <div className="h-full" />;

  if (!tauriReady) {
    return (
      <div className="flex h-full items-center justify-center">
        <EmptyState
          title="Chat needs the desktop app"
          description="Run the Tauri app (pnpm tauri dev, or the packaged build) - Chat streams through the desktop backend, not the Next.js dev server."
        />
      </div>
    );
  }

  return <ChatWorkspace />;
}

function ChatWorkspace() {
  const [providers, setProviders] = useState<ProviderConnection[]>([]);
  const [conversations, setConversations] = useState<ConversationSummary[]>([]);
  const [listLoaded, setListLoaded] = useState(false);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [activeConv, setActiveConv] = useState<Conversation | null>(null);
  // Per-conversation in-flight stream text. A key's presence means that
  // conversation is currently streaming - even if it isn't the one on
  // screen right now. See the `continuum:chat` subscription below.
  const [streamBuffers, setStreamBuffers] = useState<Record<string, string>>({});
  // Errors about a specific, already-open chat session (send failures,
  // stream errors, reload failures, model-switch failures) are keyed by
  // conversation id so a background conversation's failure can never
  // clobber the banner of whatever conversation is actually on screen.
  // `globalError` is the separate, single slot for failures that aren't
  // about any one conversation (listing conversations, creating a new
  // one, deleting one from the sidebar).
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [globalError, setGlobalError] = useState<string | null>(null);
  // Only ever set for a *send* failure (chatSendMessage rejecting, or a
  // retryable stream `error` event for a conversation whose last outgoing
  // text we remember), and always scoped to the conversation the failed
  // send belongs to - so Retry can never fire into the wrong chat, or
  // linger next to an unrelated (load/delete/model-switch) error.
  const [retryInfo, setRetryInfo] = useState<{ conversationId: string; text: string } | null>(null);
  const [composerText, setComposerText] = useState("");
  const [sendingId, setSendingId] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);

  // The `continuum:chat` subscription below is mounted once and never
  // re-subscribes, so it can't close over `activeId` state directly - by
  // the time an event arrives the closure would see whatever `activeId`
  // was at mount time. A ref sidesteps that stale-closure trap.
  const activeIdRef = useRef<string | null>(null);
  // Last text sent per conversation, so a mid-stream `error` event (which
  // carries no text of its own) can still populate `retryInfo` correctly.
  const lastSentTextRef = useRef<Record<string, string>>({});
  // Tauri does not order `continuum:chat` events against the resolution of
  // the `chatSendMessage` invoke - a fast stream (or an immediate
  // pre-stream error) can emit its `done`/`error` event, which deletes this
  // conversation's stream buffer entry below, before the invoke's `.then`
  // in `sendMessage` runs. If that `.then` then unconditionally re-seeds
  // the buffer, the conversation looks like it's streaming forever with
  // nothing left to ever clear it. This set tracks conversation ids whose
  // stream has already terminated so that seed can no-op instead; it's
  // cleared for a conversation at the start of every new send.
  const finishedStreamsRef = useRef<Set<string>>(new Set());
  const threadRef = useRef<HTMLDivElement>(null);

  const activeBuffer = activeId != null ? streamBuffers[activeId] : undefined;
  const streaming = activeBuffer !== undefined;
  const sendingActive = sendingId !== null && sendingId === activeId;

  useEffect(() => {
    activeIdRef.current = activeId;
  }, [activeId]);

  // Sets a conversation-scoped error. `retry` defaults to null, which also
  // clears out any stale retry left over from an earlier, unrelated send
  // failure *for this same conversation* - so the Retry button can never
  // appear next to a message it doesn't apply to. Only the two
  // send-failure call sites pass a real `retry`.
  const showConversationError = useCallback(
    (
      id: string,
      message: string,
      retry: { conversationId: string; text: string } | null = null
    ) => {
      setErrors((e) => ({ ...e, [id]: message }));
      setRetryInfo((r) => (retry ? retry : r?.conversationId === id ? null : r));
    },
    []
  );

  // Sets the non-conversation error slot (listing/creating conversations,
  // i.e. failures that aren't about any one already-open chat session).
  // Never carries a retry.
  const showGlobalError = useCallback((message: string) => {
    setGlobalError(message);
  }, []);

  const loadConversations = useCallback(async () => {
    try {
      const list = await continuum.chatListConversations();
      setConversations(list);
      setGlobalError(null);
      return list;
    } catch (e) {
      showGlobalError(e instanceof Error ? e.message : String(e));
      return [];
    }
  }, [showGlobalError]);

  const loadActive = useCallback(
    async (id: string) => {
      try {
        const conv = await continuum.chatGetConversation(id);
        if (activeIdRef.current === id) setActiveConv(conv);
      } catch (e) {
        // Not gated on `activeIdRef.current === id`: the error belongs to
        // conversation `id` regardless of whether the user has since
        // switched away, so it's still correct to record it in that
        // conversation's own scoped slot for whenever they look at it
        // again. Only the *visible thread* (`setActiveConv` above) should
        // stay pinned to whatever's actually still selected.
        showConversationError(id, e instanceof Error ? e.message : String(e));
      }
    },
    [showConversationError]
  );

  // Initial load: providers + conversation list, then select the most
  // recently updated conversation (backend already sorts newest-first).
  useEffect(() => {
    void (async () => {
      const [provs, list] = await Promise.all([
        continuum.providersList().catch((e) => {
          showGlobalError(e instanceof Error ? e.message : String(e));
          return [] as ProviderConnection[];
        }),
        loadConversations(),
      ]);
      setProviders(provs);
      setListLoaded(true);
      if (list.length > 0) {
        activeIdRef.current = list[0].id;
        setActiveId(list[0].id);
        void loadActive(list[0].id);
      }
    })();
  }, [loadConversations, loadActive, showGlobalError]);

  // Streaming subscription - mounted once for the lifetime of the tab.
  // Buffers by conversation id regardless of which one is active, so
  // switching away and back doesn't lose in-progress text or leave a
  // stale-enabled composer on a conversation that's still generating.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;
    void onChatEvent((payload: ChatEventPayload) => {
      const id = payload.conversation_id;
      const ev = payload.event;

      if (ev.type === "delta") {
        setStreamBuffers((b) => ({ ...b, [id]: (b[id] ?? "") + ev.text }));
        return;
      }

      // Both done/error clear this conversation's buffer, refresh the
      // sidebar (title/updated_at can change on the first turn), and pull
      // the full conversation if it's the one currently on screen - the
      // assistant message (or aborted partial) is already persisted by
      // the time either event lands.
      //
      // Mark the stream terminated before touching the buffer, so the
      // seed in `sendMessage` (see the ref comment above) can never
      // re-open a buffer entry we're about to delete.
      finishedStreamsRef.current.add(id);
      setStreamBuffers((b) => {
        if (!(id in b)) return b;
        const next = { ...b };
        delete next[id];
        return next;
      });
      if (activeIdRef.current === id) void loadActive(id);
      void loadConversations();

      if (ev.type === "done") {
        setRetryInfo((r) => (r?.conversationId === id ? null : r));
      } else {
        // Only offer Retry when the backend says this failure is worth
        // retrying - and only then do we need last-sent text at all.
        const text = lastSentTextRef.current[id];
        const retry = ev.retryable && text ? { conversationId: id, text } : null;
        showConversationError(id, ev.message, retry);
      }
    }).then((u) => {
      // The effect's cleanup can fire before this promise settles (fast
      // unmount, React StrictMode double-invoke) - if it already has,
      // unlisten immediately instead of stashing a handle nobody will
      // ever call.
      if (disposed) {
        u();
      } else {
        unlisten = u;
      }
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [loadActive, loadConversations, showConversationError]);

  // Autoscroll the thread as new messages arrive or text streams in.
  useEffect(() => {
    const el = threadRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [activeConv?.messages.length, activeBuffer]);

  function selectConversation(id: string) {
    if (id === activeId) return;
    activeIdRef.current = id;
    setActiveId(id);
    setActiveConv(null);
    void loadActive(id);
  }

  async function newChat() {
    const pick = defaultProviderModel(providers);
    if (!pick) return;
    setCreating(true);
    setGlobalError(null);
    try {
      const conv = await continuum.chatCreateConversation(pick.providerId, pick.model);
      await loadConversations();
      activeIdRef.current = conv.id;
      setActiveId(conv.id);
      setActiveConv(conv);
    } catch (e) {
      showGlobalError(e instanceof Error ? e.message : String(e));
    } finally {
      setCreating(false);
    }
  }

  function handleDeleteClick(id: string) {
    if (confirmDeleteId === id) {
      void doDelete(id);
    } else {
      setConfirmDeleteId(id);
    }
  }

  async function doDelete(id: string) {
    setDeletingId(id);
    try {
      await continuum.chatDeleteConversation(id);
      setStreamBuffers((b) => {
        if (!(id in b)) return b;
        const next = { ...b };
        delete next[id];
        return next;
      });
      // Dead state for a conversation that no longer exists - drop its
      // scoped error and any pending retry pointed at it.
      setErrors((e) => {
        if (!(id in e)) return e;
        const next = { ...e };
        delete next[id];
        return next;
      });
      setRetryInfo((r) => (r?.conversationId === id ? null : r));
      delete lastSentTextRef.current[id];
      const list = await loadConversations();
      if (activeId === id) {
        const next = list[0] ?? null;
        activeIdRef.current = next?.id ?? null;
        setActiveId(next?.id ?? null);
        setActiveConv(null);
        if (next) void loadActive(next.id);
      }
    } catch (e) {
      // Deleting is a sidebar/list action the user takes regardless of
      // which conversation is currently open, so - like loadConversations
      // and newChat - a failure here goes to the global slot rather than
      // a conversation-scoped one that might never be looked at again.
      showGlobalError(e instanceof Error ? e.message : String(e));
    } finally {
      setConfirmDeleteId(null);
      setDeletingId(null);
    }
  }

  async function updateModel(providerId: string, model: string) {
    if (!activeConv) return;
    const id = activeConv.id;
    setActiveConv((c) => (c ? { ...c, provider_id: providerId, model } : c));
    try {
      await continuum.chatSetConversationModel(id, providerId, model);
      void loadConversations();
    } catch (e) {
      showConversationError(id, e instanceof Error ? e.message : String(e));
    }
  }

  async function sendMessage(id: string, text: string) {
    const trimmed = text.trim();
    if (!trimmed) return;
    lastSentTextRef.current[id] = trimmed;
    setErrors((e) => {
      if (!(id in e)) return e;
      const next = { ...e };
      delete next[id];
      return next;
    });
    setSendingId(id);
    // This is a new send for `id` - any earlier termination recorded for
    // it is stale. Clear it before the invoke so the seed below (once the
    // invoke resolves) is judged against this send's outcome, not a
    // previous one's.
    finishedStreamsRef.current.delete(id);
    try {
      await continuum.chatSendMessage(id, trimmed);
      // This send succeeded - any earlier failed send for this same
      // conversation is no longer worth retrying.
      setRetryInfo((r) => (r?.conversationId === id ? null : r));
      // Seed the buffer so the pulse bubble shows before the first delta.
      // Tauri doesn't order events vs. this invoke's response, so two
      // things can already have happened by the time we get here: (a)
      // deltas may have already landed - preserve them (`b[id] ?? ""`,
      // never clobber with a bare ""), and (b) the stream may have
      // already finished - skip seeding entirely then, or we'd resurrect
      // a buffer key nothing will ever delete again.
      if (!finishedStreamsRef.current.has(id)) {
        setStreamBuffers((b) => ({ ...b, [id]: b[id] ?? "" }));
      }
      // The user message is persisted before this resolves - reload now
      // rather than waiting for the first delta, but only if this is
      // still the conversation on screen.
      if (activeIdRef.current === id) await loadActive(id);
    } catch (e) {
      showConversationError(id, e instanceof Error ? e.message : String(e), {
        conversationId: id,
        text: trimmed,
      });
    } finally {
      setSendingId((cur) => (cur === id ? null : cur));
    }
  }

  function handleComposerSend() {
    if (streaming || sendingActive || !composerText.trim() || !activeId) return;
    const text = composerText;
    setComposerText("");
    void sendMessage(activeId, text);
  }

  function stop() {
    if (activeId) void continuum.chatCancel(activeId);
  }

  function retry() {
    if (!retryInfo) return;
    void sendMessage(retryInfo.conversationId, retryInfo.text);
  }

  const activeProvider = providers.find((p) => p.id === activeConv?.provider_id) ?? null;

  return (
    <div className="flex h-full min-h-0">
      <aside className="flex w-64 shrink-0 flex-col border-r border-bg-border">
        <div className="p-3">
          <Button
            variant="primary"
            className="w-full"
            onClick={() => void newChat()}
            disabled={creating || providers.length === 0}
          >
            <Plus size={14} /> New chat
          </Button>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
          {providers.length === 0 ? (
            <EmptyState
              title="No providers connected"
              description="Open Settings -> Integrations to connect a model before starting a chat."
            />
          ) : !listLoaded ? (
            <div className="py-6 text-center text-xs text-ink-dim">Loading...</div>
          ) : conversations.length === 0 ? (
            <div className="px-2 py-6 text-center text-xs text-ink-dim">No conversations yet.</div>
          ) : (
            <ul className="space-y-0.5">
              {conversations.map((c) => (
                <li key={c.id} className="flex items-center gap-1">
                  <button
                    type="button"
                    onClick={() => selectConversation(c.id)}
                    aria-current={c.id === activeId ? "true" : undefined}
                    className={clsx(
                      "min-w-0 flex-1 truncate rounded-md px-2.5 py-2 text-left text-xs transition-colors",
                      c.id === activeId
                        ? "bg-accent-amber/10 text-ink"
                        : "text-ink-muted hover:bg-bg-hover hover:text-ink"
                    )}
                  >
                    {c.title}
                  </button>
                  <button
                    type="button"
                    aria-label={
                      confirmDeleteId === c.id
                        ? "Confirm delete conversation"
                        : "Delete conversation"
                    }
                    disabled={deletingId === c.id}
                    onClick={() => handleDeleteClick(c.id)}
                    className={clsx(
                      "shrink-0 rounded p-1.5 text-ink-dim transition-colors hover:text-red-300 disabled:opacity-50",
                      confirmDeleteId === c.id && "text-red-300"
                    )}
                  >
                    <Trash2 size={12} />
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      </aside>

      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        {!activeConv ? (
          <div className="flex flex-1 items-center justify-center">
            <EmptyState
              title={
                providers.length === 0 ? "Connect a provider to start" : "No conversation selected"
              }
              description={
                providers.length === 0
                  ? "Open Settings -> Integrations to add a model."
                  : "Pick a conversation on the left, or start a new chat."
              }
            />
          </div>
        ) : (
          <>
            <header className="flex items-center justify-between gap-3 border-b border-bg-border px-4 py-3">
              <div className="min-w-0 flex-1 truncate text-sm font-medium text-ink">
                {activeConv.title}
              </div>
              <div className="flex shrink-0 items-center gap-2">
                <div className="w-36">
                  <Select
                    aria-label="Provider"
                    value={activeConv.provider_id}
                    options={providers.map((p) => ({ value: p.id, label: p.display_name }))}
                    onChange={(providerId) => {
                      const target = providers.find((p) => p.id === providerId);
                      const model = target?.default_model ?? target?.models[0] ?? "";
                      void updateModel(providerId, model);
                    }}
                  />
                </div>
                <ModelField
                  provider={activeProvider}
                  value={activeConv.model}
                  onCommit={(model) => void updateModel(activeConv.provider_id, model)}
                />
              </div>
            </header>

            <div ref={threadRef} className="min-h-0 flex-1 space-y-3 overflow-y-auto p-4">
              {activeConv.messages.map((m, idx) => (
                <MessageBubble key={`${m.ts}-${idx}`} message={m} />
              ))}
              {streaming && (
                <div className="flex justify-start">
                  <div className="max-w-[85%] rounded-lg bg-bg-elevated px-3 py-2">
                    {activeBuffer && (
                      <div className={MARKDOWN_CLASSES}>
                        <ReactMarkdown>{activeBuffer}</ReactMarkdown>
                      </div>
                    )}
                    <span
                      className={clsx(
                        "inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-accent-amber",
                        activeBuffer && "ml-1"
                      )}
                    />
                  </div>
                </div>
              )}
            </div>

            {(errors[activeConv.id] ?? globalError) && (
              <div className="mx-4 mb-2 flex items-center justify-between gap-3 rounded-md bg-red-500/10 px-3 py-2 text-xs text-red-300">
                <span className="min-w-0 flex-1 truncate">
                  {errors[activeConv.id] ?? globalError}
                </span>
                {retryInfo && retryInfo.conversationId === activeConv.id && (
                  <Button size="sm" variant="ghost" onClick={retry}>
                    Retry
                  </Button>
                )}
              </div>
            )}

            <div className="flex gap-2 border-t border-bg-border p-3">
              <textarea
                value={composerText}
                onChange={(e) => setComposerText(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    handleComposerSend();
                  }
                }}
                disabled={streaming || sendingActive}
                rows={Math.min(6, Math.max(1, composerText.split("\n").length))}
                placeholder="Message..."
                className="min-h-9 flex-1 resize-none rounded-md border border-bg-border bg-transparent px-3 py-2 text-sm text-ink outline-none placeholder:text-ink-dim focus:border-accent-amber disabled:opacity-50"
              />
              {streaming ? (
                <Button onClick={stop}>
                  <Square size={14} />
                </Button>
              ) : (
                <Button
                  onClick={handleComposerSend}
                  disabled={!composerText.trim() || sendingActive}
                >
                  Send
                </Button>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function MessageBubble({ message }: { message: StoredMessage }) {
  if (message.role === "user") {
    return (
      <div className="flex justify-end">
        <div className="max-w-[80%] whitespace-pre-wrap break-words rounded-lg bg-amber-500/10 px-3 py-2 text-sm text-ink">
          {message.content}
        </div>
      </div>
    );
  }

  const footer = [
    message.model,
    message.duration_ms != null ? `${(message.duration_ms / 1000).toFixed(1)}s` : null,
    message.aborted ? "stopped" : null,
  ].filter((part): part is string => Boolean(part));

  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] rounded-lg bg-bg-elevated px-3 py-2">
        <div className={MARKDOWN_CLASSES}>
          <ReactMarkdown>{message.content}</ReactMarkdown>
        </div>
        {footer.length > 0 && (
          <div className="mt-1 text-[10px] text-ink-muted">{footer.join(" · ")}</div>
        )}
      </div>
    </div>
  );
}

/** Compact model editor for the chat header: a `<Select>` over the
 * provider's discovered models, or a free-text field when it has none
 * (Claude Code CLI, or a freshly-added connection with an empty cache). */
function ModelField({
  provider,
  value,
  onCommit,
}: {
  provider: ProviderConnection | null;
  value: string;
  onCommit: (model: string) => void;
}) {
  const [draft, setDraft] = useState(value);

  useEffect(() => {
    setDraft(value);
  }, [value]);

  function commit() {
    const trimmed = draft.trim();
    if (trimmed && trimmed !== value) onCommit(trimmed);
  }

  if (!provider || provider.kind === "claude_cli" || provider.models.length === 0) {
    return (
      <div className="w-36">
        <TextInput
          aria-label="Model"
          value={draft}
          onChange={setDraft}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") commit();
          }}
          placeholder="model name"
        />
      </div>
    );
  }

  return (
    <div className="w-36">
      <Select
        aria-label="Model"
        value={value || provider.models[0]}
        options={provider.models.map((m) => ({ value: m, label: m }))}
        onChange={onCommit}
      />
    </div>
  );
}
