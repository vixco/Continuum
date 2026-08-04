# Phase 4 — MCP Tools

Continuum's orchestrator (Claude Opus 4.6 via the `claude` CLI) can call Rust-native MCP tools at wake time. The tool server is a separate binary, `continuum-mcp`, spawned by the CLI via `--mcp-config`.

This doc describes:

1. [The tools](#tools) — what they do, their schemas, and example calls
2. [Security model](#security-model) — allowlist, deny list, reserved memory keys, audit
3. [Configuration](#configuration) — the `[mcp.fs].extra_paths` opt-in
4. [Verifying your install](#verifying-your-install) — E2E smoke test runbook

## Tools

All tools are addressed as `mcp__continuum__<name>` from the orchestrator's side. The server advertises `ProtocolVersion::V_2024_11_05`.

### Memory

#### `memory_query_episodic`

Semantic (vector) search over past events — wakes, responses, remembered moments, and prior tool calls.

```jsonc
{
  "name": "mcp__continuum__memory_query_episodic",
  "arguments": { "query": "SimCharts bug I fixed last week", "limit": 5 }
}
```

Returns a JSON array of hits with `id`, `ts`, `kind`, `summary`, `importance`, `tags`, `distance`.

#### `memory_list_facts`

Lists semantic facts. Optional `prefix` narrows to a dotted key namespace (e.g. `project.`).

```jsonc
{ "name": "mcp__continuum__memory_list_facts", "arguments": { "prefix": "project." } }
```

Returns an array of `{ key, value, confidence, source, updated_at }`.

#### `memory_get_fact`

Fetches one fact by exact key.

```jsonc
{ "name": "mcp__continuum__memory_get_fact", "arguments": { "key": "user.name" } }
```

Returns a single object or `null` if the key isn't set.

#### `memory_set_fact`

Stores/updates a semantic fact.

```jsonc
{
  "name": "mcp__continuum__memory_set_fact",
  "arguments": {
    "key": "user.preferred_language",
    "value": "nl",
    "source": "user_stated"
  }
}
```

- Keys starting with `system.` or `continuum.` are **rejected** — those are reserved for the runtime.
- `source` is one of `user_stated` / `observed` / `inferred` (default). Confidence is clamped by source: inferred ≤ 0.7, observed ≤ 0.8, user_stated ≤ 0.9.

> **Vault redirect.** `memory_set_fact` no longer writes to the legacy `semantic.sqlite` store — its request/response schema is unchanged, but internally it now writes a `type: fact` note into the memory vault (see [Vault memory](#vault-memory) below and `docs/memory.md`): title `key.replacen('.', ": ", 1)` (e.g. `user.preferred_language` → `user: preferred_language`), tagged with the key's first `.`-segment, `source: agent_run`, `source_ref: "mcp:set_fact:<key>"`. A second `memory_set_fact` call with the same key updates that note in place rather than creating a duplicate. `memory_get_fact` and `memory_list_facts` read the vault first (using the same key↔title mapping, or a tag-prefix search for `memory_list_facts`'s `prefix`) and fall back to a legacy `semantic.sqlite` read on **any** vault miss — not just "no matching note" but also a vault the server couldn't open or read at all (disk full, permission change, unrecoverable index failure; logged as a `warn` and never surfaced as a tool error). The fallback also covers facts written before this redirect shipped, or never migrated (see `docs/memory.md`'s migration section).

### Vault memory

Direct access to the memory vault — the markdown-note store described in `docs/memory.md`. Unlike `memory_set_fact`'s narrow key/value shape, these tools work with the vault's full node model: typed nodes (`project`, `goal`, `task`, `decision`, `person`, `preference`, `fact`, `error`, `session`, `note`), a status lifecycle (`candidate → confirmed | rejected | superseded | archived`), typed relations, and tags.

#### `memory_vault_search`

Full-text search over titles/bodies/tags. Optional `types` and `project` filters are applied after the text match.

```jsonc
{
  "name": "mcp__continuum__memory_vault_search",
  "arguments": { "query": "SimCharts lobby", "types": ["decision"], "limit": 5 }
}
```

Returns a JSON array of node summaries: `{ id, slug, title, type, status, project, confidence, importance, source, sensitivity, created, updated, tags, snippet }`.

#### `memory_vault_get`

Fetches a single note by id: full frontmatter, body, and backlinks (other notes with a resolved edge pointing at this one).

```jsonc
{ "name": "mcp__continuum__memory_vault_get", "arguments": { "id": "mem_01j8f3a6k2..." } }
```

Errors (`invalid_params`) if the id doesn't exist.

#### `memory_vault_save`

Creates a confirmed note (`status: confirmed`, `source: agent_run`) — or, if a note with the same title already exists (case-insensitive), updates it in place instead of creating a duplicate.

```jsonc
{
  "name": "mcp__continuum__memory_vault_save",
  "arguments": {
    "type": "decision",
    "title": "Lobby creation must be manual",
    "body": "Automatic lobby creation caused duplicate rooms; always require a manual click.",
    "project": "sidelife",
    "confidence": 0.9,
    "tags": ["lobby", "unity"]
  }
}
```

Returns `{ id, updated }` (`updated: true` when an existing note was matched and edited rather than a new one created). On an update, omitted optional fields (`confidence`, `importance`, `project`, `relations`, `tags`, `source_ref`) leave the existing note's values untouched — only fields explicitly present in the call are overwritten. `type`, `title`, and `body` are always applied.

#### `memory_vault_resolve`

Resolves a candidate note. `action` is `confirm` | `reject` | `supersede`; `supersede` requires `replaces` (the id of the node this one supersedes).

```jsonc
{
  "name": "mcp__continuum__memory_vault_resolve",
  "arguments": { "id": "mem_01j9...", "action": "supersede", "replaces": "mem_01j7..." }
}
```

Errors if `id` is not currently a candidate, or if `action` is `"supersede"` without `replaces`.

#### `memory_vault_delete`

Permanently deletes a vault note: removes its markdown file from disk and its index entry. This is different from `memory_vault_resolve`'s `reject` (or the curator's `archive` transition) — those keep the file in place and just change its `status`; `memory_vault_delete` removes the file itself, and that removal cannot be undone by any other vault tool.

```jsonc
{ "name": "mcp__continuum__memory_vault_delete", "arguments": { "id": "mem_01j8f3a6k2..." } }
```

Returns `{ deleted: true, id }`. Errors (`invalid_params`) if the id doesn't exist.

Other notes that link to the deleted id (via `[[wiki-links]]`) are not rewritten — the vault degrades this gracefully rather than erroring: on the vault's next reindex of the linking note, that link simply no longer resolves to a node (same as a `[[wiki-link]]` to a title that was never created). This is existing vault behavior, not something `memory_vault_delete` adds. The linking note's own body and relations are untouched; none of the current `memory_vault_*` tools surface unresolved links directly (that's internal index state).

#### `memory_wipe_all`

Requests a wipe of derived memory data (raw perception log, episodic memory, the vault's timeline events). Requires `confirm` to equal the literal string `"WIPE"`.

```jsonc
{ "name": "mcp__continuum__memory_wipe_all", "arguments": { "confirm": "WIPE" } }
```

Writes `<data_dir>/wipe-request.json` (atomic tmp+rename) with `{ requested_at, scopes: ["raw_log", "episodic", "events"] }` — the same contract the dashboard's "Wipe derived data" action and the runtime's daily hygiene tick use. This tool only **queues** the request; the running `continuum` runtime drains it at its next boot or daily hygiene tick. Vault markdown notes are **never** deleted by this or any other wipe path.

### System info

#### `system_current_time`

```jsonc
{ "name": "mcp__continuum__system_current_time", "arguments": {} }
```

Returns `{ iso8601, tz_offset_minutes, epoch_ms }`.

#### `system_active_window`

Returns the foreground window's title + process name. Both empty if nothing focused.

```jsonc
{ "name": "mcp__continuum__system_active_window", "arguments": {} }
```

#### `system_clipboard_get`

Best-effort Windows clipboard read. `text` is `null` for empty clipboard, non-text content, or if another app holds the lock.

```jsonc
{ "name": "mcp__continuum__system_clipboard_get", "arguments": {} }
```

#### `system_live_context`

Reads the local `live-context.json` projection shared by triage and all agent
roles. Returns `available`, `stale`, a compact source-attributed text form, and
the versioned structured state. Raw screenshots, key values, pointer data,
clipboard contents, and terminal text are not part of this contract. The tool
is read-only and reports unavailable before the runtime publishes its first
snapshot.

```jsonc
{ "name": "mcp__continuum__system_live_context", "arguments": {} }
```

### Filesystem (read-only)

#### `fs_read_file`

Reads up to 100 KB of a UTF-8 text file. Larger files get a truncation prefix: `[truncated, showing first 100KB of <N>KB total]\n\n…`.

```jsonc
{
  "name": "mcp__continuum__fs_read_file",
  "arguments": { "path": "F:\\TRYORVIA\\continuum-ai\\README.md" }
}
```

Rejected when: path is outside the allowlist, matches a deny directory (`.ssh`, `node_modules`, etc.), or matches a deny pattern (`*.pem`, `.env`, `id_rsa*`, etc.), or is binary.

#### `fs_list_dir`

Lists up to 500 entries. Child entries that would themselves be denied are silently filtered.

```jsonc
{
  "name": "mcp__continuum__fs_list_dir",
  "arguments": { "path": "F:\\TRYORVIA\\continuum-ai\\crates" }
}
```

Returns `{ path, entries: [{ name, kind, size_bytes, modified_iso }], truncated }`.

### Web

#### `web_fetch`

HTTP GET only. Response body capped at 50 KB.

```jsonc
{
  "name": "mcp__continuum__web_fetch",
  "arguments": { "url": "https://example.com/" }
}
```

Rejected when: scheme is not http(s); host resolves to a private/loopback/link-local/unspecified/CGNAT/benchmark/ULA address; the server returns a 3xx (redirects are **not** followed — re-invoke with the target URL).

### Notification

#### `system_notification`

Shows a Windows toast via `tauri-winrt-notification`.

```jsonc
{
  "name": "mcp__continuum__system_notification",
  "arguments": { "title": "Build green", "body": "cargo test passed in 12s" }
}
```

- Title truncated at 64 chars, body at 200.
- Per-process rate limit: one toast per 10 seconds. Subsequent calls inside that window return `{ shown: false, reason: "rate-limited …" }`.

## Security model

### Filesystem allowlist

A path is allowed iff **all three** checks pass:

1. After canonicalization, the path starts with one of:
   - The Continuum data directory (`~/.continuum-dev/`)
   - Any `project.*.dir` semantic fact value
   - Any path in `[mcp.fs].extra_paths` from `~/.continuum-dev/config.toml`
2. No component below the matched root matches `DENY_DIRS` (case-insensitive): `.ssh`, `.aws`, `.gnupg`, `.docker`, `.gradle`, `User Data`, `Profiles`, `Crashpad`, `keychain`, `secrets`, `private`, `node_modules`, `target`, `AppData`.
3. The filename doesn't match `DENY_PATTERNS`: `*.pem`, `*.key`, `*.pfx`, `*.p12`, `*.ppk`, `*.pkcs12`, `*.crt`, `*.cer`, `*.der`, `*.jks`, `*.asc`, `id_rsa*`, `id_ed25519*`, `id_ecdsa*`, `id_dsa*`, `.env`, `.env.*`, `.envrc`, `*.kdbx`, `*.1password`.

The deny list is hardcoded. It cannot be disabled or overridden from config.

### Reserved memory keys

`memory_set_fact` rejects keys starting with `system.` or `continuum.` — those are managed by the runtime, not the orchestrator. Attempts to write to them return an `invalid_params` error explaining the reason.

### Tool-call audit

Every tool invocation writes an episodic event with `kind=ToolCall`, tags `["tool_call", <tool_name>]`, and summary `tool=<name> args=<sanitized_json> result=<≤200-char-summary>`.

Sanitization:
- Map keys matching `/password|secret|token|apikey|auth/i` → value replaced with `[REDACTED]`.
- String values > 500 chars are truncated with a `…[+N chars]` marker.

The audit is fire-and-forget (spawned as a detached tokio task) so the tool call can return immediately; lazy episodic-store initialization (~200 ms–30 s on first use) never blocks the response.

### Web fetch

- `http` and `https` only; `file://`, `ftp://`, etc. rejected.
- Host resolved **before** the request; every resolved IP is checked against RFC 1918 private, loopback, link-local, multicast, unspecified, RFC 6598 CGNAT (100.64/10), benchmark (198.18/15), IPv6 ULA (fc00::/7), and IPv6 link-local (fe80::/10).
- 5 second total timeout, 3 second connect timeout.
- Redirects disabled entirely — `3xx` returns a `Redirected` error so the caller is forced to re-invoke against the target URL (closes redirect-SSRF).
- 50 KB body cap streamed via `Response::chunk` to prevent runaway downloads.

## Configuration

Edit `~/.continuum-dev/config.toml`:

```toml
[mcp.fs]
extra_paths = [
  "C:/code/simcharts",
  "~/Documents/notes",
]
```

Paths support `~` expansion at load time. Denied dirs and patterns still apply inside these roots — adding `~/` as an extra root does **not** let `fs_read_file` touch `~/.ssh/id_rsa`.

### Vault directory (known limitation)

The MCP server does not load the full `ContinuumConfig` today — only the `[mcp]` section (via `crate::config::load`). This means it cannot see a non-default `config.memory.vault.vault_dir` set for the runtime/dashboard; the `memory_vault_*` tools and the vault-backed `memory_set_fact`/`memory_get_fact`/`memory_list_facts` paths default to `<data_dir>/vault`, matching `MemoryVaultConfig::resolve_vault_dir`'s own default for an empty `vault_dir`.

If you've set a non-default `vault_dir` in `config.toml`, set the `CONTINUUM_VAULT_DIR` environment variable to the same absolute path in the MCP server's spawn environment (the `--mcp-config` JSON's `env` block — same place `CONTINUUM_DATA_DIR` is set) — otherwise the vault tools will silently open the wrong directory instead of erroring.

## Verifying your install

### Prerequisites

- `cargo build --release -p continuum-mcp` succeeded (binary at `target/release/continuum-mcp.exe`)
- `claude --version` prints a version (authenticated with `claude login`)
- `~/.continuum-dev/` exists with at least an empty `semantic.sqlite`

### One-shot protocol smoke test

```bash
cargo test -p continuum-mcp --test protocol
```

This spawns the binary, runs the MCP handshake, verifies the complete expected
tool registry, and calls `system_current_time`. Expected:
`test result: ok. 1 passed`.

### Real-wake test via claude CLI

```bash
# Point the CLI at the just-built binary.
cat > /tmp/continuum-test.json <<'EOF'
{
  "mcpServers": {
    "continuum": {
      "type": "stdio",
      "command": "F:/TRYORVIA/continuum-ai/target/release/continuum-mcp.exe",
      "args": [],
      "env": { "CONTINUUM_DATA_DIR": "F:/TRYORVIA/continuum-ai/target/test-continuum-data" }
    }
  }
}
EOF

claude -p \
  --mcp-config /tmp/continuum-test.json \
  --strict-mcp-config \
  --allowedTools "mcp__continuum__*" \
  --permission-mode default \
  --output-format json \
  "Call system_current_time and return only the iso8601 field."
```

Expected `result` field: an ISO-8601 timestamp such as `2026-04-12T20:47:01.698257+02:00`.

### End-to-end from Continuum Core

Run the main binary (this exercises spawn.rs → MCP config generation → orchestrator wake):

```bash
cargo run --release --bin continuum
```

Trigger a wake. In `~/.continuum/logs/orchestrator.log` (or stderr if running foreground), look for:

```
INFO … MCP enabled for this wake mcp_bin=… mcp_config=…
DEBUG MCP server "continuum": Successfully connected (transport: stdio)
DEBUG MCP server "continuum": Connection established with capabilities: {"hasTools":true,…}
```

After the wake finishes, confirm the audit event:

```bash
sqlite3 ~/.continuum-dev/semantic.sqlite "SELECT COUNT(*) FROM semantic_facts;"
# Then, for episodic events, use the Continuum dashboard or a LanceDB client —
# the audit entry has kind='tool_call' and tags include the tool name.
```

If no tool was called during the wake (silent wake), that's not a failure — the tool suite is opt-in; the orchestrator calls tools only when useful.
