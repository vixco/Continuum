# Phase 4 — MCP Tools

Continuum's orchestrator (Claude Opus 4.6 via the `claude` CLI) can call Rust-native MCP tools at wake time. The tool server is a separate binary, `continuum-mcp`, spawned by the CLI via `--mcp-config`.

## Permission gateway

Every tool handler is evaluated before its body runs. Effective policy is the
bundled `config/default-permissions.toml` overlaid by the user's
`<data_dir>/permissions.toml`:

- `auto`: execute immediately.
- `session-approved`: create an approval request unless a matching, unexpired
  session or one-use grant exists.
- `always-confirm`: require a fresh one-use grant for every call.
- `blocked`: refuse the call.

Requests and grants are stored as separate atomic JSON records under
`permission-requests/` and `permission-grants/`. They are scoped to the exact
tool, agent session, and—where available—the path, repository, URL, working
directory, or component. The desktop Tools tab persists policy overrides,
approves or denies requests, and revokes grants. Decisions are written to
`logs/actions.jsonl`; raw tool arguments are not stored in permission records.

Unknown tools and malformed policy fail closed. The Health repair MCP process
retains its separate short-lived, component-scoped capability and cannot use
that exception to reach repair operations excluded by its preview.

### Git checkpoints

| Tool | Tier | Effect |
|---|---|---|
| `git_diff` | auto | Bounded tracked unified diff plus porcelain status; optional checkpoint comparison. |
| `git_checkpoint_list` | auto | Lists refs below `refs/continuum/checkpoints/`. |
| `git_checkpoint` | session-approved | Creates a checkpoint commit through a temporary index without touching the real index/worktree. |
| `git_rollback` | always-confirm | Creates a safety checkpoint, preserves loose/sensitive files, then restores the requested checkpoint. |

All repository paths pass the filesystem allowlist. Checkpoints include
tracked and untracked files except hard-denied paths (`.env`, private keys,
credential directories, and the other deny rules). Rollback moves untracked
files to `.git/continuum-recovery/<safety-checkpoint>/untracked/` and copies
modified sensitive tracked files to the sibling `sensitive-tracked/` folder
before `git reset --hard`. Ignored files are not altered.

### Filesystem actions

| Tool | Default tier | Safety contract |
|---|---|---|
| `fs_create_file` | session-approved | Existing parent must be allowlisted; atomic create-new; never overwrites. |
| `fs_apply_patch` | session-approved | Exact `old_text` precondition; one match unless `replace_all`; original moved to recovery first. |
| `fs_move` | session-approved | Source and destination parent are allowlisted; destination must not exist. |
| `fs_delete_to_trash` | always-confirm | Moves the target to `<data_dir>/recovery/files/<date>/`; returns its recovery path. |

All content is UTF-8. `mcp.fs.max_write_bytes` defaults to 1 MiB and
`mcp.fs.max_patch_replacements` defaults to 100. New nested directories are not
created implicitly: the direct destination parent must already exist. Directory
moves across volumes are refused rather than copied recursively.

### Terminal and verifier

`terminal_run` (always-confirm) and `terminal_verify` (session-approved) share
one restricted subprocess broker. Requests contain `cwd`, `program`, `args[]`,
optional `timeout_secs`, and an optional display `label`. The broker:

- accepts only executable basenames listed in `mcp.terminal.allowed_programs`;
- passes arguments directly, with no shell interpolation;
- requires an allowlisted existing cwd and closes stdin;
- removes environment variables whose names indicate tokens, passwords,
  secrets, auth, cookies, credentials, or private keys;
- rejects credential-like arguments and caps argument count, timeout, and
  captured output.

`terminal_verify` writes the complete bounded result to
`<data_dir>/evidence/terminal/<id>.json` and returns the evidence id/path.
Batch, `.cmd`, and PowerShell script programs remain hard-blocked because they
would reintroduce shell parsing. The default native-program allowlist is
`cargo`, `git`, `dotnet`, `rustc`, and `rustfmt`; users can narrow or expand it
in config.

### Native IDE bridge

| Tool | Default tier | Effect |
|---|---|---|
| `ide_status` | auto | Lists configured VS Code-compatible editors whose native executable is available. |
| `ide_open_file` | session-approved | Opens one existing allowlisted file at an optional one-based line/column. |
| `ide_open_diff` | session-approved | Opens two existing allowlisted files in the editor's native diff view. |

The bridge accepts only aliases from `mcp.ide.allowed_editors` (`code`,
`code-insiders`, and `codium` by default), resolves a native executable, and
passes fixed arguments directly without a shell. `.cmd` launchers are never
executed; on Windows they are used only as a location hint for the adjacent
native editor executable. This bridge does not read unsaved buffers,
diagnostics, selections, terminal output, or debug state.

### Browser DOM bridge (optional)

`browser_status`, `browser_list_tabs`, and `browser_dom_snapshot` provide the
read boundary; `browser_navigate`, `browser_click`, and `browser_fill` are
always-confirm. Set `mcp.browser.enabled = true`, explicitly configure exact
`allowed_hosts`, and start Chromium with a dedicated profile and a loopback
remote-debugging port matching `mcp.browser.port`. Defaults expose only
`localhost` and `127.0.0.1`.

The bridge never accepts arbitrary JavaScript. Its fixed snapshot omits field
values and password controls; fill refuses password inputs and its content is
redacted from audit details. Incognito, banking, and unrelated tabs remain
inaccessible unless the user deliberately adds their exact host.

### Windows UI Automation

`windows_ui_focused_element` reads the focused element's name, automation id,
control type, and password flag. `windows_ui_invoke_focused` and
`windows_ui_set_focused_value` are always-confirm and act only on the element
focused when the call executes. Password values, coordinates, global input,
and arbitrary tree queries are unavailable. The same foreground privacy zone
used by observation tools blocks local-only and excluded applications.

### Tasks and agent evidence

`task_plan_write`, `task_plan_get`, and `task_plan_list` maintain bounded,
atomic plans under `<data_dir>/task-plans/`. Steps use explicit statuses and
link evidence ids. `evidence_record` and `evidence_list` store bounded outcome,
test, log, or agent evidence under `<data_dir>/evidence/agent/`; detailed
content is redacted from tool audit metadata.

### GitHub (optional)

Settings → Integrations contains a GitHub card. Connect launches the official
GitHub CLI browser/device flow; Continuum never receives the OAuth token. It
accepts the connection only when `gh auth status --json hosts` reports
`tokenSource: keyring`, and removes environment token overrides on every CLI
call. Disconnect removes local CLI auth; remote revocation remains available
on GitHub's Authorized OAuth Apps page.

Read tools are session-approved except `github_status` (auto):

- `github_me`
- `github_list_repos`
- `github_get_repo`
- `github_list_issues`
- `github_get_file`

Owner/repository/path inputs are validated, traversal is rejected, list sizes
are bounded, binary files are rejected, and `github.max_response_bytes` caps
JSON and decoded file responses. `github.enabled = false` is a hard kill switch.

Mutation tools always require a fresh confirmation:

- `github_create_issue`
- `github_comment_issue`
- `github_create_pull_request`

Each mutation is scoped to one validated `owner/repo`. Titles, bodies, issue
numbers, and branch refs are bounded or validated before the official `gh api`
call. Issue and pull-request bodies are omitted from audit details. Pull-request
creation expects the head branch to exist remotely and does not push it.

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

Other notes that link to the deleted id (via `[[wiki-links]]`) are not rewritten — the vault degrades this gracefully rather than erroring: `Vault::delete` calls `Index::remove_path`, which recomputes the resolved edge graph (`recompute_edges()`, `crates/continuum-memory/src/index.rs`) as part of the same delete call, so the link becomes unresolved immediately, not on some later reindex of the linking note. This is existing vault behavior, not something `memory_vault_delete` adds. The linking note's own body and relations are untouched; none of the current `memory_vault_*` tools surface unresolved links directly (that's internal index state).

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

> **Privacy: schema-stable but content-filtered.** `system_active_window`,
> `system_clipboard_get`, and `system_live_context` are *observation* tools:
> everything they return passes through the same `PrivacyFilter` the runtime's
> own frame loop uses, built from the same `config.toml` (`[privacy]` zones +
> scrubbers, and the legacy `[context]` sensitive lists synthesised into
> zones). Filtering changes **content only** — the tool names, argument
> schemas, response field names, and types are unchanged, so nothing published
> in a release moves. What changes is what the values contain: a window in a
> `never_observe` zone comes back as the `[excluded]` sentinel, `local_only`
> content is replaced by `[redacted by local privacy policy]`, and secrets
> (API keys, bearer tokens, card numbers, IBANs) are replaced by `[REDACTED]`
> in every free-text field. A zone the user configures binds these tools
> exactly as it binds the runtime — calling the MCP tool is not a way around
> it.

#### `system_active_window`

Returns the foreground window's title + process name. Both empty if nothing focused.

Zone behavior:

| Zone of the foreground window | `process_name` | `title` |
|---|---|---|
| `never_observe` | `[excluded]` | `""` (never the real title, not even redacted) |
| `local_only` | real process (scrubbed path) | `[redacted by local privacy policy]` |
| `cloud_allowed` | real process (scrubbed path) | real title, secrets scrubbed |

```jsonc
{ "name": "mcp__continuum__system_active_window", "arguments": {} }
```

#### `system_clipboard_get`

Best-effort Windows clipboard read. `text` is `null` for empty clipboard, non-text content, or if another app holds the lock.

Two additional `null` cases come from the privacy layer:

- **Kill-switch.** With `[context_tools] clipboard_tool_enabled = false` in
  `config.toml` (default `true`), the clipboard is never opened at all and the
  tool always answers `{"text": null}`. The tool stays registered and its
  schema is unchanged — this is a content switch, not a surface change.
- **Excluded foreground.** If the focused window is in a `never_observe` zone,
  the read is skipped: the clipboard most likely holds what was just copied out
  of the app the user excluded.

Whatever does come back is scrubbed for secrets.

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

**Both** returned fields are privacy-filtered — the structured `state` and the
`compact` text, which is rendered *from* the filtered state so the two can
never disagree. Per-field behavior: excluded monitors carry no caption at all,
`local_only` monitors and window titles carry the redaction literal, project
paths are home/username-scrubbed, commit *subjects* are scrubbed while commit
*ids* and branch names (structured fields) survive verbatim, `local_only`
inferred session goal/task generalize to "working in a private context", and
every remaining free-text field is secret-scrubbed.

```jsonc
{ "name": "mcp__continuum__system_live_context", "arguments": {} }
```

### Context (published runtime state)

The context engine's tool family (spec §5.2). The design rule is **every
context source is also a tool**: whatever the runtime observes and publishes,
the orchestrator can ask for on demand instead of waiting for it to be
packaged into a wake.

Four properties hold for every tool in this section:

- **Read-only.** They read files the runtime already wrote
  (`state.json`, `live-context.json`) and open the raw-log SQLite database
  with `PRAGMA query_only = ON` and no schema migrations. The runtime stays
  the single writer.
- **Privacy-gated.** Every response passes the same `PrivacyFilter` cloud
  gate as `system_live_context`: excluded windows/monitors return the
  sentinel, `local_only` content returns the redaction literal or a
  generalized phrase, paths are home/username-scrubbed, secrets are
  scrubbed.
- **They degrade; they never fail.** A cold runtime, a missing file, an
  unparseable snapshot, or a database that does not exist yet all answer
  `available: false` or `stale: true` with empty data. A context tool must
  never kill a wake with an error.
- **They are switchable.** `[context_tools] enabled = false` empties the
  whole family (same schemas, empty content). The `[privacy.toggles]`
  honest toggles empty the tools whose source was switched off: `mic` for
  `context_audio`, `screen` for `context_screen`, and `pause_all` for
  everything observational.

**Freshness needs the runtime.** These tools read published state; if
`continuum.exe` is not running, they report `available: false` (nothing was
ever published) or `stale: true` (publishing stopped). Two thresholds,
because the two files are written on different rules:

| File | Written | `stale` after | Meaning |
|---|---|---|---|
| `live-context.json` | only when content changed (content-versioned) | 10 s | this snapshot is old — on a genuinely static screen that is normal |
| `state.json` | every 2 s regardless of content | 30 s | the runtime is not publishing |

**There is no `context_sessions` tool** and there does not need to be:
past session summaries are vault notes, so
`memory_vault_search` with `types: ["session"]` is the search for them. The
`context_*` family covers what the runtime *observes*; the vault tools
cover what it *remembers*.

#### `context_session`

Continuum's live session state: current project, inferred goal and task with
a confidence, foreground app/window, recently-touched files, and the last
observed error / success / user command. Read this first when the user says
"continue" or "what was I doing".

Source: `state.json` → `session_state`. That file carries the **raw** state
by contract (each consumer applies its own gate), so this tool applies the
cloud gate itself: a `local_only` session reports generalized goal/task
("working in a private context") with `local_only: true` so the caller knows
why, while the mechanical project id survives.

```jsonc
{ "name": "mcp__continuum__context_session", "arguments": {} }
```

#### `context_window`

The foreground window right now — process, title, zone, pid, scrubbed exe
path, monitor id, seconds focused — plus the most recent focus switches with
their dwell times.

`limit` caps the switch list (default 10, max 50). Switches come from the
deduped `context_events` log opened read-only; a database that does not
exist yet returns `recent_switches: []` with `stale: true`, never an error.
A `never_observe` window comes back as the `[excluded]` sentinel with no
pid, exe path, or monitor id — those are the identity and placement of an
app the user excluded.

```jsonc
{ "name": "mcp__continuum__context_window", "arguments": { "limit": 5 } }
```

#### `context_screen`

Every connected monitor with its latest local-vision caption and privacy
zone, plus the same compact source-attributed world render
`system_live_context` returns.

Excluded monitors are still listed, with an **empty** caption (sentinel
semantics: no caption at all, not even a redaction marker) and
`zone: "never_observe"` — so the caller can see a screen exists and is
deliberately not described. `local_only` monitors carry the redaction
literal.

```jsonc
{ "name": "mcp__continuum__context_screen", "arguments": {} }
```

#### `context_audio`

The voice pipeline's latest published transcript with its timestamp, whether
the user appears to be in a call, and whether ambient mute is currently
suppressing voice output. The transcript arrives scrubbed from the pipeline
and is scrubbed again here.

With `[privacy.toggles] mic = false` (or `pause_all = true`) this answers
`available: false` with no transcript — an honest toggle silences the tool
as well as the watcher.

```jsonc
{ "name": "mcp__continuum__context_audio", "arguments": {} }
```

#### `context_projects`

Every project Continuum knows about: `configured` entries from
`[[projects.known]]`, user-`confirmed` ones, and `discovered`
auto-discovery proposals — which never participate in project resolution
until confirmed. The active project sorts first, discovery proposals last.
Root paths are home/username-scrubbed.

Source: the `projects` table, opened read-only. `available: false` means the
runtime has never booted against this data directory.

```jsonc
{ "name": "mcp__continuum__context_projects", "arguments": {} }
```

#### `context_processes`

Meaningful active background processes from the opt-in process collector.
The response contains executable basename, coarse category (`build`,
`runtime`, `ai`, `service`, `application`), PID, CPU, resident memory, start
time, and a home/username-scrubbed executable path. It never contains command
lines, environment variables, process memory, or hidden-window contents.

`category` is optional; `limit` defaults to 20 and is capped at 100. Lifecycle,
stop, and sustained-resource-pressure history is queried through
`context_timeline` with `source: "process"`. When `[process_watcher].enabled`
is false, the tool returns `available: false` even if an older snapshot remains
on disk.

```jsonc
{
  "name": "mcp__continuum__context_processes",
  "arguments": { "category": "build", "limit": 20 }
}
```

#### `context_timeline`

Continuum's deduped event log, filtered. Arguments: `since` / `until`
(RFC 3339), `types` (registry tokens), `project` (slug), `source`
(`window` | `git` | `file` | `process` | `screen` | `audio` | `system` | `voice`),
`limit` (default 50, max 200). Events come back oldest first.

Rows are **collapsed**, not raw occurrences: one row with `count: 14`
spanning `ts_first`…`ts_last` means the same thing happened fourteen times.
That is a stronger signal than a single occurrence — read it that way.

Two argument rules worth knowing:

- An unparseable `since` / `until` is **ignored** (no filter) rather than
  raising: a bad argument must not fail a tool call in the middle of a
  wake.
- An unrecognised `types` / `source` token **narrows to nothing**. It never
  widens back to "everything" — the caller asked for something specific and
  got the token wrong, and silently answering a broader question is the
  worse failure.

```jsonc
{
  "name": "mcp__continuum__context_timeline",
  "arguments": { "types": ["error", "commit"], "limit": 20 }
}
```

##### The privacy contract of the three event tools

`context_timeline`, `context_search` and `context_files` share one response
schema and one contract:

- **`local_only` rows never leave.** Spec §4.1 strips `local_only` content
  from everything cloud-bound, and a tool response is cloud-bound by
  destination. Those rows are omitted **and counted** in `omitted_private`:
  the orchestrator learns that something happened without learning what,
  which is exactly the amount of information it should have.
- **Live rules re-bind.** A row written before the user excluded an app is
  re-resolved against the *current* zone rules; anything that no longer
  resolves `cloud_allowed` joins `omitted_private` too.
- **A switched-off source is not replayed.** With `[privacy.toggles] mic =
  false`, audio/voice rows stop being returned — an honest toggle that only
  stopped the watcher while a tool replayed the log would be a dishonest
  mute. Those rows are *not* counted in `omitted_private`: they are silent,
  not private.

#### `context_search`

Full-text search over event summaries and window titles, best match first.
`limit` defaults to 20, capped at 50. Answers "when did I last see X"
without walking the timeline.

Query handling matches the vault search: punctuation is stripped and each
word becomes a prefix term joined by an implicit AND, so `build fail`
matches "build failed" and a query full of FTS syntax (`"`, `NEAR(`, `*`)
is neutralized rather than raising a syntax error. A query that normalizes
away to nothing returns no rows.

```jsonc
{
  "name": "mcp__continuum__context_search",
  "arguments": { "query": "cargo build failed", "limit": 10 }
}
```

#### `context_files`

Recent file-watcher events — created, modified, deleted, renamed, and the
storm-collapsed `files_bulk_change` row. Optional `project` filter; `limit`
defaults to 20, capped at 100. Paths are root-relative and scrubbed.

`available: false` means `[privacy.toggles] files` is off, or the file
watcher was never enabled (`[file_watcher] enabled` defaults to false).

```jsonc
{
  "name": "mcp__continuum__context_files",
  "arguments": { "project": "continuum", "limit": 15 }
}
```

#### `context_git`

Branch, dirty/staged/untracked counts, ahead/behind, conflicts, and the
HEAD commit. Two paths:

- **No arguments** — the *active* project's already-published state, read
  out of `live-context.json`. No subprocess, no filesystem access,
  `probed: false`.
- **`project: "<slug>"`** — one timeout-bounded `git status --porcelain=v2`
  plus `git log -1` against that project's root
  (`[git_context] command_timeout_secs`, default 10 s), `probed: true`.

**The consent rule.** A named probe runs only for a `configured` or
`confirmed` project. A `discovered` row is a *proposal* the auto-discovery
heuristic made from a window title, and Continuum never runs a command in a
directory the user has not adopted. Such a request comes back
`available: false` with a `reason` explaining it — not an error. The right
response is to ask the user to confirm the project on the Context page, not
to retry. The same refusal covers an unknown slug, a `never_observe`
project, a missing root, and a failed probe; `reason` always says which.

Root paths are home/username-scrubbed and the commit subject is scrubbed as
free text. `last_commit_id` is returned **verbatim** — an OID is a
structured identifier, not content (spec §4.1).

```jsonc
{ "name": "mcp__continuum__context_git", "arguments": {} }
{ "name": "mcp__continuum__context_git", "arguments": { "project": "continuum" } }
```

#### `context_package`

The whole picture in one call: Continuum's context package (spec §4.9)
rendered as markdown — current moment, session state, what happened just
before, relevant memories and vault notes, recent file/git changes, failed
attempts, last success. `token_budget` defaults to `[context_package]
token_budget` (1000) and is clamped to `[200, 8000]`; over budget, sections
are dropped from the documented ladder (open files → recent changes →
just-before tail → memories tail) and `dropped` reports which rungs ran.

This is the **mcp-published profile** of the same struct the wake path
renders, which is why two sections are structurally absent and always
listed in `sections_omitted`:

| Omitted | Why |
|---|---|
| `why_woken` | there is no wake to explain — you asked, nobody woke you |
| `trigger_frame_moment` | there is no trigger frame; the live-context snapshot supplies the current moment instead |
| `tools`, `recommended_next_step`, `pending_decisions`, `facts` | this process cannot know them (tool grant, continuation resolver) or has better dedicated tools for them (`memory_vault_search`, `memory_vault_resolve`, `memory_list_facts`) |

`sections_present` is computed from the final rendered headings — after the
privacy gate, per-section caps and token-budget drop ladder — rather than from
the pre-render package. Together with `sections_omitted` it always covers the
whole vocabulary, so filtered, dropped and genuinely empty sections are never
misreported as present.
`per_section_stale` reports the four sources independently — a stale screen
with a fresh event log is a normal state:

| Flag | Source |
|---|---|
| `current_moment` | `live-context.json` missing or older than 10 s |
| `session` | `state.json` missing or older than 30 s |
| `events` | the `context_events` database could not be read |
| `memories` | this process's vault / episodic stores could not be opened |

Memories come from the MCP server's **own** lazily-opened vault and
episodic stores, queried with the compact live world render — so "relevant"
means relevant to what is on screen right now.

```jsonc
{ "name": "mcp__continuum__context_package", "arguments": { "token_budget": 1500 } }
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
