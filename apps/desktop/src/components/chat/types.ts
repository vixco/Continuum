// Chat-specific types: extension of the persisted StoredMessage model with
// streaming-only fields (tool calls, thinking blocks, attachments) that the
// existing `apps/desktop/src/lib/types.ts` Conversation/Message structs do
// not yet carry. The store types in `state.ts` are the canonical in-memory
// shape; backend payloads (lib/types.ts StoredMessage) round-trip through
// `fromStored` / `toStored` in the tab to bridge the two.

export type Role = "user" | "assistant" | "system" | "tool";

export type MessageStatus =
  | "queued" // sitting in the composer
  | "streaming" // tokens are arriving right now
  | "complete" // done event received, persisted
  | "aborted" // user hit stop, or upstream cancelled
  | "error";

export type ToolStatus = "running" | "ok" | "error" | "aborted";

export interface ToolInvocation {
  /** Stable id; matches between the in-flight call and the result card. */
  id: string;
  /** Namespaced tool name, e.g. `memory_query_episodic`. */
  name: string;
  /** Free-form short label, e.g. "searching past events". */
  label?: string;
  status: ToolStatus;
  input?: unknown;
  output?: unknown;
  /** Milliseconds the tool took. Only meaningful once status !== "running". */
  durationMs?: number;
  /** ISO ts. */
  startedAt: string;
  finishedAt?: string;
  /** Optional one-line error message when status === "error". */
  error?: string;
}

export interface ThinkingBlock {
  /** Plain-text reasoning. The orchestrator emits this as a separate
   *  "thinking" stream so the UI can collapse it independently of the
   *  final answer. */
  text: string;
  startedAt: string;
  finishedAt?: string;
}

export type ContentPart =
  | { kind: "text"; text: string }
  | { kind: "thinking"; block: ThinkingBlock }
  | { kind: "tool"; invocation: ToolInvocation }
  /** Grouped parallel tool calls (same tick, same parent) collapse into one
   *  ToolGroup card so a 4-tool fan-out doesn't look like 4 unrelated rows. */
  | { kind: "tool_group"; invocations: ToolInvocation[]; startedAt: string };

export interface ChatAttachment {
  /** Local file path or a `clipboard:` sentinel for pasted images. */
  kind: "file" | "image";
  name: string;
  path?: string;
  size?: number;
  /** Image data URL when the user pasted a screenshot. */
  dataUrl?: string;
}

export interface ChatMessage {
  id: string;
  role: Role;
  parts: ContentPart[];
  /** Convenience accessor — when there's exactly one text part, this mirrors
   *  it. Tool/thinking messages may not have a `text` part at all. */
  text: string;
  model?: string | null;
  durationMs?: number | null;
  status: MessageStatus;
  ts: string;
  /** Conversation this belongs to. */
  conversationId: string;
  attachments?: ChatAttachment[];
  /** Token usage when the model reported it. */
  usage?: { input: number | null; output: number | null } | null;
}

export interface ChatSkillChip {
  name: string;
  description: string;
  active: boolean;
  manualOnly: boolean;
}

export interface ChatToolRef {
  name: string;
  description: string;
  namespace: string;
}

export interface SlashCommand {
  id: string;
  label: string;
  hint: string;
  kind: "skill" | "tool" | "command";
  /** Inserted verbatim into the composer on pick. */
  insert: string;
  searchHaystack: string;
}
