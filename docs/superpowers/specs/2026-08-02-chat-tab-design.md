# Chat tab + model gateway — design spec

Date: 2026-08-02
Status: approved by maintainer (Arda), pending implementation plan
Scope owner: desktop app + new gateway crate

## Goal

Add a working **Chat** tab to the Continuum desktop app where users talk to an
AI of their choice, and make **Settings → Integrations & Models** real so users
can connect multiple AI providers — local (LM Studio, Ollama) or cloud (API
keys) — as described in `Continuum.md` §2 (two-model architecture), §14 (model
connections), §15 (model router) and §16 (Model Gateway component).

This delivers early vertical slices of roadmap Phase 3 ("Main AI gateway met
provider-adapters", "secrets lokaal en versleuteld beheren") without claiming
the full two-model architecture.

## In scope (v1)

- New `crates/continuum-gateway` workspace crate (the Model Gateway from
  `Continuum.md` §16): provider trait, three adapters, provider catalog.
- Provider connections CRUD in Settings → Integrations, with
  test-before-save connection checks (reachability, model listing).
- API keys stored exclusively in Windows Credential Manager (`keyring` crate).
- Chat tab: multiple persisted conversations, streaming responses, per-
  conversation model picker, markdown rendering, stop/cancel, retry on error.
- Built-in system prompt: what Continuum is + a small live-status block.
- Adapters: OpenAI-compatible (covers ~30 providers + custom endpoints),
  Anthropic native (API key), Claude CLI (subscription via official CLI).

## Out of scope (v1) — explicitly deferred

- OAuth/PKCE providers (Qwen OAuth, MiniMax OAuth, SuperGrok, GitHub Copilot,
  ChatGPT/Codex OAuth). Copilot/ChatGPT/Claude-Max token flows as done by
  third-party tools violate non-negotiable #1 (never scrape OAuth tokens /
  reverse-engineer subscription auth); Claude subscription access goes through
  the official `claude` CLI instead. Any future OAuth provider needs its own
  review.
- Cloud-platform auth (AWS Bedrock, Google Vertex, Azure Continuum).
- Model router / role-based selection (§15) — the data model reserves a
  `roles` field; no routing logic in v1.
- Tool use / function calling, vision input, file attachments in chat.
- Injecting project/memory data into chat context (Phase 0 has only fixture
  data; presenting fixtures as live context is forbidden).

## Architecture

```
apps/desktop frontend (Next.js)
  ChatScreen (new)            SettingsScreen → Integrations (made real)
        │ Tauri invoke/events        │
apps/desktop src-tauri
  chat commands + provider commands
  conversation store (JSON, atomic)  keyring (Windows Credential Manager)
  system prompt builder
        │
crates/continuum-gateway (new, pure Rust, no Tauri deps)
  ChatProvider trait · OpenAICompatAdapter · AnthropicAdapter ·
  ClaudeCliAdapter · provider catalog · shared types/errors
```

Approach chosen over alternatives: (A) everything inside `src-tauri` (faster
now, must be extracted for Phase 3 anyway, bloats the desktop crate) and
(C) provider calls from the webview (keys in webview, CSP loosening, CORS,
no CLI spawning — rejected outright).

## continuum-gateway crate

### Types

```rust
pub enum ProviderKind { OpenAiCompat, Anthropic, ClaudeCli }

pub struct ProviderConnection {
    pub id: String,              // slug/uuid
    pub display_name: String,
    pub kind: ProviderKind,
    pub base_url: Option<String>,   // None for ClaudeCli
    pub catalog_id: Option<String>, // preset it was created from
    pub models: Vec<String>,        // cached from last refresh
    pub default_model: Option<String>,
    pub roles: Vec<String>,         // reserved for Phase 3 (main_ai, context_model)
    pub requires_key: bool,         // whether a credential entry exists
    pub last_tested_at: Option<DateTime<Utc>>,
    pub last_test_ok: Option<bool>,
}
// NOTE: no secret material in this struct, ever.
```

```rust
#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn test_connection(&self) -> Result<ConnectionTestReport, GatewayError>;
    async fn list_models(&self) -> Result<Vec<String>, GatewayError>;
    async fn stream_chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ChatEvent>, GatewayError>;
}

pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>, // role user|assistant, content
    pub max_tokens: u32,            // from config
    pub temperature: Option<f32>,
}

pub enum ChatEvent {
    Delta(String),
    Done { usage: Option<TokenUsage>, stop_reason: Option<String> },
    Error(GatewayError),
}
```

`GatewayError` is a `thiserror` enum with actionable variants:
`Unauthorized`, `Unreachable { url }`, `RateLimited { retry_after_secs }`,
`Timeout`, `CliNotFound`, `CliNotLoggedIn`, `BadResponse { detail }`,
`Cancelled`.

### Adapters

1. **OpenAICompatAdapter** — `POST {base_url}/chat/completions` with
   `stream: true`, SSE parsing (`data:` lines, `[DONE]` terminator);
   `GET {base_url}/models` for listing. API key sent as `Authorization:
   Bearer` when present (LM Studio/Ollama work keyless). Covers OpenRouter,
   OpenAI, DeepSeek, Fireworks, Kimi/Moonshot (+CN), z.ai/GLM, MiniMax (+CN),
   xAI, StepFun, NVIDIA Build, HuggingFace router, Gemini's OpenAI-compat
   endpoint, DashScope-compat, Arcee, GMI, LM Studio, Ollama, custom.
2. **AnthropicAdapter** — Anthropic Messages API with streaming and
   `x-api-key` auth. Exact request/stream shapes to be taken from current
   Anthropic API docs during implementation (load the `claude-api` reference
   before writing this adapter — do not code it from memory).
3. **ClaudeCliAdapter** — spawns the official CLI per the repo's canonical
   pattern (`claude --print --output-format stream-json --input-format
   stream-json --model <m>`), writes one user message (system prompt prepended
   via `--append-system-prompt-file`), maps `text_delta` stream events to
   `ChatEvent::Delta`, kills the child on cancel (`kill_on_drop`). Multi-turn:
   conversation history is replayed in the user message payload (fresh process
   per send, mirroring ADR-005). `test_connection` = `claude --version` +
   auth probe; missing binary/login map to `CliNotFound` / `CliNotLoggedIn`.

### Provider catalog

Static table in the crate: `{ id, label, kind, default_base_url,
needs_key, docs_url }` for every preset named above plus `custom`.
Unit test asserts unique ids and well-formed URLs. The catalog is data,
not code — adding a preset is a one-entry change.

## Desktop layer (src-tauri)

### New Tauri commands

Providers: `providers_list`, `provider_add { catalog_id | custom fields,
display_name, api_key? }` (stores key via keyring, then saves connection),
`provider_test(id)`, `provider_refresh_models(id)`, `provider_remove(id)`
(also deletes credential), `provider_set_default_model(id, model)`.

Chat: `chat_list_conversations`, `chat_get_conversation(id)`,
`chat_create_conversation { provider_id, model }`,
`chat_delete_conversation(id)`, `chat_rename_conversation(id, title)`,
`chat_send_message { conversation_id, text }` (persists user msg, starts
stream), `chat_cancel(conversation_id)`.

Events: `continuum:chat` with payload `{ conversation_id, event }` where
event mirrors `ChatEvent` (delta / done / error), following the existing
`continuum:log` event pattern. One in-flight stream per conversation;
`chat_send_message` while streaming returns an error.

### Storage

- `~/.continuum-dev/providers.json` — `Vec<ProviderConnection>` (no secrets),
  atomic write via tmp + rename (same helper style as `automations.json`).
- `~/.continuum-dev/chats/<conversation_id>.json` — `{ id, title,
  provider_id, model, created_at, updated_at, messages: [{ role, content,
  ts, model?, duration_ms?, usage?, aborted? }] }`.
- Credential Manager entries: service `Continuum`, account
  `provider:<connection_id>`.

### System prompt

Built per send: a static Continuum explainer (what Continuum is, its goal —
local context/handoff/permission layer for coding agents — how it is used;
sourced from README + CONTINUUM_ARCHITECTURE, stored as an asset file so it
is editable and configurable via `[chat].system_prompt_path`) + a live
status block: app version, runtime (continuum.exe) running or not, connected
provider count, active model. No fixture data is ever injected.

## Frontend

### Chat tab

- `TabId` union + NAV entry `{ id: "chat", label: "Chat", icon:
  MessagesSquare }`, placed directly after Home. CommandPalette gains
  "Open Chat".
- New file `components/continuum/ChatScreen.tsx` (do NOT grow the 1900-line
  `screens.tsx`); shared primitives reused from `continuum/ui.tsx`.
- Layout: left conversation rail (New chat, titles auto-derived from first
  user message, delete with confirm); center thread (user/assistant styling
  per the graphite/amber contract, markdown + code blocks via `react-markdown`,
  live streaming text, per-message footer: model + duration); bottom composer
  (textarea, Enter send / Shift+Enter newline, Stop button while streaming,
  provider→model dropdown persisted per conversation).
- Empty states: no providers → CTA linking to Settings → Integrations;
  browser dev mode (`isTauri()` false) → "open the desktop app to chat".
- Keyboard and 1600×1000 viewport validated (Phase 0 UI rules).

### Settings → Integrations (made real)

- Connected list: name, kind badge, status dot (green/red from last test),
  model count, default model, actions Test / Refresh models / Remove.
- Add provider modal: catalog grid → prefilled base URL; fields name +
  masked key (hidden for keyless presets); connection test runs before save
  with an explicit "save anyway" escape hatch.
- Fixture content in that panel is replaced, not left alongside live data.

## Error handling

- Every error surfaces an actionable message: 401 → check key in Settings;
  unreachable localhost → "Is LM Studio running?"; 429 → retry-after; CLI
  missing → install/`claude login` instructions.
- Mid-stream failure keeps partial text, marks the message `aborted`, shows
  inline error + Retry. No silent retries (Continuum.md §25).
- Timeouts: 10 s connect, 60 s stream-idle (configurable under `[chat]`).
- Cancel: CancellationToken aborts HTTP request or kills the CLI child;
  partial message is persisted and marked.
- Removing a provider keeps old conversations readable; sending in them
  prompts for a new model.

## Security

- Non-negotiable #1: no OAuth scraping; subscription Claude only via the
  official CLI.
- Non-negotiable #2: the only new network calls go to user-configured
  provider endpoints; stated explicitly in the PR description.
- Non-negotiable #6: keys only in Credential Manager; never in JSON, logs,
  or tracing fields; header values never logged.
- CSP unchanged — all HTTP lives in Rust.
- Localhost endpoints are deliberately allowed here, unlike `web_fetch`'s
  SSRF rules: the user explicitly configures their own endpoint. Documented
  in `docs/chat.md`.
- Config surface (non-negotiable #3): `[chat]` section — `max_tokens`,
  `temperature`, `connect_timeout_secs`, `stream_idle_timeout_secs`,
  `system_prompt_path`.

## Self-healing (non-negotiable #5)

- Gateway/chat failures log structured events (`layer=desktop`,
  `component=chat`) to the existing log pipeline.
- A `chat_providers` health probe reports Degraded when the last connection
  test of any default provider failed; recovery procedure documented in
  `docs/self-healing.md` (re-run test, re-enter key, restart app).
- No runtime restart hooks needed: chat state is plain files; the repair
  agent can read `providers.json` and the logs.

## Testing

- `continuum-gateway` (pure Rust, light CI job): SSE parser unit tests
  (chunked deltas, `[DONE]`, malformed lines), request builders per adapter,
  error mapping (401/429/timeout/unreachable), catalog integrity; adapters
  against a local mock HTTP server; ClaudeCliAdapter against a stub
  executable (worker dry-run pattern).
- Desktop: conversation/provider store roundtrips (tempfile pattern),
  key-flow against a mock keystore trait.
- Frontend: `pnpm typecheck` + lint; manual smoke: LM Studio local + one
  real API key + Claude CLI.
- CHANGELOG under `[Unreleased]`; new `docs/chat.md`; ROADMAP note that this
  ships early Phase-3 slices.

## Milestones (implementation order)

1. `continuum-gateway` crate: types, catalog, OpenAI-compat adapter + tests.
2. Anthropic + Claude CLI adapters + tests.
3. src-tauri: provider commands, keyring, providers.json, health probe.
4. src-tauri: conversation store, chat commands, streaming events.
5. Frontend: Settings Integrations panel.
6. Frontend: Chat tab.
7. Docs, CHANGELOG, ROADMAP, smoke tests, PR.
