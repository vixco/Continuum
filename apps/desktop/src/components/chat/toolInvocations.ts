// Pure mapping helpers between the backend's chat tool-call payloads
// (`lib/types` StoredToolCall + `tool_call`/`tool_result` stream events)
// and the UI-side ToolInvocation model rendered by ToolInvocationCard.
// Kept free of runtime imports so the node --experimental-strip-types
// tests can load it directly (see tests/chat-tool-invocations.test.mjs).

import type { StoredMessage, StoredToolCall } from "@/lib/types";

import type { ContentPart, ToolInvocation, ToolStatus } from "./types";

/** Tool output arrives as a raw string; most tools emit JSON. Parse when
 *  possible so the card can pretty-print, else keep the raw text. */
export function parseToolOutput(raw: string): unknown {
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

/** Append a `running` invocation for a `tool_call` stream event. */
export function applyToolCall(
  list: ToolInvocation[],
  ev: { id: string; name: string; input: unknown },
  now: string = new Date().toISOString()
): ToolInvocation[] {
  return [
    ...list,
    { id: ev.id, name: ev.name, status: "running", input: ev.input, startedAt: now },
  ];
}

/** Resolve the matching `running` invocation for a `tool_result` stream
 *  event. An orphan result (no visible call — the CLI adapter passes these
 *  through by design) becomes its own card so nothing silently disappears.
 *  A `duration_ms` of 0 means unknown and is hidden. */
export function applyToolResult(
  list: ToolInvocation[],
  ev: { id: string; output: string; is_error: boolean; duration_ms: number },
  now: string = new Date().toISOString()
): ToolInvocation[] {
  const status: ToolStatus = ev.is_error ? "error" : "ok";
  const output = parseToolOutput(ev.output);
  const durationMs = ev.duration_ms > 0 ? ev.duration_ms : undefined;
  const idx = list.findIndex((inv) => inv.id === ev.id && inv.status === "running");
  if (idx === -1) {
    return [
      ...list,
      { id: ev.id, name: "", status, output, durationMs, startedAt: now, finishedAt: now },
    ];
  }
  const next = [...list];
  next[idx] = { ...next[idx], status, output, durationMs, finishedAt: now };
  return next;
}

/** Convert a persisted tool call into a card-ready invocation. A missing
 *  output means the turn ended before the result arrived → `aborted`.
 *  Exact per-call timestamps weren't persisted; the message `ts` stands in. */
export function invocationFromStoredToolCall(tc: StoredToolCall, ts: string): ToolInvocation {
  const finished = tc.output != null;
  const status: ToolStatus = !finished ? "aborted" : tc.is_error ? "error" : "ok";
  const durationMs = tc.duration_ms != null && tc.duration_ms > 0 ? tc.duration_ms : undefined;
  return {
    id: tc.id,
    name: tc.name,
    status,
    input: tc.input,
    output: tc.output != null ? parseToolOutput(tc.output) : undefined,
    durationMs,
    startedAt: ts,
    finishedAt: finished ? ts : undefined,
  };
}

/** Build the ContentPart list for a persisted message: tool cards first
 *  (in stored call order), then the text part. The text part is dropped
 *  only when the message has tool calls and no content at all. */
export function storedMessageParts(stored: StoredMessage): ContentPart[] {
  const toolParts: ContentPart[] = (stored.tool_calls ?? []).map((tc) => ({
    kind: "tool",
    invocation: invocationFromStoredToolCall(tc, stored.ts),
  }));
  if (toolParts.length > 0 && stored.content === "") return toolParts;
  return [...toolParts, { kind: "text", text: stored.content }];
}
