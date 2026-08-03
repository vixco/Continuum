# Memory Vault, Graph UI & Curator — Design

**Date:** 2026-08-03
**Status:** Approved by maintainer (brainstorm session, visual companion used for UI choices)
**Implements:** Continuum.md §7 (geheugenlagen), §8 (Obsidian-achtige opslag), §9 (knowledge graph), §22 (memory-compressie), §23 (contextvervuiling)

## Goal

Replace Continuum's flat key-value semantic memory with an Obsidian-like, user-ownable memory system: a markdown **vault** as source of truth, a derived SQLite **index** for graph/search queries, a rebuilt **Memory tab** centered on an interactive knowledge graph, and a hybrid **curator** pipeline that continuously distills perception into typed, linked, confidence-scored memories.

## Decisions locked during brainstorming

| Decision | Choice |
|---|---|
| Scope | Everything in one project: datamodel + vault + graph UI + curator (implemented as two sequential plans, see Phasing) |
| Source of truth | **Vault = truth.** Markdown files on disk; SQLite index is derived and always rebuildable |
| Curator brain | **Hybrid.** Local triage LLM (Qwen) does continuous work; orchestrator (Claude) handles batched hard decisions on wake |
| Main UI frame | **Graph-centric** — the force graph IS the page |
| Note interaction | **Dock + expand** — click opens resizable right panel; ⤢ / double-click promotes to full-screen overlay editor |
| Curator & timeline placement | **Floating proposal cards** top-right in the graph + thin **timeline scrub strip** at the bottom |
| Node style | **Obsidian-minimal** — colored dots, size ∝ importance, pulsing halo for candidates, dimmed for superseded, labels on hover/zoom |

## Out of scope (v1)

- Embedding/vector search over vault notes (episodic LanceDB retrieval stays as-is; vault search is FTS5)
- Multi-vault support, sync/cloud, encryption at rest
- Rich text editor (v1 = textarea + markdown preview via existing react-markdown)
- Importance decay / automatic re-scoring (only expiry sweep)
- Renaming files on title change (slug/filename is stable after creation)
- Automatic Obsidian config emission (`.obsidian/`) — the vault is Obsidian-*compatible* plain markdown, nothing more

## Architecture overview

```
┌────────────────────────┐        ┌─────────────────────────────┐
│ continuum.exe (runtime)│        │ continuum-desktop (Tauri)   │
│  senses → triage       │        │  MemoryTab (graph UI)       │
│  distiller → events    │        │  Tauri commands (in-proc)   │
│  curator (Qwen+Claude) │        │                             │
└──────────┬─────────────┘        └──────────────┬──────────────┘
           │       both link crates/continuum-memory       │
           ▼                                     ▼
   ┌──────────────────────────────────────────────────────┐
   │ vault/  (markdown = TRUTH)                           │
   │ vault/.continuum/index.db (derived: nodes, edges,    │
   │   FTS5, events, quarantine — WAL, both processes)    │
   └──────────────────────────────────────────────────────┘
```

- Both processes link `continuum-memory` (no heavy native deps) and open the same vault dir + index (SQLite WAL, `busy_timeout` 5 s).
- Cross-process change propagation is the **file-watcher**: every write is atomic (tmp+rename); the other process reindexes the changed file and (desktop) emits `continuum:memory` to the frontend.
- The dashboard is fully functional with the runtime stopped (browse/edit/create/migrate). Only live events + curator proposals require the runtime.
- Layer hierarchy respected: curator is part of the runtime's memory layer. Claude involvement happens only by including pending items in the orchestrator's wake context (data flows up); the orchestrator resolves them via MCP tools (commands flow down). Workers never touch the curator.

## Data model

### Node types

`project | goal | task | decision | person | preference | fact | error | session | note`

(`note` = free-form; Continuum.md's "File" relations are expressed as wiki-links to `note` or plain ghost links in v1.)

### Statuses

`candidate → confirmed | rejected | superseded | archived`

- **candidate**: proposed by curator, awaiting resolution. Excluded from orchestrator context.
- **confirmed**: normal live memory.
- **rejected**: kept on disk (so the curator can dedupe against it and never re-propose), hidden in UI by default, never in context.
- **superseded**: replaced by a newer node; `superseded_by`/`supersedes` link the two. Hidden by default, dimmed in graph when shown, never in context.
- **archived**: expired (past `expires`) or manually parked. Hidden by default, never in context.

### Frontmatter schema (canonical)

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

Required fields: `id`, `type`, `title`, `status`, `created`. Everything else has defaults (`confidence` 0.5, `importance` 0.5, `source` manual, `sensitivity` internal, `updated` = `created`). Unknown frontmatter keys are preserved on rewrite (round-trip safe: parse → edit body/known keys → write keeps unknown keys verbatim).

### Relations

- **Typed edges**: `relations:` frontmatter — `{to: <slug|mem_id>, rel: <string>, confidence: <f32>}`. `rel` is free text with recommended vocabulary: `belongs_to, works_on, blocks, caused_by, decided_in, mentions, prefers, owns`.
- **Untyped edges**: every `[[Wiki-Link]]` in the body → edge `rel: mentions`, confidence 1.0, marked `origin: body` in the index.
- **Link resolution order**: exact slug → case-insensitive title → node id. Unresolved targets become **ghost nodes** (rendered hollow in the graph; clicking one offers "create note").

### File conventions

- Path: `vault/<type-plural>/<slug>.md` (e.g. `decisions/lobby-creation-must-be-manual.md`). The folder is a convention only; frontmatter `type` is authoritative if they disagree.
- Slug: lowercase ASCII alnum + `-`, derived from title at creation, collisions get `-2`, `-3`, …. Slug (= file stem) is stable; title changes do not rename the file.
- Writes are atomic: write `<file>.tmp` then rename over the target. Stray `.tmp` files are ignored by the indexer and cleaned at startup.
- Files that fail to parse (broken frontmatter/YAML) are **quarantined**: listed in `quarantine` (index) with the error, skipped by everything else, surfaced in the UI as a warning chip. Never crash, never delete.

## Index (derived, `vault/.continuum/index.db`)

```sql
nodes(id TEXT PK, slug TEXT UNIQUE, path TEXT, type TEXT, title TEXT, status TEXT,
      project TEXT, confidence REAL, importance REAL, source TEXT, sensitivity TEXT,
      created TEXT, updated TEXT, last_used TEXT, expires TEXT,
      supersedes TEXT, superseded_by TEXT, tags_json TEXT, mtime INTEGER, body_hash TEXT);
edges(from_id TEXT, to_id TEXT, rel TEXT, confidence REAL, origin TEXT, -- 'frontmatter'|'body'
      PRIMARY KEY(from_id, to_id, rel));
unresolved_links(from_id TEXT, target TEXT, PRIMARY KEY(from_id, target));
nodes_fts(title, body, tags)  -- FTS5 table, kept in sync by the indexer on every file (re)index
events(id INTEGER PK AUTOINCREMENT, ts TEXT, kind TEXT, text TEXT,
       project TEXT, node_id TEXT, ref TEXT);
quarantine(path TEXT PK, error TEXT, mtime INTEGER);
meta(key TEXT PK, value TEXT);  -- schema_version, last_full_index_at
```

- **Rebuildability is the invariant**: `rebuild()` scans the vault and reproduces the index deterministically; a corrupt/missing/outdated-schema index.db is deleted and rebuilt automatically at open (fail-safe, logged, health-visible). Rebuilding never touches markdown.
- Incremental update per file event (create/modify/delete) keyed by path; `mtime`+`body_hash` skip no-ops.
- **FTS5 must be verified in the first implementation task** (sqlx bundled SQLite): a test creates an FTS5 table. If unavailable, the build gates on fixing that (vendored feature flags), not on a LIKE fallback.
- `events` is the one non-vault data set (Continuum.md §22: raw events need not be permanent). Retention: `events_retention_days` (default 30), pruned on runtime startup + daily.
- WAL mode, `busy_timeout=5000`, max 4 connections per process.

## `continuum-memory` crate (new, `crates/continuum-memory/`)

Lightweight deps only: `sqlx` (sqlite), `serde`/`serde_yaml`, `ulid`, `notify` + debouncer, `chrono`, `anyhow`/`thiserror`, `tracing`, `regex` (wiki-links). **No** llama/whisper/lancedb/fastembed. Library errors use `thiserror` (`MemoryError`), per house rules; every public item documented; module docs explain layer fit.

Public API (all async where IO-bound):

```rust
pub struct Vault { /* dir, pool */ }
impl Vault {
    pub async fn open(dir: &Path) -> Result<Vault>;       // creates structure, opens/rebuilds index
    pub async fn rebuild_index(&self) -> Result<IndexStats>;
    pub fn watch(&self) -> Result<VaultWatcher>;           // debounced; caller drains a channel of VaultChange

    pub async fn create(&self, draft: NoteDraft) -> Result<Note>;     // allocates id+slug+path
    pub async fn get(&self, id: &str) -> Result<Note>;                // frontmatter + body + backlinks
    pub async fn save(&self, note: &Note) -> Result<()>;              // atomic write + reindex file
    pub async fn delete(&self, id: &str) -> Result<()>;               // removes file + index rows

    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<NodeSummary>>;   // FTS5
    pub async fn graph(&self, f: &GraphFilter) -> Result<GraphData>;  // nodes+edges+ghosts, capped
    pub async fn neighbors(&self, id: &str, depth: u8) -> Result<GraphData>;
    pub async fn pending(&self) -> Result<Vec<NodeSummary>>;          // status=candidate
    pub async fn resolve_candidate(&self, id: &str, r: Resolution) -> Result<()>;
    // Resolution::Confirm | Reject | Supersede { replaces: String }

    pub async fn append_event(&self, e: NewEvent) -> Result<()>;
    pub async fn events(&self, range: EventRange) -> Result<Vec<Event>>;
    pub async fn prune_events(&self, keep_days: u32) -> Result<u64>;

    pub async fn info(&self) -> Result<VaultInfo>;  // counts, quarantine list, index health, path
    pub async fn touch_last_used(&self, ids: &[String]) -> Result<()>;
    pub async fn sweep_expired(&self) -> Result<u64>;                 // -> archived
}
pub async fn migrate_legacy_semantic(vault: &Vault, semantic_db: &Path) -> Result<MigrationReport>;
```

`GraphFilter`: types, status set (default confirmed+candidate), project, text query, time window (created/updated), `limit` (default `graph_max_nodes`). When capped, highest-importance nodes win and `GraphData.truncated = true`.

`resolve_candidate` semantics: Confirm → `status: confirmed`. Reject → `status: rejected`. Supersede → new node confirmed with `supersedes`, old node `status: superseded` + `superseded_by`. All three are single-file frontmatter edits (plus one for the superseded partner), atomic per file.

## Curator (runtime, `crates/continuum-core/src/curator/`)

Runs inside the runtime beside the existing distiller. All stages log `layer="memory", component="curator"`. LLM access goes through a `CuratorLlm` trait (implemented by the triage LLM handle; mocked in tests).

**Stage 1 — Event feed.** The existing distiller additionally writes compact timeline events (`vault.append_event`) for: project/window switches, builds, errors, wake-ups, distilled episodic summaries, session boundaries. (Source of the UI timeline strip.)

**Stage 2 — Candidate extraction.** Every `curator_interval_minutes` (default 10): collect events + episodic summaries since last pass; FTS-search the vault for related existing notes; prompt Qwen (prompt file `prompts/curator-extract.md`) to propose 0–`curator_max_candidates_per_pass` (default 3) memory candidates as strict JSON `{type,title,body,project,confidence,importance,relations,source_ref,tags}`. Invalid JSON → one retry with error appended → skip pass. Dedupe before writing: FTS titles (incl. rejected/confirmed) with normalized-title match or high overlap → drop. Survivors → vault files `status: candidate`; `source` is set by the model per candidate: `user_statement` (the user literally said/typed it in a transcript), `observed` (seen happening), or `inferred` (concluded from patterns).

**Stage 3 — Threshold rules** (config, applied at write time):
- `confidence ≥ auto_confirm_threshold` (default 0.85) **and** `source: user_statement` → written directly as `confirmed` (still appears in the UI feed as "auto-saved", undoable like any note).
- `confidence < discard_floor` (default 0.4) → not written at all (logged).
- Otherwise → `candidate`, waits for user or Claude.

**Stage 4 — Contradiction / supersede detection.** For each new candidate/confirmed note: FTS + same-project same-type query; Qwen judges pairwise "does NEW contradict/replace OLD?" (prompt `prompts/curator-conflict.md`). A hit creates a **supersede proposal**: a `relations` entry `{to: <old>, rel: proposes_supersede, confidence: c}` on the candidate. The UI renders these as "old → new" cards; accepting calls `resolve_candidate(Supersede)`. Claude may also resolve them (below). Old memories are never auto-superseded by Qwen alone.

**Stage 5 — Claude escalation.** No extra wake-ups: when the orchestrator wakes for any reason, the wake context gains a "Pending memory decisions" block (max `curator_claude_batch` = 10 items: mid-confidence candidates older than 30 min + open supersede proposals). The orchestrator resolves them via the new MCP tools. A daily scheduled wake (existing scheduler) guarantees the queue drains even on quiet days.

**Stage 6 — Session summaries.** On session boundary (project switch or `session_summary_idle_minutes` = 20 of inactivity, signaled by triage context): Qwen drafts a `type: session` note (goal/changed/problem/tried/result/next — the Continuum.md §7.3 template, prompt `prompts/curator-session.md`), `status: confirmed`, `source: observed`, linked to the project. Sessions skip candidate review (they are records, not claims).

**Stage 7 — Hygiene.** Daily: `sweep_expired()`, `prune_events()`.

## MCP additions (`continuum-mcp`) — additive only, existing tools untouched

- `memory_vault_search {query, types?, project?, limit?}` → node summaries
- `memory_vault_get {id}` → full note
- `memory_vault_save {type, title, body, project?, confidence?, importance?, relations?, tags?, source_ref?}` → creates/updates a **confirmed** note (`source: agent_run`)
- `memory_vault_resolve {id, action: "confirm"|"reject"|"supersede", replaces?}` → candidate resolution

Each: rmcp schema struct, permission entry in `config/default-permissions.toml`, docs in `docs/mcp-tools.md`, integration test via MCP protocol.

## Retrieval-on-wake changes (runtime)

Wake context assembly adds vault retrieval before episodic vector search: FTS on trigger text + active-project graph neighborhood (confirmed only, `sensitivity != sensitive` unless `include_sensitive_in_context = true`), top-N by importance (`wake_vault_notes_max` = 8). Injected notes get `touch_last_used`. Episodic/LanceDB retrieval is unchanged.

## Desktop backend (Tauri)

New `apps/desktop/src-tauri/src/memory.rs` with `MemoryState { vault: Arc<Vault> }` opened at startup from config; watcher task forwards `VaultChange` batches as `continuum:memory` events `{kind: "changed"|"rebuilt", ids: [...]}` (debounced ≥ 300 ms).

Commands (replacing the old stubs — `search_episodic`, `delete_episodic`, `list_semantic`, `set_semantic`, `delete_semantic` are **removed** along with their `tauri.ts` wrappers/types; MemoryTab is fully rebuilt):

`memory_graph(filter)`, `memory_search(query, limit)`, `memory_get_note(id)`, `memory_create_note(draft)`, `memory_save_note(note)`, `memory_delete_note(id)`, `memory_resolve_candidate(id, action, replaces?)`, `memory_pending()`, `memory_events(range)`, `memory_vault_info()`, `memory_migrate_legacy()`, `memory_open_vault()` (Explorer via opener plugin).

`wipe_memory` is **re-scoped**: it wipes derived/perception data (raw log, episodic, events; the index is rebuilt from the vault afterwards, so vault-backed nodes reappear) but **never deletes vault markdown**. UI copy updated accordingly; deleting the vault itself is a manual file-system act (button opens the folder instead).

## Frontend (MemoryTab rebuild)

One new dependency: `force-graph` (canvas, d3-force based) wrapped in our own thin React component (`MemoryGraph.tsx`, ref + effect, no react-force-graph wrapper). Everything styled with existing tokens (bg-*, ink-*, accent-purple, state-*).

Layout (locked during brainstorm):

- **Full-bleed graph canvas.** Node color by type (fixed palette in `lib/memoryTheme.ts`), radius ∝ importance (clamped), pulsing halo = candidate, 40 % opacity = superseded/archived (only when the status filter shows them), hollow ring = ghost node. Labels on hover always; permanent labels above a zoom threshold. Click = select + dock panel; double-click or ⤢ = overlay editor; click ghost = "create note?" prompt; scroll = zoom, drag = pan, drag node = reposition (pinned for the session).
- **Topbar:** FTS search box with dropdown results (Enter focuses/centers the node), filter chips: type (multi), project, status (default confirmed+candidate), time window. Legend toggle. "Open vault folder" button. Quarantine warning chip when `quarantine` is non-empty.
- **Docked right panel** (resizable 320–70 %, persisted width): rendered markdown (react-markdown), metadata chips (type, status, confidence, importance, source, sensitivity, dates), editable fields (title, type, project, status, confidence/importance sliders, tags), relations list (add/remove typed edges with rel + confidence), backlinks list, danger row (delete with confirm). ⤢ promotes to:
- **Overlay editor:** full-screen modal over the graph — textarea + preview toggle, same metadata sidebar, Esc closes back to graph (selection preserved), Ctrl+S / Save button saves.
- **Floating curator stack** (top-right, max 3 visible + "N more"): candidate cards (title, type, confidence, source, excerpt; ✓ confirm / ✕ reject / "later" collapses to badge) and supersede cards (old → new with both titles; Accept / Keep both). Backed by `memory_pending()` + live `continuum:memory` refresh.
- **Timeline strip** (bottom): event-density bars for the visible window (default today), drag to scrub → graph dims nodes not created/updated in window + popover lists events at cursor; "nu ▶" returns to live. Data: `memory_events`.
- **Saved views:** name + save current filter set (zustand persisted store, local only); dropdown in topbar.
- **Empty state:** short explainer + "Open vault folder" + **Migrate button** when `memory_migrate_legacy` detects a legacy semantic.db (shows report toast after).

The tab follows the existing Shell/NAV structure and design tokens — MemoryTab stays a top-level tab, no sub-tabs.

## Config (`continuum-core/src/config.rs`, `#[serde(default)]` at both levels)

```toml
[memory.vault]
vault_dir = ""                    # empty = <data_dir>/vault (~/.continuum-dev/vault in dev)
watcher_debounce_ms = 500
events_retention_days = 30
graph_max_nodes = 1500
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
```

All thresholds/intervals surfaced later in Settings (not in this project's UI scope beyond what exists).

## Error handling & self-healing

- **Indexer:** parse failure → quarantine row + `warn`, never crash. index.db corruption/schema-mismatch → delete + rebuild at open (`error` log + health event). Watcher death → health probe degrades; `should_restart()` true; restart re-opens vault (stateless).
- **Curator:** LLM failure/invalid JSON → retry once → skip pass (`warn`). Repeated failures (3 consecutive) → component `degrading` in system_health.
- **Desktop:** every command maps `MemoryError` → user-readable string (same style as chat commands); vault-open failure at startup → MemoryTab shows error state with retry, rest of app unaffected.
- **Health:** runtime `system_health` gains `memory_vault` (index ok, watcher alive, quarantine count, last index time) and `curator` (last pass, consecutive failures). Desktop `components.rs` gains a `memory_vault` probe. Recovery procedures documented in `docs/self-healing.md`.
- **Logs:** `~/.continuum/logs/memory.log` structured events per house rules.

## Security & privacy

- Everything local; no new network calls (non-negotiable #2 upheld).
- `sensitivity: sensitive` notes never enter orchestrator/chat context unless `include_sensitive_in_context = true`.
- No secrets in vault tooling; vault path validated (no traversal outside `vault_dir` when resolving slugs/paths).
- Vault files are the user's property: nothing ever bulk-deletes or rewrites them except explicit user action per note (wipe excludes the vault; migration only creates files).

## Migration (legacy semantic.db → vault)

`migrate_legacy_semantic`: reads `semantic_facts` + `semantic_edges` (read-only), converts each fact to a `type: fact` note — key becomes title (`user.name` → "user: name"), first key segment becomes a tag, value becomes body, confidence/source/updated_at carried over; edges become typed relations where both endpoints migrated. Report `{migrated, skipped, errors}` shown in UI. Idempotent: facts whose title already exists are skipped. The legacy DB is never modified; the runtime keeps writing to it until the curator lands (Plan B), after which SemanticStore writes are retired (existing MCP semantic tools keep working against the legacy store until then — no breaking change mid-way).

## Testing contract

- **continuum-memory** (~25+ tests): frontmatter round-trip incl. unknown-key preservation, CRLF, multibyte UTF-8; quarantine on broken YAML; slug collision; link resolution (slug/title/id) + ghost nodes; atomic write leaves no partial file; index rebuild determinism (rebuild twice → identical row sets); incremental vs full rebuild equivalence; **FTS5 availability** + search ranking sanity; graph filter/cap/truncated flag; neighbors depth; candidate resolution incl. supersede pair-edit; events append/query/prune; watcher integration test on tempdir (real notify, debounce); migration fixture (legacy db → expected notes, idempotency); path-traversal rejection.
- **curator** (continuum-core, mocked `CuratorLlm`): extraction prompt builder snapshot; JSON parse + retry + skip; threshold rule table (auto-confirm / candidate / discard); dedupe against existing+rejected; conflict-proposal creation; session-summary trigger; wake-context pending block assembly.
- **continuum-mcp:** integration test per new tool via MCP protocol.
- **desktop:** command-layer tests against a tempdir vault (`cargo test -p continuum-desktop`); frontend `corepack pnpm typecheck && lint && build` + Prettier.
- **Perf smoke:** generate 1 000 notes, `rebuild_index` < 5 s and `graph()` payload respects cap (asserted in a test, generous CI margin).
- Verification commands per crate (debug ok): `cargo test -p continuum-memory`, `-p continuum-core --no-default-features --lib`, `-p continuum-desktop`, `-p continuum-mcp`. Never full-workspace release builds during tasks.

## Phasing — one spec, two sequential plans

**Plan A — Vault + Graph UI (standalone value):** crate (model/vault/index/watcher/events API/migration), desktop MemoryState + commands + `continuum:memory` events, full MemoryTab rebuild (graph, dock+overlay, curator stack UI reading `pending()` — empty until Plan B, timeline reading `events` — empty until Plan B, saved views, migration flow), stub removal, config section, health probes (desktop), docs (`docs/memory.md`, CHANGELOG, ARCHITECTURE.md memory section).

**Plan B — Curator + runtime integration:** distiller event feed, curator stages 2–7, prompts, MCP tools, wake-context injection + `touch_last_used`, runtime health components, retirement of SemanticStore writes, docs (self-healing, mcp-tools, triage/orchestrator docs touch-ups, CHANGELOG).

Plan A ships a fully working manual memory system (create/edit/link/graph/migrate); Plan B makes it ambient.
