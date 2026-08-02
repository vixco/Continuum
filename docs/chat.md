# Chat

The Chat tab is a general-purpose conversation surface inside the Continuum
desktop app. It talks to whatever AI provider you connect — a local model
running on your own machine, a cloud API you hold a key for, or your existing
Claude Code subscription — through a small internal gateway crate
(`continuum-gateway`) rather than by calling any provider directly from the
webview.

This is an early, vertical slice of ROADMAP Fase 3 ("Main AI gateway met
provider-adapters", "secrets lokaal en versleuteld beheren"): the provider
abstraction, the adapters, and encrypted local secret storage all exist and
are exercised by real UI today, well ahead of the full two-model
(Context Model / Main AI) architecture. See
`docs/superpowers/specs/2026-08-02-chat-tab-design.md` for the original design
spec this shipped from.

## What it does

- Multiple persisted conversations, each bound to one provider connection and
  one model.
- Streamed replies rendered as markdown (code blocks, lists, links) as tokens
  arrive — no waiting for the full response.
- Stop mid-stream: the partial reply is kept and marked `stopped` rather than
  discarded.
- Retry on a failed send, when the failure is one the gateway considers worth
  retrying (see the error glossary below).
- Per-conversation provider/model switching — later turns use the new
  model, earlier messages keep whatever model produced them.
- A built-in system prompt explaining what Continuum is, plus a live status
  footer (app version, whether the background `continuum.exe` runtime is
  currently running, which provider/model is answering). No perception,
  memory, or other runtime data is ever injected into chat context — Chat is
  a separate, sandboxed conversation, not a window into the four-layer
  runtime.

Out of scope in this first pass (see the design spec for the full list):
OAuth-based providers, cloud-platform auth (Bedrock/Vertex/Azure), the
model router / role-based selection (the data model reserves a `roles`
field for this), tool use, vision input, and file attachments.

## Connecting a provider

Providers are managed from **Settings → AI providers** (the panel commonly
called "Integrations" — component `IntegrationsPanel.tsx`). Click **Add
provider**, pick a preset from the catalog grid (or **Custom endpoint**),
fill in the fields it asks for, then **Test & save**. The connection is
tested before it's persisted; if the test fails you get an actionable error
plus a **Save anyway** button as an explicit escape hatch (the connection is
kept even though the last test failed, and shows a red status dot until you
test it again successfully).

### LM Studio / Ollama (local, no key)

1. Start the local server: in LM Studio, load a model and start its local
   server (default `http://localhost:1234/v1`); for Ollama, run
   `ollama serve` (default `http://localhost:11434/v1`).
2. In Continuum, **Settings → AI providers → Add provider**, pick the
   **LM Studio** or **Ollama** tile — the base URL prefills automatically.
   Neither preset needs an API key.
3. Click **Test & save**. Continuum calls the provider's `/models` endpoint;
   on success the connection is saved with its discovered model list, and
   the first discovered model becomes the default. On failure you'll see
   "Could not reach `<url>`. Is the server running?" — start the local server
   and test again, or use **Save anyway** to keep the connection for later.
4. Open **Chat → New chat** and start typing. Pick a different model for
   this connection any time from the model dropdown in the chat header.

### API-key providers

OpenRouter, OpenAI, DeepSeek, Fireworks, Kimi/Moonshot (+CN), z.ai/GLM,
MiniMax, xAI, StepFun, NVIDIA Build, Hugging Face, Google Gemini (OpenAI-
compatible endpoint), Qwen/DashScope, the Anthropic API, and a **Custom
endpoint** option for anything else OpenAI-compatible are all available as
catalog presets (`crates/continuum-gateway/src/catalog.rs`). Pick the preset,
paste your key into the masked **API key** field, and **Test & save**. The
key never touches `providers.json` or any log line — see
[Where data lives](#where-data-lives) below.

### Claude Code CLI (subscription)

Pick the **Claude Code (subscription)** tile. It needs no base URL and no
key — it uses your existing `claude` CLI login (`claude login`), per
non-negotiable #1: Continuum never scrapes OAuth tokens or reimplements
subscription auth, it only shells out to the official CLI, exactly like the
orchestrator does. Testing this connection runs `claude --version` plus an
auth probe; a missing binary or a logged-out CLI produce the
`CliNotFound` / `CliNotLoggedIn` errors described below.

## Where data lives

| Data | Location | Contains secrets? |
|---|---|---|
| Provider connections | `~/.continuum-dev/providers.json` | No — id, display name, kind, base URL, cached model list, default model, `requires_key` (a boolean, not the key itself), last-test timestamp/result. |
| API keys | Windows Credential Manager, service `Continuum`, account `provider:<connection_id>` | Yes — this is the *only* place a key is ever written. |
| Conversations | `~/.continuum-dev/chats/<conversation_id>.json` | No — title, provider/model, and each message's role/content/timestamp/model/duration/token-usage/`aborted` flag. |

You can open `providers.json` yourself and confirm there is no key material
in it — a unit test in `crates/continuum-gateway/src/error.rs`
(`provider_connection_serde_roundtrip_has_no_secret_fields`) enforces this at
the type level by asserting the serialized JSON never contains `"key"` (other
than the boolean field name `requires_key`), `"token"`, or `"secret"`. Keys
are read from and written to the credential store through the `keyring`
crate (`apps/desktop/src-tauri/src/providers.rs`, `KeyringSecretStore`) and
are never logged — `GatewayError::user_message()` strings are built without
interpolating header or key values.

Both `providers.json` and the per-conversation files are written atomically
(temp file + rename), the same pattern used elsewhere in the dashboard
(`automations.json`, voice intents).

## `[chat]` configuration

All knobs live under `[chat]` in `~/.continuum-dev/config.toml`
(`ChatConfig` in `crates/continuum-core/src/config.rs`) and are safe to omit
— every field has a default and the section itself is optional.

| Key | Default | Meaning |
|---|---|---|
| `max_tokens` | `8192` | Max output tokens requested per response. |
| `temperature` | unset | Sampling temperature. Only forwarded to OpenAI-compatible providers; ignored by the Anthropic and Claude CLI adapters. |
| `connect_timeout_secs` | `10` | HTTP connect timeout for provider requests. |
| `stream_idle_timeout_secs` | `60` | Abort a stream if no delta arrives for this many seconds. |
| `cli_timeout_secs` | `120` | Idle timeout for a single Claude CLI send: max seconds allowed to pass WITHOUT a new line of CLI stdout before the send is aborted. Resets on every line, so it is not an overall/end-to-end timeout — a steadily-streaming long generation is never killed by this. |
| `system_prompt_path` | unset | Path to a file that replaces the built-in system prompt (`apps/desktop/src-tauri/assets/chat-system-prompt.md`). The live-status footer is still appended after whatever text this file contains. |

Example override:

```toml
[chat]
max_tokens = 4096
stream_idle_timeout_secs = 30
system_prompt_path = "C:/Users/you/my-continuum-prompt.md"
```

## Error glossary

Every failure surfaced by the gateway maps to one actionable, secret-free
message (`GatewayError::user_message()` in
`crates/continuum-gateway/src/error.rs`). These are the exact strings you'll
see in the chat error banner or the "Add provider" modal:

| Error | Message shown | Retryable? |
|---|---|---|
| `Unauthorized` (401) | "API key rejected (401). Check the key for this provider in Settings → Integrations." | No — fix the key first. |
| `Unreachable { url }` | "Could not reach `<url>`. Is the server running? For LM Studio check the local server is started; for Ollama run `ollama serve`." | Yes. |
| `RateLimited { retry_after_secs }` | "Rate limited by the provider. Try again in `<n>` seconds." (or "…Try again shortly." if the provider didn't send a retry-after) | Yes. |
| `Timeout` | "The provider did not respond in time. Try again or pick a smaller model." | Yes. |
| `CliNotFound` | "Claude Code CLI not found. Install it: npm install -g @anthropic-ai/claude-code" | No — install first. |
| `CliNotLoggedIn` | "Claude Code is not logged in. Run: claude login" | No — log in first. |
| `BadResponse { detail }` | "Provider returned an unexpected response: `<detail>`" | Yes. |
| `Cancelled` | "Stopped." | No — this is what you get after pressing Stop, not a failure. |

"Retryable" here is what drives whether the Chat tab's inline **Retry**
button appears next to a failed send — non-retryable errors (bad key, CLI
not installed/logged in) need you to fix something in Settings first, so no
button is offered for them. A mid-stream failure never discards the partial
reply: it's persisted and the message is marked `aborted: true`, same as a
manual Stop.

## Why `localhost` is allowed here, unlike `web_fetch`

The `web_fetch` MCP tool (`crates/continuum-mcp/src/tools/web.rs`) is called
by the orchestrator against arbitrary, model-chosen URLs, so it hard-blocks
every private/loopback/link-local/multicast address to prevent SSRF: a
compromised or confused orchestrator prompt could otherwise be tricked into
fetching `http://localhost:<port>/admin` on your machine.

Chat's provider endpoints are different: the base URL comes from a form you,
the user, filled in yourself in Settings — including deliberately pointing
it at `http://localhost:1234/v1` for LM Studio or `http://localhost:11434/v1`
for Ollama. There is no untrusted model output choosing the URL, so the
SSRF threat model that justifies `web_fetch`'s restrictions doesn't apply,
and blocking localhost here would break the two most common local-model
workflows Chat exists to support. This is a deliberate, reviewed asymmetry,
not an oversight — see the design spec's Security section.

## Troubleshooting

**"Could not reach `http://localhost:1234/v1`."** LM Studio's local server
isn't running, or is running on a different port. Start/restart the local
server in LM Studio (or `ollama serve` for Ollama) and press **Test** on the
provider row in Settings.

**Claude Code CLI provider says "Claude CLI not found" even though `claude
--version` works in your terminal.** On Windows, the CLI installs as an npm
shim (`claude.cmd`), and `std::process::Command`'s PATH search doesn't
always resolve extension-less shim names the way `cmd.exe` does. Continuum
works around this by probing once with `where claude.cmd` at first use and
preferring that name when found (`resolve_claude_binary()` in
`apps/desktop/src-tauri/src/providers.rs`), falling back to the bare
`claude` otherwise. If you still see `CliNotFound`, confirm `claude.cmd` is
actually on `PATH` for the account running the desktop app (`where
claude.cmd` from the same shell/session Continuum was launched from) —
a per-user npm global install that isn't on the *system* PATH is the most
common cause when a service or a different user session launches Continuum.

**"Claude Code is not logged in."** Run `claude login` in a terminal, then
press **Test** on the connection again — Continuum doesn't manage this login
flow itself (see the localhost/OAuth non-negotiable above).

**API key rejected (401) right after pasting a fresh key.** Some providers
issue keys with a short activation delay, and a handful (OpenRouter, some
regional endpoints) reject keys that were copied with trailing whitespace.
Re-check the key field has no leading/trailing spaces and re-test.

**A provider I removed still shows up in old conversations.** That's
intentional — removing a provider deletes the connection and its credential,
but old conversations stay readable. Sending a new message in one of them
will ask you to pick a live model again.

**Nothing happens when I open the Chat tab in a browser (`pnpm dev` on
`:3000`, no Tauri).** Chat only works inside the Tauri desktop app — it
streams through the Rust backend, not through any browser-reachable API.
The tab shows an explicit "Chat needs the desktop app" empty state in this
case instead of silently doing nothing; run `pnpm tauri dev` (or the
packaged build) instead.

**Streaming stalls partway through a long reply.** Check
`[chat].stream_idle_timeout_secs` — the default (60s) aborts a stream if the
provider goes silent for that long. A local model on a slow/CPU-only
machine can legitimately take that long between tokens on a big prompt;
raise the timeout rather than assuming the provider is broken.

**Nothing shows a green status dot after Test even though the request
seemed to succeed.** The dot reflects `last_test_ok`, refreshed by every
Test/Refresh/Add call — if the very first request after adding the provider
timed out or 429'd, retry with **Test** on that row rather than relying on
the initial add.
