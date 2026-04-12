# Phase 4 — MCP Tools

Kairo's orchestrator (Claude Opus 4.6 via the `claude` CLI) can call Rust-native MCP tools at wake time. The tool server is a separate binary, `kairo-mcp`, spawned by the CLI via `--mcp-config`.

This doc describes:

1. [The 11 tools](#tools) — what they do, their schemas, and example calls
2. [Security model](#security-model) — allowlist, deny list, reserved memory keys, audit
3. [Configuration](#configuration) — the `[mcp.fs].extra_paths` opt-in
4. [Verifying your install](#verifying-your-install) — E2E smoke test runbook

## Tools

All tools are addressed as `mcp__kairo__<name>` from the orchestrator's side. The server advertises `ProtocolVersion::V_2024_11_05`.

### Memory

#### `memory_query_episodic`

Semantic (vector) search over past events — wakes, responses, remembered moments, and prior tool calls.

```jsonc
{
  "name": "mcp__kairo__memory_query_episodic",
  "arguments": { "query": "SimCharts bug I fixed last week", "limit": 5 }
}
```

Returns a JSON array of hits with `id`, `ts`, `kind`, `summary`, `importance`, `tags`, `distance`.

#### `memory_list_facts`

Lists semantic facts. Optional `prefix` narrows to a dotted key namespace (e.g. `project.`).

```jsonc
{ "name": "mcp__kairo__memory_list_facts", "arguments": { "prefix": "project." } }
```

Returns an array of `{ key, value, confidence, source, updated_at }`.

#### `memory_get_fact`

Fetches one fact by exact key.

```jsonc
{ "name": "mcp__kairo__memory_get_fact", "arguments": { "key": "user.name" } }
```

Returns a single object or `null` if the key isn't set.

#### `memory_set_fact`

Stores/updates a semantic fact.

```jsonc
{
  "name": "mcp__kairo__memory_set_fact",
  "arguments": {
    "key": "user.preferred_language",
    "value": "nl",
    "source": "user_stated"
  }
}
```

- Keys starting with `system.` or `kairo.` are **rejected** — those are reserved for the runtime.
- `source` is one of `user_stated` / `observed` / `inferred` (default). Confidence is clamped by source: inferred ≤ 0.7, observed ≤ 0.8, user_stated ≤ 0.9.

### System info

#### `system_current_time`

```jsonc
{ "name": "mcp__kairo__system_current_time", "arguments": {} }
```

Returns `{ iso8601, tz_offset_minutes, epoch_ms }`.

#### `system_active_window`

Returns the foreground window's title + process name. Both empty if nothing focused.

```jsonc
{ "name": "mcp__kairo__system_active_window", "arguments": {} }
```

#### `system_clipboard_get`

Best-effort Windows clipboard read. `text` is `null` for empty clipboard, non-text content, or if another app holds the lock.

```jsonc
{ "name": "mcp__kairo__system_clipboard_get", "arguments": {} }
```

### Filesystem (read-only)

#### `fs_read_file`

Reads up to 100 KB of a UTF-8 text file. Larger files get a truncation prefix: `[truncated, showing first 100KB of <N>KB total]\n\n…`.

```jsonc
{
  "name": "mcp__kairo__fs_read_file",
  "arguments": { "path": "F:\\TRYORVIA\\kairo-ai\\README.md" }
}
```

Rejected when: path is outside the allowlist, matches a deny directory (`.ssh`, `node_modules`, etc.), or matches a deny pattern (`*.pem`, `.env`, `id_rsa*`, etc.), or is binary.

#### `fs_list_dir`

Lists up to 500 entries. Child entries that would themselves be denied are silently filtered.

```jsonc
{
  "name": "mcp__kairo__fs_list_dir",
  "arguments": { "path": "F:\\TRYORVIA\\kairo-ai\\crates" }
}
```

Returns `{ path, entries: [{ name, kind, size_bytes, modified_iso }], truncated }`.

### Web

#### `web_fetch`

HTTP GET only. Response body capped at 50 KB.

```jsonc
{
  "name": "mcp__kairo__web_fetch",
  "arguments": { "url": "https://example.com/" }
}
```

Rejected when: scheme is not http(s); host resolves to a private/loopback/link-local/unspecified/CGNAT/benchmark/ULA address; the server returns a 3xx (redirects are **not** followed — re-invoke with the target URL).

### Notification

#### `system_notification`

Shows a Windows toast via `tauri-winrt-notification`.

```jsonc
{
  "name": "mcp__kairo__system_notification",
  "arguments": { "title": "Build green", "body": "cargo test passed in 12s" }
}
```

- Title truncated at 64 chars, body at 200.
- Per-process rate limit: one toast per 10 seconds. Subsequent calls inside that window return `{ shown: false, reason: "rate-limited …" }`.

## Security model

### Filesystem allowlist

A path is allowed iff **all three** checks pass:

1. After canonicalization, the path starts with one of:
   - The Kairo data directory (`~/.kairo-dev/`)
   - Any `project.*.dir` semantic fact value
   - Any path in `[mcp.fs].extra_paths` from `~/.kairo-dev/config.toml`
2. No component below the matched root matches `DENY_DIRS` (case-insensitive): `.ssh`, `.aws`, `.gnupg`, `.docker`, `.gradle`, `User Data`, `Profiles`, `Crashpad`, `keychain`, `secrets`, `private`, `node_modules`, `target`, `AppData`.
3. The filename doesn't match `DENY_PATTERNS`: `*.pem`, `*.key`, `*.pfx`, `*.p12`, `*.ppk`, `*.pkcs12`, `*.crt`, `*.cer`, `*.der`, `*.jks`, `*.asc`, `id_rsa*`, `id_ed25519*`, `id_ecdsa*`, `id_dsa*`, `.env`, `.env.*`, `.envrc`, `*.kdbx`, `*.1password`.

The deny list is hardcoded. It cannot be disabled or overridden from config.

### Reserved memory keys

`memory_set_fact` rejects keys starting with `system.` or `kairo.` — those are managed by the runtime, not the orchestrator. Attempts to write to them return an `invalid_params` error explaining the reason.

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

Edit `~/.kairo-dev/config.toml`:

```toml
[mcp.fs]
extra_paths = [
  "C:/code/simcharts",
  "~/Documents/notes",
]
```

Paths support `~` expansion at load time. Denied dirs and patterns still apply inside these roots — adding `~/` as an extra root does **not** let `fs_read_file` touch `~/.ssh/id_rsa`.

## Verifying your install

### Prerequisites

- `cargo build --release -p kairo-mcp` succeeded (binary at `target/release/kairo-mcp.exe`)
- `claude --version` prints a version (authenticated with `claude login`)
- `~/.kairo-dev/` exists with at least an empty `semantic.sqlite`

### One-shot protocol smoke test

```bash
cargo test -p kairo-mcp --test protocol
```

This spawns the binary, runs the MCP handshake, verifies all 11 tools are registered, and calls `system_current_time`. Expected: `test result: ok. 1 passed`.

### Real-wake test via claude CLI

```bash
# Point the CLI at the just-built binary.
cat > /tmp/kairo-test.json <<'EOF'
{
  "mcpServers": {
    "kairo": {
      "type": "stdio",
      "command": "F:/TRYORVIA/kairo-ai/target/release/kairo-mcp.exe",
      "args": [],
      "env": { "KAIRO_DATA_DIR": "F:/TRYORVIA/kairo-ai/target/test-kairo-data" }
    }
  }
}
EOF

claude -p \
  --mcp-config /tmp/kairo-test.json \
  --strict-mcp-config \
  --allowedTools "mcp__kairo__*" \
  --permission-mode default \
  --output-format json \
  "Call system_current_time and return only the iso8601 field."
```

Expected `result` field: an ISO-8601 timestamp such as `2026-04-12T20:47:01.698257+02:00`.

### End-to-end from Kairo Core

Run the main binary (this exercises spawn.rs → MCP config generation → orchestrator wake):

```bash
cargo run --release --bin kairo
```

Trigger a wake. In `~/.kairo/logs/orchestrator.log` (or stderr if running foreground), look for:

```
INFO … MCP enabled for this wake mcp_bin=… mcp_config=…
DEBUG MCP server "kairo": Successfully connected (transport: stdio)
DEBUG MCP server "kairo": Connection established with capabilities: {"hasTools":true,…}
```

After the wake finishes, confirm the audit event:

```bash
sqlite3 ~/.kairo-dev/semantic.sqlite "SELECT COUNT(*) FROM semantic_facts;"
# Then, for episodic events, use the Kairo dashboard or a LanceDB client —
# the audit entry has kind='tool_call' and tags include the tool name.
```

If no tool was called during the wake (silent wake), that's not a failure — the tool suite is opt-in; the orchestrator calls tools only when useful.
