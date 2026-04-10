# CLAUDE.md

This file tells Claude Code how to work on the Kairo repository. Read it before touching any code.

## Who you are in this repo

You are the primary developer of Kairo. The human maintainer (Toshan) is the architect and reviewer, but you will write most of the code. Your job is to implement the design described in `ARCHITECTURE.md` faithfully, stay consistent with the principles in this document, and keep the codebase in a state that another Claude Code instance can pick up and continue.

## What Kairo is

Kairo is a desktop-native, local-first, ambient AI assistant for Windows. It uses a four-layer cognitive architecture:

1. **Senses** — local vision (Moondream), audio (whisper.cpp), and context polling
2. **Triage** — a small local LLM (Qwen 2.5 3B default) that decides what matters
3. **Orchestrator** — Claude Opus 4.6 via the official `claude` CLI in headless mode, woken up only when genuine reasoning is needed
4. **Workers** — headless Claude Code sessions spawned by the orchestrator to do actual work

Read `README.md` for the public pitch, `ARCHITECTURE.md` for the full technical blueprint, and `SOUL.md` for Kairo's personality. If any of those are unclear, stop and ask before writing code.

## Non-negotiables

These rules are absolute. Do not violate them without explicit permission from the maintainer.

1. **Never scrape OAuth tokens or reverse-engineer subscription auth.** Kairo uses the official `claude` CLI as a subprocess. That is the only legal way. If you find yourself wanting to call the Anthropic API directly from Kairo Core, stop — that is the wrong approach.
2. **Never phone home.** Kairo does not send telemetry, crash reports, or usage data to any server that is not the Anthropic API (via Claude Code) or optionally ElevenLabs if the user has enabled premium TTS. Every new network call requires justification in the PR description.
3. **Never assume defaults without making them configurable.** Every model, interval, threshold, and prompt must be readable from config and overridable via the dashboard. If you hardcode something, you are creating technical debt that someone will have to remove later.
4. **Never bypass the layer hierarchy.** Layer 1 outputs perception frames. Layer 2 triages them. Layer 3 plans. Layer 4 executes. Do not let a worker trigger the orchestrator. Do not let the orchestrator directly touch the senses. Data flows up, commands flow down.
5. **Never ship a feature without self-healing hooks.** If a new component can fail, it must expose a health check, log its failures in a way the repair agent can read, and have a documented recovery procedure.
6. **Never commit secrets.** API keys, tokens, user data, telemetry endpoints. Use environment variables and config files outside the repo.
7. **Never break the public API of `kairo-mcp`.** Once a tool name and schema is published in a release, it cannot change without a version bump. Downstream Claude Code instances rely on stable tool signatures.

## Project structure

This is a monorepo with a Cargo workspace for Rust crates and a pnpm workspace for JavaScript/TypeScript apps.

```
apps/desktop/          Tauri desktop app (Rust backend + Next.js frontend)
crates/kairo-core/     Main orchestration runtime
crates/kairo-mcp/      MCP server exposing Windows-specific tools
crates/kairo-llm/      Local LLM runtime (llama.cpp wrapper)
crates/kairo-vision/   Local vision model runtime
skills/                Bundled Kairo skills (SKILL.md files)
docs/                  User-facing documentation
prompts/               System prompts for triage, orchestrator, repair agent
config/                Default config files
scripts/               Install and dev setup scripts
```

See `ARCHITECTURE.md` for the full directory layout and what goes where.

## Coding standards

### Rust

- **Rust edition 2021**, toolchain pinned to stable in `rust-toolchain.toml` (Tauri 2 deps require >=1.85)
- **Formatting:** `cargo fmt` must pass, no exceptions
- **Linting:** `cargo clippy --all-targets --all-features -- -D warnings` must pass
- **Error handling:** Use `anyhow` for application errors, `thiserror` for library errors. Never use `.unwrap()` in production code paths. Use `.expect("...")` with a descriptive message only for things that genuinely cannot fail.
- **Async runtime:** tokio, single runtime instance. Never spawn a second runtime.
- **Logging:** `tracing` crate with structured fields. Every log event must include the layer name and a component name.
- **Tests:** Every public function has at least one unit test. Integration tests live in `crates/*/tests/`. Use `mockall` or handwritten mocks for external dependencies.
- **Documentation:** Every public item has a doc comment. Every module has a module-level doc comment explaining what it does and how it fits into the layer architecture.

### TypeScript / Next.js

- **Strict TypeScript**, `strict: true` in tsconfig, no `any` without a `// eslint-disable-next-line` and a reason
- **Formatting:** Prettier with 2-space indentation
- **Linting:** ESLint with the Next.js + TypeScript recommended rules
- **Components:** Functional components only. Hooks over classes. Prefer server components where possible, client components only when state or effects are needed.
- **Styling:** Tailwind CSS exclusively. No CSS modules, no styled-components, no inline styles except for computed values.
- **State:** Zustand for global state, React state for local. No Redux.

### Git workflow

- **Branches:** `main` is always releasable. Feature branches are named `feat/<short-description>`, fixes are `fix/<short-description>`, docs are `docs/<short-description>`.
- **Commits:** Conventional commits. `feat(scope): message`, `fix(scope): message`, `docs(scope): message`, `refactor(scope): message`, `chore(scope): message`, `test(scope): message`. Scopes are the crate or directory name: `core`, `mcp`, `desktop`, `llm`, `vision`, `docs`, etc.
- **PRs:** Every PR updates `CHANGELOG.md` under `## [Unreleased]`. Every PR that changes architecture updates `ARCHITECTURE.md` in the same commit.
- **No force-pushing to main, ever.** Feature branches can be rebased and force-pushed.

## How to run Claude Code from within Kairo

This is the single most important technical pattern in the codebase. Get it right.

Kairo Core spawns Claude Code as a child process using tokio. The command template is:

```rust
let mut cmd = tokio::process::Command::new("claude");
cmd.arg("--print");
cmd.arg("--output-format").arg("stream-json");
cmd.arg("--input-format").arg("stream-json");
cmd.arg("--verbose");
cmd.arg("--include-partial-messages");
cmd.arg("--model").arg(model_name); // "claude-opus-4-6" or "claude-sonnet-4-6"
cmd.arg("--append-system-prompt-file").arg(&prompt_path);
cmd.arg("--mcp-config").arg(&mcp_config_path);
cmd.arg("--allowedTools").arg(&allowed_tools_csv);
cmd.stdin(Stdio::piped());
cmd.stdout(Stdio::piped());
cmd.stderr(Stdio::piped());
```

Then write a JSON user message to stdin:

```json
{"type":"user","message":{"role":"user","content":"<the wake context>"}}
```

And read newline-delimited JSON events from stdout, parsing each one into a strongly-typed event enum in `kairo-core/src/orchestrator/events.rs`. The key event types you will see are (verified against CLI v2.1.100):

- `system` init events — subtype `"init"`, session_id, tools array, model, cwd, claude_code_version, permissionMode, apiKeySource, agents, skills, plugins, uuid, fast_mode_state
- `stream_event` events wrapping raw Anthropic API events in an `event` field: `message_start`, `content_block_start`, `content_block_delta` (with `text_delta`), `content_block_stop`, `message_delta`, `message_stop`. Each also has session_id, parent_tool_use_id, uuid
- `assistant` messages with content blocks (text, tool_use) — emitted as partial snapshots when using `--include-partial-messages`
- `user` messages containing tool_result blocks (in multi-turn interactions)
- `rate_limit_event` — rate limit status with resetsAt, rateLimitType, overageStatus (undocumented as of 2026-04)
- `result` — the final event with subtype, is_error, total_cost_usd, duration_ms, duration_api_ms, num_turns, result text, session_id, usage, modelUsage, stop_reason, terminal_reason, uuid

When streaming text from the orchestrator, pipe `text_delta` events directly into the TTS queue as they arrive. Do not wait for the full response. Low latency is the whole point of using stream-json.

When the orchestrator wants to spawn a worker, it calls the `mcp__kairo__workers__spawn_worker` MCP tool. The MCP server (running as a separate process) spawns the worker Claude Code process, captures its output, and streams progress back.

## How to write and test MCP tools

The Kairo MCP server is in `crates/kairo-mcp`. It uses the `rmcp` crate. Every tool follows this pattern:

1. Define the input schema as a Rust struct with `#[derive(Serialize, Deserialize, JsonSchema)]`
2. Define the handler as an async function that takes the input struct and returns a `Result<ToolResult>`
3. Register the tool in `src/main.rs` via the rmcp builder
4. Add a permission entry in `config/default-permissions.toml`
5. Document the tool in `docs/mcp-tools.md`
6. Write an integration test that calls the tool via the MCP protocol

Example test template:

```rust
#[tokio::test]
async fn test_memory_query_returns_top_results() {
    let server = test_server().await;
    let result = server.call_tool("memory_query", json!({
        "query": "SimCharts debugging",
        "limit": 3
    })).await.unwrap();
    
    assert!(result.content.len() <= 3);
    assert!(result.is_success());
}
```

## Working with local models

Local models (llama.cpp for LLMs, ONNX Runtime for vision) live in `kairo-llm` and `kairo-vision`. These crates wrap the native libraries and expose safe Rust APIs.

Rules for local model code:

- **Model files are never checked into git.** They live in `~/.kairo/models/` on the user's machine. Provide download scripts in `scripts/download-models.ps1`.
- **Always quantize by default.** Q4_K_M for LLMs, FP16 for vision. Expose quantization as a config option.
- **Always stream when possible.** llama.cpp supports streaming. Use it. The triage layer should start producing tokens within 200 ms of being called.
- **Always respect GPU detection.** If CUDA is available, use it. If not, fall back to CPU gracefully. Never require a GPU.

## When you don't know something

- **You don't know the Claude Code CLI flags?** Check `code.claude.com/docs/en/headless` or run `claude --help`. Do not guess.
- **You don't know an MCP protocol detail?** Check `modelcontextprotocol.io` or the `rmcp` crate docs. Do not guess.
- **You don't know a Windows API?** Check the `windows` crate docs at `microsoft.github.io/windows-docs-rs/`. Do not guess.
- **You don't know what the maintainer wants for a new feature?** Stop and ask. Do not invent requirements.
- **You found a design decision in the code that contradicts `ARCHITECTURE.md`?** The architecture doc is the source of truth. Either update the doc (if the code is right) or update the code (if the doc is right). Do not leave the contradiction.

## How to use TodoWrite

For any task with more than 3 steps, create a todo list at the start of your work and update it as you go. The maintainer can see your progress in the dashboard and will intervene if something looks wrong. Aim for at least 5 items in any substantial todo list so the plan is legible.

## How to use sub-agents

When you are implementing a feature that touches multiple crates or has a distinct research phase, use the `Task` tool to spawn sub-agents. Examples of good sub-agent delegation:

- "Research the best Rust crate for Windows screen capture and return a comparison" → spawn a research sub-agent
- "Implement the `perception_screenshot` MCP tool in `kairo-mcp`" → spawn an implementation sub-agent with narrow scope
- "Write integration tests for the triage layer" → spawn a test-writing sub-agent

Sub-agents should have narrow, well-scoped tasks. Do not delegate vague goals like "build the dashboard." Break it down first.

## How to handle failure

When a build fails, a test fails, or something doesn't work:

1. **Read the full error message.** Do not assume. Do not skim.
2. **Check if the problem is upstream.** Did a dependency version change? Did a crate API shift? Did Claude Code update its CLI?
3. **Make a small, focused fix.** Do not rewrite a module to fix a single failing test.
4. **Verify the fix.** Re-run the failing test or build. Do not mark work as done until the verification passes.
5. **Add a regression test.** If it broke once, it can break again.

When you are stuck, log what you tried and hand off to the maintainer rather than guessing wildly. A paused PR with a clear status is more valuable than a confused PR with half-right code.

## Skills

Kairo has its own skills directory at `skills/` with `SKILL.md` files that tell the orchestrator how to handle specific workflows. If you add a new capability that the orchestrator needs to know how to use in a specific context, write a skill file. Follow the Anthropic skills format: a `SKILL.md` with frontmatter name and description, and supporting files in the same directory.

Example skill:

```
skills/
└── simcharts-dev/
    ├── SKILL.md
    └── templates/
        └── component-template.tsx
```

The `SKILL.md` frontmatter should explain exactly when the skill triggers so the orchestrator picks it up automatically.

## Self-healing compatibility

Every new component must support the repair agent. This means:

- It writes logs to `~/.kairo/logs/<component>.log` with structured events
- It has a health check exposed via the `system_health` MCP tool
- It has a documented recovery procedure in `docs/self-healing.md`
- It can be restarted via a `repair_restart_component` call without losing user data
- It has a `should_restart()` function that returns true if something is obviously wrong

If a new component cannot be repaired by the repair agent, it is a liability. Either make it repairable or justify its irrepairability in the PR description.

## Final rule

**If you are not sure, ask.** Kairo is an ambitious project. Ambitious projects die from confident wrong decisions, not from cautious questions. The maintainer would rather answer ten clarifying questions than review one PR that took the wrong direction.

---

Last updated: 2026-04-10. Update this file whenever the conventions change.