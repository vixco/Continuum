# Memory Curator + Runtime Integration (Plan B) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the memory vault ambient — the runtime continuously distills perception into typed vault memories (Qwen candidates, supersede proposals, session summaries), the orchestrator resolves hard decisions via new MCP tools, wake context draws from the vault, and the Plan-A hardening debt is paid.

**Architecture:** A `curator` module in `continuum-core` (trait-driven LLM access, fully testable with a mock) runs as a background tokio task beside the existing distiller. The runtime opens the same vault the desktop uses (file-watcher keeps both fresh). `continuum-mcp` gains 5 additive tools backed by a lazy `Vault`. Wake context gains vault retrieval + a pending-decisions block. Since the runtime has **no scheduler and no HealthRegistry** (verified — the spec's "existing scheduler" claim was wrong), Plan B builds a minimal daily-maintenance ticker and surfaces curator status through `RuntimeSnapshot`/state.json instead.

**Tech Stack:** Rust (tokio, sqlx via continuum-memory, existing `LocalLlm`/`TriageLayer` Qwen handle), rmcp for MCP tools, existing prompts/ dir conventions.

## Global Constraints

- **Never** build the full workspace, never `--release` during tasks. Verify per crate: `cargo test -p continuum-memory`, `cargo test -p continuum-core --lib` (default features — debug lib tests are fine per CLAUDE.md; llama/whisper compile once, sccache-cached), `cargo test -p continuum-core --no-default-features --lib` (must ALSO stay green — desktop builds core featureless), `cargo test -p continuum-mcp`, `cargo test -p continuum-desktop`, `cargo clippy -p continuum-core -p continuum-mcp -p continuum-memory -p continuum-desktop --all-targets -- -D warnings`, `cargo fmt --all -- --check`.
- bash needs `export PATH="$HOME/.cargo/bin:$PATH"`. Frontend checks (only Task 11 touches TS): PowerShell in `apps/desktop`: `corepack pnpm typecheck && corepack pnpm lint && corepack pnpm build && corepack pnpm format`.
- **Non-negotiable #7:** existing MCP tool names/schemas NEVER change. `memory_set_fact`/`memory_get_fact`/`memory_list_facts`/`memory_query_episodic` keep their exact input/output schemas; only internals may redirect. New tools are additive. Update `crates/continuum-mcp/tests/protocol.rs` `EXPECTED_TOOLS`, `config/default-permissions.toml`, and `docs/mcp-tools.md` in the same commit as any tool addition.
- **Layer hierarchy:** curator lives in the runtime's memory layer. Claude involvement ONLY via wake-context blocks (data up) + MCP tool calls (commands down). The curator never spawns the orchestrator directly; the daily-maintenance ticker synthesizes a wake through the same `do_wake` path triage uses.
- Anything touching `TriageLayer`/`LocalLlm` is gated `#[cfg(feature = "runtime")]` (mirror `triage/mod.rs`). The `curator` module itself compiles featureless (trait + logic + mock tests); only the `TriageLayer` impl and the runtime wiring are gated.
- All thresholds/intervals from `CuratorConfig` (already in config.rs — spec values). New knobs added in this plan (`maintenance_wake_hour`) must be config fields with defaults, documented (non-negotiable #3).
- Curator log events: `layer = "memory", component = "curator"`. LLM failure → retry once → skip pass (warn). 3 consecutive failures → surfaced in snapshot.
- Vault markdown is user property: curator writes go through `Vault` APIs only; wipe never touches vault markdown.
- Conventional commits, scopes: `memory`, `core`, `mcp`, `desktop`, `docs`.
- Prompts live in `prompts/curator-*.md`, loaded at runtime with `include_str!` fallback (same pattern as chat-system-prompt: baked in, overridable later — v1 bakes only).

## Integration anchors (verified against source 2026-08-03)

- Runtime boot: `crates/continuum-core/src/bin/continuum.rs` — distiller spawned ~L343-351 (`run_memory_distiller(raw_log, episodic, config, shutdown_rx)`); curator + vault open + signal channel go right after. `dev_dir` L81, `config` L84, shutdown watch L195. Wake begins in the `TriageDecision::WakeOrchestrator` arm (~L664-798) → `do_wake()` (~L885-1052) → `build_wake_message` call ~L916.
- `TriageLayer` (`crates/continuum-core/src/triage/llm.rs` L59-65): `Clone`; `llm: LocalLlm` field is private — Task 3 adds a passthrough. `LocalLlm::generate(&self, prompt: &str, opts: &GenerateOpts) -> Result<String>` serializes internally (ctx_cache mutex) — safe to share, calls queue.
- Distiller: `distill_once(raw_log, episodic, config)` in `memory/distill.rs` L75-124; event conversion `frame_to_memory_event` L127-137.
- Wake context: `orchestrator/wake_context.rs` `build_wake_message(trigger_frame, history_frames, memory_context, wake_reason) -> String` L33-90; retrieval: `memory/retrieval.rs` `retrieve_context(trigger_frame, episodic, semantic) -> Result<MemoryContext>` L44-92.
- MCP: `crates/continuum-mcp/src/server.rs` — `ServerState { data_dir, http, fs_extra_paths, semantic: OnceCell<SemanticStore>, episodic: OnceCell<Mutex<EpisodicStore>> }` L54-61; `#[tool_router]` macro pattern; lazy `OnceCell::get_or_try_init` accessors L139-166; `run_tool` wrapper L171-195. Tests: `crates/continuum-mcp/tests/protocol.rs` black-box subprocess, `EXPECTED_TOOLS` const L25-50. Semantic write site to retire: `server.rs:325` (`upsert_fact` in `memory_set_fact`).
- Runtime status: `RuntimeSnapshot` via `runtime_publish` (`continuum.rs` ~L469-499, state.json every 2 s). NO HealthRegistry in the runtime process; do not invent one — extend the snapshot.
- Session signals: `PerceptionFrame.context: ContextObservation { foreground_window_title, foreground_process_name, idle_seconds, in_call, ts }` (`senses/types.rs` L52-64). No existing project-switch/idle-boundary tracker — Task 6 builds one.
- Desktop wipe stub: `apps/desktop/src-tauri/src/commands.rs` `wipe_memory` (~L288-302).
- continuum-memory is currently a dependency of the desktop only — Tasks 2/8 add it to `continuum-core` and `continuum-mcp`.

## File structure (locked)

```
crates/continuum-memory/src/index.rs      T1: BEGIN IMMEDIATE writes, atomic rebuild, no-op skip, .MD case
crates/continuum-core/
├── Cargo.toml                            T2: + continuum-memory (unconditional dep)
├── src/bin/continuum.rs                  T2 vault boot/watcher/distiller thread; T4 curator spawn + signals;
│                                         T7 wipe executor; T9 do_wake threading; T10 maintenance ticker
├── src/memory/distill.rs                 T2: event feed into vault
├── src/memory/retrieval.rs               T9: vault retrieval + sensitivity filter
├── src/orchestrator/wake_context.rs      T9: vault-notes + pending-decisions sections
├── src/triage/llm.rs                     T3: complete() passthrough
├── src/runtime_publish.rs                T11: curator snapshot fields
└── src/curator/
    ├── mod.rs                            T3: module docs, CuratorLlm trait, MockLlm (cfg(test)), CuratorStatus
    ├── extract.rs                        T3/T4: candidate types, prompt builder, JSON parse, dedupe, thresholds
    ├── conflict.rs                       T5: supersede detection
    ├── session.rs                        T6: session tracker + summary notes
    └── run.rs                            T4: run_curator ticker loop; T7 hygiene tick + wipe executor
prompts/curator-extract.md                T4
prompts/curator-conflict.md               T5
prompts/curator-session.md                T6
crates/continuum-mcp/
├── Cargo.toml                            T8: + continuum-memory
├── src/server.rs                         T8: vault OnceCell + 5 tools + set_fact redirect
├── src/tools/memory.rs                   T8: request structs
├── src/tools/repair.rs                   T8: Memory target checks vault dir
└── tests/protocol.rs                     T8: EXPECTED_TOOLS + new tool tests
config/default-permissions.toml           T8
apps/desktop/src-tauri/src/commands.rs    T7: wipe writes request file + clears vault events
apps/desktop/src/components/tabs/MemoryTab.tsx  T7: wipe copy update (real behavior)
apps/desktop/src/lib/types.ts             T11: optional curator snapshot fields
docs/…, CHANGELOG.md, prompts/orchestrator-system.md   T12
```

---

### Task 1: Index hardening (Plan-A debt)

**Files:**
- Modify: `crates/continuum-memory/src/index.rs`
- Modify: `crates/continuum-memory/tests/index_test.rs`

**Interfaces:**
- Consumes: existing `Index` internals (Task 3 of Plan A): `index_file_inner(vault_dir, path, recompute)`, `upsert_node`, `remove_path_inner`, `quarantine_path`, `recompute_edges`, `rebuild`, `index_files`.
- Produces: same public API, hardened semantics. New private helper `immediate_tx(&self) -> Result<sqlx::Transaction<'_, sqlx::Sqlite>>` used by every write path.

- [ ] **Step 1: Write failing tests** (append to `tests/index_test.rs`):

```rust
#[tokio::test]
async fn reindex_skips_unchanged_files() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "facts/a.md", &note("mem_a", "fact", "A", "body"));
    let idx = open_index(tmp.path()).await;
    idx.rebuild(tmp.path()).await.unwrap();
    let before: (String,) =
        sqlx::query_as("SELECT value FROM meta WHERE key='reindex_ops'")
            .fetch_optional(idx.pool()).await.unwrap().unwrap_or(("0".into(),));
    // Re-index the same unchanged file — must be a no-op (mtime+hash short-circuit).
    idx.index_file(tmp.path(), &tmp.path().join("facts/a.md")).await.unwrap();
    let after: (String,) =
        sqlx::query_as("SELECT value FROM meta WHERE key='reindex_ops'")
            .fetch_optional(idx.pool()).await.unwrap().unwrap_or(("0".into(),));
    assert_eq!(before.0, after.0, "unchanged file must not bump reindex_ops");
    // Changing the body must reindex (ops bumps).
    write(tmp.path(), "facts/a.md", &note("mem_a", "fact", "A", "body changed"));
    idx.index_file(tmp.path(), &tmp.path().join("facts/a.md")).await.unwrap();
    let after2: (String,) =
        sqlx::query_as("SELECT value FROM meta WHERE key='reindex_ops'")
            .fetch_one(idx.pool()).await.unwrap();
    assert_ne!(after.0, after2.0);
}

#[tokio::test]
async fn uppercase_md_extension_is_indexed() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "facts/upper.MD", &note("mem_u", "fact", "Upper", ""));
    let idx = open_index(tmp.path()).await;
    let stats = idx.rebuild(tmp.path()).await.unwrap();
    assert_eq!(stats.indexed, 1);
}

#[tokio::test]
async fn rebuild_is_atomic_for_concurrent_readers() {
    // A second connection must never observe an EMPTY nodes table while a
    // rebuild over a non-empty vault is in flight (old-or-new, never empty).
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..300 {
        write(tmp.path(), &format!("notes/n{i}.md"),
              &note(&format!("mem_{i}"), "note", &format!("N {i}"), "x"));
    }
    let idx = std::sync::Arc::new(open_index(tmp.path()).await);
    idx.rebuild(tmp.path()).await.unwrap();
    let reader = {
        let db = tmp.path().join(".continuum/index.db");
        tokio::spawn(async move {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .connect(&format!("sqlite:{}", db.display())).await.unwrap();
            let mut min_seen = i64::MAX;
            for _ in 0..200 {
                let n: (i64,) = sqlx::query_as("SELECT count(*) FROM nodes")
                    .fetch_one(&pool).await.unwrap();
                min_seen = min_seen.min(n.0);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            min_seen
        })
    };
    let idx2 = idx.clone();
    let root = tmp.path().to_path_buf();
    let rebuilder = tokio::spawn(async move {
        for _ in 0..3 { idx2.rebuild(&root).await.unwrap(); }
    });
    rebuilder.await.unwrap();
    let min_seen = reader.await.unwrap();
    assert!(min_seen > 0, "reader observed an empty nodes table mid-rebuild");
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p continuum-memory --test index_test` (reindex_ops meta key doesn't exist; .MD skipped; rebuild currently clears tables non-transactionally so min_seen can hit 0).

- [ ] **Step 3: Implement.**

1. **`immediate_tx` helper** — sqlx has no built-in `BEGIN IMMEDIATE`; acquire a connection and issue it raw:

```rust
/// Begin an IMMEDIATE transaction: takes SQLite's write lock at BEGIN time,
/// closing the check-then-write TOCTOU window between two processes
/// (deferred BEGIN only locks at first write). All write paths use this.
async fn immediate_conn(&self) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>> {
    let mut conn = self.pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    Ok(conn)
}
async fn commit(conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>) -> Result<()> {
    sqlx::query("COMMIT").execute(&mut **conn).await?;
    Ok(())
}
async fn rollback_quiet(conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>) {
    let _ = sqlx::query("ROLLBACK").execute(&mut **conn).await;
}
```

Convert `upsert_node`, `remove_path_inner`, `quarantine_path`, and `recompute_edges` from `self.pool.begin()` to this pattern (execute statements on `&mut *conn`; on any error, `rollback_quiet` then return the error). Keep public signatures identical.

2. **No-op skip**: in `index_file_inner`, before parsing, read the file's mtime (unix millis) + compute `fnv1a(body)` only after reading the text (cheap); compare against the stored `nodes` row for this rel path (`SELECT mtime, body_hash FROM nodes WHERE path=?`). If both match AND the row exists → return `IndexOutcome::Skipped` without any write. On every non-skip write path bump a counter: `INSERT INTO meta(key,value) VALUES('reindex_ops','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1` (the test hook; cheap and useful for the dashboard later). Note the ordering subtlety: the hash covers the FULL file text (frontmatter+body) — hash the raw file string, not the parsed body, so frontmatter-only edits (status flips!) still reindex. Name the column meaning accordingly in a comment (it stores the whole-file hash from now on; rebuild refreshes all rows so no migration is needed, but bump `SCHEMA_VERSION` anyway to force one clean rebuild).
3. **Atomic rebuild**: restructure `rebuild()` to: (a) walk + parse ALL files first into an in-memory `Vec<ParsedEntry>` (entry = rowdata + links, or quarantine entry), sorted by rel path (existing determinism rule, first-wins slug dedupe done in memory against a `HashSet<String>` of taken slugs); (b) open ONE `immediate_conn`; (c) inside it: `DELETE` the five derived tables (`nodes, links, nodes_fts, unresolved_links, edges, quarantine` — events/meta untouched except `last_full_index_at`/`reindex_ops`), bulk-insert everything, recompute edges in-memory (reuse `resolve_links`) and insert; (d) COMMIT. Readers on other connections see the old snapshot until commit (WAL). `index_files` (batch) keeps its per-file transactions (bounded batches don't need the full atomicity).
4. **.MD case**: in `index_file_inner`'s skip check and `rebuild`'s walker (`collect_md_files`), match the extension case-insensitively (`ext.eq_ignore_ascii_case("md")`), matching the watcher.

- [ ] **Step 4: Run** — `cargo test -p continuum-memory` (all suites; the concurrent-reader test a few times), `cargo clippy -p continuum-memory --all-targets -- -D warnings`, `cargo fmt --all -- --check`.
- [ ] **Step 5: Commit** — `fix(memory): immediate write locks, atomic rebuild, no-op reindex skip, case-insensitive md`

---

### Task 2: Runtime opens the vault — boot, watcher, distiller event feed

**Files:**
- Modify: `crates/continuum-core/Cargo.toml` (add `continuum-memory = { path = "../continuum-memory" }` as a NORMAL dependency — it is lightweight by design; do NOT gate it behind `runtime`)
- Modify: `crates/continuum-core/src/bin/continuum.rs`
- Modify: `crates/continuum-core/src/memory/distill.rs`

**Interfaces:**
- Consumes: `continuum_memory::{Vault, VaultOptions, NewEvent}`; `config.memory.vault.resolve_vault_dir(&dev_dir)` etc.
- Produces (later tasks rely on): in `continuum.rs`, an `let vault: Arc<continuum_memory::Vault>` in scope at the distiller-spawn point and inside `do_wake` (thread it via clone into the wake spawn like `orch_config`); `run_memory_distiller` signature gains a `vault: Arc<Vault>` param; `distill_once(raw_log, episodic, vault, config)` writes one `NewEvent` per distilled frame.

- [ ] **Step 1: Failing test** (in `distill.rs` `#[cfg(test)]` — the module is runtime-gated; if its existing tests live behind `#[cfg(all(test, feature="runtime"))]`, follow that):

```rust
#[tokio::test]
async fn distill_once_feeds_vault_events() {
    // Build a tempdir vault + an in-memory raw log with one undistilled
    // high-salience frame (reuse this module's existing test fixtures for
    // RawLog + PerceptionFrame — see the current distill tests for the
    // constructors), run distill_once, then assert:
    let events = vault.events(&continuum_memory::EventRange::default()).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "distilled");
    assert!(!events[0].text.is_empty()); // the frame summary
}
```

(The implementer adapts fixture names from the existing tests in that file — the assertion block above is the contract.)

- [ ] **Step 2: Verify failure.**
- [ ] **Step 3: Implement:**
  - `run_memory_distiller(raw_log, episodic, vault: Arc<Vault>, config, shutdown)` — pass through to `distill_once`.
  - In `distill_once`, after `store.insert_event(&event)` succeeds for a frame, also:

```rust
let _ = vault
    .append_event(continuum_memory::NewEvent {
        ts: Some(event.ts),
        kind: "distilled".to_string(),
        text: event.summary.clone(),
        project: None,
        node_id: None,
        reference: event.source_frame_id.clone().map(|f| format!("frame:{f}")),
    })
    .await
    .map_err(|e| {
        tracing::warn!(layer = "memory", component = "distiller",
            error = %e.user_message(), "vault event append failed");
    });
```

  - In `continuum.rs`: after config load, open the vault once:

```rust
let vault_dir = config.memory.vault.resolve_vault_dir(&dev_dir);
let vault = Arc::new(
    continuum_memory::Vault::open_with(
        &vault_dir,
        continuum_memory::VaultOptions {
            watcher_debounce_ms: config.memory.vault.watcher_debounce_ms,
            graph_max_nodes: config.memory.vault.graph_max_nodes,
        },
    )
    .await
    .context("open memory vault")?,
);
```

  - Spawn a watcher drain task (keeps the runtime's index fresh when the user/desktop edits files; mirror the desktop bridge but without emit):

```rust
{
    let vault = vault.clone();
    let mut shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        let mut watcher = match vault.watch() {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(layer = "memory", component = "runtime",
                    error = %e.user_message(), "vault watcher unavailable");
                return;
            }
        };
        loop {
            tokio::select! {
                Some(paths) = watcher.rx.recv() => {
                    if let Err(e) = vault.reindex_paths(&paths).await {
                        tracing::warn!(layer = "memory", component = "runtime",
                            error = %e.user_message(), "vault reindex failed");
                    }
                }
                _ = shutdown.changed() => { if *shutdown.borrow() { break; } }
            }
        }
    });
}
```

  - Pass `vault.clone()` into the distiller spawn.
- [ ] **Step 4: Run** — `cargo test -p continuum-core --lib`, AND `cargo test -p continuum-core --no-default-features --lib` (must still compile featureless — the vault boot code lives in `bin/continuum.rs` which is runtime-only, and distill.rs is behind the runtime feature already; the unconditional `continuum-memory` dep must not break the featureless build), clippy + fmt.
- [ ] **Step 5: Commit** — `feat(core): runtime opens the memory vault — boot, watcher drain, distiller event feed`

---

### Task 3: CuratorLlm trait, TriageLayer passthrough, extraction core (types/parse/dedupe/thresholds)

**Files:**
- Create: `crates/continuum-core/src/curator/mod.rs`
- Create: `crates/continuum-core/src/curator/extract.rs`
- Modify: `crates/continuum-core/src/lib.rs` (add `pub mod curator;` — unconditional)
- Modify: `crates/continuum-core/src/triage/llm.rs` (add passthrough)

**Interfaces:**
- Consumes: `continuum_memory::{Vault, NoteDraft, NodeType, NodeStatus, Source, Relation, NodeSummary}`; `CuratorConfig`.
- Produces (Tasks 4-6 rely on — exact):

```rust
// curator/mod.rs
#[async_trait::async_trait]
pub trait CuratorLlm: Send + Sync {
    /// One-shot completion. Implementations serialize internally.
    async fn complete(&self, prompt: &str, max_tokens: u32) -> anyhow::Result<String>;
}
/// Rolling curator status for the runtime snapshot (Task 11).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CuratorStatus {
    pub last_pass_at: Option<chrono::DateTime<chrono::Utc>>,
    pub consecutive_failures: u32,
    pub candidates_written_total: u64,
    pub pending_count: u64,
}
pub type SharedCuratorStatus = std::sync::Arc<std::sync::Mutex<CuratorStatus>>;

// curator/extract.rs
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CandidateJson {
    pub r#type: String,          // one of the 10 node types
    pub title: String,
    pub body: String,
    #[serde(default)] pub project: Option<String>,
    pub confidence: f32,
    #[serde(default = "default_importance")] pub importance: f32, // 0.5
    #[serde(default)] pub source: Option<String>, // user_statement|observed|inferred
    #[serde(default)] pub relations: Vec<RelationJson>,
    #[serde(default)] pub tags: Vec<String>,
}
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RelationJson { pub to: String, pub rel: String, #[serde(default = "default_rel_conf")] pub confidence: f32 }

pub fn build_extract_prompt(events_block: &str, related_notes_block: &str, max_candidates: u32) -> String;
pub fn parse_candidates(raw: &str) -> anyhow::Result<Vec<CandidateJson>>;   // finds the JSON array, strict serde
pub fn normalize_title(t: &str) -> String;                                   // lowercase, collapse ws, strip punct
pub async fn is_duplicate(vault: &Vault, title: &str) -> anyhow::Result<bool>; // FTS top-5 + normalized-title equality (any status incl. rejected)
/// Threshold routing per spec: returns the status to write with, or None to discard.
pub fn route_candidate(c: &CandidateJson, cfg: &CuratorConfig) -> Option<NodeStatus>;
pub fn candidate_to_draft(c: &CandidateJson, status: NodeStatus) -> NoteDraft;
```

- `TriageLayer` gains (runtime-gated, in `triage/llm.rs`):

```rust
/// One-shot text completion against the shared local model. Used by the
/// curator; calls serialize on LocalLlm's internal context mutex.
pub async fn complete(&self, prompt: &str, max_tokens: u32) -> anyhow::Result<String> {
    let opts = continuum_llm::GenerateOpts {
        temperature: 0.2,
        max_tokens: Some(max_tokens),
        ..Default::default()
    };
    self.llm.generate(prompt, &opts).await
}
```

(check `GenerateOpts` field set/Default in continuum-llm and construct accordingly — if no `Default`, build all fields explicitly with top_k/top_p from the triage config's values). Then in `curator/mod.rs`, gated:

```rust
#[cfg(feature = "runtime")]
#[async_trait::async_trait]
impl CuratorLlm for crate::triage::llm::TriageLayer {
    async fn complete(&self, prompt: &str, max_tokens: u32) -> anyhow::Result<String> {
        TriageLayer::complete(self, prompt, max_tokens).await
    }
}
```

- [ ] **Step 1: Failing tests** (bottom of `extract.rs`; a `MockLlm` lives in `mod.rs` under `#[cfg(test)]`: struct holding a `Vec<String>` of scripted replies + call counter):

```rust
#[test]
fn parse_candidates_accepts_wrapped_json() {
    let raw = "Sure! Here are the memories:\n[{\"type\":\"preference\",\"title\":\"Prefers pnpm\",\"body\":\"uses pnpm\",\"confidence\":0.7}]\nDone.";
    let c = parse_candidates(raw).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].title, "Prefers pnpm");
    assert_eq!(c[0].importance, 0.5); // default backfill
}

#[test]
fn parse_candidates_rejects_garbage() {
    assert!(parse_candidates("no json here").is_err());
    assert!(parse_candidates("[{\"title\":\"missing type\"}]").is_err());
}

#[test]
fn route_candidate_threshold_table() {
    let cfg = CuratorConfig::default(); // auto_confirm 0.85, floor 0.4
    let mk = |conf: f32, source: &str| CandidateJson {
        r#type: "fact".into(), title: "T".into(), body: "b".into(), project: None,
        confidence: conf, importance: 0.5, source: Some(source.into()),
        relations: vec![], tags: vec![],
    };
    // >= threshold AND user_statement -> Confirmed
    assert_eq!(route_candidate(&mk(0.9, "user_statement"), &cfg), Some(NodeStatus::Confirmed));
    // >= threshold but NOT user_statement -> stays candidate
    assert_eq!(route_candidate(&mk(0.9, "observed"), &cfg), Some(NodeStatus::Candidate));
    // below floor -> discard
    assert_eq!(route_candidate(&mk(0.3, "observed"), &cfg), None);
    // in between -> candidate
    assert_eq!(route_candidate(&mk(0.6, "inferred"), &cfg), Some(NodeStatus::Candidate));
}

#[tokio::test]
async fn is_duplicate_matches_rejected_notes_too() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = continuum_memory::Vault::open(tmp.path()).await.unwrap();
    let mut d = /* NoteDraft for a Fact titled "Prefers PNPM" */;
    d.status = continuum_memory::NodeStatus::Rejected;
    vault.create(d).await.unwrap();
    assert!(is_duplicate(&vault, "prefers pnpm").await.unwrap());
    assert!(!is_duplicate(&vault, "totally new idea").await.unwrap());
}
```

(construct the draft with the full literal like Plan A's vault tests; `tempfile` goes into continuum-core `[dev-dependencies]` if absent.)

- [ ] **Step 2: Verify failure. Step 3: Implement:**
  - `parse_candidates`: find first `[` and matching last `]`, `serde_json::from_str::<Vec<CandidateJson>>` on that slice; validate `r#type` parses via `NodeType::parse` and title non-blank (else error listing the offender).
  - `route_candidate`: exactly the spec's Stage-3 rules (see the test table). Source string compare on `"user_statement"`.
  - `candidate_to_draft`: map type via `NodeType::parse` (fallback `Note` never happens — validated), source via match (unknown → `Observed`), status from arg, relations mapped, `source_ref: Some("curator:extract")`.
  - `is_duplicate`: `vault.search(title, 5)` + compare `normalize_title` equality on results; ALSO check `vault.graph(&GraphFilter{ statuses: Some(vec![Rejected]), query: Some(title), ..Default })` — simpler: since Plan A's FTS indexes every status, `vault.search` already covers rejected notes; verify that with the test (it does — search has no status filter).
  - `MockLlm` in mod.rs tests: `struct MockLlm(std::sync::Mutex<Vec<String>>)` popping scripted replies; errors when empty.
- [ ] **Step 4: Run** — `cargo test -p continuum-core --lib curator`, plus featureless build check `cargo test -p continuum-core --no-default-features --lib` (curator module must compile featureless — only the TriageLayer impl is gated), clippy, fmt.
- [ ] **Step 5: Commit** — `feat(core): curator llm trait, candidate parsing, threshold routing`

---

### Task 4: Extraction pass + curator run loop + runtime spawn

**Files:**
- Create: `crates/continuum-core/src/curator/run.rs`
- Create: `prompts/curator-extract.md`
- Modify: `crates/continuum-core/src/curator/mod.rs` (`pub mod run;` etc.)
- Modify: `crates/continuum-core/src/bin/continuum.rs` (spawn + signal channel)

**Interfaces:**
- Consumes: Task 3 items; `Vault::{events, search, create, pending}`.
- Produces:

```rust
// curator/run.rs
/// Per-frame signal from the perception loop (watch channel — latest wins).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivitySignal {
    pub project_hint: Option<String>,   // from foreground window/process heuristic
    pub process: String,
    pub idle_seconds: u64,
    pub ts: Option<chrono::DateTime<chrono::Utc>>,
}
pub async fn run_curator(
    vault: std::sync::Arc<continuum_memory::Vault>,
    llm: std::sync::Arc<dyn CuratorLlm>,
    cfg: crate::config::CuratorConfig,
    status: SharedCuratorStatus,
    mut activity: tokio::sync::watch::Receiver<ActivitySignal>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
);
/// One extraction pass. Public for tests. Returns number of candidates written.
pub async fn extract_pass(
    vault: &continuum_memory::Vault,
    llm: &dyn CuratorLlm,
    cfg: &crate::config::CuratorConfig,
    since: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<usize>;
pub const EXTRACT_PROMPT: &str = include_str!("../../../../prompts/curator-extract.md");
```

(compute the include path relative to `curator/run.rs` — verify with a compile; adjust `../` count to reach repo-root `prompts/`.)

- [ ] **Step 1: `prompts/curator-extract.md`** (complete content):

```markdown
You are Continuum's memory curator. You turn a window of recent activity
into at most {{MAX}} lasting memories. Most windows contain NOTHING worth
remembering — an empty list is the correct answer for routine activity.

Only propose a memory when the window shows:
- an explicit user statement of fact, preference, or decision;
- a decision visibly taken (tool switched, approach abandoned, config chosen);
- a recurring error and its resolution;
- a new person, project, or goal appearing.

Never propose: raw screen descriptions, one-off actions, anything already in
KNOWN MEMORIES below, speculation about feelings, or sensitive content
(passwords, private messages) — skip those entirely.

RECENT ACTIVITY (chronological events):
{{EVENTS}}

KNOWN MEMORIES possibly related (do not duplicate any of these):
{{RELATED}}

Reply with ONLY a JSON array (no prose). Each element:
{"type": "project|goal|task|decision|person|preference|fact|error|session|note",
 "title": "short imperative title",
 "body": "1-3 sentences, markdown allowed, [[Wiki-Links]] to related titles",
 "project": "slug-or-null",
 "confidence": 0.0-1.0,   // how sure you are this is true and lasting
 "importance": 0.0-1.0,   // how much future-Continuum benefits from knowing it
 "source": "user_statement|observed|inferred",
 "relations": [{"to": "slug-or-title", "rel": "belongs_to|works_on|caused_by|mentions", "confidence": 0.0-1.0}],
 "tags": ["lowercase"]}

Reply with [] when nothing qualifies.
```

- [ ] **Step 2: Failing test** (bottom of run.rs):

```rust
#[tokio::test]
async fn extract_pass_writes_routed_candidates() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = continuum_memory::Vault::open(tmp.path()).await.unwrap();
    vault.append_event(/* NewEvent kind "distilled", text "User said they prefer pnpm over npm" */).await.unwrap();
    let llm = MockLlm::scripted(vec![
        r#"[{"type":"preference","title":"Prefers pnpm over npm","body":"Stated in terminal.","confidence":0.9,"source":"user_statement"},
            {"type":"fact","title":"Maybe uses Unity","body":"Seen once.","confidence":0.5,"source":"inferred"},
            {"type":"fact","title":"Noise","body":"x","confidence":0.2,"source":"observed"}]"#.into(),
    ]);
    let cfg = crate::config::CuratorConfig::default();
    let written = extract_pass(&vault, &llm, &cfg, chrono::Utc::now() - chrono::Duration::hours(1)).await.unwrap();
    assert_eq!(written, 2); // 0.2 discarded
    let pending = vault.pending().await.unwrap();
    assert_eq!(pending.len(), 1); // the 0.5 inferred one
    let hits = vault.search("pnpm", 5).await.unwrap();
    assert!(hits.iter().any(|h| h.status == continuum_memory::NodeStatus::Confirmed)); // 0.9 user_statement auto-confirmed
    // Second pass with the same scripted candidate — dedupe drops it.
    let llm2 = MockLlm::scripted(vec![
        r#"[{"type":"preference","title":"Prefers pnpm over npm","body":"again","confidence":0.9,"source":"user_statement"}]"#.into(),
    ]);
    let written2 = extract_pass(&vault, &llm2, &cfg, chrono::Utc::now() - chrono::Duration::hours(1)).await.unwrap();
    assert_eq!(written2, 0);
}

#[tokio::test]
async fn extract_pass_retries_once_then_skips() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = continuum_memory::Vault::open(tmp.path()).await.unwrap();
    vault.append_event(/* any event */).await.unwrap();
    let llm = MockLlm::scripted(vec!["not json".into(), "still not json".into()]);
    let cfg = crate::config::CuratorConfig::default();
    let written = extract_pass(&vault, &llm, &cfg, chrono::Utc::now() - chrono::Duration::hours(1)).await.unwrap();
    assert_eq!(written, 0);
    assert_eq!(llm.calls(), 2); // initial + one retry with the error appended
}
```

- [ ] **Step 3: Implement `extract_pass`:** fetch `vault.events(&EventRange{ since: Some(since), until: None, limit: Some(200) })`; empty → return 0 without an LLM call. Build `events_block` ("HH:MM kind — text" lines) and `related_block` from `vault.search` over the distinct significant words of the last 10 event texts (simple: search with the concatenated last-3 event texts, take top 8 titles+snippets). Fill `{{MAX}}/{{EVENTS}}/{{RELATED}}` placeholders. `llm.complete(prompt, 1024)`; `parse_candidates`; on parse error retry ONCE with `format!("{prompt}\n\nYour previous reply was invalid: {err}. Reply with ONLY the JSON array.")`; second failure → warn + return Ok(0). Truncate to `cfg.max_candidates_per_pass`. Per candidate: `is_duplicate`? skip. `route_candidate` → None? skip (debug log). Else `vault.create(candidate_to_draft(...))` (collision-safe via Plan A slug logic). Count writes; info-log the pass summary.
- **`run_curator`:** if `!cfg.enabled` → log + park on shutdown (mirror distiller's disabled arm). Ticker `interval_minutes.max(1)*60`; each tick: `extract_pass(vault, llm, cfg, last_pass_ts)` (first pass: now - interval); update `status` (last_pass_at, consecutive_failures 0 on Ok / +1 on Err, candidates_written_total, pending_count via `vault.pending().len()`); Task 5/6 add their calls here later. `tokio::select!` with shutdown (distiller shape).
- **continuum.rs wiring** (runtime feature — this is the bin, always full-featured): create `let (activity_tx, activity_rx) = tokio::sync::watch::channel(ActivitySignal::default());` before the main loop; in the perception loop, after each frame is built, send:

```rust
let _ = activity_tx.send(curator::run::ActivitySignal {
    project_hint: crate::memory::retrieval::infer_project_hint(&frame), // Task 9 makes this pub; until then use None
    process: frame.context.foreground_process_name.clone(),
    idle_seconds: frame.context.idle_seconds,
    ts: Some(frame.context.ts),
});
```

(For THIS task send `project_hint: None` — Task 9 wires the helper; leave a `// Task 9 fills project_hint` comment.) Spawn after the distiller block, only when triage exists:

```rust
if let Some(triage) = triage.clone() {
    let status: curator::SharedCuratorStatus = Default::default();
    // keep a clone in scope for Task 11's snapshot publisher
    let llm: Arc<dyn curator::CuratorLlm> = Arc::new(triage);
    tokio::spawn(curator::run::run_curator(
        vault.clone(), llm, config.memory.curator.clone(),
        status.clone(), activity_rx.clone(), shutdown_rx.clone(),
    ));
} else {
    tracing::info!(layer = "memory", component = "curator",
        "curator disabled: no triage model loaded");
}
```

- [ ] **Step 4: Run** — `cargo test -p continuum-core --lib curator`, featureless lib build, clippy, fmt.
- [ ] **Step 5: Commit** — `feat(core): curator extraction pass and runtime loop`

---

### Task 5: Conflict / supersede detection

**Files:**
- Create: `crates/continuum-core/src/curator/conflict.rs`
- Create: `prompts/curator-conflict.md`
- Modify: `curator/run.rs` (call after each pass), `curator/mod.rs`

**Interfaces:**
- Consumes: `CuratorLlm`, `Vault::{search, get, save, pending}`, `Relation`.
- Produces: `pub async fn detect_conflicts(vault: &Vault, llm: &dyn CuratorLlm, new_note_ids: &[String]) -> anyhow::Result<usize>` (number of proposals created); `extract_pass` returns the created ids (change its return to `anyhow::Result<Vec<String>>` — update Task 4's callers/tests accordingly, count = len).

- [ ] **Step 1: `prompts/curator-conflict.md`:**

```markdown
You compare two memories from the same knowledge base and decide whether the
NEW one contradicts or replaces the OLD one.

OLD memory:
{{OLD}}

NEW memory:
{{NEW}}

Answer with ONLY one JSON object:
{"verdict": "supersedes" | "contradicts" | "unrelated" | "same_topic_compatible",
 "confidence": 0.0-1.0,
 "reason": "one sentence"}

"supersedes": the NEW memory states a newer decision/fact that replaces OLD.
"contradicts": they cannot both be true and it is unclear which is current.
Anything else: "unrelated" or "same_topic_compatible".
```

- [ ] **Step 2: Failing test** (bottom of conflict.rs):

```rust
#[tokio::test]
async fn supersede_verdict_creates_proposal_relation() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = continuum_memory::Vault::open(tmp.path()).await.unwrap();
    let old = vault.create(/* Decision "Use MongoDB", status Confirmed */).await.unwrap();
    let mut cand = /* Decision "Use PostgreSQL", body "switching db" */;
    cand.status = continuum_memory::NodeStatus::Candidate;
    let new = vault.create(cand).await.unwrap();
    let llm = MockLlm::scripted(vec![
        r#"{"verdict":"supersedes","confidence":0.9,"reason":"newer db decision"}"#.into(),
    ]);
    let n = detect_conflicts(&vault, &llm, &[new.frontmatter.id.clone()]).await.unwrap();
    assert_eq!(n, 1);
    let refreshed = vault.get(&new.frontmatter.id).await.unwrap();
    assert!(refreshed.frontmatter.relations.iter().any(|r|
        r.rel == "proposes_supersede" && r.to == old.frontmatter.id));
    // OLD note untouched (never auto-superseded by Qwen alone)
    assert_eq!(vault.get(&old.frontmatter.id).await.unwrap().frontmatter.status,
               continuum_memory::NodeStatus::Confirmed);
}

#[tokio::test]
async fn unrelated_verdict_creates_nothing() {
    /* same setup, MockLlm scripted {"verdict":"unrelated",...} → n == 0, no relation */
}
```

(the second test is written out fully by the implementer — same shape.)

- [ ] **Step 3: Implement:** for each new id: `vault.get(id)`; find comparison partners: `vault.search(&note.frontmatter.title, 5)` filtered to same `node_type`, status `Confirmed`, id != self, same project when both have one; take top 2. Per partner: fill `{{OLD}}/{{NEW}}` (title + body + created date each); `llm.complete(prompt, 256)`; parse the single JSON object (reuse a small `parse_verdict` with the same find-braces approach); on parse error retry once then skip pair. `verdict == "supersedes" || "contradicts"` with confidence ≥ 0.5 → append `Relation { to: partner_id, rel: "proposes_supersede".into(), confidence }` to the NEW note's relations (skip if already present) + `vault.save` it. Never touch the old note. Wire into `run_curator`: `let ids = extract_pass(...)?; if !ids.is_empty() { detect_conflicts(&vault, llm.as_ref(), &ids).await?; }`.
- [ ] **Step 4: Run + clippy/fmt. Step 5: Commit** — `feat(core): curator supersede/contradiction proposals`

---

### Task 6: Session summaries

**Files:**
- Create: `crates/continuum-core/src/curator/session.rs`
- Create: `prompts/curator-session.md`
- Modify: `curator/run.rs` (tracker check each tick + on activity change), `curator/mod.rs`

**Interfaces:**
- Consumes: `ActivitySignal` watch, `CuratorLlm`, `Vault::{events, create}`, `cfg.session_summary_idle_minutes`.
- Produces:

```rust
pub struct SessionTracker { /* current: Option<SessionState { started, last_activity, project_hint, process }> */ }
impl SessionTracker {
    pub fn new() -> Self;
    /// Feed the latest signal; returns Some(ended_session) when a boundary
    /// fired (process/project changed after >=5 min of session, or idle
    /// exceeded the configured minutes).
    pub fn observe(&mut self, sig: &ActivitySignal, idle_limit_min: u64) -> Option<EndedSession>;
}
pub struct EndedSession { pub started: DateTime<Utc>, pub ended: DateTime<Utc>, pub project_hint: Option<String>, pub process: String }
pub async fn write_session_summary(vault: &Vault, llm: &dyn CuratorLlm, ended: &EndedSession) -> anyhow::Result<Option<String>>; // note id; None when too little happened
```

- [ ] **Step 1: `prompts/curator-session.md`:**

```markdown
Summarize this work session for Continuum's long-term memory. Use the exact
section layout below. Be concrete; name files, errors, and outcomes. If the
events show trivial/idle activity only, reply with exactly: SKIP

Session: {{START}} – {{END}}  (main app: {{PROCESS}}, project hint: {{PROJECT}})

Events:
{{EVENTS}}

Reply with markdown in exactly this shape (no preamble):
## Goal
<one line>
## Changed
<bullets>
## Problem
<one line or "none">
## Tried
<bullets or "–">
## Result
<one line>
## Next step
<one line>
```

- [ ] **Step 2: Failing tests** (bottom of session.rs): a `SessionTracker` table test — boundary on process change after ≥5 min (use fabricated `ts` values; `observe` uses signal `ts`, not wall clock), boundary on idle ≥ limit, NO boundary on brief process flick (<5 min session never emits), tracker resets after emit; and `write_session_summary`: MockLlm scripted with a valid summary → note created with `type: session`, `status: Confirmed`, `source: Observed`, title `"Session: {{PROCESS}} — {{END:%Y-%m-%d %H:%M}}"`, body = the LLM markdown, relation `{to: project_hint, rel: "belongs_to"}` when hint present; scripted `"SKIP"` → returns None, no note. Write the tests fully (same fixture style as Tasks 4/5).
- [ ] **Step 3: Implement.** `observe` state machine: no current → start one (not a boundary). Current + signal.ts − last_activity > idle_limit → end at last_activity. Current + process changed and session length ≥ 5 min → end at previous ts. Else update last_activity/project_hint. `write_session_summary`: `vault.events` between started/ended (limit 300); < 3 events → Ok(None) without LLM. Fill placeholders; `llm.complete(prompt, 700)`; reply trimmed == "SKIP" → Ok(None); else create the note (session summaries skip candidate review per spec — records, not claims). In `run_curator`: on every tick AND on every `activity.changed()` (add a select arm), run `tracker.observe`; boundary → `write_session_summary` (failures logged, don't kill the loop).
- [ ] **Step 4: Run + featureless + clippy/fmt. Step 5: Commit** — `feat(core): curator session summaries with idle/project boundaries`

---

### Task 7: Hygiene tick + real wipe path

**Files:**
- Modify: `crates/continuum-core/src/curator/run.rs` (daily hygiene + wipe-request executor)
- Modify: `crates/continuum-core/src/memory/raw_log.rs` (add `wipe_all`), `crates/continuum-core/src/memory/episodic.rs` (add `wipe_all`)
- Modify: `crates/continuum-core/src/bin/continuum.rs` (boot-time wipe check)
- Modify: `apps/desktop/src-tauri/src/commands.rs` (`wipe_memory` writes the request file + clears vault events + rebuilds index)
- Modify: `apps/desktop/src/components/tabs/MemoryTab.tsx` (copy: request executes immediately in-app for events/index; perception stores clear when the runtime processes the request)

**Interfaces:**
- Produces: request-file contract `<dev_dir>/wipe-request.json` = `{"requested_at": "<rfc3339>", "scopes": ["raw_log", "episodic", "events"]}`; `pub async fn process_wipe_request(dev_dir: &Path, raw_log: &RawLog, episodic: &Arc<Mutex<EpisodicStore>>, vault: &Vault) -> anyhow::Result<bool>` in `curator/run.rs` (gated `#[cfg(feature = "runtime")]` since it touches RawLog/Episodic); `RawLog::wipe_all(&self) -> Result<u64>` (DELETE FROM the frames table(s), return rows); `EpisodicStore::wipe_all(&mut self) -> Result<()>` (drop + recreate the lancedb table — mirror however `open` creates it).
- Desktop `wipe_memory` new behavior: validate `"DELETE"`; write the request file (serde_json, atomic tmp+rename); `vault.prune_events(0)` + `vault.rebuild_index()` via MemoryState; keep the tracing warn. MCP is NOT involved (Task 8's `memory_wipe_all` writes the same file).

- [ ] **Step 1: Failing tests:** in `curator/run.rs` (runtime-gated test): create tempdir with request file + tempdir vault with 2 events + in-memory RawLog with rows (reuse raw_log test fixtures) → `process_wipe_request` returns true, events empty, raw log row count 0, request file deleted; second call returns false (no file). In `commands.rs` tests: `wipe_memory_writes_request_and_clears_events` via the `*_inner` pattern (MemoryState on tempdir; pass a temp dev_dir into MemoryState — add the dev_dir path to MemoryState::new if not already stored (it stores legacy_semantic_db; derive dev_dir as its parent, or add a field — pick adding `dev_dir: PathBuf` to `MemoryState::new`'s signature and update main.rs).
- [ ] **Step 2: Verify failure. Step 3: Implement** per the contracts above. Hygiene in `run_curator`: track `last_hygiene: Option<Date>`; on each tick, if the local date changed since last run: `vault.sweep_expired()`, `vault.prune_events(cfg_vault.events_retention_days)` (thread `MemoryVaultConfig` into `run_curator` — extend its signature: `vault_cfg: MemoryVaultConfig`), `process_wipe_request(...)`. Also call `process_wipe_request` once at curator start (boot drain). continuum.rs passes `raw_log.clone()`/`episodic.clone()` into `run_curator` (extend signature; featureless build unaffected — run.rs's runtime-gated section).
- [ ] **Step 4: Run all core + desktop gates + frontend gates (copy change). Step 5: Commit** — `feat(core,desktop): daily hygiene and a real derived-data wipe path`

---

### Task 8: MCP vault tools

**Files:**
- Modify: `crates/continuum-mcp/Cargo.toml` (+ `continuum-memory = { path = "../continuum-memory" }`)
- Modify: `crates/continuum-mcp/src/server.rs`, `src/tools/memory.rs`, `src/tools/repair.rs`
- Modify: `crates/continuum-mcp/tests/protocol.rs`, `config/default-permissions.toml`, `docs/mcp-tools.md`

**Interfaces:**
- Produces — 5 new tools (names are the published API, never change after this):
  - `memory_vault_search` — req `{ query: String, types: Option<Vec<String>>, project: Option<String>, limit: Option<u32> }` → JSON list of `NodeSummary`
  - `memory_vault_get` — `{ id: String }` → full note (frontmatter + body + backlinks)
  - `memory_vault_save` — `{ r#type: String, title: String, body: String, project: Option<String>, confidence: Option<f32>, importance: Option<f32>, relations: Option<Vec<{to,rel,confidence}>>, tags: Option<Vec<String>>, source_ref: Option<String> }` → created/updated note id. Creates `status: confirmed`, `source: agent_run`. If a note with the same normalized title exists → UPDATE its body/metadata instead of creating a duplicate.
  - `memory_vault_resolve` — `{ id: String, action: String ("confirm"|"reject"|"supersede"), replaces: Option<String> }` → ok
  - `memory_wipe_all` — `{ confirm: String }` (must equal `"WIPE"`) → writes the Task-7 request file; returns the path written.
- `ServerState` gains `vault: OnceCell<continuum_memory::Vault>` + `async fn vault(&self) -> Result<&Vault, McpError>` opening `Vault::open(self.data_dir.join("vault"))` lazily — **note**: data_dir == dev_dir (`CONTINUUM_DATA_DIR`), and the vault dir must match the runtime's `resolve_vault_dir` default (`<dev_dir>/vault`); read `config.memory.vault.vault_dir` override the same way other MCP config reads happen — if the MCP server doesn't load ContinuumConfig today, use the default `<data_dir>/vault` and document the limitation in docs/mcp-tools.md ("custom vault_dir requires CONTINUUM_VAULT_DIR env", and support that env var).
- `memory_set_fact` redirect: keep schema; internally write a vault fact note (title `key.replacen('.', ": ", 1)`, tag = first segment, `source: agent_run`, `source_ref: Some("mcp:set_fact:<key>")`, same-title update semantics as memory_vault_save) and STOP writing SemanticStore (delete the `upsert_fact` call — the single write site). `memory_get_fact`/`memory_list_facts`: vault-first (lookup by the same title mapping / tag-prefix search), fall back to the legacy store read when the vault has no match — schemas unchanged.
- `repair.rs`: `RepairTarget::Memory` file check → `dir_status(data_dir.join("vault"))` (vault dir exists + index.db present) instead of semantic.sqlite (keep the old check as a secondary line if trivially cheap).

- [ ] **Step 1: Failing protocol tests** — add the 5 names to `EXPECTED_TOOLS`; add `tools/call` round-trips: `memory_vault_save` (create → returns id) then `memory_vault_get` (finds it) then `memory_vault_search` (finds it) then `memory_vault_resolve` on a candidate (create candidate via save? save always confirms — create the candidate by writing a vault file directly into the test's temp CONTINUUM_DATA_DIR before the call, using the frontmatter template from docs/memory.md); `memory_wipe_all` with wrong confirm → error, with "WIPE" → request file exists in temp data dir. Follow protocol.rs's existing spawn/temp-dir conventions exactly.
- [ ] **Step 2: Verify failure. Step 3: Implement** per contracts. Permission entries (default-permissions.toml `[memory]`): `memory_vault_search = "auto"`, `memory_vault_get = "auto"`, `memory_vault_save = "session-approved"`, `memory_vault_resolve = "session-approved"`, `memory_wipe_all = "always-confirm"`. docs/mcp-tools.md entries in the existing `#### \`tool\`` format with JSON examples.
- [ ] **Step 4: Run** — `cargo test -p continuum-mcp`, clippy, fmt. **Step 5: Commit** — `feat(mcp): vault tools, set_fact redirect to vault, wipe request tool`

---

### Task 9: Wake-context vault retrieval + pending-decisions block

**Files:**
- Modify: `crates/continuum-core/src/memory/retrieval.rs`
- Modify: `crates/continuum-core/src/orchestrator/wake_context.rs`
- Modify: `crates/continuum-core/src/bin/continuum.rs` (`do_wake` threading + `touch_last_used`)

**Interfaces:**
- Produces:
  - `MemoryContext` gains `pub vault_notes: Vec<continuum_memory::NodeSummary>` and `pub pending_decisions: Vec<continuum_memory::NodeSummary>` (default empty; every existing constructor updated).
  - `pub fn infer_project_hint(frame: &PerceptionFrame) -> Option<String>` (rename/pub-ify the existing private `infer_project_prefix` heuristic, minus the `"project."` prefix formatting — returns the bare hint; keep a thin private wrapper for the legacy prefix use). Task 4's TODO comment in continuum.rs is now filled (`project_hint: infer_project_hint(&frame)`).
  - `pub async fn retrieve_vault_context(vault: &Vault, frame: &PerceptionFrame, curator_cfg: &CuratorConfig) -> (Vec<NodeSummary>, Vec<NodeSummary>)` — (notes, pending): notes = `vault.search(&build_query_from_frame(frame), 24)` filtered to `status == Confirmed` and (`sensitivity != Sensitive` unless `curator_cfg.include_sensitive_in_context`), sorted by importance desc, truncated to `curator_cfg.wake_vault_notes_max`; pending = `vault.pending()` items older than 30 minutes (compare `created`), truncated to `curator_cfg.claude_batch`. Failures inside → warn + empty vecs (a wake must never die on vault trouble).
  - `build_wake_message` gains two sections (function signature unchanged — data rides in `MemoryContext`): after the existing facts section: `## Long-term memory (vault)` listing `- [type] title — snippet (importance 0.9)` per note, section omitted when empty; and as the LAST section before the wake reason: `## Pending memory decisions` — for each: `- id: <id> — [type] "title" (confidence 0.6, source observed)` + trailing instruction line: `Resolve these with the memory_vault_resolve tool (confirm/reject/supersede) or improve them with memory_vault_save. Skip any you are unsure about.` Section omitted when empty.
- Consumes: `do_wake` calls `retrieve_vault_context` next to the existing `retrieve_context` call and fills the new fields; after a successful wake-message build, `vault.touch_last_used(&ids)` for the injected note ids (spawned best-effort, not blocking the wake).

- [ ] **Step 1: Failing tests:** in wake_context.rs's existing test module: build a `MemoryContext` with one vault note + one pending item → assert both section headers + the id line + the instruction line appear, and an empty-vec context omits both headers. In retrieval.rs tests (runtime-gated if the module is): tempdir vault with confirmed/sensitive/candidate notes → `retrieve_vault_context` returns only the confirmed non-sensitive one; with `include_sensitive_in_context = true` the sensitive one appears; pending only returns candidates older than 30 min (create one, backdate `created` via direct frontmatter edit + save_preserving? — simplest: create then `vault.get`, set `frontmatter.created = now - 1h`, save via `save` (updated re-stamp irrelevant, filter uses created), reindex happens on save).
- [ ] **Step 2: Verify failure. Step 3: Implement per contracts. Step 4: Run** — core lib both feature sets, clippy, fmt. **Step 5: Commit** — `feat(core): vault notes and pending decisions in wake context`

---

### Task 10: Daily maintenance wake

**Files:**
- Modify: `crates/continuum-core/src/config.rs` (CuratorConfig + `maintenance_wake_hour: Option<u32>` default `Some(4)`, doc: local hour 0-23 for the daily memory-maintenance wake; `None` disables — spec-gap fix: no scheduler exists, this is the "queue drains on quiet days" guarantee)
- Modify: `crates/continuum-core/src/bin/continuum.rs`

**Interfaces:**
- Consumes: the `seconds_until_next_local(hour)` pattern from `health/backup.rs` (copy the helper's usage, or make that fn `pub` and reuse — prefer making it `pub` in health/backup.rs with a doc comment).
- Produces: a spawned ticker that once per local day at the configured hour: if curator enabled AND `vault.pending()` is non-empty AND the orchestrator is not busy (`orchestrator_busy` AtomicBool) → trigger the same wake path triage uses with reason `"daily memory maintenance: N pending memory decisions"`. Implement by calling the existing `do_wake(...)` invocation shape used in the WakeOrchestrator arm — read that arm carefully and reuse its argument construction (clone pattern); a synthetic `PerceptionFrame` is NOT fabricated: use the most recent frame kept by the loop (the loop keeps `history_frames`; store an `Arc<Mutex<Option<PerceptionFrame>>>` "last_frame" updated per frame, and skip the maintenance wake when it's `None` (nothing perceived yet)).

- [ ] **Step 1: Failing config test** (config.rs test mod): `assert_eq!(cfg.memory.curator.maintenance_wake_hour, Some(4));` + toml override `maintenance_wake_hour = 2` parses; `[memory.curator]\nmaintenance_wake_hour = false`? — no: use Option<u32>, absent = default Some(4), explicit `0` valid; disabling = `maintenance_wake_hour = -1`? — **decision: use `i32`, negative disables, default `4`** (Option<u32> in TOML can't express "explicitly none" cleanly). Adjust the Interfaces text accordingly: `pub maintenance_wake_hour: i32` default `4`, `< 0` disables, values ≥ 24 clamp to 23 with a warn at use site. Test asserts default 4 and that `-1` parses.
- [ ] **Step 2-3: Implement** per the corrected contract. **Step 4: Run** — core lib both feature sets + clippy + fmt. **Step 5: Commit** — `feat(core): daily memory-maintenance wake ticker`

---

### Task 11: Curator status in RuntimeSnapshot + dashboard surfacing

**Files:**
- Modify: `crates/continuum-core/src/runtime_publish.rs` (RuntimeSnapshot + curator fields)
- Modify: `crates/continuum-core/src/bin/continuum.rs` (publisher reads SharedCuratorStatus)
- Modify: `apps/desktop/src/lib/types.ts` (+ optional fields on the state type), `apps/desktop/src/components/tabs/BrainTab.tsx` OR `HealthTab.tsx` — locate where runtime component statuses render from state.json and add a "Curator" row (last pass, pending count, failures); pick the tab that already lists runtime memory stats (`MemoryTab` overview used `state.memory` before the rebuild — check `HomeTab`/`BrainTab` for the current consumer of `snapshot.memory` and put the row beside it).

**Interfaces:**
- Produces: `RuntimeSnapshot` gains `#[serde(default)] pub curator: Option<CuratorSnapshot>` with `CuratorSnapshot { last_pass_at: Option<String>, consecutive_failures: u32, candidates_written_total: u64, pending_count: u64, enabled: bool }`; publisher fills it from `SharedCuratorStatus` (the clone kept in Task 4's spawn — thread it to where the publisher is spawned). TS: optional `curator?: {...} | null` on the state type; UI renders "Curator: last pass 12:41, 3 pending" + failure badge when `consecutive_failures >= 3` (matches spec's degrading rule).
- `#[serde(default)]` keeps old state.json files parsing (dashboard tolerant of missing field).

- [ ] **Steps:** failing Rust test (snapshot serde round-trip with and without curator field), implement, then frontend: types + row + gates (`corepack pnpm typecheck/lint/build/format`), then all cargo gates. **Commit** — `feat(core,desktop): curator status in runtime snapshot and dashboard`

---

### Task 12: Docs, prompts, changelog, final gates

**Files:**
- Modify: `prompts/orchestrator-system.md` (memory section: introduce the vault tools — search/get/save/resolve semantics, "prefer memory_vault_save over memory_set_fact", pending-decisions block explanation)
- Modify: `docs/mcp-tools.md` (verify Task 8 entries complete), `docs/memory.md` (curator section: stages, config keys incl. maintenance_wake_hour, wipe flow reality), `docs/self-healing.md` (curator component: degrading at 3 consecutive failures, recovery = restart runtime; wipe-request file location), `docs/dashboard.md` (curator status row), `ARCHITECTURE.md` (curator + wake-context sections now real; remove "Plan B will…" phrasing), `ROADMAP.md`, `CHANGELOG.md` (`## [Unreleased]`: curator pipeline, MCP vault tools + set_fact redirect, wake-context vault retrieval, maintenance wake, real wipe path, index hardening)
- Modify: `apps/desktop/src-tauri/assets/chat-system-prompt.md` — the chat explainer's memory description mentions the old three-store design; refresh the memory paragraph.

**Steps:**
- [ ] **Step 1:** write all doc updates (accuracy verified against the code as built — no aspirational claims; the T15-Plan-A lesson: docs that lie are worse than no docs).
- [ ] **Step 2:** full verification battery — every command from Global Constraints (all five cargo test targets, clippy, fmt, all four pnpm gates).
- [ ] **Step 3: Commit** — `docs(memory): curator pipeline, mcp vault tools, wake context — plan B docs`

---

## Plan self-review (done at write time)

- **Spec coverage (Plan B scope):** Stage 1 → T2; Stages 2-3 → T3/T4; Stage 4 → T5; Stage 5 → T9 (block) + T8 (tools) + T10 (quiet-day drain — replaces the spec's nonexistent "existing scheduler" with a built ticker, documented as spec-gap fix); Stage 6 → T6; Stage 7 → T7; MCP additions → T8 (spec's 4 tools + `memory_wipe_all` which the spec's desktop section promised as "memory__wipe_all" follow-up — normalized to single-underscore `memory_wipe_all` to match every existing tool name); retrieval-on-wake + touch_last_used + sensitivity → T9; SemanticStore write retirement → T8 (single write site verified); runtime health → T11 via RuntimeSnapshot (spec said "system_health MCP tool" — no such tool exists and the runtime has no HealthRegistry; snapshot+dashboard is the mechanism that actually exists; documented in T12); Plan-A deferred hardening (BEGIN IMMEDIATE, no-op skip, .MD case, atomic rebuild, wipe) → T1/T7.
- **Known deviations from spec text (all forced by verified reality, documented in T12):** "existing scheduler" doesn't exist → T10 builds one; "system_health MCP tool" doesn't exist → RuntimeSnapshot; "memory__wipe_all" → `memory_wipe_all`; session-summary trigger "signaled by triage context" → curator-owned SessionTracker (no such signal exists).
- **Placeholder scan:** the two "implementer writes the second test fully — same shape" notes (T5/T6) are bounded instructions with the exact assertion contract stated, and T2/T4 fixture-reuse notes name the file whose fixtures to reuse — acceptable; no TBD/TODO items remain.
- **Type consistency:** `extract_pass` returns `Vec<String>` from T5 onward (T4's tests use `.len()`); `run_curator` signature grows in T7 (raw_log/episodic/vault_cfg) — T7 explicitly owns updating the T4 spawn; `ActivitySignal`/`SharedCuratorStatus`/`CuratorLlm` names consistent across T3/T4/T6/T11; `maintenance_wake_hour: i32` corrected in-place in T10 (Interfaces text updated by its own Step 1 decision note).
