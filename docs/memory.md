# Memory

Continuum's memory is a **vault**: a folder of plain markdown files that is
the single source of truth, plus a derived SQLite index that makes the vault
searchable and graph-shaped. This document covers the vault's file format,
lifecycle, and the config knobs that control it. See
`docs/superpowers/specs/2026-08-03-memory-vault-design.md` for the original
design decisions if you need the full rationale, and `ARCHITECTURE.md`'s
"Memory system" section for how the vault fits alongside the raw perception
log and episodic memory.

The vault, its index, and the graph-centric Memory tab (create/edit/link/
search/migrate) shipped as **Plan A**. The **curator** — the background
pipeline that ambiently proposes new memories from what Continuum observes,
detects conflicts between them, summarizes sessions, and drains a daily
maintenance/hygiene tick — shipped as **Plan B** and is covered in its own
section below. `memory_pending()` and the timeline strip (Memory tab) now
show live data once the curator has run at least one pass.

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

[memory.curator]
enabled = true
interval_minutes = 10
max_candidates_per_pass = 3
auto_confirm_threshold = 0.85
discard_floor = 0.4
claude_batch = 10
session_summary_idle_minutes = 20
wake_vault_notes_max = 8
include_sensitive_in_context = false
supersede_confidence_floor = 0.5
maintenance_wake_hour = 4
```

See [The curator](#the-curator) below for what each `[memory.curator]` key
actually controls.

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
| `candidate` | Proposed by the curator (or written directly at a low confidence), awaiting a decision | Yes (pulsing halo) | No (unless stale — see [Pending decisions in wake context](#the-curator)) |
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

## Observation-derived candidates (triage classification)

The curator is not the only producer of candidates. Since the context
engine's Task B3 the triage call also classifies the frame it judges
(spec §4.7), and that classification is consumed in
`crates/continuum-core/src/triage/consume.rs`:

- every classified frame becomes a `context_events` row (source `screen`,
  or `audio` when the frame carried a transcript), tagged with the
  frame's privacy zone;
- a classification with `should_store`, or any `remember` decision,
  additionally proposes a **vault candidate** — `status: candidate`,
  `source: observed`, project from the resolver (a project the classifier
  names is only trusted when the Projects table knows it), tagged
  `observed` plus an epistemic label: `user_stated` when the belief came
  from speech, `system_inferred` when it came from the screen;
- the frame's `triage_decision` raw-log column records what triage did
  (`ignore`, `wake_orchestrator/error`, …).

Mapping from classification type to vault type:

| classification | vault type |
|---|---|
| `error` | `error` |
| `decision` | `decision` |
| `preference` | `preference` |
| `task_progress` | `task` |
| `success` | `note` (tagged `result`) |
| `communication`, `other` | `note` |
| `routine` | *no candidate* |

These candidates expire unless someone confirms them. The TTL is per type,
from `[memory.candidate_ttl_days]` (defaults: task 30, error 30, note 90,
decision and preference never) and lands in the note's `expires`
frontmatter, which the vault's expiry sweep already archives on.

Windows in a `never_observe` zone produce neither an event nor a
candidate; `local_only` windows produce both, with the event tagged
`local_only` and the note written `sensitivity: sensitive` so the cloud
gate strips it (spec §4.1 propagation rule). Duplicate suppression reuses
the curator's near-duplicate check, so the same observation repeated does
not stack up pending notes.

## The curator

The curator is a background pipeline, owned by the headless `continuum`
runtime (it does not run in the dashboard process), that ambiently turns
observed activity into vault candidates, catches contradictions, summarizes
sessions, and keeps the vault tidy. It only spawns when a triage LLM is
loaded at boot (`Some(triage)`); with no local model available it logs
`curator disabled: no triage model loaded` and never starts. Its prompt
templates live at `prompts/curator-extract.md`, `prompts/curator-conflict.md`,
and `prompts/curator-session.md`.

Every stage below is implemented in `crates/continuum-core/src/curator/`.

1. **Vault feed.** At boot the runtime opens the vault (same directory the
   dashboard uses) and spawns a watcher-drain task that reindexes any file
   changed outside the process (a hand edit, Obsidian, the dashboard). The
   existing memory distiller additionally appends a `kind: "distilled"`
   event into the vault's event timeline (carrying the memory's `project`)
   for everything it distils, so the curator's extraction pass has something
   to read. Since the context engine's §4.11 compression ladder the
   distiller's primary input is **deduped `context_events` rows**, not raw
   frames: one collapsed row becomes one episodic memory whose summary shows
   the repeat count (`"build failed (×14)"`) and whose importance gets a
   bounded boost (+0.05 per doubling of the count, capped at +0.20). Raw
   frames that never produced a classified event still distil through the
   original salience predicate as a fallback; frames that *did* produce an
   event are excluded so a moment is never recorded twice. Every episodic
   memory carries an optional `project`, and wake retrieval filters on it
   *softly* — memories of other projects are dropped, unattributed ones
   (everything written before the field existed) always survive.
2. **Extraction.** Every `interval_minutes` (default 10), `extract_pass`
   reads vault events since the previous pass, asks the triage LLM (via the
   `CuratorLlm` trait) to propose up to `max_candidates_per_pass` candidate
   notes as a JSON array (`prompts/curator-extract.md`), and parses the
   reply with a balanced-bracket scanner tolerant of prose around the JSON.
3. **Threshold routing.** Each parsed candidate is deduplicated (normalized
   title + an FTS check that also covers previously-rejected notes) and then
   routed by confidence: `>= auto_confirm_threshold` (0.85) from a
   `user_statement` source is written `status: confirmed` directly;
   `>= auto_confirm_threshold` from any other source, or anywhere in
   `[discard_floor, auto_confirm_threshold)` (0.4–0.85), is written
   `status: candidate`; below `discard_floor` (0.4) the candidate is dropped
   and never written. A single bad candidate never aborts the pass — a write
   failure for one candidate is logged and skipped, the rest of the batch
   still lands.
4. **Conflict / supersede detection.** After a pass writes new notes,
   `detect_conflicts` looks up to 2 same-type, same-project **confirmed**
   "partner" notes per new note (FTS search on the new note's title), and
   asks the LLM for a verdict — `unrelated` / `supersedes` / `contradicts`
   (`prompts/curator-conflict.md`). At or above `supersede_confidence_floor`
   (0.5 default), a `{to: <partner>, rel: "proposes_supersede", confidence}`
   relation is attached to the **new** note only — the old note's `status`
   is never flipped automatically. Acting on the proposal (confirming the
   supersede, which sets both sides' `supersedes`/`superseded_by` and flips
   the old note to `status: superseded`) still requires an explicit
   `memory_vault_resolve` call or a dashboard action.
5. **Getting candidates in front of a reviewer.** Three surfaces exist:
   - **Wake context.** Every orchestrator wake runs `retrieve_vault_context`:
     up to `wake_vault_notes_max` (8) confirmed notes matched to the trigger
     frame render under a `## Long-term memory (vault)` section, and pending
     candidates older than 30 minutes (oldest-first, capped at
     `claude_batch`) render under `## Pending memory decisions` with an
     instruction to resolve them via `memory_vault_resolve` /
     `memory_vault_save`. Both lists exclude `sensitivity: sensitive` notes
     unless `include_sensitive_in_context = true`. See `docs/mcp-tools.md`.
   - **MCP tools.** `memory_vault_search`, `memory_vault_get`,
     `memory_vault_save`, and `memory_vault_resolve` let the orchestrator act
     on candidates directly during a wake. Full schemas in `docs/mcp-tools.md`.
   - **Dashboard.** The Memory tab's curator-card stack
     (Confirm / Reject / Later) and the timeline strip now show live data —
     see `docs/dashboard.md`.
   - **Daily maintenance wake.** A purpose-built ticker (there was no
     pre-existing scheduler to hook into) fires once per local calendar day
     at `maintenance_wake_hour` (default 4; negative disables it entirely;
     values `>= 24` clamp to 23 with a `warn`). It wakes the orchestrator
     specifically to drain pending decisions on a day nothing else would
     have — but only when the curator is enabled, the vault actually has
     pending candidates, a recent perception frame exists to wake with, and
     the orchestrator isn't already busy (claimed via the same atomic
     compare-exchange the triage wake path uses, so the two can never double
     wake). A busy orchestrator at the exact tick means that day's
     maintenance wake is skipped outright, not retried later.
6. **Session summaries.** A curator-owned `SessionTracker` (not signaled by
   triage — no such signal exists) watches an `ActivitySignal` published on
   every perception frame (foreground process + an inferred project hint)
   and emits a session boundary when either: the gap since the last signal
   exceeds `session_summary_idle_minutes` (default 20), or the foreground
   process changes after the session has run at least `MIN_SESSION_MINUTES`
   — a hardcoded constant (5 minutes) in `curator/session.rs`, not a config
   key. On a boundary, `write_session_summary` asks the LLM to compress the
   session's vault events into a `## Goal` / `## What happened` /
   `## Next step` markdown body (`prompts/curator-session.md`); a reply of
   literally `SKIP` (or fewer than 3 events in the session, which skips the
   LLM call entirely) writes nothing. A session note is written directly as
   `status: confirmed` — it compresses events already in the vault's
   timeline rather than asserting a new claim, so it doesn't go through
   candidate review.
7. **Daily hygiene and the wipe path.** On each local calendar-date
   rollover, the same ticker loop that drives extraction also runs
   `vault.sweep_expired()`, `vault.prune_events(events_retention_days)`, and
   drains any pending wipe request. **Wipe reality**: both the desktop's
   "Wipe derived data" action (`wipe_memory`, requires typing the literal
   string `DELETE`) and the MCP `memory_wipe_all` tool (requires the literal
   string `WIPE`) write the same `<dev_dir>/wipe-request.json` —
   `{ requested_at, scopes: ["raw_log", "episodic", "events"] }`, atomic
   tmp+rename. The runtime drains it via `process_wipe_request`: once at
   boot (even when the curator itself is disabled — a pending wipe must not
   wait on the extraction loop) and again on every daily hygiene tick. Each
   named scope is wiped (`raw_log` → all perception frames + their
   screenshots, `episodic` → the LanceDB table dropped and recreated empty,
   `events` → `vault.prune_events(0)`), then the request file is deleted.
   **Vault markdown notes are never deleted by any wipe path.** The desktop
   additionally prunes vault events and rebuilds the index immediately on
   click (it already holds a vault handle), so the Memory tab's timeline
   clears right away; `raw_log`/`episodic` wait for the runtime's next boot
   or daily hygiene tick to actually clear, since those stores only exist in
   the separate headless runtime process. A malformed `wipe-request.json` is
   **not** silently discarded — `process_wipe_request` returns an error, the
   file is left in place, and the failure is logged; as of this writing that
   means a corrupt request file is retried (and re-logged) on every boot and
   every daily tick until a human fixes or deletes it by hand.

**Failure policy.** Every LLM call in stages 2–4 that expects structured
output (candidate JSON, a conflict verdict) retries once on a parse failure,
appending the parse error to the prompt; a second failure skips that pass or
pair rather than erroring the whole curator loop. Separately, if a whole
extraction window fails outright (the LLM is unreachable, not a parse
issue) 3 times in a row, that window is abandoned — the window boundary
advances to "now" and the local failure streak resets, logged at `warn`, so
one stuck window can't wedge the curator forever. This local streak is
distinct from the dashboard's lifetime `consecutive_failures` counter (see
`docs/self-healing.md`), which only resets on a genuinely successful pass.

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
migration. Nothing about this migration is destructive — it only ever
creates vault files.

The legacy `semantic.sqlite` store is now read-only from the runtime's own
perspective: `memory_set_fact` (see `docs/mcp-tools.md`) writes exclusively
into the vault (a `type: fact` note, matched/updated by title), and
`memory_get_fact` / `memory_list_facts` only fall back to `semantic.sqlite`
when the vault has no matching note or is itself unavailable. Facts that
predate the vault, or were never migrated, are still served this way — the
legacy database is never written to again, only read as a fallback.

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
  notes are hidden by default — toggle "Show hidden" in the Memory tab
  topbar to see them).
- **I want to inspect or back up the vault outside Continuum** — it's just a
  folder of `.md` files; copy it, `git init` it, sync it, whatever you'd do
  with any other document folder. `vault/.continuum/` is safe to exclude
  from any of that, since it's fully derived.
