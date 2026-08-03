# Memory

Continuum's memory is a **vault**: a folder of plain markdown files that is
the single source of truth, plus a derived SQLite index that makes the vault
searchable and graph-shaped. This document covers the vault's file format,
lifecycle, and the config knobs that control it. See
`docs/superpowers/specs/2026-08-03-memory-vault-design.md` for the original
design decisions if you need the full rationale, and `ARCHITECTURE.md`'s
"Memory system" section for how the vault fits alongside the raw perception
log and episodic memory.

This is a **Plan A** feature: the vault, its index, and the graph-centric
Memory tab all exist and are fully usable today (create/edit/link/search/
migrate). The **curator** — the background process that ambiently proposes
new memories from what Continuum observes — is Plan B and has not landed
yet. `memory_pending()` and the timeline strip are wired up and will start
showing data the moment the curator ships; until then they are simply empty.

## Where the vault lives

Default location: `<continuum-dev-dir>/vault` (`~/.continuum-dev/vault` in a
dev checkout). Both the headless `continuum` runtime and the
`continuum-desktop` dashboard open the **same** vault directory — they link
the `continuum-memory` crate directly rather than talking to each other over
IPC, so the dashboard works fully (browse/search/edit/create/migrate) even
when the runtime isn't running. Only live curator proposals and the event
timeline (Plan B) require the runtime.

Open the folder any time via the Memory tab's **Open vault** button (topbar),
or the `memory_open_vault` Tauri command. This works even when the index has
failed to open — see [Troubleshooting](#troubleshooting).

## Config (`[memory.vault]`, `[memory.curator]`)

Every value below is read from `config.toml` (`ContinuumConfig::memory`) and
falls back to its default when omitted, per non-negotiable #3 — nothing here
is hardcoded elsewhere in the runtime or dashboard.

```toml
[memory.vault]
vault_dir = ""                    # empty = <data_dir>/vault (~/.continuum-dev/vault in dev)
watcher_debounce_ms = 500         # file-watcher debounce window
events_retention_days = 30        # how long timeline events are kept
graph_max_nodes = 1500            # cap on nodes returned by graph()/neighbors() when unset per-query

[memory.curator]                  # Plan B — configurable now, not yet consumed by any running code
enabled = true
interval_minutes = 10
max_candidates_per_pass = 3
auto_confirm_threshold = 0.85
discard_floor = 0.4
claude_batch = 10
session_summary_idle_minutes = 20
wake_vault_notes_max = 8
include_sensitive_in_context = false
```

`vault_dir` resolves via `MemoryVaultConfig::resolve_vault_dir` — an empty
string means "under the current data directory", any other value is used
verbatim (absolute paths recommended). Changing it moves where Continuum
looks; it does not move existing files for you.

## The vault directory

```
vault/
├── projects/     goals/       tasks/        decisions/
├── people/       preferences/ facts/        errors/
├── sessions/     notes/
└── .continuum/
    └── index.db      # derived SQLite index — see "The index" below
```

Each subdirectory corresponds to one node `type` and is a **convention
only**: the frontmatter `type` field is authoritative if a file's folder and
its `type` ever disagree (e.g. after a manual move). Every note is one
markdown file, path `vault/<type-plural>/<slug>.md`.

## Frontmatter schema (canonical)

Copied verbatim from the design spec — this is the format every vault note
must satisfy:

```yaml
---
id: mem_01J8F3A6K2...     # ULID, generated at creation, never changes
type: decision             # one of the 10 node types
title: Lobby creation must be manual
project: sidelife          # optional slug reference
status: confirmed
confidence: 0.95           # 0.0–1.0
importance: 0.92           # 0.0–1.0
source: user_statement     # user_statement|observed|inferred|agent_run|chat|manual
source_ref: "frame:8843"   # optional provenance pointer (frame/event/conversation id)
sensitivity: internal      # public|internal|sensitive
created: 2026-08-01T21:45:00Z
updated: 2026-08-02T10:12:00Z
last_used: 2026-08-02T10:12:00Z   # optional; set when injected into orchestrator context
expires: 2026-09-01T00:00:00Z     # optional
supersedes: mem_01J7...           # optional
superseded_by: mem_01J9...        # optional
relations:                        # typed edges with confidence
  - { to: sidelife, rel: belongs_to, confidence: 1.0 }
tags: [lobby, unity]
---
Body is plain markdown. [[Wiki-links]] here become untyped "mentions" edges.
```

Required fields: `id`, `type`, `title`, `status`, `created`. Everything else
has a default: `confidence` 0.5, `importance` 0.5, `source` `manual`,
`sensitivity` `internal`, `updated` = `created`. **Unknown frontmatter keys
are preserved on rewrite** — the parser round-trips anything it doesn't
recognize verbatim, so hand-added keys (or fields a future Continuum version
adds) survive being edited through the Memory tab.

Slugs are lowercase ASCII alphanumeric + `-`, derived from the title at
creation time (`slugify` in `crates/continuum-memory/src/slug.rs`).
Collisions get `-2`, `-3`, … appended. The slug is the filename stem and
never changes after creation — renaming a note's title does **not** rename
its file.

Writes are always atomic: a `.tmp` sibling is written first, then renamed
over the target. A crash mid-write leaves the target file untouched and a
stray `.tmp` file, which is cleaned up automatically the next time the vault
opens.

## Node types, statuses, lifecycle

**Node types** (fixed set of 10): `project`, `goal`, `task`, `decision`,
`person`, `preference`, `fact`, `error`, `session`, `note`. `note` is the
free-form catch-all.

**Status lifecycle:** `candidate → confirmed | rejected | superseded |
archived`

| Status | Meaning | Shown in graph by default? | In orchestrator context? |
|---|---|---|---|
| `candidate` | Proposed (by the curator, once it ships), awaiting a decision | Yes (pulsing halo) | No |
| `confirmed` | Normal, live memory | Yes | Yes |
| `rejected` | Explicitly declined; kept on disk so it's never re-proposed | No (hidden by default) | No |
| `superseded` | Replaced by a newer node; linked via `supersedes`/`superseded_by` | No (dimmed if shown) | No |
| `archived` | Expired (past `expires`) or manually parked | No | No |

Resolving a candidate (`memory_resolve_candidate` / `Vault::resolve_candidate`)
has three outcomes: **Confirm** sets `status: confirmed`; **Reject** sets
`status: rejected`; **Supersede** confirms the new node and flips the old
one to `status: superseded` with both sides' `supersedes`/`superseded_by`
filled in — always a pair of atomic single-file edits, never a bulk rewrite.

Nothing in the vault is ever silently deleted by Continuum: `rejected`,
`superseded`, and `archived` notes stay on disk (just hidden from the
default graph view) until a user explicitly deletes the file or calls
`memory_delete_note`.

## Wiki-links vs. typed relations

Two ways a note links to another:

- **Typed relations** — the `relations:` frontmatter list:
  `{to: <slug|mem_id>, rel: <string>, confidence: <f32>}`. `rel` is free
  text; the recommended vocabulary (used consistently across the UI and any
  future curator output) is:

  `belongs_to, works_on, blocks, caused_by, decided_in, mentions, prefers, owns`

  You aren't restricted to this list — `rel` is a plain string — but sticking
  to it keeps graph queries and future automation predictable.
- **Untyped wiki-links** — any `[[Target]]` in the note body becomes an edge
  with `rel: mentions`, `confidence: 1.0`, `origin: body` in the index (vs.
  `origin: frontmatter` for typed relations).

**Link resolution order:** exact slug match → case-insensitive title match →
node id match. A target that resolves to none of those becomes a **ghost
node**: rendered hollow in the graph, and clicking it in the Memory tab
offers "create note" pre-filled with that title.

## The index (`vault/.continuum/index.db`)

The index is a derived SQLite database (WAL mode) holding `nodes`, `edges`,
`unresolved_links`, an FTS5 full-text table (`nodes_fts`), the `events`
timeline, and `quarantine`. **It is fully rebuildable from the markdown at
any time** — this is the load-bearing guarantee of the whole design. Every
`Vault::open` performs a full rebuild, so a missing, corrupt, or
schema-mismatched `index.db` is simply deleted and regenerated automatically,
logged, and reflected in the `memory_vault` health probe (see
`docs/self-healing.md`).

`events` is the one table a rebuild never touches (only a schema-version
reset — which happens automatically on a version bump — clears it); it's
retained for `events_retention_days` (default 30) and pruned by
`Vault::prune_events`.

### Quarantine

A markdown file with broken YAML frontmatter (bad indentation, missing
required field, invalid date, …) is **quarantined**, not crashed on or
deleted: it's recorded in the `quarantine` table with the parse error,
skipped by search/graph/everything else, and surfaced in the Memory tab as a
warning chip in the topbar ("N file(s) in quarantine"). Fix the file's
frontmatter (directly, or by opening the vault folder in any editor) and it
rejoins the index on the next reindex — no restart required, the file
watcher picks up the edit.

## Migration from the legacy semantic store

Before the vault existed, "semantic memory" was a flat `semantic.sqlite`
key/value table (`semantic_facts`) plus a typed-edge table
(`semantic_edges`). `migrate_legacy_semantic` (exposed as the Memory tab's
**Import legacy memory** action, shown only when `semantic.sqlite` still
exists) converts each fact into a `type: fact` note — the key becomes the
title (`user.name` → "user: name"), the key's first segment becomes a tag,
the value becomes the body, and confidence/source/timestamp carry over.
Edges become typed `relations` entries where both endpoints migrated.

The migration is **idempotent**: a fact whose mapped title already exists in
the vault is skipped and counted separately in the returned report
(`{migrated, skipped, errors}`), so re-running it is always safe. The legacy
database is opened read-only and is never modified or deleted by the
migration; the runtime keeps writing to it via the existing
`mcp__continuum__memory_*` semantic tools until the curator lands (Plan B),
at which point those writes retire. Nothing about this migration is
destructive — it only ever creates vault files.

## Obsidian interop

The vault is plain markdown with YAML frontmatter and `[[wiki-link]]`
syntax — nothing Continuum-proprietary. It is safe to open `vault/` directly
as an Obsidian vault (or in any other markdown-aware editor) to browse,
search, or edit notes outside Continuum. Continuum does not emit an
`.obsidian/` config folder itself (out of scope for v1) — if you open it in
Obsidian, Obsidian will create its own config folder on first open, which
Continuum ignores (the index only reindexes `.md` files outside
`.continuum/`). Edits made externally are picked up by Continuum's own
file-watcher like any other change — no special handling needed, no need to
close Continuum first.

## Troubleshooting

- **"N file(s) in quarantine" chip** — one or more notes have invalid
  frontmatter. Open the vault folder (Memory tab → **Open vault**, or the
  `memory_vault` health probe's recovery note) and fix the offending file's
  YAML. Continuum never edits or deletes a quarantined file for you.
- **Index looks wrong / stale / won't open** — delete
  `vault/.continuum/index.db` and restart the app (or use the Memory tab's
  **"…" menu → Rebuild index**, which does the same thing without a
  restart). The index is always fully reconstructible from the markdown;
  nothing is lost.
- **A note I know exists doesn't show up in search or the graph** — check
  the quarantine chip first (a parse failure hides the note everywhere);
  otherwise check the status filter (`rejected`/`superseded`/`archived`
  notes are hidden by default — toggle "Toon verborgen" in the Memory tab
  topbar to see them).
- **I want to inspect or back up the vault outside Continuum** — it's just a
  folder of `.md` files; copy it, `git init` it, sync it, whatever you'd do
  with any other document folder. `vault/.continuum/` is safe to exclude
  from any of that, since it's fully derived.
