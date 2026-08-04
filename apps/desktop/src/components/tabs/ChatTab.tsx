"use client";

// Chat tab — the redesigned ambient AI assistant surface.
//
// Design intent (matches the Linear-styled design system):
//   * Two-column layout: conversation list on the left, current
//     conversation on the right.
//   * Header: model selector (kept from the old tab so users can swap
//     models mid-conversation) + small new-chat button.
//   * Message list: virtualized (react-virtuoso). Streaming tail follows
//     the bottom of the scroll region; scrolling up pauses autoscroll.
//   * Each assistant message: model + duration + token usage footer,
//     expandable thinking blocks, tool cards with input/output JSON,
//     grouped tool fan-outs. User messages: ambient-amber bubble with
//     attachment chips above.
//   * Composer: multi-line auto-grow, slash menu, skill chips, file
//     attachments, voice toggle, send/stop.
//   * Empty state: one-line copy + kbd hints + clickable suggestions.
//   * No marketing visuals, no rounded-2xl, no animated avatars.

import { useEffect, useMemo, useState } from "react";
import { clsx } from "clsx";
import { Loader2, MessageSquare, PanelLeftClose, PanelLeftOpen, Plus, Trash2 } from "lucide-react";

import { Button, EmptyState, Kbd } from "@/components/ui/primitives";
import { isTauri } from "@/lib/tauri";
import type { ProviderConnection } from "@/lib/types";

import { useChatStore } from "../chat/state";
import { storedMessageParts } from "../chat/toolInvocations";
import { InputBar } from "../chat/InputBar";
import { MessageList } from "../chat/MessageList";
import { ChatEmptyState } from "../chat/EmptyState";
import { VoiceInputBubble } from "../chat/VoiceInputBubble";
import { ModelSwitcher } from "../chat/ModelSwitcher";
import type { ChatMessage, ContentPart } from "../chat/types";

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
  const bootstrap = useChatStore((s) => s.bootstrap);
  const conversations = useChatStore((s) => s.conversations);
  const conversationsLoaded = useChatStore((s) => s.conversationsLoaded);
  const activeId = useChatStore((s) => s.activeId);
  const activeConv = useChatStore((s) => s.activeConv);
  const providers = useChatStore((s) => s.providers);
  const skills = useChatStore((s) => s.skills);
  const sendingId = useChatStore((s) => s.sendingId);
  const streamBuffers = useChatStore((s) => s.streamBuffers);
  const streamToolCalls = useChatStore((s) => s.streamToolCalls);
  const errors = useChatStore((s) => s.errors);
  const globalError = useChatStore((s) => s.globalError);
  const retryInfo = useChatStore((s) => s.retryInfo);
  const voiceListening = useChatStore((s) => s.voiceListening);
  const voicePartial = useChatStore((s) => s.voicePartial);
  const beginVoice = useChatStore((s) => s.beginVoice);
  const endVoice = useChatStore((s) => s.endVoice);
  const selectConversation = useChatStore((s) => s.selectConversation);
  const newConversation = useChatStore((s) => s.newConversation);
  const deleteConversation = useChatStore((s) => s.deleteConversation);
  const setComposer = useChatStore((s) => s.setComposer);
  const retry = useChatStore((s) => s.retry);
  const setConversationModel = useChatStore((s) => s.setConversationModel);

  // Conversation list is collapsed by default so the chat pane gets the
  // full width. The user pops it open to switch/start conversations.
  const [conversationsOpen, setConversationsOpen] = useState(false);

  useEffect(() => {
    void bootstrap();
  }, [bootstrap]);

  const isStreaming = activeId != null && activeId in streamBuffers;
  const streamBuffer = activeId ? (streamBuffers[activeId] ?? "") : "";

  const messages: ChatMessage[] = useMemo(() => {
    if (!activeConv) return [];
    return activeConv.messages.map((m, i) => ({
      id: `m_${i}_${m.ts}`,
      role: m.role,
      parts: storedMessageParts(m),
      text: m.content,
      model: m.model,
      durationMs: m.duration_ms,
      status: m.aborted ? "aborted" : "complete",
      ts: m.ts,
      conversationId: activeConv.id,
      usage: m.usage ? { input: m.usage.input_tokens, output: m.usage.output_tokens } : null,
    }));
  }, [activeConv]);

  // The streaming tail: an ephemeral assistant message that lives only
  // while `streamBuffer` is non-empty. It is appended to the rendered
  // list (in MessageList) so the live cursor can scroll-along without
  // polluting the persisted history.
  const streamingMessage: ChatMessage | null = useMemo(() => {
    if (!isStreaming || !activeId) return null;
    // In-flight tool invocations render before the text tail, in arrival
    // order — matching how the persisted message will look after reload.
    const invocations = streamToolCalls[activeId] ?? [];
    const parts: ContentPart[] = [
      ...invocations.map((invocation): ContentPart => ({ kind: "tool", invocation })),
      { kind: "text", text: streamBuffer },
    ];
    return {
      id: "streaming_tail",
      role: "assistant",
      parts,
      text: streamBuffer,
      status: "streaming",
      ts: new Date().toISOString(),
      conversationId: activeId,
    };
  }, [isStreaming, activeId, streamBuffer, streamToolCalls]);

  const activeProvider = providers.find((p) => p.id === activeConv?.provider_id) ?? null;
  const activeError = activeId ? (errors[activeId] ?? globalError) : globalError;

  return (
    <div className="flex h-full min-h-0">
      <ConversationList
        conversations={conversations}
        conversationsLoaded={conversationsLoaded}
        activeId={activeId}
        providers={providers}
        onSelect={selectConversation}
        onDelete={deleteConversation}
        onNew={newConversation}
        collapsed={!conversationsOpen}
        onToggle={() => setConversationsOpen((v) => !v)}
      />
      <div className="flex min-h-0 min-w-0 flex-1 flex-col bg-bg">
        {activeConv ? (
          <>
            <ChatHeader
              providers={providers}
              activeConv={activeConv}
              onSelectModel={setConversationModel}
            />
            <div className="relative min-h-0 flex-1">
              <MessageList
                messages={messages}
                streamingMessage={streamingMessage}
                isStreaming={isStreaming}
              />
            </div>
            {activeError && (
              <div className="mx-3 mb-1 flex items-center justify-between gap-3 rounded-md border border-state-error/30 bg-state-error/[0.08] px-3 py-2 text-[11px] text-state-error">
                <span className="min-w-0 flex-1 truncate">{activeError}</span>
                {retryInfo && retryInfo.conversationId === activeConv.id && (
                  <Button size="sm" variant="default" onClick={() => void retry()}>
                    Retry
                  </Button>
                )}
              </div>
            )}
            <InputBar
              voiceListening={voiceListening}
              onToggleVoice={() => {
                if (voiceListening) {
                  endVoice();
                } else {
                  beginVoice();
                }
              }}
            />
            <AmbientFooter
              activeProvider={activeProvider}
              activeModel={activeConv.model}
              isStreaming={isStreaming}
              sendingActive={sendingId === activeId}
            />
          </>
        ) : (
          <ChatEmptyState
            hasProviders={providers.length > 0}
            hasSkills={skills.length > 0}
            onSuggest={(s) => setComposer(s)}
          />
        )}
        <VoiceInputBubble
          open={voiceListening}
          partial={voicePartial}
          onCancel={() => endVoice()}
          onCommit={(t) => {
            endVoice(t);
          }}
        />
      </div>
    </div>
  );
}

function ConversationList({
  conversations,
  conversationsLoaded,
  activeId,
  providers,
  onSelect,
  onDelete,
  onNew,
  collapsed,
  onToggle,
}: {
  conversations: ReturnType<typeof useChatStore.getState>["conversations"];
  conversationsLoaded: boolean;
  activeId: string | null;
  providers: ProviderConnection[];
  onSelect: (id: string) => Promise<void>;
  onDelete: (id: string) => Promise<void>;
  onNew: () => Promise<void>;
  collapsed: boolean;
  onToggle: () => void;
}) {
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);

  // Collapsed: a thin rail with just a toggle to expand and a new-chat
  // button. The chat pane gets the full remaining width.
  if (collapsed) {
    return (
      <aside className="flex w-12 shrink-0 flex-col items-center gap-1.5 border-r border-bg-border bg-bg-surface/30 py-3">
        <button
          type="button"
          onClick={onToggle}
          aria-label="Show conversations"
          title="Show conversations"
          className="press flex h-9 w-9 items-center justify-center rounded-md text-ink-muted transition-colors hover:bg-bg-elevated hover:text-ink"
        >
          <PanelLeftOpen size={16} />
        </button>
        <button
          type="button"
          onClick={() => void onNew()}
          disabled={providers.length === 0}
          aria-label="New chat"
          title="New chat"
          className="press flex h-9 w-9 items-center justify-center rounded-md text-ink-muted transition-colors hover:bg-bg-elevated hover:text-ink disabled:pointer-events-none disabled:opacity-40"
        >
          <Plus size={16} />
        </button>
      </aside>
    );
  }

  return (
    <aside className="flex w-64 shrink-0 flex-col border-r border-bg-border bg-bg-surface/30">
      <div className="flex items-center gap-2 p-3">
        <Button
          size="sm"
          variant="primary"
          className="flex-1"
          onClick={() => void onNew()}
          disabled={providers.length === 0}
        >
          <Plus size={12} /> New chat
        </Button>
        <button
          type="button"
          onClick={onToggle}
          aria-label="Hide conversations"
          title="Hide conversations"
          className="press flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-ink-muted transition-colors hover:bg-bg-elevated hover:text-ink"
        >
          <PanelLeftClose size={15} />
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
        {providers.length === 0 ? (
          <EmptyState
            title="No providers"
            description="Open Settings → Integrations to connect a model."
          />
        ) : !conversationsLoaded ? (
          <div className="flex items-center justify-center gap-2 py-8 text-[11px] text-ink-dim">
            <Loader2 size={11} className="animate-spin" />
            Loading…
          </div>
        ) : conversations.length === 0 ? (
          <div className="px-2 py-8 text-center text-[11px] text-ink-dim">
            No conversations yet.
          </div>
        ) : (
          <ul className="space-y-0.5">
            {conversations.map((c) => (
              <li key={c.id} className="group flex items-center gap-1">
                <button
                  type="button"
                  onClick={() => void onSelect(c.id)}
                  aria-current={c.id === activeId ? "page" : undefined}
                  className={clsx(
                    "press min-w-0 flex-1 truncate rounded-md px-2.5 py-2 text-left text-[12px] transition-colors",
                    c.id === activeId
                      ? "bg-amber-500/[0.08] text-ink"
                      : "text-ink-muted hover:bg-bg-elevated hover:text-ink"
                  )}
                  title={c.title}
                >
                  <span className="flex items-center gap-1.5">
                    <MessageSquare size={11} className="shrink-0 text-ink-dim/70" />
                    <span className="min-w-0 flex-1 truncate">{c.title}</span>
                  </span>
                </button>
                <button
                  type="button"
                  aria-label={confirmDelete === c.id ? "Confirm delete" : "Delete conversation"}
                  onClick={() => {
                    if (confirmDelete === c.id) {
                      void onDelete(c.id);
                      setConfirmDelete(null);
                    } else {
                      setConfirmDelete(c.id);
                      setTimeout(() => setConfirmDelete(null), 2500);
                    }
                  }}
                  className={clsx(
                    "press shrink-0 rounded p-1.5 text-ink-dim transition-colors hover:text-state-error",
                    confirmDelete === c.id && "bg-state-error/[0.12] text-state-error"
                  )}
                >
                  <Trash2 size={11} />
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
      <div className="border-t border-bg-border px-3 py-2 text-[10px] text-ink-dim">
        <Kbd>⌘K</Kbd> to jump anywhere · <Kbd>⌘⇧Space</Kbd> for voice
      </div>
    </aside>
  );
}

function ChatHeader({
  providers,
  activeConv,
  onSelectModel,
}: {
  providers: ProviderConnection[];
  activeConv: NonNullable<ReturnType<typeof useChatStore.getState>["activeConv"]>;
  onSelectModel: (providerId: string, model: string) => Promise<void>;
}) {
  // Model switcher is the only thing the legacy chat tab needed from a
  // per-conversation header. We keep it because changing the model
  // mid-conversation is a common pattern; the new tab just doesn't put
  // a title in the header anymore (the active title is in the sidebar).
  return (
    <header className="flex items-center justify-between gap-3 px-4 py-2.5">
      <div className="min-w-0 flex-1 truncate text-[13px] font-medium text-ink">
        {activeConv.title}
      </div>
      <ModelSwitcher
        providers={providers}
        providerId={activeConv.provider_id}
        model={activeConv.model}
        onSelect={onSelectModel}
      />
    </header>
  );
}

function AmbientFooter({
  activeProvider,
  activeModel,
  isStreaming,
  sendingActive,
}: {
  activeProvider: ProviderConnection | null;
  activeModel: string;
  isStreaming: boolean;
  sendingActive: boolean;
}) {
  // Footer shows the current model and a low-key state pill so the user
  // knows whether the model is thinking or idle. Kept extremely small.
  const state = isStreaming ? "streaming" : sendingActive ? "sending" : "idle";
  return (
    <div className="flex items-center justify-between px-4 py-1.5 text-[10px] text-ink-dim">
      <div className="flex items-center gap-2 font-mono tabular-nums">
        <span
          className={clsx(
            "inline-block h-1.5 w-1.5 rounded-full",
            state === "streaming" || state === "sending"
              ? "bg-amber-400 motion-safe:animate-pulse"
              : "bg-ink-subtle"
          )}
        />
        <span>
          {state === "streaming" ? "Generating…" : state === "sending" ? "Sending…" : "Ready"}
        </span>
      </div>
      <div className="font-mono text-ink-dim">
        {activeProvider ? `${activeProvider.display_name} · ${activeModel || "—"}` : "No provider"}
      </div>
    </div>
  );
}
