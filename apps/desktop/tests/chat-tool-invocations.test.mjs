import assert from "node:assert/strict";
import test from "node:test";

import {
  applyToolCall,
  applyToolResult,
  invocationFromStoredToolCall,
  parseToolOutput,
  storedMessageParts,
} from "../src/components/chat/toolInvocations.ts";

const NOW = "2026-08-04T12:00:00.000Z";
const LATER = "2026-08-04T12:00:03.000Z";

test("parseToolOutput parses JSON and falls back to the raw string", () => {
  assert.deepEqual(parseToolOutput('{"hits":3}'), { hits: 3 });
  assert.deepEqual(parseToolOutput("[1,2]"), [1, 2]);
  assert.equal(parseToolOutput("plain text result"), "plain text result");
  assert.equal(parseToolOutput(""), "");
});

test("applyToolCall appends a running invocation in arrival order", () => {
  const one = applyToolCall([], { id: "t1", name: "memory_query", input: { q: "x" } }, NOW);
  const two = applyToolCall(one, { id: "t2", name: "memory_save", input: null }, LATER);
  assert.equal(two.length, 2);
  assert.deepEqual(two[0], {
    id: "t1",
    name: "memory_query",
    status: "running",
    input: { q: "x" },
    startedAt: NOW,
  });
  assert.equal(two[1].id, "t2");
  assert.equal(two[1].status, "running");
  // Pure: the input list is untouched.
  assert.equal(one.length, 1);
});

test("applyToolResult resolves the matching running invocation", () => {
  const running = applyToolCall([], { id: "t1", name: "memory_query", input: { q: "x" } }, NOW);
  const done = applyToolResult(
    running,
    { id: "t1", output: '{"hits":2}', is_error: false, duration_ms: 340 },
    LATER
  );
  assert.equal(done.length, 1);
  assert.equal(done[0].status, "ok");
  assert.deepEqual(done[0].output, { hits: 2 });
  assert.equal(done[0].durationMs, 340);
  assert.equal(done[0].finishedAt, LATER);
  assert.equal(done[0].startedAt, NOW);
});

test("applyToolResult maps is_error to error and hides duration_ms 0", () => {
  const running = applyToolCall([], { id: "t1", name: "memory_query", input: {} }, NOW);
  const done = applyToolResult(
    running,
    { id: "t1", output: "boom", is_error: true, duration_ms: 0 },
    LATER
  );
  assert.equal(done[0].status, "error");
  assert.equal(done[0].output, "boom");
  assert.equal(done[0].durationMs, undefined);
});

test("applyToolResult surfaces an orphan result as its own card", () => {
  const list = applyToolResult(
    [],
    { id: "ghost", output: "late", is_error: false, duration_ms: 5 },
    LATER
  );
  assert.equal(list.length, 1);
  assert.equal(list[0].id, "ghost");
  assert.equal(list[0].name, "");
  assert.equal(list[0].status, "ok");
  assert.equal(list[0].output, "late");
});

test("invocationFromStoredToolCall maps output presence and serde defaults", () => {
  const ok = invocationFromStoredToolCall(
    { id: "a", name: "memory_query", input: { q: "x" }, output: '{"n":1}' },
    NOW
  );
  assert.equal(ok.status, "ok"); // missing is_error → false
  assert.deepEqual(ok.output, { n: 1 });
  assert.equal(ok.durationMs, undefined); // missing duration_ms → 0 → hidden
  assert.equal(ok.startedAt, NOW);
  assert.equal(ok.finishedAt, NOW);

  const err = invocationFromStoredToolCall(
    { id: "b", name: "memory_save", input: {}, output: "failed", is_error: true, duration_ms: 12 },
    NOW
  );
  assert.equal(err.status, "error");
  assert.equal(err.durationMs, 12);

  const aborted = invocationFromStoredToolCall(
    { id: "c", name: "memory_save", input: {}, output: null },
    NOW
  );
  assert.equal(aborted.status, "aborted"); // turn ended without a result
  assert.equal(aborted.output, undefined);
  assert.equal(aborted.finishedAt, undefined);
});

test("storedMessageParts puts tool cards before the text part", () => {
  const parts = storedMessageParts({
    role: "assistant",
    content: "Found it.",
    ts: NOW,
    model: "m",
    duration_ms: null,
    usage: null,
    aborted: false,
    tool_calls: [
      { id: "a", name: "memory_query", input: { q: "x" }, output: "{}" },
      { id: "b", name: "memory_save", input: {}, output: null },
    ],
  });
  assert.deepEqual(
    parts.map((p) => p.kind),
    ["tool", "tool", "text"]
  );
  assert.equal(parts[2].text, "Found it.");
  assert.equal(parts[1].invocation.status, "aborted");
});

test("storedMessageParts keeps plain messages as a single text part", () => {
  const base = {
    role: "assistant",
    content: "hello",
    ts: NOW,
    model: null,
    duration_ms: null,
    usage: null,
    aborted: false,
  };
  assert.deepEqual(storedMessageParts(base), [{ kind: "text", text: "hello" }]);
  // Tool-only turn with no prose: no empty trailing text part.
  const toolOnly = storedMessageParts({
    ...base,
    content: "",
    tool_calls: [{ id: "a", name: "memory_query", input: {}, output: "{}" }],
  });
  assert.deepEqual(
    toolOnly.map((p) => p.kind),
    ["tool"]
  );
});
