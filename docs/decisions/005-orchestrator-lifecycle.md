# ADR 005 — Orchestrator lifecycle: fresh process per wake

## Status

Accepted (2026-04-11)

## Context

The orchestrator is Claude Opus 4.6, invoked via the `claude` CLI in headless mode (`-p --output-format stream-json`). When the triage layer decides to wake the orchestrator, Continuum needs to send it a context package and get a streamed response.

The key architectural constraint: **each wake must be functionally independent**. Continuum's LanceDB episodic memory is the authoritative memory store, not Claude's implicit conversation history. Between wakes, Opus's accumulated context must NOT carry meaning — we supply all required context explicitly via the user message.

Three options were considered:

### Option A: Long-lived process (streaming input mode)

Keep one `claude` process alive with stdin open, send messages as wakes occur.

- **Problem:** The Claude Code SDK explicitly documents that streaming input mode provides "Context Persistence: Maintain conversation context across multiple turns naturally." There is **no documented mechanism** to clear or reset conversation history within a running session. No `/clear` command, no control message, no reset signal exists in the stream-json input protocol.
- **Consequence:** Conversation history would accumulate across wakes, violating our constraint that each wake is independent. Opus would "remember" prior wakes implicitly, creating two conflicting sources of truth (its own context vs our LanceDB).

### Option B: Long-lived process with `--resume` reset

Keep a process alive but somehow reset between turns.

- **Problem:** `--resume` is for resuming a session from a *different* process, not resetting within one. `--fork-session` creates a copy of history, not a clean slate. `--no-session-persistence` only affects disk writes, not in-memory accumulation.
- **Consequence:** No viable mechanism exists.

### Option C: Fresh process per wake ✓

Spawn a new `claude -p` process for each wake. Process starts clean, receives our context package, responds, exits.

- **Benefit:** Perfect conversation purity. Each wake sees exactly what we put in the user message — nothing more.
- **Cost:** ~500ms process startup overhead per wake. Acceptable given that wakes happen 20–50 times/day and the user is already expecting 3–5 second response time.
- **Benefit:** Simpler crash handling — if a process hangs or crashes, we just don't spawn the next one until the current one exits/times out.

## Decision

**Use Option C: fresh `claude -p` process per wake.**

### Process template

```
claude -p \
  --output-format stream-json \
  --verbose \
  --include-partial-messages \
  --model claude-opus-4-6 \
  --no-session-persistence \
  --append-system-prompt-file <orchestrator-system.md path> \
  --allowedTools "" \
  --permission-mode plan
```

Flags rationale:
- `--no-session-persistence`: Don't write session files to disk. Continuum manages its own memory via LanceDB and SQLite.
- `--allowedTools ""`: No tool use in Phase 3. Opus can only respond with text. Tools come in Phase 4 (MCP).
- `--permission-mode plan`: Restrictive mode since we're not giving tools anyway.
- No `--input-format stream-json`: Not needed for single-turn. We write one user message to stdin, close stdin, read events until `result`.

### Lifecycle per wake

1. Triage emits `WakeOrchestrator(reason)`
2. Memory retrieval runs (~100–200ms)
3. Wake context builder produces user message (~400 tokens)
4. Spawn `claude -p` with flags above
5. Write user message JSON to stdin, close stdin
6. Read stdout line-by-line, emit text deltas to terminal
7. On `result` event: capture cost, duration, full response text
8. Process exits naturally
9. Store wake event + response in episodic memory

### Future considerations

- Phase 4 (MCP tools): Add `--mcp-config` and expand `--allowedTools`
- If process startup becomes a bottleneck: consider process pool (pre-spawn 1 warm process, rotate on use)
- If conversation continuity becomes needed (user in active back-and-forth): use `--resume <session_id>` to continue the prior wake's session, but only when explicitly triggered by triage detecting "ongoing conversation" mode

## Consequences

- Each wake costs ~500ms extra for process spawn (vs ~0ms for a kept-alive process)
- No risk of context pollution between wakes
- No risk of runaway memory usage from accumulating conversation history
- Simpler implementation: no long-lived process management, no stdin handle lifecycle
- Clean crash semantics: process death = wake failure, try again next time
