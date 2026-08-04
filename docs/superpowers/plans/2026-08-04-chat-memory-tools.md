# Chat Memory Tools Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The chat AI (all three provider kinds) can read and write the memory vault: it saves memories when the user asks ("onthoud dit", "ik heet Arda"), searches them when relevant ("hoe heet ik"), and deletes them on request — with every tool call visible in the chat UI as a ToolInvocationCard.

**Architecture:** Tool calling is implemented inside the gateway adapters. `ChatRequest` gains a provider-agnostic tool declaration list plus an executor callback (`Arc<dyn ToolExecutor>`); the OpenAI-compat and Anthropic adapters run an internal request→execute→continue loop and emit new `ChatEvent::ToolCall`/`ToolResult` events. The Claude CLI adapter instead attaches the continuum-mcp server via `--mcp-config` (the CLI executes tools itself) and parses tool_use/tool_result from its stream into the same events. The desktop app implements the executor against the already-open in-process `Arc<Vault>` (`MemoryState`), injects relevant vault notes into the system prompt as fallback/ambient context, and persists tool calls on `StoredMessage` so the existing `ToolInvocationCard` UI renders them live and after reload.

**Tech Stack:** Rust (continuum-gateway, continuum-core config, continuum-mcp, Tauri src-tauri), TypeScript/Next.js (chat frontend), rmcp, serde_json.

## Global Constraints

- **Never run full-workspace cargo builds.** Per-crate gates only: `cargo test -p continuum-gateway`, `cargo test -p continuum-core --lib`, `cargo test -p continuum-core --no-default-features --lib`, `cargo test -p continuum-mcp`, `cargo test -p continuum-desktop`, `cargo clippy -p <crate> --all-targets -- -D warnings`, `cargo fmt --all -- --check`. Cargo commands STRICTLY serial, foreground — never two cargo processes at once.
- Frontend gates from `apps/desktop` in PowerShell: `corepack pnpm typecheck`, `corepack pnpm lint`, `corepack pnpm build`. In bash, `export PATH="$HOME/.cargo/bin:$PATH"` first for cargo.
- Non-negotiable #3: every new threshold/flag is a `ChatConfig` field with a serde default — no hardcoded values.
- Non-negotiable #7: continuum-mcp changes are ADDITIVE only. Existing tool names/schemas must not change. New tool `memory_vault_delete` gets: schema struct, handler, permission entry in `config/default-permissions.toml`, docs in `docs/mcp-tools.md`, `EXPECTED_TOOLS` +1 in `crates/continuum-mcp/tests/protocol.rs`.
- Sensitivity gate: notes with `sensitivity == Sensitive` are excluded from chat memory search results and injected context unless `ChatConfig.include_sensitive_memory` is true (mirrors `CuratorConfig.include_sensitive_in_context` at `crates/continuum-core/src/config.rs:484`).
- New chat tool executor writes use `Source::Chat` (`crates/continuum-memory/src/model.rs:123-133`).
- Conventional commits, scopes: `gateway`, `mcp`, `desktop`, `core`, `docs`. Work directly on `main` (maintainer-approved for this session pattern).
- All user-facing UI strings in English. Tailwind tokens only (`bg-*`, `ink-*`, `state-*`, `accent-amber`).
- Every public Rust item gets a doc comment; every new module a module-level doc comment.

---

### Task 1: Gateway tool plumbing (types + trait)

**Files:**
- Modify: `crates/continuum-gateway/src/types.rs`
- Modify: `crates/continuum-gateway/src/lib.rs` (re-exports only, trait unchanged)

**Interfaces (Produces):**

```rust
/// A tool the model may call, in provider-neutral form.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema object for the arguments.
    pub input_schema: serde_json::Value,
}

/// Executes tool calls on behalf of an adapter's internal loop.
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Returns the tool result payload as a string (JSON or prose).
    /// Err(String) is delivered to the model as an error tool result —
    /// it must NOT abort the chat turn.
    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<String, String>;
}

/// MCP attachment for the Claude CLI adapter (ignored by HTTP adapters).
#[derive(Debug, Clone)]
pub struct McpSpec {
    /// Path to the MCP server binary (continuum-mcp).
    pub server_command: std::path::PathBuf,
    /// Extra env vars for the server process (e.g. CONTINUUM_VAULT_DIR).
    pub env: Vec<(String, String)>,
    /// Values for --allowedTools (joined with ","), e.g. "mcp__continuum__memory_vault_search".
    pub allowed_tools: Vec<String>,
}
```

`ChatRequest` (types.rs:64-72) gains three fields, all defaulted by a new `ChatRequest::simple(...)`-free approach — existing constructors are struct literals, so give the new fields `Default`-friendly types and update all construction sites in tests:

```rust
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    /// Tools offered to the model (HTTP adapters). Empty = no tool calling.
    pub tools: Vec<ToolDef>,
    /// Executes tool calls mid-stream. None = tools ignored even if non-empty.
    pub executor: Option<std::sync::Arc<dyn ToolExecutor>>,
    /// MCP config for the Claude CLI adapter. None = no MCP.
    pub mcp: Option<McpSpec>,
}
```

`ChatRequest` currently derives `Debug, Clone`; the trait object breaks `Debug` — replace with a manual `impl std::fmt::Debug` that prints `executor: Some(<dyn ToolExecutor>)/None`.

`ChatEvent` (types.rs:84-98) gains two variants (serde tag stays `type`, snake_case):

```rust
/// The model invoked a tool; emitted when the call is fully known.
ToolCall {
    id: String,
    name: String,
    input: serde_json::Value,
},
/// A tool finished; `output` is the string given back to the model.
ToolResult {
    id: String,
    output: String,
    is_error: bool,
    duration_ms: u64,
},
```

**Steps:**
- [ ] Write failing unit tests in `types.rs` `#[cfg(test)]`: (a) `ChatEvent::ToolCall`/`ToolResult` serialize to `{"type":"tool_call",...}` / `{"type":"tool_result",...}`; (b) manual `Debug` for `ChatRequest` with an executor present doesn't panic and mentions the field.
- [ ] `cargo test -p continuum-gateway` → FAIL (missing types).
- [ ] Implement `ToolDef`, `ToolExecutor`, `McpSpec`, the three `ChatRequest` fields, the manual `Debug`, the two `ChatEvent` variants. Re-export `ToolDef`, `ToolExecutor`, `McpSpec` from `lib.rs` alongside the existing type re-exports.
- [ ] Fix every `ChatRequest { .. }` literal in the crate (adapters' tests, `apps/desktop/src-tauri/src/chat.rs:328` comes in Task 6 — for now only the gateway crate must compile; desktop compiles in Task 6, that is acceptable mid-plan ONLY if Task 6 lands in the same session; otherwise add `..Default::default()`-style field values now). Add `tools: vec![], executor: None, mcp: None` at each site.
- [ ] `cargo test -p continuum-gateway` → PASS; `cargo clippy -p continuum-gateway --all-targets -- -D warnings`; `cargo fmt --all`.
- [ ] Commit `feat(gateway): tool-calling types (ToolDef, ToolExecutor, McpSpec, tool ChatEvents)`.

### Task 2: OpenAI-compat adapter tool loop

**Files:**
- Modify: `crates/continuum-gateway/src/providers/openai_compat.rs`

**Interfaces:**
- Consumes: `ToolDef`, `ToolExecutor`, `ChatEvent::{ToolCall,ToolResult}` from Task 1.
- Produces: behavior only (trait impl unchanged).

**Requirements:**
1. When `req.tools` is non-empty AND `req.executor.is_some()`, the request body gains `"tools": [{"type":"function","function":{"name","description","parameters":<input_schema>}}]` (OpenAI wire shape). Otherwise body is byte-identical to today.
2. Stream parsing: accumulate `choices[0].delta.tool_calls[]` fragments (each has `index`, optional `id`, optional `function.name`, `function.arguments` string fragments) into complete calls, keyed by `index`. On `finish_reason == "tool_calls"`: for each accumulated call, emit `ChatEvent::ToolCall`, run `executor.execute(name, parsed_args)` (args parse failure → error result, not a crash), time it, emit `ChatEvent::ToolResult`, append to the conversation: one assistant message with the `tool_calls` array (OpenAI shape, content null) and one `{"role":"tool","tool_call_id":id,"content":output}` message per call. Then issue a NEW streaming request with the extended messages (same body shape) and continue.
3. Loop bound: `max_rounds` tool rounds (passed in — see the `ToolLoopLimits` note below); on exceeding, append no further requests, emit `ChatEvent::Error { message: "Tool-call limit reached", retryable: false }`.
4. Containment: a 4xx response to the FIRST request when tools were included → retry once without tools (log via `tracing::warn!`), so endpoints without function-calling support still chat. Mid-loop errors surface as today.
5. `max_rounds` comes from a new public `const DEFAULT_MAX_TOOL_ROUNDS: u32 = 8;` in types.rs plus a `pub tool_max_rounds: u32` field on `ChatRequest` (add in this task, default set by callers; update Task 1's construction sites) — config plumbs it in Task 6.
6. The internal loop must respect `cancel` between rounds and during execution (select on `cancel.cancelled()` around `executor.execute`).

**Testing:** The existing tests in this file show the harness pattern (stub SSE server or parser-level tests — follow whichever exists). Required cases: (a) fragment accumulation merges split `arguments` across deltas; (b) `finish_reason:"tool_calls"` triggers executor and a second request containing the `role:"tool"` message; (c) executor `Err` produces a tool message and the loop continues; (d) tools omitted from body when `tools` empty; (e) round limit emits the error event. Use a mock `ToolExecutor` (handwritten, records calls).

**Steps:**
- [ ] Write failing tests for (a)–(e).
- [ ] `cargo test -p continuum-gateway` → FAIL.
- [ ] Implement per requirements 1–6.
- [ ] `cargo test -p continuum-gateway` → PASS; clippy; fmt.
- [ ] Commit `feat(gateway): OpenAI-compat function-calling loop with vault executor support`.

### Task 3: Anthropic adapter tool loop

**Files:**
- Modify: `crates/continuum-gateway/src/providers/anthropic.rs`

**Interfaces:** Consumes Task 1 types; behavior only.

**Requirements:**
1. `req.tools` non-empty + executor present → body gains `"tools": [{"name","description","input_schema"}]` (Anthropic wire shape, no wrapper object).
2. Stream parsing additions: `content_block_start` with `content_block.type == "tool_use"` opens an accumulator (`id`, `name`, JSON string buffer); `content_block_delta` with `delta.type == "input_json_delta"` appends `partial_json`; `content_block_stop` closes it. On `message_delta` with `stop_reason == "tool_use"`: emit `ToolCall` per block, execute (same timing/error semantics as Task 2), emit `ToolResult`, then continue the conversation with a NEW request whose `messages` gain: one assistant message with `content` = the tool_use blocks (`[{"type":"tool_use","id","name","input"}]`, plus any text emitted earlier this round as a leading text block) and one user message with `content` = `[{"type":"tool_result","tool_use_id":id,"content":output,"is_error":bool}]` per call.
3. Same round limit, cancel, and containment semantics as Task 2 (including the 4xx retry-without-tools on the first request).
4. Text deltas emitted before a tool round still stream to the UI as today; `Done` is only emitted at the final round's `message_stop`.

**Testing:** same pattern as the file's existing tests. Cases: (a) input_json_delta accumulation across fragments to valid JSON; (b) tool_use stop_reason triggers execute + follow-up request with correct assistant/user block shapes; (c) executor error → `is_error: true` tool_result; (d) no tools → body identical to today; (e) round limit.

**Steps:**
- [ ] Failing tests (a)–(e) → `cargo test -p continuum-gateway` FAIL → implement → PASS; clippy; fmt.
- [ ] Commit `feat(gateway): Anthropic tool_use loop`.

### Task 4: Claude CLI adapter — MCP attachment + tool event passthrough

**Files:**
- Modify: `crates/continuum-gateway/src/providers/claude_cli.rs`

**Interfaces:** Consumes `McpSpec` from Task 1.

**Requirements:**
1. In `build_command` (claude_cli.rs:85-113): when `req.mcp` is `Some(spec)`, write a `NamedTempFile` containing `{"mcpServers":{"continuum":{"type":"stdio","command":<spec.server_command>,"args":[],"env":{<spec.env>}}}}`, then pass `--mcp-config <path> --strict-mcp-config --allowedTools <spec.allowed_tools joined ",">` and keep `--permission-mode default`. When `None`, flags stay exactly as today (`--allowedTools ""`). The temp file must live as long as the child (same `_keep_alive` pattern as the prompt file at claude_cli.rs:192).
2. Stream parsing additions in the stdout loop: `assistant` events whose `message.content[]` contains `tool_use` blocks → emit `ChatEvent::ToolCall { id, name, input }` once per block id (the CLI re-emits snapshots with `--include-partial-messages`; dedupe by id, only emit when `input` is complete — i.e. from the non-partial assistant message event, recognizable because partial snapshots carry `"partial":true`-style markers or simply dedupe: first sighting of an id with non-empty input wins, later sightings ignored). `user` events whose `message.content[]` contains `tool_result` blocks → emit `ChatEvent::ToolResult { id: tool_use_id, output: <stringified content>, is_error, duration_ms: 0 }` (CLI does not report durations; 0 means unknown — frontend hides 0).
3. `tool_result` content can be a string or an array of `{type:"text",text}` blocks — flatten to a single string.
4. No executor involvement — the CLI runs the tools itself.

**Testing:** this file's existing tests use a stub binary — follow that pattern. Cases: (a) `mcp: Some` produces `--mcp-config`/`--strict-mcp-config`/joined allowedTools in the command (inspect via a stub that echoes args, or test a extracted `fn mcp_args(spec) -> Vec<OsString>` helper); (b) a stdout fixture with assistant tool_use + user tool_result lines yields ToolCall + ToolResult events exactly once per id; (c) tool_result array content flattens.

**Steps:**
- [ ] Failing tests → implement → `cargo test -p continuum-gateway` PASS; clippy; fmt.
- [ ] Commit `feat(gateway): Claude CLI MCP attachment + tool event passthrough`.

### Task 5: MCP `memory_vault_delete` tool (additive)

**Files:**
- Modify: `crates/continuum-mcp/src/tools/memory.rs`, `crates/continuum-mcp/src/server.rs`
- Modify: `config/default-permissions.toml`, `docs/mcp-tools.md`
- Test: `crates/continuum-mcp/tests/protocol.rs`

**Requirements:**
1. New tool `memory_vault_delete`: input `{ "id": String }`, calls `Vault::delete(id)` (`crates/continuum-memory/src/vault.rs:441`). Response `{ "deleted": true, "id": ... }`. Unknown id → the vault error maps through the existing `vault_err_to_mcp` (memory.rs:195) as invalid_params/not-found, NOT a crash.
2. Register in `server.rs` `#[tool_router]` block next to `memory_vault_resolve` (server.rs:769). Follow the exact doc-comment + schema-struct pattern of the neighbors.
3. Permission entry in `config/default-permissions.toml` mirroring `memory_vault_save`'s entry.
4. Document in `docs/mcp-tools.md` next to the other vault tools; note it permanently deletes the markdown file (unlike resolve/reject which keeps it with a status).
5. `EXPECTED_TOOLS` in `tests/protocol.rs` +1. Existing tool schemas untouched (non-negotiable #7).

**Steps:**
- [ ] Failing protocol test (EXPECTED_TOOLS updated first) → `cargo test -p continuum-mcp` FAIL → implement tool → PASS; clippy `-p continuum-mcp`; fmt.
- [ ] Commit `feat(mcp): memory_vault_delete tool`.

### Task 6: Desktop — VaultToolExecutor, config, chat.rs wiring

**Files:**
- Create: `apps/desktop/src-tauri/src/chat_tools.rs`
- Modify: `apps/desktop/src-tauri/src/chat.rs`, `apps/desktop/src-tauri/src/main.rs` (module decl), `apps/desktop/src-tauri/src/chat_store.rs`
- Modify: `crates/continuum-core/src/config.rs` (ChatConfig)

**Interfaces (Produces):**

`ChatConfig` (config.rs:214-249) gains, all `#[serde(default = ...)]`:
```rust
pub memory_tools_enabled: bool,        // default true
pub memory_tool_max_rounds: u32,       // default 8
pub memory_context_notes_max: u32,     // default 6  (0 disables prompt injection)
pub include_sensitive_memory: bool,    // default false
```

`chat_tools.rs`:
```rust
/// In-process vault executor for chat tool calls (all layers stay in the
/// desktop process; no extra subprocess for HTTP providers).
pub struct VaultToolExecutor {
    pub vault: std::sync::Arc<continuum_memory::Vault>,
    pub include_sensitive: bool,
}
#[async_trait::async_trait]
impl continuum_gateway::ToolExecutor for VaultToolExecutor { ... }

/// The four provider-neutral tool definitions.
pub fn memory_tool_defs() -> Vec<continuum_gateway::ToolDef>;
/// Allowed-tools list + env for the Claude CLI MCP path.
pub fn mcp_spec(vault_dir: &Path, dev_dir: &Path) -> Option<continuum_gateway::McpSpec>;
/// Formats vault hits into a "## Memory context" system-prompt section.
pub fn memory_context_section(notes: &[continuum_memory::NodeSummary]) -> String;
```

**Requirements:**
1. Four tools (names, schemas exact):
   - `memory_search` `{query: string, limit?: integer}` → JSON array of `{id, title, type, status, snippet, tags, project, updated}` from `Vault::search(query, limit.min(25))`; filter out `Sensitivity::Sensitive` unless `include_sensitive`; filter out `Rejected`/`Superseded` statuses.
   - `memory_get` `{id: string}` → JSON `{id, title, type, status, tags, project, body}` from `Vault::get`.
   - `memory_save` `{title: string, content: string, type?: string, tags?: string[], project?: string}` → same-title upsert (mirror `find_existing_note_id` semantics from `crates/continuum-mcp/src/tools/memory.rs:213-226`: `Vault::search(title, 25)` + case-insensitive exact title match → `Vault::save` on the existing note with updated body/tags, else `Vault::create(NoteDraft { source: Source::Chat, status: Confirmed, .. })`). Invalid `type` string → default `note`, don't error. Returns `{id, updated: bool}`.
   - `memory_delete` `{id: string}` → `Vault::delete`; returns `{deleted: true}`.
2. Executor errors: every vault error → `Err(short message)` (delivered to the model), never a panic; log with `tracing::warn!(layer = "desktop", component = "chat_tools", ...)`.
3. `mcp_spec`: resolve `continuum-mcp.exe` — try `CONTINUUM_MCP_BIN` env, then sibling of `std::env::current_exe()`, then bare name (mirrors `resolve_mcp_binary`, `crates/continuum-core/src/orchestrator/spawn.rs:335-373`). env: `("CONTINUUM_VAULT_DIR", vault_dir)`, `("CONTINUUM_DATA_DIR", dev_dir)`. allowed_tools: `mcp__continuum__memory_vault_search`, `mcp__continuum__memory_vault_get`, `mcp__continuum__memory_vault_save`, `mcp__continuum__memory_vault_resolve`, `mcp__continuum__memory_vault_delete`, `mcp__continuum__memory_get_fact`, `mcp__continuum__memory_list_facts`, `mcp__continuum__memory_query_episodic`. Returns `None` when the binary can't be found anywhere (chat degrades to no tools, log once).
4. `chat.rs` wiring in `chat_send_message` (chat.rs:230-357): add `memory: tauri::State<'_, Arc<crate::memory::MemoryState>>` param; when `chat_cfg.memory_tools_enabled` and the vault opens (`memory.vault().await` — on Err, log + continue without tools): build executor + `memory_tool_defs()`; for `ProviderKind::ClaudeCli` set `req.mcp = mcp_spec(...)` and leave `tools` empty (CLI path uses MCP); for the HTTP kinds set `tools` + `executor`. Set `tool_max_rounds` from config.
5. Prompt injection: before building `req`, when `memory_context_notes_max > 0` and vault open: `Vault::search(<last user message, truncated to 200 chars>, memory_context_notes_max * 2)`, keep `Confirmed` + sensitivity-gated, truncate each body/snippet to 300 chars, cap at `memory_context_notes_max`, append `memory_context_section(...)` to the system prompt: a `## Memory context` block listing `- [type] title: snippet` lines plus one instruction line ("These are the user's saved memories. Use them; call memory_search for more."). Empty results → no section.
6. System prompt: extend `system_prompt()` (chat.rs:117-134) with a `## Memory tools` section ONLY when tools are active this turn, instructing: save when the user asks to remember something or states a durable personal fact (name, preference, decision); search before answering questions about the user or past work; delete/update when asked; keep saved titles short and reuse existing titles to update. Mention the exact tool names that are active for this provider kind (the four generic ones, or the `mcp__continuum__memory_vault_*` family for CLI).
7. Persistence: `StoredMessage` (chat_store.rs:13-26) gains `#[serde(default)] pub tool_calls: Vec<StoredToolCall>` with `StoredToolCall { id: String, name: String, input: serde_json::Value, output: Option<String>, is_error: bool, duration_ms: u64 }`. The consumer task in `chat_send_message` (chat.rs:370-471) accumulates `ToolCall`/`ToolResult` events into this vec and saves it on the assistant `StoredMessage` alongside `content`.
8. `preferred_language` and existing behavior untouched; `cargo test -p continuum-desktop` keeps passing.

**Testing:** unit tests in `chat_tools.rs` against `Vault::open` on a `tempfile::tempdir` vault: (a) save creates then updates on same title (case-insensitive); (b) search hides Sensitive notes unless flag, hides Rejected; (c) delete removes; (d) unknown tool name → Err; (e) `memory_context_section` formats and truncates. `chat_store.rs` roundtrip test: message with tool_calls serializes/deserializes; old JSON without the field still loads (serde default).

**Steps:**
- [ ] Failing tests → `cargo test -p continuum-desktop` FAIL → implement (config first: `cargo test -p continuum-core --lib` + `--no-default-features --lib` both pass) → all four gates PASS; clippy `-p continuum-desktop -p continuum-core`; fmt.
- [ ] Commit `feat(desktop): chat memory tools — vault executor, prompt context, config, persistence`.

### Task 7: Frontend — tool events into the chat UI

**Files:**
- Modify: `apps/desktop/src/lib/types.ts` (ChatStreamEvent union ~:709-712, StoredMessage type)
- Modify: `apps/desktop/src/components/chat/state.ts` (event handling :186-218, `fromStored` :111-127)
- Modify: `apps/desktop/src/components/tabs/ChatTab.tsx` (messages memo :91-105) — only if it maps parts itself

**Requirements:**
1. `ChatStreamEvent` gains `{type:"tool_call"; id; name; input}` and `{type:"tool_result"; id; output; is_error; duration_ms}` matching the Rust serde output exactly.
2. Streaming: `state.ts` keeps per-conversation in-flight invocation lists next to `streamBuffers`. `tool_call` → append a `ToolInvocation` with status `running`, `startedAt: now`; `tool_result` → mark `ok`/`error`, set `output` (parse as JSON when possible, else raw string), `durationMs` (hide when 0), `finishedAt`. The active streaming message renders these as `ContentPart{kind:"tool"}` entries BEFORE the text part (matching arrival order is fine: tools then final text). `done`/`error` clears the in-flight list (conversation reload takes over).
3. Persistence: `StoredMessage` TS type gains `tool_calls?: StoredToolCall[]`; `fromStored` maps them to leading `{kind:"tool"}` parts (status from `is_error`, `aborted` messages → status `aborted` for unfinished ones) before the text part.
4. No new components — `MessageBubble.tsx:149-153` already renders tool parts.
5. Gates: `corepack pnpm typecheck && corepack pnpm lint && corepack pnpm build` clean (warnings tolerated only where pre-existing).

**Steps:**
- [ ] Implement 1–4 → run the three gates → fix until clean.
- [ ] Commit `feat(desktop): render chat tool calls live and from history`.

### Task 8: Docs, changelog, config docs

**Files:**
- Modify: `CHANGELOG.md` (`## [Unreleased]`), `docs/mcp-tools.md` (if Task 5 left gaps), `docs/dashboard.md` or `docs/chat.md` (whichever documents the chat tab — check; add a "Memory access" subsection: what the AI can do, the four tools, the sensitivity gate, the config flags, CLI/MCP note that `continuum-mcp.exe` must be built for the Claude CLI provider)
- Modify: `config/` default config file that documents chat settings (find the one carrying `[chat]`; add the four new keys with comments)

**Steps:**
- [ ] Write docs; verify every claim against the shipped code (tool names, config keys, defaults — no invented names).
- [ ] Commit `docs(desktop): chat memory access documentation + changelog`.

---

## Self-review notes

- Task 1 intentionally lands `tool_max_rounds` via Task 2 (first consumer); Task 1's construction-site sweep must therefore be re-checked in Task 2's review.
- The CLI path cannot delete via the four generic names — it uses `memory_vault_delete` (Task 5); the prompt section (Task 6 req 6) states per-provider names precisely to avoid the model calling tools that don't exist.
- Cross-process index writes (desktop vault open + MCP server subprocess on the same vault) are safe by design: WAL + busy_timeout + watcher, per the vault spec.
- Rejected/Superseded filtering in `memory_search` prevents the model resurrecting dismissed memories; `memory_get` stays unfiltered (explicit id).
