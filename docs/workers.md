# Workers

Workers are independent Claude Code sessions that Continuum's orchestrator spawns to do actual, possibly-long-running work. This document covers how workers are queued, how a model is chosen, how their lifecycle is observed, and how the audit trail makes every worker action reviewable after the fact.

## Why workers exist

The orchestrator (Layer 3) is designed to stay in the conversational loop with the user. It wakes, thinks, speaks, and hands off. Anything that would take more than a handful of tool calls — a PR review, a codebase search, a file reorganisation, a nightly cleanup — is not a good use of the orchestrator's context or the user's attention.

Workers solve that. Each worker is:

- A fresh `claude --print --output-format stream-json` subprocess.
- Run in its own working directory (usually a project folder).
- Given its own model tier (Sonnet by default, Opus for reasoning-heavy tasks).
- Given its own tool allowlist (narrower than the orchestrator's — no access to `mcp__continuum__workers_*`, so workers can't spawn more workers through MCP).
- Tracked independently through a full lifecycle (queued → starting → running → terminal).

## Layers involved

```
Orchestrator          (claude CLI + MCP tool call)
  └─► continuum-mcp       (workers_spawn_worker writes intent file)
        └─► continuum-core::workers::pool::WorkerPool
              ├─ WorkerSupervisor (spawns child claude process)
              └─ snapshot file  ──► dashboard + MCP workers_worker_status
```

The MCP server and the continuum runtime often live in different processes (the runtime owns the pool, the MCP server is re-spawned on every wake). They communicate through two disk folders:

- `~/.continuum-dev/worker-intents/` — JSON files the MCP server writes to request actions.
- `~/.continuum-dev/workers/` — per-worker JSON snapshots the runtime publishes.

The runtime drains the intents folder every `status_refresh_ms` milliseconds (default 500 ms) and updates the snapshots atomically (temp file + rename).

## Configuration

All worker behaviour comes from `[workers]` in `~/.continuum-dev/config.toml` (defaults in `config/default-models.toml`):

| Key                        | Default              | Meaning |
|---------------------------|----------------------|---------|
| `mode`                    | `"auto"`             | `auto` heuristic, `budget` forces Sonnet, `power` forces Opus |
| `budget_model`            | `claude-sonnet-4-6`  | Model id used in budget mode / by the auto heuristic for mechanical tasks |
| `power_model`             | `claude-opus-4-6`    | Model id used in power mode / by the auto heuristic for reasoning-heavy tasks |
| `max_concurrent`          | `3`                  | Cap on running workers; excess queue. Hard ceiling: 10 |
| `default_timeout_secs`    | `1800`               | Per-worker wall-clock timeout |
| `default_allowed_tools`   | (see config)         | CSV passed to `--allowedTools` |
| `status_refresh_ms`       | `500`                | How often the pool ticks |
| `failure_streak_limit`    | `3`                  | Refuse a task pattern after this many recent failures |
| `failure_window_secs`     | `600`                | Window the failure streak is counted over |

### Model selection heuristic

When `mode = "auto"` and the orchestrator doesn't force a tier, the pool looks at the task text:

- **Power keywords** (Opus): `refactor`, `architect`, `design`, `debug complex`, `root cause`, `investigate`, `migration`, `redesign`, `audit`, `performance tuning`, …
- **Budget keywords** (Sonnet): `format`, `rename`, `move files`, `summarise`, `draft`, `boilerplate`, `scaffold`, `email`, `todo`, …

Ties lean Opus (because mis-downgrading a complex task costs more than mis-upgrading a simple one). Tasks with no keyword match default to Sonnet. The pool logs the chosen model plus a human-readable reason (`"auto: power — keywords [\"refactor\"]"`) in the worker snapshot's `model_reason` field.

## MCP tools

| Tool                                   | Purpose |
|---------------------------------------|---------|
| `mcp__continuum__workers_spawn_worker`     | Queue a new worker; returns `worker_id` immediately |
| `mcp__continuum__workers_worker_status`    | Read one worker's current snapshot |
| `mcp__continuum__workers_worker_wait`      | Block until a worker reaches a terminal state |
| `mcp__continuum__workers_worker_cancel`    | Stop a running or queued worker |
| `mcp__continuum__workers_worker_list`      | List recent workers (filterable by status) |

All tools are documented in `docs/mcp-tools.md`.

## Skills and workers

When the pool launches a worker, it asks the active `SkillLoader` which skills apply (matching the task text + optional `skills: [...]` override from the spawn request). Matched skill content is concatenated with the base worker system prompt and written to `~/.continuum-dev/worker-prompts/<worker-id>.md`, which the supervisor passes to `claude --append-system-prompt-file`.

## Audit trail

Every worker action is captured:

- **Per tool call**: an episodic memory event with kind `tool_call`, tags `worker`, `worker:<id>`, and the tool name.
- **Per finish** (success or failure): a single audit event summarising the outcome, cost, duration, and error if any.
- **Per spawn intent**: the MCP server's audit log already records every `spawn_worker` invocation with the chosen model and priority.

Tag-filter queries over episodic memory find a worker's full activity: `mcp__continuum__memory_query_episodic` with the query `"worker:<id>"`.

## Self-healing

The `workers` component in the Health tab surfaces:

- Recent failures within the last 10 minutes.
- Pool overload (queued count exceeds `max_concurrent * 5`).

If a specific task pattern fails `failure_streak_limit` times within `failure_window_secs`, the pool refuses further retries with `"refused: task pattern has failed repeatedly"` so the repair agent can escalate before money is wasted re-running the same broken work.

## Dry-run mode

`CONTINUUM_WORKER_DRY_RUN=1` swaps the supervisor's claude CLI call for an in-process fake that produces a synthetic stream. Used by:

- `cargo test` (avoids live API hits)
- `cargo run --example worker_demo -p continuum-core` (local smoke test)
- The `workers` health probe (verifies the pool can actually spawn)

Set `CONTINUUM_WORKER_DRY_RUN=0` (or unset it) before running against the real claude CLI.

## Example: spawn a worker from inside the orchestrator

The orchestrator, mid-wake, decides the user's request ("refactor the auth middleware") warrants a worker:

```json
{
  "tool": "mcp__continuum__workers_spawn_worker",
  "input": {
    "task": "Refactor crates/continuum-core/src/voice/streaming.rs to use the new SpeechController API. Keep public surface stable. Write tests.",
    "cwd": "F:/TRYORVIA/continuum-ai",
    "model": "power",
    "priority": "user",
    "requested_by": "orchestrator",
    "tags": ["refactor", "voice"]
  }
}
```

The server returns a `worker_id` immediately. The orchestrator can either:

1. Tell the user it started the work and end the wake (fire-and-forget).
2. Poll `workers_worker_status` and relay progress.
3. Call `workers_worker_wait` (up to 300 s) and include the final result in its own response.

The third option is useful for sequential workflows where the orchestrator needs the worker's output before its own next step.
