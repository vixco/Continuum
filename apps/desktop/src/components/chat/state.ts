// Chat state slice + bridge to the persisted store + the streaming
// `continuum:chat` event. The state slice is *not* persisted — it's a
// local mirror of: (a) the on-disk conversation list (`conversations`),
// (b) per-conversation in-flight streaming buffers + tool invocations,
// and (c) the chat composer's transient UI state (active skills,
// slash-menu open, voice listening). The store is the only thing that
// talks to Tauri; the components subscribe via thin selectors.

"use client";

import { create } from "zustand";

import { continuum, onChatEvent, isTauri } from "@/lib/tauri";
import { onProvidersChanged } from "@/lib/providers";
import type {
  ChatEventPayload,
  Conversation,
  ConversationSummary,
  ProviderConnection,
  Skill,
  StoredMessage,
} from "@/lib/types";

import type {
  ChatMessage,
  ChatSkillChip,
  ChatToolRef,
  ContentPart,
  Role,
  ToolInvocation,
  ToolStatus,
} from "./types";
import { applyToolCall, applyToolResult, storedMessageParts } from "./toolInvocations";

interface ChatState {
  // --- catalog ---
  providers: ProviderConnection[];
  providersLoaded: boolean;
  skills: Skill[];
  tools: ChatToolRef[];

  // --- conversations ---
  conversations: ConversationSummary[];
  conversationsLoaded: boolean;
  activeId: string | null;
  activeConv: Conversation | null;
  /** Map of conversationId -> assistant message currently streaming. */
  streamBuffers: Record<string, string>;
  /** Map of conversationId -> tool invocations of the in-flight assistant
   *  turn, in arrival order. Same lifecycle as `streamBuffers`: cleared on
   *  done/error, at which point the persisted conversation (which carries
   *  `tool_calls`) takes over. */
  streamToolCalls: Record<string, ToolInvocation[]>;
  /** Map of conversationId -> per-conversation UI error. */
  errors: Record<string, string>;
  globalError: string | null;
  /** Pending retry for a failed send, scoped to a single conversation. */
  retryInfo: { conversationId: string; text: string } | null;
  /** Per-conversation in-flight send tracking. */
  sendingId: string | null;
  /** Set of conversationIds whose stream has already terminated; used to
   *  prevent the post-send buffer seed from resurrecting a deleted entry. */
  finishedStreams: Set<string>;

  // --- composer / ambient state ---
  composerText: string;
  composerAttachments: import("./types").ChatAttachment[];
  slashMenuOpen: boolean;
  slashMenuQuery: string;
  activeSkills: ChatSkillChip[]; // skills the user toggled on for *this* turn
  voiceListening: boolean;
  voicePartial: string;
  ambientContext: string | null; // last perception frame / foreground app

  // --- actions ---
  bootstrap: () => Promise<void>;
  refreshProviders: () => Promise<void>;
  setConversationModel: (providerId: string, model: string) => Promise<void>;
  selectConversation: (id: string) => Promise<void>;
  newConversation: () => Promise<void>;
  deleteConversation: (id: string) => Promise<void>;
  setComposer: (text: string) => void;
  setSlashMenu: (open: boolean, query?: string) => void;
  addAttachment: (a: import("./types").ChatAttachment) => void;
  removeAttachment: (index: number) => void;
  toggleSkill: (name: string) => Promise<void>;
  send: () => Promise<void>;
  cancel: () => Promise<void>;
  retry: () => Promise<void>;
  beginVoice: () => void;
  endVoice: (transcript?: string) => void;
  setVoicePartial: (text: string) => void;
}

let unlistenChat: (() => void) | null = null;
let unlistenProviders: (() => void) | null = null;
const activeIdRef: { current: string | null } = { current: null };
const lastSentTextRef: Record<string, string> = {};
let bootstrapped = false;

const STREAM_FINISHED: Set<string> = new Set();

function genId() {
  // Short, time-based; collisions don't matter (UI only).
  return `m_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 6)}`;
}

function partsToText(parts: ContentPart[]): string {
  return parts
    .filter((p) => p.kind === "text")
    .map((p) => (p as { kind: "text"; text: string }).text)
    .join("");
}

/** Convert a persisted StoredMessage (lib/types) into a ChatMessage (chat
 *  types): persisted tool calls become leading tool parts, followed by the
 *  text part. */
function fromStored(stored: StoredMessage, conversationId: string): ChatMessage {
  const role: Role = stored.role;
  return {
    id: `stored_${stored.ts}_${Math.random().toString(36).slice(2, 6)}`,
    role,
    parts: storedMessageParts(stored),
    text: stored.content,
    model: stored.model,
    durationMs: stored.duration_ms,
    status: stored.aborted ? "aborted" : "complete",
    ts: stored.ts,
    conversationId,
    usage: stored.usage
      ? { input: stored.usage.input_tokens, output: stored.usage.output_tokens }
      : null,
  };
}

export const useChatStore = create<ChatState>((set, get) => ({
  providers: [],
  providersLoaded: false,
  skills: [],
  tools: [],
  conversations: [],
  conversationsLoaded: false,
  activeId: null,
  activeConv: null,
  streamBuffers: {},
  streamToolCalls: {},
  errors: {},
  globalError: null,
  retryInfo: null,
  sendingId: null,
  finishedStreams: STREAM_FINISHED,
  composerText: "",
  composerAttachments: [],
  slashMenuOpen: false,
  slashMenuQuery: "",
  activeSkills: [],
  voiceListening: false,
  voicePartial: "",
  ambientContext: null,

  async bootstrap() {
    if (bootstrapped) return;
    bootstrapped = true;
    const tauri = await isTauri();
    if (!tauri) {
      set({ providersLoaded: true, conversationsLoaded: true });
      return;
    }
    try {
      const [providers, conversations, skills] = await Promise.all([
        continuum.providersList().catch(() => [] as ProviderConnection[]),
        continuum.chatListConversations().catch(() => [] as ConversationSummary[]),
        continuum.listSkills().catch(() => [] as Skill[]),
      ]);
      set({
        providers,
        conversations,
        skills,
        providersLoaded: true,
        conversationsLoaded: true,
      });
      if (conversations.length > 0) {
        const first = conversations[0].id;
        activeIdRef.current = first;
        set({ activeId: first });
        const conv = await continuum.chatGetConversation(first).catch(() => null);
        if (conv) set({ activeConv: conv });
      }
    } catch (e) {
      set({ globalError: e instanceof Error ? e.message : String(e) });
    }

    if (!unlistenChat) {
      unlistenChat = await onChatEvent((payload: ChatEventPayload) => {
        const id = payload.conversation_id;
        const ev = payload.event;
        if (ev.type === "delta") {
          set((s) => ({
            streamBuffers: { ...s.streamBuffers, [id]: (s.streamBuffers[id] ?? "") + ev.text },
          }));
          return;
        }
        if (ev.type === "tool_call") {
          // Seed the stream buffer too so the streaming tail (and its tool
          // cards) renders even before the first text delta arrives.
          set((s) => ({
            streamBuffers: { ...s.streamBuffers, [id]: s.streamBuffers[id] ?? "" },
            streamToolCalls: {
              ...s.streamToolCalls,
              [id]: applyToolCall(s.streamToolCalls[id] ?? [], ev),
            },
          }));
          return;
        }
        if (ev.type === "tool_result") {
          set((s) => ({
            streamBuffers: { ...s.streamBuffers, [id]: s.streamBuffers[id] ?? "" },
            streamToolCalls: {
              ...s.streamToolCalls,
              [id]: applyToolResult(s.streamToolCalls[id] ?? [], ev),
            },
          }));
          return;
        }
        // done / error: clear the buffer and in-flight tool list, reload
        // persisted state (which carries the finished tool_calls), mark
        // finished. Unfinished invocations surface as "aborted" via the
        // persisted mapping.
        STREAM_FINISHED.add(id);
        set((s) => {
          const next = { ...s.streamBuffers };
          delete next[id];
          const nextTools = { ...s.streamToolCalls };
          delete nextTools[id];
          return { streamBuffers: next, streamToolCalls: nextTools };
        });
        if (activeIdRef.current === id) {
          void continuum.chatGetConversation(id).then((conv) => {
            if (conv) set({ activeConv: conv });
          });
        }
        void continuum.chatListConversations().then((list) => {
          if (list) set({ conversations: list });
        });
        if (ev.type === "done") {
          set((s) => (s.retryInfo?.conversationId === id ? { retryInfo: null } : {}));
        } else {
          const retryable = ev.retryable;
          const text = lastSentTextRef[id];
          const retry = retryable && text ? { conversationId: id, text } : null;
          set((s) => ({ errors: { ...s.errors, [id]: ev.message }, retryInfo: retry }));
        }
      });
    }
    if (!unlistenProviders) {
      unlistenProviders = onProvidersChanged(() => {
        void get().refreshProviders();
      });
    }
  },

  async refreshProviders() {
    const providers = await continuum.providersList().catch(() => [] as ProviderConnection[]);
    set({ providers, providersLoaded: true });
  },

  async setConversationModel(providerId, model) {
    const conversation = get().activeConv;
    if (!conversation) return;
    await continuum.chatSetConversationModel(conversation.id, providerId, model);
    const [activeConv, conversations] = await Promise.all([
      continuum.chatGetConversation(conversation.id),
      continuum.chatListConversations(),
    ]);
    set({ activeConv, conversations });
  },

  async selectConversation(id) {
    if (id === get().activeId) return;
    activeIdRef.current = id;
    set({ activeId: id, activeConv: null });
    const conv = await continuum.chatGetConversation(id).catch(() => null);
    if (conv && activeIdRef.current === id) set({ activeConv: conv });
  },

  async newConversation() {
    const providers = get().providers;
    if (providers.length === 0) return;
    const target = providers.find((p) => p.default_model) ?? providers[0];
    const conv = await continuum.chatCreateConversation(
      target.id,
      target.default_model ?? target.models[0] ?? ""
    );
    const list = await continuum.chatListConversations();
    set({ conversations: list, activeId: conv.id, activeConv: conv });
    activeIdRef.current = conv.id;
  },

  async deleteConversation(id) {
    await continuum.chatDeleteConversation(id);
    set((s) => {
      const next = { ...s.streamBuffers };
      delete next[id];
      const nextTools = { ...s.streamToolCalls };
      delete nextTools[id];
      const nextErr = { ...s.errors };
      delete nextErr[id];
      return {
        conversations: s.conversations.filter((c) => c.id !== id),
        streamBuffers: next,
        streamToolCalls: nextTools,
        errors: nextErr,
        activeId: s.activeId === id ? null : s.activeId,
        activeConv: s.activeId === id ? null : s.activeConv,
        retryInfo: s.retryInfo?.conversationId === id ? null : s.retryInfo,
      };
    });
  },

  setComposer(text) {
    set({ composerText: text });
    if (text.startsWith("/")) {
      set({ slashMenuOpen: true, slashMenuQuery: text.slice(1) });
    } else {
      set({ slashMenuOpen: false, slashMenuQuery: "" });
    }
  },

  setSlashMenu(open, query) {
    set({ slashMenuOpen: open, slashMenuQuery: query ?? get().slashMenuQuery });
  },

  addAttachment(a) {
    set((s) => ({ composerAttachments: [...s.composerAttachments, a] }));
  },

  removeAttachment(index) {
    set((s) => ({
      composerAttachments: s.composerAttachments.filter((_, i) => i !== index),
    }));
  },

  async toggleSkill(name) {
    const existing = get().activeSkills.find((s) => s.name === name);
    if (existing) {
      set((s) => ({ activeSkills: s.activeSkills.filter((x) => x.name !== name) }));
    } else {
      const skill = get().skills.find((s) => s.name === name);
      if (!skill) return;
      set((s) => ({
        activeSkills: [
          ...s.activeSkills,
          {
            name: skill.name,
            description: skill.description,
            active: true,
            manualOnly: skill.manual_only,
          },
        ],
      }));
    }
  },

  async send() {
    const { composerText, activeId, composerAttachments, activeSkills } = get();
    const text = composerText.trim();
    if (!text || !activeId) return;
    // Build the wire text. Skill chips prefix the message so the model sees
    // which skills the user activated for this turn. Attachments are
    // appended as a fenced block the model can read.
    const lines: string[] = [];
    if (activeSkills.length > 0) {
      lines.push(`[active skills: ${activeSkills.map((s) => s.name).join(", ")}]`);
    }
    lines.push(text);
    if (composerAttachments.length > 0) {
      lines.push("");
      lines.push("[attachments]");
      for (const a of composerAttachments) {
        lines.push(
          `- ${a.kind === "image" ? "image" : "file"}: ${a.name}${a.path ? ` (${a.path})` : ""}`
        );
      }
    }
    const wire = lines.join("\n");

    lastSentTextRef[activeId] = wire;
    STREAM_FINISHED.delete(activeId);
    set({
      composerText: "",
      composerAttachments: [],
      sendingId: activeId,
      errors: (() => {
        const next = { ...get().errors };
        delete next[activeId];
        return next;
      })(),
      retryInfo: null,
    });
    try {
      await continuum.chatSendMessage(activeId, wire);
      if (!STREAM_FINISHED.has(activeId)) {
        set((s) => ({
          streamBuffers: { ...s.streamBuffers, [activeId]: s.streamBuffers[activeId] ?? "" },
        }));
      }
      if (activeIdRef.current === activeId) {
        const conv = await continuum.chatGetConversation(activeId).catch(() => null);
        if (conv) set({ activeConv: conv });
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      set((s) => ({
        errors: { ...s.errors, [activeId]: msg },
        retryInfo: { conversationId: activeId, text: wire },
        sendingId: null,
      }));
      return;
    }
    set({ sendingId: null });
  },

  async cancel() {
    const { activeId } = get();
    if (!activeId) return;
    await continuum.chatCancel(activeId).catch(() => {});
  },

  async retry() {
    const r = get().retryInfo;
    if (!r) return;
    lastSentTextRef[r.conversationId] = r.text;
    STREAM_FINISHED.delete(r.conversationId);
    set({ sendingId: r.conversationId, retryInfo: null });
    try {
      await continuum.chatSendMessage(r.conversationId, r.text);
      if (activeIdRef.current === r.conversationId) {
        const conv = await continuum.chatGetConversation(r.conversationId).catch(() => null);
        if (conv) set({ activeConv: conv });
      }
    } catch (e) {
      set((s) => ({
        errors: { ...s.errors, [r.conversationId]: e instanceof Error ? e.message : String(e) },
        retryInfo: { conversationId: r.conversationId, text: r.text },
        sendingId: null,
      }));
      return;
    }
    set({ sendingId: null });
  },

  beginVoice() {
    set({ voiceListening: true, voicePartial: "" });
  },
  endVoice(transcript) {
    if (transcript) {
      set((s) => ({
        voiceListening: false,
        voicePartial: "",
        composerText: (s.composerText + " " + transcript).trim() + " ",
      }));
    } else {
      set({ voiceListening: false, voicePartial: "" });
    }
  },
  setVoicePartial(text) {
    set({ voicePartial: text });
  },
}));

// Selectors — kept outside the create() call so callers can use them with
// React DevTools equality and so the chat tab isn't re-rendered by store
// changes that don't affect it.
export const selectActiveMessages = (s: ChatState): ChatMessage[] => {
  if (!s.activeConv) return [];
  return s.activeConv.messages.map((m) => fromStored(m, s.activeConv!.id));
};

export const selectIsStreaming = (s: ChatState): boolean => {
  if (!s.activeId) return false;
  return s.activeId in s.streamBuffers;
};

export const selectActiveBuffer = (s: ChatState): string =>
  s.activeId ? (s.streamBuffers[s.activeId] ?? "") : "";

const EMPTY_TOOL_CALLS: ToolInvocation[] = [];

/** Stable-reference selector: in-flight tool invocations of the active
 *  conversation (empty while nothing is streaming). */
export const selectActiveToolCalls = (s: ChatState): ToolInvocation[] =>
  s.activeId ? (s.streamToolCalls[s.activeId] ?? EMPTY_TOOL_CALLS) : EMPTY_TOOL_CALLS;

export { fromStored, genId, partsToText };
export type { ToolInvocation, ToolStatus };
