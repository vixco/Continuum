# Orchestrator (Layer 3)

The orchestrator is Claude Opus 4.6, invoked via the official Claude Code CLI in headless mode. It is the only cloud component of Continuum and only runs when the triage layer decides genuine reasoning is needed.

## How it works

1. Triage emits a `wake_orchestrator` decision with a reason
2. Memory retrieval fetches relevant episodic events (LanceDB vector search) and semantic facts (SQLite)
3. Wake context builder assembles a compact user message (~400 tokens):
   - Current perception frame
   - 5 preceding frames (compressed)
   - 3 similar past events from memory
   - 8 relevant user/project facts
   - The wake reason
4. A fresh `claude -p` process is spawned with the system prompt
5. Text deltas stream to the terminal in real time with `CONTINUUM:` prefix
6. On completion, cost/duration are logged and the interaction is stored in episodic memory

## Architecture decision: fresh process per wake

See [ADR 005](decisions/005-orchestrator-lifecycle.md) for full context.

Each wake spawns a fresh process. There is no long-lived Claude session. This ensures:
- **Conversation purity**: each wake sees only what we put in the user message
- **Continuum's LanceDB is the authoritative memory**, not Claude's conversation history
- **Simple crash handling**: if a process dies, we just skip that wake

The ~500ms process startup overhead is acceptable for 20-50 wakes/day.

## System prompt

The orchestrator system prompt lives at `prompts/orchestrator-system.md` (~400 tokens). It is a compact operational version of SOUL.md:
- Continuum's identity and core traits
- When to speak vs stay silent
- Response style (short, direct, bilingual NL/EN)
- Phase 3 guardrails (no tools, text-only)

Passed via `--append-system-prompt-file`.

## CLI flags

```bash
claude -p \
  --output-format stream-json \
  --verbose \
  --include-partial-messages \
  --model claude-opus-4-6 \
  --no-session-persistence \
  --bare \
  --append-system-prompt-file <path> \
  --allowedTools "" \
  --permission-mode plan
```

## Cost

Expected $0.02-0.10 per wake depending on response length. With 20-50 wakes/day, that's $0.40-5.00/day. Cost is logged per wake in the terminal output.

## Configuration

In `~/.continuum-dev/config.toml`:

```toml
[orchestrator]
model = "claude-opus-4-6"      # or "claude-sonnet-4-6" for cheaper wakes
timeout_secs = 60
```

## Running

```bash
# Full system (perception + triage + orchestrator)
cargo run --release --bin continuum

# Perception + triage only (Phase 2 behavior, no API calls)
cargo run --release --bin continuum-perception -- --triage
```
