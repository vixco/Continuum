# Memory Vault + Graph UI (Plan A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `continuum-memory` crate (markdown vault = source of truth + derived SQLite index) and rebuild the desktop Memory tab as a graph-centric Obsidian-like UI.

**Architecture:** A new lightweight crate `continuum-memory` owns the vault format (frontmatter markdown + wiki-links), a rebuildable SQLite index (FTS5, graph, events, quarantine), a debounced file-watcher, and legacy migration. The Tauri desktop links it directly (no runtime required), exposes ~13 commands, and the rebuilt MemoryTab renders a force-directed canvas graph with a dockable note panel, overlay editor, curator-proposal stack (empty until Plan B), and timeline strip. Spec: `docs/superpowers/specs/2026-08-03-memory-vault-design.md`.

**Tech Stack:** Rust (sqlx 0.8 sqlite, serde_yaml, ulid, notify 6 + notify-debouncer-mini 0.4, regex, thiserror), Tauri 2, Next.js 15 static export, React 19, `force-graph` (canvas), Tailwind tokens, zustand.

## Global Constraints

- **Never** build the full workspace and never `cargo build --release` during tasks. Verify per crate: `cargo test -p continuum-memory`, `cargo test -p continuum-core --no-default-features --lib`, `cargo test -p continuum-desktop`, `cargo clippy -p continuum-memory -p continuum-desktop --all-targets -- -D warnings`, `cargo fmt --all -- --check`.
- Bash sessions need `export PATH="$HOME/.cargo/bin:$PATH"` before any cargo command. Frontend checks run from **PowerShell** in `apps/desktop`: `corepack pnpm typecheck`, `corepack pnpm lint`, `corepack pnpm build`, `corepack pnpm format` (`pnpm` alone is not on PATH; use `corepack pnpm`).
- `continuum-memory` must NOT depend on llama-cpp, whisper, lancedb, fastembed, or anything with native build steps beyond sqlx's bundled SQLite.
- Vault markdown is user property: no code path may bulk-delete or rewrite vault files except explicit per-note user action. Index/derived data is always rebuildable and disposable.
- Library errors: `thiserror` (`MemoryError`); application errors: `anyhow`. No `.unwrap()` in production paths. Every log event carries `layer = "memory"` and a `component` field. Every public item gets a doc comment.
- Conventional commits with crate scope: `feat(memory): …`, `feat(desktop): …`, `docs(memory): …`.
- Existing MCP tool names/schemas must not change (additive-only; MCP work itself is Plan B).
- Frontend: strict TS (no `any` without eslint-disable + reason), Tailwind tokens only (`bg-*`, `ink-*`, `accent-purple`, `state-*` — **no** hex colors in components; the palette lives in `lib/memoryTheme.ts` as canvas-drawing constants which is the one allowed exception since canvas can't use Tailwind), Prettier-clean.
- Serde casing: all wire types snake_case (match existing chat types). Tauri v2 camelCases *command argument names* only (TS `invoke("memory_get_note", { id })`; multi-word args like `graph filter` are passed as one `filter` object).
- The 10 node types, statuses, config defaults, and thresholds come verbatim from the spec — do not invent others.

---

## File structure (locked)

```
crates/continuum-memory/
├── Cargo.toml
├── src/lib.rs            module docs + re-exports
├── src/error.rs          MemoryError + Result
├── src/model.rs          all wire/data types
├── src/frontmatter.rs    parse/render markdown docs + wiki-link extraction
├── src/slug.rs           slugify + unique_slug
├── src/index.rs          SQLite schema, rebuild/incremental, all queries
├── src/vault.rs          Vault façade: CRUD, resolve, sweep, info, watch glue
├── src/watcher.rs        notify debouncer → tokio channel
├── src/migrate.rs        legacy semantic.sqlite → vault notes
└── tests/
    ├── vault_test.rs     CRUD, atomic writes, resolve, traversal, sweep
    ├── index_test.rs     rebuild determinism, quarantine, queries, perf smoke
    ├── watcher_test.rs   real-notify tempdir integration
    └── migrate_test.rs   legacy fixture, idempotency
apps/desktop/src-tauri/src/memory.rs        MemoryState + 13 commands + watcher bridge
apps/desktop/src/lib/memoryTheme.ts         type palette for canvas
apps/desktop/src/lib/memoryViews.ts         saved-views zustand store
apps/desktop/src/components/memory/MemoryGraph.tsx
apps/desktop/src/components/memory/NotePanel.tsx
apps/desktop/src/components/memory/NoteEditorOverlay.tsx
apps/desktop/src/components/memory/CuratorStack.tsx
apps/desktop/src/components/memory/TimelineStrip.tsx
apps/desktop/src/components/tabs/MemoryTab.tsx   (full rewrite)
docs/memory.md
```

---

### Task 1: Crate scaffold, error type, model types, FTS5 gate

**Files:**
- Modify: `Cargo.toml` (workspace root — add member)
- Create: `crates/continuum-memory/Cargo.toml`
- Create: `crates/continuum-memory/src/lib.rs`
- Create: `crates/continuum-memory/src/error.rs`
- Create: `crates/continuum-memory/src/model.rs`

**Interfaces:**
- Consumes: nothing (first task).
- Produces: every type in `model.rs` below and `MemoryError`/`Result` — later tasks use these names exactly. `NodeType::folder()`, `NodeType::ALL`, `NodeStatus::as_str()`.

- [ ] **Step 1: Workspace member + crate manifest**

Add `"crates/continuum-memory",` to the workspace `members` list in the root `Cargo.toml` (after `"crates/continuum-gateway",`).

`crates/continuum-memory/Cargo.toml`:

```toml
[package]
name = "continuum-memory"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Continuum memory vault: markdown source of truth + derived SQLite index"

[dependencies]
anyhow = { workspace = true }
thiserror = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
serde_yaml = "0.9"
chrono = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite"] }
ulid = "1"
regex = "1"
notify = "6.1"
notify-debouncer-mini = "0.4"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write the failing FTS5 gate test in `src/lib.rs`**

`src/lib.rs`:

```rust
//! # continuum-memory
//!
//! The Continuum memory vault. Markdown files on disk are the source of
//! truth (Obsidian-compatible, user-editable); a derived SQLite index
//! provides fast graph/search queries and the event timeline. Both the
//! runtime (layer 2/memory) and the desktop dashboard link this crate and
//! open the same vault directory — cross-process change propagation is
//! the file-watcher, and every write is atomic (tmp + rename).
//!
//! The index is disposable by design: delete `vault/.continuum/index.db`
//! and it is rebuilt from the markdown on next open.

pub mod error;
pub mod frontmatter;
pub mod index;
pub mod migrate;
pub mod model;
pub mod slug;
pub mod vault;
pub mod watcher;

pub use error::{MemoryError, Result};
pub use migrate::migrate_legacy_semantic;
pub use model::*;
pub use vault::{Vault, VaultOptions};
pub use watcher::VaultWatcher;

#[cfg(test)]
mod fts_gate {
    /// The whole design depends on FTS5 being present in sqlx's bundled
    /// SQLite. If this test fails, STOP and escalate — do not work around
    /// it with LIKE queries (spec: "FTS5 must be verified in the first
    /// implementation task").
    #[tokio::test]
    async fn bundled_sqlite_has_fts5() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        sqlx::query("CREATE VIRTUAL TABLE t USING fts5(body)")
            .execute(&pool)
            .await
            .expect("FTS5 unavailable in bundled SQLite — escalate, do not fall back");
        sqlx::query("INSERT INTO t(body) VALUES ('hello vault world')")
            .execute(&pool)
            .await
            .unwrap();
        let n: (i64,) = sqlx::query_as("SELECT count(*) FROM t WHERE t MATCH 'vault'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n.0, 1);
    }
}
```

For this step create empty placeholder modules so it compiles: `error.rs`, `model.rs` with the real content below, and one-line stubs `frontmatter.rs`/`slug.rs`/`index.rs`/`vault.rs`/`watcher.rs`/`migrate.rs` each containing only `//! (implemented in a later task)` and — where lib.rs re-exports need them — minimal empty items: `vault.rs` needs `pub struct Vault; pub struct VaultOptions;` stubs? **No.** Keep lib.rs exports honest instead: in THIS task, comment out the re-export lines for not-yet-implemented modules and their `pub mod` declarations; each later task uncomments its own line. The committed lib.rs for this task is:

```rust
pub mod error;
pub mod model;
// pub mod frontmatter;   // Task 2
// pub mod slug;          // Task 2
// pub mod index;         // Task 3
// pub mod vault;         // Task 5
// pub mod watcher;       // Task 7
// pub mod migrate;       // Task 8

pub use error::{MemoryError, Result};
pub use model::*;
```

(keep the module doc comment from above).

- [ ] **Step 3: error.rs**

```rust
//! Error type for the memory vault (library errors use thiserror per house rules).

/// Errors produced by the memory vault.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// Filesystem error, annotated with the path involved.
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// A markdown file's frontmatter could not be parsed.
    #[error("frontmatter parse error: {0}")]
    Parse(String),
    /// SQLite/index error.
    #[error("index database error: {0}")]
    Db(#[from] sqlx::Error),
    /// Note id or slug not found.
    #[error("note not found: {0}")]
    NotFound(String),
    /// Caller-supplied input was invalid (bad type, empty title, traversal…).
    #[error("invalid input: {0}")]
    Invalid(String),
    /// File-watcher error.
    #[error("watcher error: {0}")]
    Watch(String),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, MemoryError>;

impl MemoryError {
    /// Short, user-presentable message (dashboard surfaces this string).
    pub fn user_message(&self) -> String {
        match self {
            Self::Io { path, .. } => format!("Could not access {path}"),
            Self::Parse(m) => format!("This note's header is not valid YAML: {m}"),
            Self::Db(_) => "The memory index hit a database error; it will rebuild on restart.".into(),
            Self::NotFound(id) => format!("Memory {id} no longer exists"),
            Self::Invalid(m) => m.clone(),
            Self::Watch(_) => "The vault file-watcher failed; live updates are paused.".into(),
        }
    }

    /// Helper to build an Io error from a path + io::Error.
    pub fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Self::Io { path: path.display().to_string(), source }
    }
}
```

- [ ] **Step 4: model.rs (complete — this is the crate's vocabulary)**

```rust
//! Data types for the memory vault. All wire types serialize snake_case to
//! match the rest of the Continuum IPC surface.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

fn default_score() -> f32 {
    0.5
}
fn default_rel_confidence() -> f32 {
    1.0
}

/// The kind of memory a node represents (Continuum.md §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Project,
    Goal,
    Task,
    Decision,
    Person,
    Preference,
    Fact,
    Error,
    Session,
    Note,
}

impl NodeType {
    /// Vault subdirectory for this type.
    pub fn folder(&self) -> &'static str {
        match self {
            Self::Project => "projects",
            Self::Goal => "goals",
            Self::Task => "tasks",
            Self::Decision => "decisions",
            Self::Person => "people",
            Self::Preference => "preferences",
            Self::Fact => "facts",
            Self::Error => "errors",
            Self::Session => "sessions",
            Self::Note => "notes",
        }
    }

    /// All types, used to create vault folders and validate filters.
    pub const ALL: [NodeType; 10] = [
        Self::Project,
        Self::Goal,
        Self::Task,
        Self::Decision,
        Self::Person,
        Self::Preference,
        Self::Fact,
        Self::Error,
        Self::Session,
        Self::Note,
    ];

    /// snake_case string form (matches serde).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Goal => "goal",
            Self::Task => "task",
            Self::Decision => "decision",
            Self::Person => "person",
            Self::Preference => "preference",
            Self::Fact => "fact",
            Self::Error => "error",
            Self::Session => "session",
            Self::Note => "note",
        }
    }

    /// Parse the snake_case form; `None` for unknown strings.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == s)
    }
}

/// Lifecycle status of a node (spec: candidate → confirmed | rejected |
/// superseded | archived).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Candidate,
    #[default]
    Confirmed,
    Rejected,
    Superseded,
    Archived,
}

impl NodeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Confirmed => "confirmed",
            Self::Rejected => "rejected",
            Self::Superseded => "superseded",
            Self::Archived => "archived",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Candidate,
            Self::Confirmed,
            Self::Rejected,
            Self::Superseded,
            Self::Archived,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }
}

/// How Continuum learned a memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    UserStatement,
    Observed,
    Inferred,
    AgentRun,
    Chat,
    #[default]
    Manual,
}

/// Who may see this memory in generated context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    #[default]
    Internal,
    Sensitive,
}

/// A typed edge declared in frontmatter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    /// Target: slug, title, or node id (resolved by the index).
    pub to: String,
    /// Relation kind (free text; recommended vocabulary in docs/memory.md).
    pub rel: String,
    #[serde(default = "default_rel_confidence")]
    pub confidence: f32,
}

/// Parsed YAML frontmatter of a vault note. Unknown keys are preserved in
/// `extra` and written back verbatim (round-trip safety).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frontmatter {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    pub title: String,
    #[serde(default)]
    pub status: NodeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default = "default_score")]
    pub confidence: f32,
    #[serde(default = "default_score")]
    pub importance: f32,
    #[serde(default)]
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default)]
    pub sensitivity: Sensitivity,
    pub created: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<Relation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Unknown frontmatter keys, preserved on rewrite.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

/// A full note: frontmatter + body + location. `backlinks` is filled by
/// `Vault::get`, empty elsewhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub frontmatter: Frontmatter,
    pub body: String,
    /// Absolute path of the markdown file.
    pub path: PathBuf,
    /// File stem; stable identifier for wiki-links.
    pub slug: String,
    #[serde(default)]
    pub backlinks: Vec<NodeSummary>,
}

/// Input for `Vault::create`. Everything except type + title is optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteDraft {
    #[serde(rename = "type")]
    pub node_type: NodeType,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub status: NodeStatus,
    #[serde(default = "default_score")]
    pub confidence: f32,
    #[serde(default = "default_score")]
    pub importance: f32,
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub source_ref: Option<String>,
    #[serde(default)]
    pub sensitivity: Sensitivity,
    #[serde(default)]
    pub relations: Vec<Relation>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Compact node row used in lists, search results, and backlinks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSummary {
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    pub status: NodeStatus,
    pub project: Option<String>,
    pub confidence: f32,
    pub importance: f32,
    pub source: Source,
    pub sensitivity: Sensitivity,
    pub created: String,
    pub updated: String,
    pub tags: Vec<String>,
    /// First ~160 chars of the body (search/pending lists).
    pub snippet: Option<String>,
}

/// Filter for `Vault::graph`. `statuses: None` means confirmed + candidate
/// (the spec's default view).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphFilter {
    #[serde(default)]
    pub types: Option<Vec<NodeType>>,
    #[serde(default)]
    pub statuses: Option<Vec<NodeStatus>>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub updated_since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    pub status: NodeStatus,
    pub project: Option<String>,
    pub confidence: f32,
    pub importance: f32,
    pub created: String,
    pub updated: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub rel: String,
    pub confidence: f32,
    /// "frontmatter" (typed) or "body" (wiki-link mention).
    pub origin: String,
}

/// An unresolved wiki-link target, rendered hollow in the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhostNode {
    pub target: String,
    pub ref_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub ghosts: Vec<GhostNode>,
    pub truncated: bool,
}

/// How to resolve a candidate note.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Resolution {
    Confirm,
    Reject,
    /// Confirm the candidate and mark `replaces` (a node id) superseded.
    Supersede { replaces: String },
}

/// Timeline event to append (id assigned by the index).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEvent {
    /// Event time; `None` = now.
    #[serde(default)]
    pub ts: Option<DateTime<Utc>>,
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
}

/// Stored timeline event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub ts: String,
    pub kind: String,
    pub text: String,
    pub project: Option<String>,
    pub node_id: Option<String>,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
}

/// Query range for events. `limit` defaults to 500.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventRange {
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineEntry {
    pub path: String,
    pub error: String,
}

/// Vault health/summary for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultInfo {
    pub path: String,
    pub note_count: u64,
    pub counts_by_status: BTreeMap<String, u64>,
    pub quarantined: Vec<QuarantineEntry>,
    pub last_full_index_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexStats {
    pub indexed: u64,
    pub quarantined: u64,
    pub removed: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MigrationReport {
    pub migrated: u64,
    pub skipped: u64,
    pub errors: Vec<String>,
}
```

- [ ] **Step 5: Serde unit tests at the bottom of model.rs**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_type_roundtrip_and_folders() {
        for t in NodeType::ALL {
            assert_eq!(NodeType::parse(t.as_str()), Some(t));
            assert!(!t.folder().is_empty());
        }
        let json = serde_json::to_string(&NodeType::Decision).unwrap();
        assert_eq!(json, "\"decision\"");
    }

    #[test]
    fn resolution_serde_shape() {
        let r: Resolution =
            serde_json::from_str(r#"{"action":"supersede","replaces":"mem_x"}"#).unwrap();
        match r {
            Resolution::Supersede { replaces } => assert_eq!(replaces, "mem_x"),
            _ => panic!("wrong variant"),
        }
        assert_eq!(
            serde_json::to_string(&Resolution::Confirm).unwrap(),
            r#"{"action":"confirm"}"#
        );
    }

    #[test]
    fn frontmatter_defaults_apply() {
        let yaml = "id: mem_1\ntype: fact\ntitle: T\ncreated: 2026-08-01T10:00:00Z\n";
        let fm: Frontmatter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(fm.status, NodeStatus::Confirmed);
        assert_eq!(fm.confidence, 0.5);
        assert_eq!(fm.sensitivity, Sensitivity::Internal);
        assert!(fm.extra.is_empty());
    }

    #[test]
    fn frontmatter_preserves_unknown_keys() {
        let yaml = "id: mem_1\ntype: fact\ntitle: T\ncreated: 2026-08-01T10:00:00Z\ncustom_field: hello\n";
        let fm: Frontmatter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            fm.extra.get("custom_field").and_then(|v| v.as_str()),
            Some("hello")
        );
        let back = serde_yaml::to_string(&fm).unwrap();
        assert!(back.contains("custom_field: hello"));
    }
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p continuum-memory`
Expected: PASS (incl. `bundled_sqlite_has_fts5` — if that one fails, STOP and report BLOCKED).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/continuum-memory
git commit -m "feat(memory): scaffold continuum-memory crate with model types and FTS5 gate"
```

---

### Task 2: Frontmatter parser/renderer, wiki-links, slugs

**Files:**
- Create: `crates/continuum-memory/src/frontmatter.rs`
- Create: `crates/continuum-memory/src/slug.rs`
- Modify: `crates/continuum-memory/src/lib.rs` (uncomment `pub mod frontmatter; pub mod slug;`)

**Interfaces:**
- Consumes: `Frontmatter`, `MemoryError`, `Result` (Task 1).
- Produces: `frontmatter::ParsedDoc { frontmatter, body }`, `frontmatter::parse_document(&str) -> Result<ParsedDoc>`, `frontmatter::render_document(&Frontmatter, &str) -> Result<String>`, `frontmatter::extract_wiki_links(&str) -> Vec<String>`, `slug::slugify(&str) -> String`, `slug::unique_slug(&str, impl Fn(&str) -> bool) -> String`.

- [ ] **Step 1: Write the failing tests (bottom of each new file)**

frontmatter.rs tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\nid: mem_1\ntype: decision\ntitle: Lobby creation must be manual\ncreated: 2026-08-01T21:45:00Z\nrelations:\n- to: sidelife\n  rel: belongs_to\n  confidence: 1.0\n---\nBody with [[SideLife]] and [[ReadyZone.cs|the zone]].\n";

    #[test]
    fn parse_extracts_frontmatter_and_body() {
        let doc = parse_document(DOC).unwrap();
        assert_eq!(doc.frontmatter.id, "mem_1");
        assert_eq!(doc.frontmatter.relations.len(), 1);
        assert!(doc.body.starts_with("Body with"));
    }

    #[test]
    fn roundtrip_preserves_unknown_keys_and_body() {
        let src = "---\nid: mem_2\ntype: fact\ntitle: T\ncreated: 2026-08-01T10:00:00Z\nmystery: 42\n---\nBody €🧠 stays.\n";
        let doc = parse_document(src).unwrap();
        let out = render_document(&doc.frontmatter, &doc.body).unwrap();
        let again = parse_document(&out).unwrap();
        assert_eq!(again.frontmatter.extra.get("mystery").unwrap().as_u64(), Some(42));
        assert_eq!(again.body, doc.body);
    }

    #[test]
    fn parse_accepts_crlf() {
        let src = DOC.replace('\n', "\r\n");
        let doc = parse_document(&src).unwrap();
        assert_eq!(doc.frontmatter.node_type, crate::model::NodeType::Decision);
    }

    #[test]
    fn parse_rejects_missing_fence() {
        assert!(matches!(
            parse_document("no frontmatter here"),
            Err(crate::MemoryError::Parse(_))
        ));
    }

    #[test]
    fn parse_rejects_broken_yaml() {
        let src = "---\nid: [unclosed\n---\nbody\n";
        assert!(matches!(parse_document(src), Err(crate::MemoryError::Parse(_))));
    }

    #[test]
    fn wiki_links_dedup_and_strip_alias() {
        let links = extract_wiki_links("See [[A]] then [[B|alias]] then [[A]] and [[ C ]].");
        assert_eq!(links, vec!["A".to_string(), "B".into(), "C".into()]);
    }
}
```

slug.rs tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("Lobby creation MUST be manual!"), "lobby-creation-must-be-manual");
        assert_eq!(slugify("Café déjà-vu ×3"), "caf-d-j-vu-3");
        assert_eq!(slugify("!!!"), "note");
    }

    #[test]
    fn unique_slug_appends_counter() {
        let existing = ["a", "a-2"];
        let s = unique_slug("a", |c| existing.contains(&c));
        assert_eq!(s, "a-3");
        assert_eq!(unique_slug("fresh", |_| false), "fresh");
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p continuum-memory` → compile errors (functions missing).

- [ ] **Step 3: Implement frontmatter.rs**

```rust
//! Markdown-document parsing: `---` YAML fence + body, plus wiki-link
//! extraction. Round-trip safe: unknown frontmatter keys survive rewrite.

use regex::Regex;
use std::sync::OnceLock;

use crate::error::{MemoryError, Result};
use crate::model::Frontmatter;

/// A parsed vault document.
pub struct ParsedDoc {
    pub frontmatter: Frontmatter,
    pub body: String,
}

/// Parse a full markdown document (frontmatter fence required).
pub fn parse_document(text: &str) -> Result<ParsedDoc> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text
        .strip_prefix("---\r\n")
        .or_else(|| text.strip_prefix("---\n"))
        .ok_or_else(|| MemoryError::Parse("missing opening --- fence".into()))?;
    // Find the closing fence on its own line.
    let mut yaml_end = None;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            yaml_end = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let (yaml_to, body_from) =
        yaml_end.ok_or_else(|| MemoryError::Parse("missing closing --- fence".into()))?;
    let yaml = &rest[..yaml_to];
    let body = rest[body_from..].to_string();
    let frontmatter: Frontmatter =
        serde_yaml::from_str(yaml).map_err(|e| MemoryError::Parse(e.to_string()))?;
    Ok(ParsedDoc { frontmatter, body })
}

/// Render frontmatter + body back to a document string.
pub fn render_document(fm: &Frontmatter, body: &str) -> Result<String> {
    let yaml = serde_yaml::to_string(fm).map_err(|e| MemoryError::Parse(e.to_string()))?;
    Ok(format!("---\n{yaml}---\n{body}"))
}

/// Extract `[[wiki-link]]` targets from a body: order-preserving, deduped,
/// `|alias` stripped, whitespace trimmed.
pub fn extract_wiki_links(body: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\[\[([^\[\]\n]+)\]\]").expect("static regex"));
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for cap in re.captures_iter(body) {
        let raw = &cap[1];
        let target = raw.split('|').next().unwrap_or(raw).trim();
        if !target.is_empty() && seen.insert(target.to_lowercase()) {
            out.push(target.to_string());
        }
    }
    out
}
```

- [ ] **Step 4: Implement slug.rs**

```rust
//! Slug generation: lowercase ASCII alnum + '-', stable after creation.

/// Turn a title into a filesystem slug. Falls back to "note" when nothing
/// survives filtering.
pub fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut last_dash = true; // suppress leading dash
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "note".to_string()
    } else {
        trimmed
    }
}

/// Append `-2`, `-3`, … until `exists` returns false.
pub fn unique_slug(base: &str, exists: impl Fn(&str) -> bool) -> String {
    if !exists(base) {
        return base.to_string();
    }
    for n in 2u32.. {
        let candidate = format!("{base}-{n}");
        if !exists(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 exhausted")
}
```

Note the `Café` test expectation: non-ASCII letters are dropped (é → dash), consecutive dashes collapse — `caf-d-j-vu-3`.

- [ ] **Step 5: Run tests** — `cargo test -p continuum-memory` → PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/continuum-memory
git commit -m "feat(memory): frontmatter parser, wiki-link extraction, slugs"
```

---

### Task 3: Index — schema, full rebuild, incremental reindex, quarantine

**Files:**
- Create: `crates/continuum-memory/src/index.rs`
- Create: `crates/continuum-memory/tests/index_test.rs`
- Modify: `crates/continuum-memory/src/lib.rs` (uncomment `pub mod index;`)

**Interfaces:**
- Consumes: Task 1 types, Task 2 `parse_document`/`extract_wiki_links`.
- Produces (used by vault.rs in Task 5 — exact signatures):
  - `pub struct Index { … }` with `pub async fn open(db_path: &Path) -> Result<Index>`
  - `pub async fn index_file(&self, vault_dir: &Path, path: &Path) -> Result<IndexOutcome>` where `pub enum IndexOutcome { Indexed(String /*id*/), Quarantined, Removed }`
  - `pub async fn remove_path(&self, vault_dir: &Path, path: &Path) -> Result<()>`
  - `pub async fn rebuild(&self, vault_dir: &Path) -> Result<IndexStats>`
  - `pub async fn recompute_edges(&self) -> Result<()>`
  - `pub fn pool(&self) -> &sqlx::SqlitePool`
- Storage invariant: paths are stored **relative to the vault dir** with forward slashes (portable, and lets tests compare).

- [ ] **Step 1: Write failing integration tests** (`tests/index_test.rs`)

```rust
use std::fs;
use std::path::Path;

use continuum_memory::index::{Index, IndexOutcome};

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn note(id: &str, ty: &str, title: &str, body: &str) -> String {
    format!(
        "---\nid: {id}\ntype: {ty}\ntitle: {title}\ncreated: 2026-08-01T10:00:00Z\n---\n{body}\n"
    )
}

async fn open_index(dir: &Path) -> Index {
    fs::create_dir_all(dir.join(".continuum")).unwrap();
    Index::open(&dir.join(".continuum/index.db")).await.unwrap()
}

#[tokio::test]
async fn rebuild_indexes_notes_and_quarantines_broken() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "facts/alpha.md", &note("mem_a", "fact", "Alpha", "links [[Beta]]"));
    write(tmp.path(), "facts/beta.md", &note("mem_b", "fact", "Beta", "no links"));
    write(tmp.path(), "facts/broken.md", "---\nid: [oops\n---\nbody");
    write(tmp.path(), "facts/ignore.tmp", "not markdown");
    let idx = open_index(tmp.path()).await;
    let stats = idx.rebuild(tmp.path()).await.unwrap();
    assert_eq!(stats.indexed, 2);
    assert_eq!(stats.quarantined, 1);
    let n: (i64,) = sqlx::query_as("SELECT count(*) FROM nodes")
        .fetch_one(idx.pool()).await.unwrap();
    assert_eq!(n.0, 2);
    // edge alpha -> beta resolved via title
    let e: (i64,) = sqlx::query_as("SELECT count(*) FROM edges WHERE rel='mentions'")
        .fetch_one(idx.pool()).await.unwrap();
    assert_eq!(e.0, 1);
}

#[tokio::test]
async fn rebuild_is_deterministic() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..20 {
        write(
            tmp.path(),
            &format!("notes/n{i}.md"),
            &note(&format!("mem_{i}"), "note", &format!("Note {i}"), "links [[Note 1]] [[Ghost]]"),
        );
    }
    let idx = open_index(tmp.path()).await;
    idx.rebuild(tmp.path()).await.unwrap();
    let rows1: Vec<(String, String)> =
        sqlx::query_as("SELECT from_id, to_id FROM edges ORDER BY from_id, to_id")
            .fetch_all(idx.pool()).await.unwrap();
    idx.rebuild(tmp.path()).await.unwrap();
    let rows2: Vec<(String, String)> =
        sqlx::query_as("SELECT from_id, to_id FROM edges ORDER BY from_id, to_id")
            .fetch_all(idx.pool()).await.unwrap();
    assert_eq!(rows1, rows2);
    let g: (i64,) = sqlx::query_as("SELECT count(*) FROM unresolved_links WHERE target='Ghost'")
        .fetch_one(idx.pool()).await.unwrap();
    assert!(g.0 >= 1);
}

#[tokio::test]
async fn incremental_matches_full_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "facts/a.md", &note("mem_a", "fact", "A", "see [[B]]"));
    let idx = open_index(tmp.path()).await;
    idx.rebuild(tmp.path()).await.unwrap();
    // B appears later — incremental index must resolve A's ghost link.
    write(tmp.path(), "facts/b.md", &note("mem_b", "fact", "B", ""));
    let out = idx.index_file(tmp.path(), &tmp.path().join("facts/b.md")).await.unwrap();
    assert!(matches!(out, IndexOutcome::Indexed(_)));
    let e: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM edges WHERE from_id='mem_a' AND to_id='mem_b'")
        .fetch_one(idx.pool()).await.unwrap();
    assert_eq!(e.0, 1);
    // deleting B turns the edge back into a ghost
    std::fs::remove_file(tmp.path().join("facts/b.md")).unwrap();
    idx.remove_path(tmp.path(), &tmp.path().join("facts/b.md")).await.unwrap();
    let e: (i64,) = sqlx::query_as("SELECT count(*) FROM edges").fetch_one(idx.pool()).await.unwrap();
    assert_eq!(e.0, 0);
    let g: (i64,) = sqlx::query_as("SELECT count(*) FROM unresolved_links")
        .fetch_one(idx.pool()).await.unwrap();
    assert_eq!(g.0, 1);
}

#[tokio::test]
async fn perf_smoke_1000_notes() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..1000 {
        write(
            tmp.path(),
            &format!("notes/n{i}.md"),
            &note(&format!("mem_{i}"), "note", &format!("Note {i}"), "body [[Note 0]]"),
        );
    }
    let idx = open_index(tmp.path()).await;
    let t0 = std::time::Instant::now();
    let stats = idx.rebuild(tmp.path()).await.unwrap();
    assert_eq!(stats.indexed, 1000);
    assert!(t0.elapsed().as_secs() < 5, "rebuild took {:?}", t0.elapsed());
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p continuum-memory --test index_test` → compile error (module missing).

- [ ] **Step 3: Implement index.rs**

Key implementation requirements (write real code for each):

```rust
//! Derived SQLite index over the vault. Disposable and rebuildable; the
//! markdown files are the source of truth. Layer: memory.

use std::collections::HashMap;
use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::error::{MemoryError, Result};
use crate::frontmatter::{extract_wiki_links, parse_document};
use crate::model::IndexStats;

/// Bump when the schema changes; mismatch triggers drop + recreate.
const SCHEMA_VERSION: &str = "1";

pub struct Index {
    pool: SqlitePool,
}

pub enum IndexOutcome {
    Indexed(String),
    Quarantined,
    Removed,
}
```

- `open`: `SqliteConnectOptions::new().filename(db_path).create_if_missing(true).journal_mode(Wal).busy_timeout(Duration::from_secs(5))`, pool max 4. Run `create_schema`. Read `meta.schema_version`; on mismatch run `reset_schema` (DROP TABLE IF EXISTS nodes, links, edges, unresolved_links, nodes_fts, events, quarantine, meta → recreate → store version). **`events` and `quarantine` survive rebuilds but not schema resets** (rebuild() must NOT touch the events table).
- `create_schema` executes (one statement per `sqlx::query`):

```sql
CREATE TABLE IF NOT EXISTS nodes(
  id TEXT PRIMARY KEY, slug TEXT UNIQUE, path TEXT UNIQUE, type TEXT NOT NULL,
  title TEXT NOT NULL, status TEXT NOT NULL, project TEXT,
  confidence REAL NOT NULL, importance REAL NOT NULL, source TEXT NOT NULL,
  sensitivity TEXT NOT NULL, created TEXT NOT NULL, updated TEXT NOT NULL,
  last_used TEXT, expires TEXT, supersedes TEXT, superseded_by TEXT,
  tags_json TEXT NOT NULL DEFAULT '[]', mtime INTEGER NOT NULL DEFAULT 0,
  body_hash TEXT NOT NULL DEFAULT '', snippet TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS links(
  from_id TEXT NOT NULL, target TEXT NOT NULL, rel TEXT NOT NULL,
  confidence REAL NOT NULL, origin TEXT NOT NULL,
  PRIMARY KEY(from_id, target, rel, origin));
CREATE TABLE IF NOT EXISTS edges(
  from_id TEXT NOT NULL, to_id TEXT NOT NULL, rel TEXT NOT NULL,
  confidence REAL NOT NULL, origin TEXT NOT NULL,
  PRIMARY KEY(from_id, to_id, rel, origin));
CREATE TABLE IF NOT EXISTS unresolved_links(
  from_id TEXT NOT NULL, target TEXT NOT NULL, PRIMARY KEY(from_id, target));
CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(node_id UNINDEXED, title, body, tags);
CREATE TABLE IF NOT EXISTS events(
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, kind TEXT NOT NULL,
  text TEXT NOT NULL, project TEXT, node_id TEXT, ref TEXT);
CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
CREATE TABLE IF NOT EXISTS quarantine(path TEXT PRIMARY KEY, error TEXT NOT NULL, mtime INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

(`ref` must be written as `"ref"` — quoted — in every SQL statement touching it; it is not a reserved word in SQLite but quoting keeps intent clear.)
- `rel_path(vault_dir, path) -> String`: strip prefix, `\\` → `/`. Files outside `vault_dir` → `MemoryError::Invalid`.
- `index_file`: skip non-`.md` and anything whose rel path starts with `.continuum/` (return Removed? — no: return `Ok(IndexOutcome::Removed)` only for deletions; for skips return `Quarantined`? Neither — add a fourth variant `Skipped` to `IndexOutcome` and handle it in callers by ignoring). Read file (`fs::read_to_string`); on missing file → delegate to `remove_path` → `Removed`. On parse error → upsert `quarantine` row, delete any `nodes/links/nodes_fts` rows for that path, → `Quarantined`. On success: delete quarantine row; FNV-1a hash of body (inline helper `fn fnv1a(s: &str) -> String` — 64-bit, hex); upsert node row (`INSERT ... ON CONFLICT(id) DO UPDATE`, but **also** handle path-uniqueness: `DELETE FROM nodes WHERE path=? AND id<>?` first so a rewritten file with a new id can't collide); slug = file stem lowercase; `updated` defaults to `created` when frontmatter `updated` is None; snippet = first 160 chars of body (char-boundary safe: `body.chars().take(160).collect()`), whitespace-collapsed; replace `links` rows for the node: frontmatter relations → `(from_id, to, rel, confidence, 'frontmatter')`, body wiki-links → `(from_id, target, 'mentions', 1.0, 'body')`; replace `nodes_fts` rows (`DELETE FROM nodes_fts WHERE node_id=?` then INSERT with title/body/tags-joined); then `recompute_edges().await`; → `Indexed(id)`.
- `remove_path`: delete nodes/links/nodes_fts/quarantine rows for rel path, then `recompute_edges`.
- `recompute_edges`: load `links` and `nodes(id, slug, lower(title))` into memory; resolve each link target by priority **slug (lowercased target) → title (case-insensitive) → id**; skip self-edges; write within one transaction: `DELETE FROM edges; DELETE FROM unresolved_links;` then bulk INSERT resolved + unresolved (unresolved deduped per (from_id, target)). Pure resolution logic in a free function for testability:

```rust
pub(crate) struct RawLink { pub from_id: String, pub target: String, pub rel: String, pub confidence: f64, pub origin: String }
pub(crate) fn resolve_links(
    links: &[RawLink],
    by_slug: &HashMap<String, String>,   // slug -> id
    by_title: &HashMap<String, String>,  // lower(title) -> id
) -> (Vec<(String, String, String, f64, String)>, Vec<(String, String)>)
```
- `rebuild`: `DELETE FROM nodes; DELETE FROM links; DELETE FROM nodes_fts; DELETE FROM unresolved_links; DELETE FROM edges; DELETE FROM quarantine;` then walk the vault dir recursively (`std::fs::read_dir`, skip `.continuum`, skip non-md, skip `.tmp`), `index_file` each **but defer `recompute_edges` to a single call at the end** (add a private `index_file_inner(&self, …, recompute: bool)`), set `meta.last_full_index_at = now RFC3339`, return stats. Use `tracing::info!(layer = "memory", component = "index", indexed, quarantined, "vault reindexed")`.

- [ ] **Step 4: Run tests** — `cargo test -p continuum-memory` → PASS (all 4 index tests + earlier tasks).

- [ ] **Step 5: Commit**

```bash
git add crates/continuum-memory
git commit -m "feat(memory): sqlite index with rebuild, incremental reindex, quarantine"
```

---

### Task 4: Index queries — search, graph, neighbors, pending, backlinks

**Files:**
- Modify: `crates/continuum-memory/src/index.rs` (add query methods + `fts_query` helper)
- Modify: `crates/continuum-memory/tests/index_test.rs` (add query tests)

**Interfaces:**
- Consumes: Task 3 `Index`.
- Produces (vault.rs delegates to these — exact signatures):
  - `pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<NodeSummary>>`
  - `pub async fn graph(&self, f: &GraphFilter, default_limit: u32) -> Result<GraphData>`
  - `pub async fn neighbors(&self, id: &str, depth: u8, default_limit: u32) -> Result<GraphData>`
  - `pub async fn pending(&self) -> Result<Vec<NodeSummary>>` (status = candidate, oldest first)
  - `pub async fn backlinks(&self, id: &str) -> Result<Vec<NodeSummary>>`
  - `pub async fn get_node_path(&self, id: &str) -> Result<Option<String>>` (rel path)
  - `pub async fn find_by_slug_or_title(&self, needle: &str) -> Result<Option<String>>` (id)
  - `pub async fn counts(&self) -> Result<(u64, std::collections::BTreeMap<String, u64>)>`
  - `pub async fn quarantined(&self) -> Result<Vec<QuarantineEntry>>`
  - `pub(crate) fn fts_query(user: &str) -> String`

- [ ] **Step 1: Write failing tests** (append to `tests/index_test.rs`)

```rust
#[tokio::test]
async fn search_graph_pending_backlinks() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "projects/sidelife.md", &note("mem_p", "project", "SideLife", "the game"));
    write(
        tmp.path(),
        "decisions/lobby.md",
        "---\nid: mem_d\ntype: decision\ntitle: Manual lobby\nstatus: confirmed\nimportance: 0.9\ncreated: 2026-08-01T10:00:00Z\nrelations:\n- to: sidelife\n  rel: belongs_to\n---\nLobbies are [[SideLife]] manual.\n",
    );
    write(
        tmp.path(),
        "facts/cand.md",
        "---\nid: mem_c\ntype: fact\ntitle: Pnpm preferred\nstatus: candidate\ncreated: 2026-08-02T10:00:00Z\n---\nuser prefers pnpm\n",
    );
    let idx = open_index(tmp.path()).await;
    idx.rebuild(tmp.path()).await.unwrap();

    // FTS search hits body text
    let hits = idx.search("pnpm", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "mem_c");
    assert!(hits[0].snippet.as_deref().unwrap_or_default().contains("pnpm"));

    // graph default statuses = confirmed + candidate
    let g = idx.graph(&continuum_memory::GraphFilter::default(), 1500).await.unwrap();
    assert_eq!(g.nodes.len(), 3);
    // typed edge + mention edge both present between d and p
    assert_eq!(g.edges.iter().filter(|e| e.from == "mem_d" && e.to == "mem_p").count(), 2);

    // type filter
    let g2 = idx
        .graph(&continuum_memory::GraphFilter {
            types: Some(vec![continuum_memory::NodeType::Decision]),
            ..Default::default()
        }, 1500)
        .await
        .unwrap();
    assert_eq!(g2.nodes.len(), 1);

    // cap + truncated flag: limit 2 keeps the 2 highest-importance nodes
    let g3 = idx
        .graph(&continuum_memory::GraphFilter { limit: Some(2), ..Default::default() }, 1500)
        .await
        .unwrap();
    assert_eq!(g3.nodes.len(), 2);
    assert!(g3.truncated);

    let pend = idx.pending().await.unwrap();
    assert_eq!(pend.len(), 1);
    assert_eq!(pend[0].id, "mem_c");

    let back = idx.backlinks("mem_p").await.unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].id, "mem_d");

    assert_eq!(idx.find_by_slug_or_title("sidelife").await.unwrap(), Some("mem_p".into()));
    assert_eq!(idx.find_by_slug_or_title("SIDELIFE").await.unwrap(), Some("mem_p".into()));
    assert_eq!(idx.find_by_slug_or_title("nope").await.unwrap(), None);
}

#[tokio::test]
async fn neighbors_bfs_depth() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "notes/a.md", &note("mem_a", "note", "A", "[[B]]"));
    write(tmp.path(), "notes/b.md", &note("mem_b", "note", "B", "[[C]]"));
    write(tmp.path(), "notes/c.md", &note("mem_c", "note", "C", ""));
    let idx = open_index(tmp.path()).await;
    idx.rebuild(tmp.path()).await.unwrap();
    let g1 = idx.neighbors("mem_a", 1, 1500).await.unwrap();
    assert_eq!(g1.nodes.len(), 2); // a + b
    let g2 = idx.neighbors("mem_a", 2, 1500).await.unwrap();
    assert_eq!(g2.nodes.len(), 3);
}

#[test]
fn fts_query_sanitizes() {
    assert_eq!(continuum_memory::index::fts_query("hello world"), "\"hello\"* \"world\"*");
    assert_eq!(continuum_memory::index::fts_query("a-b (c)"), "\"ab\"* \"c\"*");
    assert_eq!(continuum_memory::index::fts_query("  "), "");
}
```

(make `fts_query` `pub` for the test — it is harmless API.)

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement.** Notes beyond the obvious:

- `fts_query`: split whitespace, keep alphanumeric chars per token, drop empties, wrap `"tok"*`, join with spaces; empty input → empty string, and `search` returns `Ok(vec![])` for empty query strings.
- `search`: `SELECT node_id FROM nodes_fts WHERE nodes_fts MATCH ?1 ORDER BY rank LIMIT ?2`, then hydrate `NodeSummary` rows from `nodes` (single query with `IN`, preserve FTS order in Rust).
- Row→`NodeSummary` mapping helper `fn summary_from_row(row: &SqliteRow) -> NodeSummary` (parse type/status/source/sensitivity with the `parse` helpers, defaulting to `Note`/`Confirmed`/`Manual`/`Internal` on unknown — log a `warn!` once per call site).
- `graph`: build SQL dynamically with a `QueryBuilder`; statuses default `('confirmed','candidate')`; when `query` is non-empty first run FTS to get an id set and add `id IN (…)`; `ORDER BY importance DESC, id ASC LIMIT ?+1`; pop the extra row → `truncated=true`. Edges: `SELECT * FROM edges WHERE from_id IN set AND to_id IN set`. Ghosts: `SELECT target, count(*) FROM unresolved_links WHERE from_id IN set GROUP BY target ORDER BY count(*) DESC LIMIT 100`.
- `neighbors`: BFS in Rust over `edges` (both directions), collect ids up to depth (1 or 2), then reuse the node/edge hydration of `graph` with an explicit id set.
- `counts`: `SELECT count(*) FROM nodes` + `SELECT status, count(*) FROM nodes GROUP BY status`.

- [ ] **Step 4: Run tests** — PASS. Also run `cargo clippy -p continuum-memory --all-targets -- -D warnings` and fix.

- [ ] **Step 5: Commit**

```bash
git add crates/continuum-memory
git commit -m "feat(memory): index queries — fts search, graph, neighbors, pending, backlinks"
```

---

### Task 5: Vault façade — CRUD, atomic writes, candidate resolution, sweep, info

**Files:**
- Create: `crates/continuum-memory/src/vault.rs`
- Create: `crates/continuum-memory/tests/vault_test.rs`
- Modify: `crates/continuum-memory/src/lib.rs` (uncomment `pub mod vault;` + re-export)

**Interfaces:**
- Consumes: Tasks 1–4.
- Produces (desktop + Plan B runtime use exactly these):

```rust
pub struct VaultOptions { pub watcher_debounce_ms: u64, pub graph_max_nodes: u32 } // Default: 500, 1500
pub struct Vault { /* dir: PathBuf, index: Index, opts: VaultOptions */ }
impl Vault {
    pub async fn open(dir: &Path) -> Result<Vault>;                    // defaults
    pub async fn open_with(dir: &Path, opts: VaultOptions) -> Result<Vault>; // creates folders, opens index, full rebuild
    pub fn dir(&self) -> &Path;
    pub async fn rebuild_index(&self) -> Result<IndexStats>;
    pub async fn reindex_paths(&self, paths: &[PathBuf]) -> Result<Vec<String>>; // changed node ids
    pub async fn create(&self, draft: NoteDraft) -> Result<Note>;
    pub async fn get(&self, id: &str) -> Result<Note>;                 // + backlinks
    pub async fn save(&self, note: &Note) -> Result<()>;               // by id; path from index
    pub async fn delete(&self, id: &str) -> Result<()>;
    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<NodeSummary>>;
    pub async fn graph(&self, f: &GraphFilter) -> Result<GraphData>;
    pub async fn neighbors(&self, id: &str, depth: u8) -> Result<GraphData>;
    pub async fn pending(&self) -> Result<Vec<NodeSummary>>;
    pub async fn resolve_candidate(&self, id: &str, r: Resolution) -> Result<()>;
    pub async fn append_event(&self, e: NewEvent) -> Result<()>;
    pub async fn events(&self, range: &EventRange) -> Result<Vec<Event>>;
    pub async fn prune_events(&self, keep_days: u32) -> Result<u64>;
    pub async fn info(&self) -> Result<VaultInfo>;
    pub async fn touch_last_used(&self, ids: &[String]) -> Result<()>;
    pub async fn sweep_expired(&self) -> Result<u64>;
    pub fn watch(&self) -> Result<VaultWatcher>;                       // Task 7
}
```

(`append_event`/`events`/`prune_events` land in Task 6; `watch` in Task 7 — declare them in those tasks, not here.)

- [ ] **Step 1: Write failing tests** (`tests/vault_test.rs`)

```rust
use continuum_memory::{NodeStatus, NodeType, NoteDraft, Resolution, Vault};

fn draft(ty: NodeType, title: &str, body: &str) -> NoteDraft {
    NoteDraft {
        node_type: ty,
        title: title.into(),
        body: body.into(),
        project: None,
        status: NodeStatus::Confirmed,
        confidence: 0.5,
        importance: 0.5,
        source: Default::default(),
        source_ref: None,
        sensitivity: Default::default(),
        relations: vec![],
        tags: vec![],
    }
}

#[tokio::test]
async fn create_get_save_delete_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let note = vault.create(draft(NodeType::Decision, "Manual lobby", "Body [[Ghost]]")).await.unwrap();
    assert!(note.frontmatter.id.starts_with("mem_"));
    assert_eq!(note.slug, "manual-lobby");
    assert!(tmp.path().join("decisions/manual-lobby.md").exists());
    // no stray tmp files
    assert!(std::fs::read_dir(tmp.path().join("decisions")).unwrap()
        .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));

    let mut got = vault.get(&note.frontmatter.id).await.unwrap();
    assert_eq!(got.frontmatter.title, "Manual lobby");
    got.body = "New body".into();
    got.frontmatter.importance = 0.9;
    vault.save(&got).await.unwrap();
    let again = vault.get(&note.frontmatter.id).await.unwrap();
    assert_eq!(again.body.trim(), "New body");
    assert!(again.frontmatter.updated.is_some());

    vault.delete(&note.frontmatter.id).await.unwrap();
    assert!(!tmp.path().join("decisions/manual-lobby.md").exists());
    assert!(vault.get(&note.frontmatter.id).await.is_err());
}

#[tokio::test]
async fn slug_collision_appends_counter() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let a = vault.create(draft(NodeType::Fact, "Same Title", "")).await.unwrap();
    let b = vault.create(draft(NodeType::Fact, "Same Title", "")).await.unwrap();
    assert_eq!(a.slug, "same-title");
    assert_eq!(b.slug, "same-title-2");
}

#[tokio::test]
async fn backlinks_are_populated_on_get() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let target = vault.create(draft(NodeType::Project, "SideLife", "")).await.unwrap();
    vault.create(draft(NodeType::Decision, "D1", "about [[SideLife]]")).await.unwrap();
    let got = vault.get(&target.frontmatter.id).await.unwrap();
    assert_eq!(got.backlinks.len(), 1);
    assert_eq!(got.backlinks[0].title, "D1");
}

#[tokio::test]
async fn resolve_candidate_confirm_reject_supersede() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let old = vault.create(draft(NodeType::Decision, "Use MongoDB", "")).await.unwrap();
    let mut cand = draft(NodeType::Decision, "Use PostgreSQL", "");
    cand.status = NodeStatus::Candidate;
    let new = vault.create(cand).await.unwrap();

    vault
        .resolve_candidate(&new.frontmatter.id, Resolution::Supersede { replaces: old.frontmatter.id.clone() })
        .await
        .unwrap();
    let new2 = vault.get(&new.frontmatter.id).await.unwrap();
    let old2 = vault.get(&old.frontmatter.id).await.unwrap();
    assert_eq!(new2.frontmatter.status, NodeStatus::Confirmed);
    assert_eq!(new2.frontmatter.supersedes.as_deref(), Some(old.frontmatter.id.as_str()));
    assert_eq!(old2.frontmatter.status, NodeStatus::Superseded);
    assert_eq!(old2.frontmatter.superseded_by.as_deref(), Some(new.frontmatter.id.as_str()));

    // reject path
    let mut c2 = draft(NodeType::Fact, "Maybe wrong", "");
    c2.status = NodeStatus::Candidate;
    let c2 = vault.create(c2).await.unwrap();
    vault.resolve_candidate(&c2.frontmatter.id, Resolution::Reject).await.unwrap();
    assert_eq!(vault.get(&c2.frontmatter.id).await.unwrap().frontmatter.status, NodeStatus::Rejected);
    // resolving a non-candidate errors
    assert!(vault.resolve_candidate(&old.frontmatter.id, Resolution::Confirm).await.is_err());
}

#[tokio::test]
async fn sweep_expired_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let n = vault.create(draft(NodeType::Fact, "Old", "")).await.unwrap();
    let mut note = vault.get(&n.frontmatter.id).await.unwrap();
    note.frontmatter.expires = Some(chrono::Utc::now() - chrono::Duration::days(1));
    vault.save(&note).await.unwrap();
    assert_eq!(vault.sweep_expired().await.unwrap(), 1);
    assert_eq!(vault.get(&n.frontmatter.id).await.unwrap().frontmatter.status, NodeStatus::Archived);
}

#[tokio::test]
async fn traversal_and_bad_input_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let mut d = draft(NodeType::Fact, "..", "");
    d.title = "../../escape".into();
    let n = vault.create(d).await.unwrap();
    // slugify strips the dots — file stays inside the vault
    assert!(n.path.starts_with(tmp.path()));
    assert!(vault.get("mem_does_not_exist").await.is_err());
    let mut e = draft(NodeType::Fact, "", "");
    e.title = "   ".into();
    assert!(vault.create(e).await.is_err());
}

#[tokio::test]
async fn info_reports_counts_and_quarantine() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    vault.create(draft(NodeType::Fact, "Ok", "")).await.unwrap();
    std::fs::write(tmp.path().join("facts/broken.md"), "---\nid: [x\n---\n").unwrap();
    vault.rebuild_index().await.unwrap();
    let info = vault.info().await.unwrap();
    assert_eq!(info.note_count, 1);
    assert_eq!(info.quarantined.len(), 1);
    assert_eq!(info.counts_by_status.get("confirmed"), Some(&1));
}
```

- [ ] **Step 2: Run to verify failure.**

- [ ] **Step 3: Implement vault.rs.** Key points:

- `open_with`: canonicalize? No — store dir as given (canonicalize breaks UNC paths on Windows); create all `NodeType::ALL` folders + `.continuum`; remove stray `*.tmp` recursively; `Index::open(dir/.continuum/index.db)`; always `rebuild()` (spec decision: full rebuild at open; the 1 000-note smoke keeps this honest).
- ID generation: `format!("mem_{}", ulid::Ulid::new().to_string().to_lowercase())`.
- `create`: validate title non-blank (`MemoryError::Invalid("title must not be empty")`); slug via `slug::slugify` + `slug::unique_slug` checking `nodes` table (`SELECT 1 FROM nodes WHERE slug=?`) **and** file existence in the type folder; build `Frontmatter` (created = now, updated = None); `atomic_write(path, render_document(...))`; `index.index_file(...)`; return the note via `self.get(id)` minus backlinks shortcut (just build it directly).
- `atomic_write(path, content)`: write `path.with_extension("md.tmp")`, `std::fs::rename` over target (std rename replaces on Windows), map errors with `MemoryError::io`.
- `save`: look up rel path by id (`index.get_node_path`), `NotFound` if absent; set `frontmatter.updated = Some(now)`; render + atomic write to the **existing** path (folder never changes even if `node_type` changed — folder is convention, frontmatter is truth); reindex file.
- `get`: path by id → read file → parse → attach `index.backlinks(id)`.
- `delete`: path by id → `fs::remove_file` → `index.remove_path`.
- `resolve_candidate`: load note; `Invalid` unless `status == Candidate`; apply per `Resolution` (supersede loads + edits the partner too; partner `NotFound` → error, nothing written); save via the same atomic path (each file its own write; frontmatter edits only).
- `sweep_expired`: `SELECT id FROM nodes WHERE expires IS NOT NULL AND expires < now AND status IN ('confirmed','candidate')` → for each: load, set `Archived`, save. Returns count.
- `touch_last_used`: for each id: load, set `last_used = Some(now)`, save (skip `NotFound` silently — logs `debug!`).
- `info`: counts + quarantined + `meta.last_full_index_at` + dir display string.
- `graph`/`neighbors` pass `self.opts.graph_max_nodes` as the default limit.

- [ ] **Step 4: Run tests** — `cargo test -p continuum-memory` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/continuum-memory
git commit -m "feat(memory): vault facade — atomic CRUD, candidate resolution, sweep, info"
```

---

### Task 6: Events API

**Files:**
- Modify: `crates/continuum-memory/src/vault.rs` (add `append_event`, `events`, `prune_events`)
- Modify: `crates/continuum-memory/tests/vault_test.rs` (add tests)

**Interfaces:**
- Consumes: `events` table (Task 3), `NewEvent`/`Event`/`EventRange` (Task 1).
- Produces: the three methods with signatures from Task 5's block. Timestamps stored RFC3339 UTC; `events` returns ascending by ts; `limit` default 500.

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test]
async fn events_append_query_prune() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let old = chrono::Utc::now() - chrono::Duration::days(40);
    vault.append_event(continuum_memory::NewEvent {
        ts: Some(old), kind: "build".into(), text: "old build".into(),
        project: None, node_id: None, reference: None,
    }).await.unwrap();
    vault.append_event(continuum_memory::NewEvent {
        ts: None, kind: "error".into(), text: "fresh error".into(),
        project: Some("sidelife".into()), node_id: None, reference: None,
    }).await.unwrap();

    let all = vault.events(&continuum_memory::EventRange::default()).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].text, "old build"); // ascending

    let recent = vault.events(&continuum_memory::EventRange {
        since: Some(chrono::Utc::now() - chrono::Duration::days(1)),
        until: None, limit: None,
    }).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].kind, "error");

    assert_eq!(vault.prune_events(30).await.unwrap(), 1);
    assert_eq!(vault.events(&Default::default()).await.unwrap().len(), 1);
}
```

- [ ] **Step 2: Verify failure. Step 3: Implement** (straight SQL on `index.pool()`; `prune_events` = `DELETE FROM events WHERE ts < ?` with cutoff `now - keep_days`). **Step 4: Run — PASS.**

- [ ] **Step 5: Commit** — `git commit -m "feat(memory): timeline events api with retention"`

---

### Task 7: File-watcher

**Files:**
- Create: `crates/continuum-memory/src/watcher.rs`
- Create: `crates/continuum-memory/tests/watcher_test.rs`
- Modify: `crates/continuum-memory/src/vault.rs` (add `watch()`), `src/lib.rs` (uncomment module + re-export)

**Interfaces:**
- Consumes: Vault (Task 5), `notify`/`notify-debouncer-mini`.
- Produces:

```rust
pub struct VaultWatcher {
    /// Batches of changed absolute paths (already filtered to vault .md files).
    pub rx: tokio::sync::mpsc::UnboundedReceiver<Vec<std::path::PathBuf>>,
    // keeps the debouncer (and its thread) alive
    _debouncer: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
}
impl Vault { pub fn watch(&self) -> Result<VaultWatcher>; }
```

Consumers loop `while let Some(paths) = w.rx.recv().await { vault.reindex_paths(&paths).await; }`.

- [ ] **Step 1: Failing integration test** (`tests/watcher_test.rs`)

```rust
use continuum_memory::Vault;

#[tokio::test]
async fn watcher_reports_external_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let mut w = vault.watch().unwrap();

    std::fs::write(
        tmp.path().join("facts/external.md"),
        "---\nid: mem_ext\ntype: fact\ntitle: External\ncreated: 2026-08-01T10:00:00Z\n---\nwritten outside\n",
    )
    .unwrap();

    // Debounce default is 500 ms; wait up to 10 s for the batch.
    let paths = tokio::time::timeout(std::time::Duration::from_secs(10), w.rx.recv())
        .await
        .expect("watcher timed out")
        .expect("channel closed");
    assert!(paths.iter().any(|p| p.ends_with("external.md")));

    let ids = vault.reindex_paths(&paths).await.unwrap();
    assert!(ids.contains(&"mem_ext".to_string()));
    assert!(vault.get("mem_ext").await.is_ok());
}

#[tokio::test]
async fn watcher_ignores_index_db_churn() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let mut w = vault.watch().unwrap();
    // Touch a file inside .continuum — must NOT produce a batch.
    std::fs::write(tmp.path().join(".continuum/scratch.txt"), "x").unwrap();
    let res = tokio::time::timeout(std::time::Duration::from_millis(1500), w.rx.recv()).await;
    assert!(res.is_err(), "expected no event for .continuum writes");
}
```

- [ ] **Step 2: Verify failure. Step 3: Implement watcher.rs**

```rust
//! Debounced file-watcher: external edits (Obsidian, editors, the other
//! Continuum process) surface as batches of changed paths.

use std::path::PathBuf;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};

use crate::error::{MemoryError, Result};

pub struct VaultWatcher {
    pub rx: tokio::sync::mpsc::UnboundedReceiver<Vec<PathBuf>>,
    _debouncer: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
}

pub(crate) fn spawn(dir: &std::path::Path, debounce_ms: u64) -> Result<VaultWatcher> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let root = dir.to_path_buf();
    let mut debouncer = new_debouncer(Duration::from_millis(debounce_ms), move |res| {
        if let Ok(events) = res {
            let paths: Vec<PathBuf> = events
                .into_iter()
                .filter(|e| matches!(e.kind, DebouncedEventKind::Any))
                .map(|e| e.path)
                .filter(|p| {
                    p.extension().map(|x| x == "md").unwrap_or(false)
                        && !p
                            .strip_prefix(&root)
                            .map(|r| r.starts_with(".continuum"))
                            .unwrap_or(true)
                })
                .collect();
            if !paths.is_empty() {
                let _ = tx.send(paths);
            }
        }
    })
    .map_err(|e| MemoryError::Watch(e.to_string()))?;
    debouncer
        .watcher()
        .watch(dir, RecursiveMode::Recursive)
        .map_err(|e| MemoryError::Watch(e.to_string()))?;
    Ok(VaultWatcher { rx, _debouncer: debouncer })
}
```

`Vault::watch` calls `watcher::spawn(self.dir(), self.opts.watcher_debounce_ms)`. `Vault::reindex_paths` iterates `index.index_file` per path (missing file → `remove_path`), returns indexed ids, logs one `info!` per batch.

- [ ] **Step 4: Run tests** — the two watcher tests can be timing-sensitive on CI; they use generous timeouts. PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat(memory): debounced vault file-watcher"`

---

### Task 8: Legacy migration

**Files:**
- Create: `crates/continuum-memory/src/migrate.rs`
- Create: `crates/continuum-memory/tests/migrate_test.rs`
- Modify: `src/lib.rs` (uncomment module + re-export)

**Interfaces:**
- Consumes: Vault (Task 5), sqlx.
- Produces: `pub async fn migrate_legacy_semantic(vault: &Vault, semantic_db: &Path) -> Result<MigrationReport>`.

- [ ] **Step 1: Failing test** (`tests/migrate_test.rs`) — build a legacy fixture db with sqlx in the test:

```rust
use continuum_memory::{migrate_legacy_semantic, Vault};

async fn make_legacy(path: &std::path::Path) {
    let url = format!("sqlite:{}?mode=rwc", path.display());
    let pool = sqlx::sqlite::SqlitePoolOptions::new().connect(&url).await.unwrap();
    sqlx::query("CREATE TABLE semantic_facts(key TEXT PRIMARY KEY, value TEXT NOT NULL, confidence REAL NOT NULL DEFAULT 0.5, source TEXT NOT NULL DEFAULT 'observed', source_frame_id TEXT, updated_at TEXT NOT NULL)")
        .execute(&pool).await.unwrap();
    sqlx::query("CREATE TABLE semantic_edges(from_key TEXT NOT NULL, to_key TEXT NOT NULL, relation TEXT NOT NULL, PRIMARY KEY(from_key,to_key,relation))")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO semantic_facts VALUES ('user.name', '\"Toshan\"', 1.0, 'user_stated', NULL, '2026-07-01T10:00:00Z')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO semantic_facts VALUES ('project.sidelife.stack', '\"Unity\"', 0.8, 'observed', NULL, '2026-07-02T10:00:00Z')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO semantic_edges VALUES ('user.name', 'project.sidelife.stack', 'works_on')")
        .execute(&pool).await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn migrates_facts_and_edges_idempotently() {
    let tmp = tempfile::tempdir().unwrap();
    let legacy = tmp.path().join("semantic.sqlite");
    make_legacy(&legacy).await;
    let vault_dir = tmp.path().join("vault");
    std::fs::create_dir_all(&vault_dir).unwrap();
    let vault = Vault::open(&vault_dir).await.unwrap();

    let report = migrate_legacy_semantic(&vault, &legacy).await.unwrap();
    assert_eq!(report.migrated, 2);
    assert_eq!(report.skipped, 0);
    assert!(report.errors.is_empty());

    let hits = vault.search("Toshan", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "user: name");
    // source mapped user_stated -> user_statement, confidence carried
    assert!(matches!(hits[0].source, continuum_memory::Source::UserStatement));
    assert_eq!(hits[0].confidence, 1.0);

    // edge became a typed relation
    let g = vault.graph(&Default::default()).await.unwrap();
    assert_eq!(g.edges.iter().filter(|e| e.rel == "works_on").count(), 1);

    // second run skips everything
    let again = migrate_legacy_semantic(&vault, &legacy).await.unwrap();
    assert_eq!(again.migrated, 0);
    assert_eq!(again.skipped, 2);
}

#[tokio::test]
async fn missing_legacy_db_is_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(&tmp.path().join("v")).await.unwrap();
    assert!(migrate_legacy_semantic(&vault, &tmp.path().join("nope.sqlite")).await.is_err());
}
```

- [ ] **Step 2: Verify failure. Step 3: Implement migrate.rs**

- Error `Invalid` when the db file does not exist. Open read-only (`SqliteConnectOptions::new().filename(db).read_only(true)`).
- Title mapping: `key.replacen('.', ": ", 1)` (`user.name` → `user: name`); tag = first `.`-segment; body: if `value` parses as a JSON string take the inner string, else the raw value; source map `user_stated→UserStatement, observed→Observed, inferred→Inferred`, unknown → `Observed`; `created`/`updated` from `updated_at` (fallback now on parse failure — count as error string but still migrate with now); `type: fact`, `status: confirmed`, `source_ref: Some("legacy:semantic:<key>")`.
- Idempotency: before creating, `vault` lookup by exact title (case-insensitive) via `index.find_by_slug_or_title(&title)` → skip if `Some`.
- Edges pass 2: map legacy key → new node id (only for keys migrated **or already present** by title); for each edge with both endpoints mapped: load the from-note, append `Relation { to: <to-slug>, rel, confidence: 0.5 }` if an identical relation isn't already there, save. Edge whose endpoints are missing → count into `errors` as `"edge user.name→x skipped: target not migrated"`.
- The legacy DB is never written to.

- [ ] **Step 4: Run — PASS. Step 5: Commit** — `git commit -m "feat(memory): legacy semantic.sqlite to vault migration"`

Also run the full crate gate now: `cargo test -p continuum-memory && cargo clippy -p continuum-memory --all-targets -- -D warnings && cargo fmt --all -- --check`.

---

### Task 9: Config section (continuum-core)

**Files:**
- Modify: `crates/continuum-core/src/config.rs` (MemoryConfig + two new structs + defaults + tests)

**Interfaces:**
- Consumes: existing `MemoryConfig` (fields distillation_*), `continuum_dev_dir()`.
- Produces (desktop Task 10 + Plan B use): `MemoryConfig.vault: MemoryVaultConfig` and `MemoryConfig.curator: CuratorConfig`;
  `MemoryVaultConfig { vault_dir: String, watcher_debounce_ms: u64, events_retention_days: u32, graph_max_nodes: u32 }` with `resolve_vault_dir(&self, base: &Path) -> PathBuf`;
  `CuratorConfig { enabled: bool, interval_minutes: u64, max_candidates_per_pass: u32, auto_confirm_threshold: f32, discard_floor: f32, claude_batch: u32, session_summary_idle_minutes: u64, wake_vault_notes_max: u32, include_sensitive_in_context: bool }`.

- [ ] **Step 1: Failing test** (append to config.rs's existing `#[cfg(test)] mod`):

```rust
#[test]
fn memory_vault_defaults_and_toml_nesting() {
    let cfg = ContinuumConfig::default();
    assert_eq!(cfg.memory.vault.watcher_debounce_ms, 500);
    assert_eq!(cfg.memory.vault.events_retention_days, 30);
    assert_eq!(cfg.memory.vault.graph_max_nodes, 1500);
    assert!(cfg.memory.vault.vault_dir.is_empty());
    assert!(cfg.memory.curator.enabled);
    assert_eq!(cfg.memory.curator.interval_minutes, 10);
    assert_eq!(cfg.memory.curator.max_candidates_per_pass, 3);
    assert_eq!(cfg.memory.curator.auto_confirm_threshold, 0.85);
    assert_eq!(cfg.memory.curator.discard_floor, 0.4);
    assert_eq!(cfg.memory.curator.claude_batch, 10);
    assert_eq!(cfg.memory.curator.session_summary_idle_minutes, 20);
    assert_eq!(cfg.memory.curator.wake_vault_notes_max, 8);
    assert!(!cfg.memory.curator.include_sensitive_in_context);

    let parsed: ContinuumConfig = toml::from_str(
        "[memory.vault]\nvault_dir = \"D:/x\"\n[memory.curator]\nenabled = false\n",
    ).unwrap();
    assert_eq!(parsed.memory.vault.vault_dir, "D:/x");
    assert!(!parsed.memory.curator.enabled);
    assert_eq!(parsed.memory.vault.watcher_debounce_ms, 500); // default backfill

    let base = std::path::Path::new("/tmp/base");
    assert_eq!(
        ContinuumConfig::default().memory.vault.resolve_vault_dir(base),
        base.join("vault")
    );
}
```

(If config.rs has no `toml` dev-dependency in continuum-core, check `Cargo.toml` — `toml` is already a dependency for config load; if only in `[dependencies]` that's fine for unit tests too.)

- [ ] **Step 2: Verify failure** — `cargo test -p continuum-core --no-default-features --lib memory_vault_defaults` → FAIL (fields missing).

- [ ] **Step 3: Implement** — add both structs (doc comments per house style, `#[serde(default)]` at struct level, `Default` impls with the spec values), add to `MemoryConfig`:

```rust
    /// Vault storage settings (Obsidian-like markdown memory).
    #[serde(default)]
    pub vault: MemoryVaultConfig,
    /// Curator pipeline settings (Plan B consumes these; configurable now
    /// per non-negotiable #3).
    #[serde(default)]
    pub curator: CuratorConfig,
```

`resolve_vault_dir`: empty → `base.join("vault")`, else `PathBuf::from(&self.vault_dir)`.

- [ ] **Step 4: Run** — `cargo test -p continuum-core --no-default-features --lib` → PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat(core): memory vault + curator config sections"`

---

### Task 10: Desktop backend — MemoryState, 13 commands, watcher bridge, stub removal, health probe

**Files:**
- Create: `apps/desktop/src-tauri/src/memory.rs`
- Modify: `apps/desktop/src-tauri/Cargo.toml` (add `continuum-memory = { path = "../../../crates/continuum-memory" }`)
- Modify: `apps/desktop/src-tauri/src/main.rs` (mod, manage, register commands, spawn bridge; **remove** registrations of `search_episodic`, `delete_episodic`, `list_semantic`, `set_semantic`, `delete_semantic`)
- Modify: `apps/desktop/src-tauri/src/commands.rs` (delete those 5 command fns + `SemanticFact`; re-scope `wipe_memory` doc comment: derived data only, vault untouched)
- Modify: `apps/desktop/src-tauri/src/components.rs` (add `memory_vault` probe)

**Interfaces:**
- Consumes: `continuum_memory::{Vault, VaultOptions, …}` (Tasks 5–8), `runtime.config_snapshot().memory.vault` + `runtime.dev_dir()` (Task 9), Tauri `AppHandle::emit` pattern from `chat.rs` (topic string constant style).
- Produces (frontend Task 11 calls these — names are wire contract):
  commands `memory_graph(filter)`, `memory_search(query, limit)`, `memory_get_note(id)`, `memory_create_note(draft)`, `memory_save_note(note)`, `memory_delete_note(id)`, `memory_resolve_candidate(id, resolution)`, `memory_pending()`, `memory_events(range)`, `memory_vault_info()`, `memory_migrate_legacy()`, `memory_rebuild_index()`, `memory_open_vault()`; event topic `continuum:memory` with payload `{ kind: "changed" | "rebuilt", ids: string[] }`; `memory_vault_info` returns `VaultInfoDto = VaultInfo + { legacy_semantic_present: bool }`.

- [ ] **Step 1: Write failing command-layer tests** (bottom of new `memory.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    async fn state() -> (tempfile::TempDir, MemoryState) {
        let tmp = tempfile::tempdir().unwrap();
        let s = MemoryState::new(tmp.path().join("vault"), tmp.path().join("semantic.sqlite"));
        s.vault().await.unwrap(); // force init
        (tmp, s)
    }

    #[tokio::test]
    async fn create_and_graph_roundtrip() {
        let (_tmp, s) = state().await;
        let note = create_note_inner(
            &s,
            continuum_memory::NoteDraft {
                node_type: continuum_memory::NodeType::Fact,
                title: "T".into(),
                body: "b".into(),
                project: None,
                status: Default::default(),
                confidence: 0.5,
                importance: 0.5,
                source: Default::default(),
                source_ref: None,
                sensitivity: Default::default(),
                relations: vec![],
                tags: vec![],
            },
        )
        .await
        .unwrap();
        let g = graph_inner(&s, Default::default()).await.unwrap();
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, note.frontmatter.id);
    }

    #[tokio::test]
    async fn vault_info_reports_legacy_flag() {
        let (tmp, s) = state().await;
        let info = vault_info_inner(&s).await.unwrap();
        assert!(!info.legacy_semantic_present);
        std::fs::write(tmp.path().join("semantic.sqlite"), b"x").unwrap();
        let info = vault_info_inner(&s).await.unwrap();
        assert!(info.legacy_semantic_present);
    }

    #[tokio::test]
    async fn error_maps_to_user_string() {
        let (_tmp, s) = state().await;
        let err = get_note_inner(&s, "mem_missing").await.unwrap_err();
        assert!(err.contains("mem_missing"));
    }
}
```

Pattern: every `#[tauri::command]` is a thin wrapper over a testable `*_inner(state, …)` free function (same style the chat commands use — check `chat.rs` and mirror it).

- [ ] **Step 2: Verify failure**, then **Step 3: implement memory.rs**:

```rust
//! Memory vault state + Tauri commands for the dashboard. The vault opens
//! lazily (first command) so a broken vault degrades the Memory tab, not
//! the whole app. Layer: memory, component: dashboard.

use std::path::PathBuf;
use std::sync::Arc;

use continuum_memory::{
    EventRange, GraphData, GraphFilter, MigrationReport, NodeSummary, Note, NoteDraft,
    Resolution, Vault, VaultOptions,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// Topic for live vault-change pushes to the frontend.
pub const MEMORY_EVENT_TOPIC: &str = "continuum:memory";

#[derive(Clone, Serialize)]
pub struct MemoryEventPayload {
    pub kind: String, // "changed" | "rebuilt"
    pub ids: Vec<String>,
}

#[derive(Serialize)]
pub struct VaultInfoDto {
    #[serde(flatten)]
    pub info: continuum_memory::VaultInfo,
    pub legacy_semantic_present: bool,
}

pub struct MemoryState {
    vault_dir: PathBuf,
    legacy_semantic_db: PathBuf,
    vault: tokio::sync::OnceCell<Arc<Vault>>,
    pub opts: VaultOptions,
}

impl MemoryState {
    pub fn new(vault_dir: PathBuf, legacy_semantic_db: PathBuf) -> Self {
        Self {
            vault_dir,
            legacy_semantic_db,
            vault: tokio::sync::OnceCell::new(),
            opts: VaultOptions::default(),
        }
    }
    /// Override options from config (main.rs).
    pub fn with_opts(mut self, opts: VaultOptions) -> Self {
        self.opts = opts;
        self
    }
    pub async fn vault(&self) -> Result<Arc<Vault>, String> {
        self.vault
            .get_or_try_init(|| async {
                Vault::open_with(&self.vault_dir, self.opts.clone())
                    .await
                    .map(Arc::new)
            })
            .await
            .cloned()
            .map_err(|e| e.user_message())
    }
}
```

(`VaultOptions` needs `Clone` — add `#[derive(Debug, Clone)]` in Task 5 if not already.)

Inner functions (all `pub(crate) async fn …_inner(state: &MemoryState, …) -> Result<T, String>` mapping `MemoryError` via `.map_err(|e| e.user_message())` — except `NotFound`, where include the id: use `format!("{}", e.user_message())` which already carries it):

- `graph_inner(filter)` → `vault.graph(&filter)`
- `search_inner(query, limit)` → `vault.search(&query, limit.unwrap_or(20))`
- `get_note_inner(id)`, `create_note_inner(draft)`, `save_note_inner(note)` (validate `note.frontmatter.id` non-empty), `delete_note_inner(id)`
- `resolve_inner(id, resolution)` → `vault.resolve_candidate`
- `pending_inner()`, `events_inner(range)`
- `vault_info_inner()` → `vault.info()` + `legacy_semantic_present: state.legacy_semantic_db.exists()`
- `migrate_inner()` → `continuum_memory::migrate_legacy_semantic(&vault, &state.legacy_semantic_db)`
- `rebuild_inner()` → `vault.rebuild_index()` (returns `IndexStats`)

`#[tauri::command]` wrappers: `memory_graph(state: State<'_, Arc<MemoryState>>, filter: GraphFilter)` etc. — one per command listed in Interfaces. `memory_open_vault(state)` uses `tauri_plugin_opener::OpenerExt`: `app.opener().open_path(dir.to_string_lossy(), None::<&str>)` — mirror however `chat.rs`/`commands.rs` already opens paths; if nothing does, use the plugin's `open_path`.

Watcher bridge (called from main.rs after builder setup, same place chat spawns its tasks):

```rust
pub fn spawn_watcher_bridge(app: AppHandle, state: Arc<MemoryState>) {
    tauri::async_runtime::spawn(async move {
        let vault = match state.vault().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(layer = "memory", component = "dashboard", error = %e,
                    "vault unavailable; watcher bridge not started");
                return;
            }
        };
        let mut watcher = match vault.watch() {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(layer = "memory", component = "dashboard", error = %e.user_message(),
                    "vault watcher failed to start");
                return;
            }
        };
        // Announce initial index state.
        let _ = app.emit(MEMORY_EVENT_TOPIC, MemoryEventPayload { kind: "rebuilt".into(), ids: vec![] });
        while let Some(paths) = watcher.rx.recv().await {
            match vault.reindex_paths(&paths).await {
                Ok(ids) => {
                    let _ = app.emit(MEMORY_EVENT_TOPIC, MemoryEventPayload { kind: "changed".into(), ids });
                }
                Err(e) => tracing::warn!(layer = "memory", component = "dashboard",
                    error = %e.user_message(), "reindex after watch event failed"),
            }
        }
    });
}
```

main.rs wiring:

```rust
let cfg = runtime.config_snapshot();
let vault_dir = cfg.memory.vault.resolve_vault_dir(&dev_dir);
let memory_state = Arc::new(
    memory::MemoryState::new(vault_dir, dev_dir.join("semantic.sqlite")).with_opts(
        continuum_memory::VaultOptions {
            watcher_debounce_ms: cfg.memory.vault.watcher_debounce_ms,
            graph_max_nodes: cfg.memory.vault.graph_max_nodes,
        },
    ),
);
```

`.manage(memory_state.clone())`, register the 13 commands, and in `.setup()` (or right after `.run` builder — mirror where chat/events spawn) call `memory::spawn_watcher_bridge(app_handle, memory_state)`.

commands.rs: delete the 5 stub fns + `SemanticFact` struct; update `wipe_memory`'s doc comment to state it never touches vault markdown.

components.rs: add a `memory_vault` `ComponentProbe` following the existing pattern in that file — healthy when `MemoryState::vault()` succeeds and quarantine is empty; degrading when quarantine non-empty (message lists count); error when vault open fails. (Look at how existing probes get their data — components.rs registers closures over state; pass the `Arc<MemoryState>` in from main.rs via `components::register_default`'s call site or a new `components::register_memory(&health, memory_state)` helper — choose the latter to avoid touching the existing signature.)

- [ ] **Step 4: Run** — `cargo test -p continuum-desktop` → PASS; `cargo clippy -p continuum-desktop --all-targets -- -D warnings`.

- [ ] **Step 5: Commit**

```bash
git add apps/desktop/src-tauri Cargo.lock
git commit -m "feat(desktop): memory vault state, 13 commands, watcher bridge; drop legacy memory stubs"
```

---

### Task 11: Frontend plumbing — types, wrappers, theme, saved views

**Files:**
- Modify: `apps/desktop/src/lib/types.ts` (remove `SemanticFact`; add memory types)
- Modify: `apps/desktop/src/lib/tauri.ts` (remove `searchEpisodic`, `deleteEpisodic`, `listSemantic`, `setSemantic`, `deleteSemantic`; add memory API + `onMemoryEvent`)
- Create: `apps/desktop/src/lib/memoryTheme.ts`
- Create: `apps/desktop/src/lib/memoryViews.ts`

**Interfaces:**
- Consumes: Task 10 command names/payloads (snake_case fields; camelCase invoke arg names).
- Produces (components in Tasks 12–14 import these): types `MemoryNodeType`, `MemoryNodeStatus`, `MemoryRelation`, `MemoryFrontmatter`, `MemoryNote`, `MemoryNoteDraft`, `MemoryNodeSummary`, `MemoryGraphFilter`, `MemoryGraphNode`, `MemoryGraphEdge`, `MemoryGhostNode`, `MemoryGraphData`, `MemoryEvent`, `MemoryEventRange`, `MemoryVaultInfo`, `MemoryMigrationReport`, `MemoryIndexStats`, `MemoryResolution`, `MemoryEventPayload`; API object `continuum.memoryGraph(filter)`, `.memorySearch(q, limit)`, `.memoryGetNote(id)`, `.memoryCreateNote(draft)`, `.memorySaveNote(note)`, `.memoryDeleteNote(id)`, `.memoryResolveCandidate(id, resolution)`, `.memoryPending()`, `.memoryEvents(range)`, `.memoryVaultInfo()`, `.memoryMigrateLegacy()`, `.memoryRebuildIndex()`, `.memoryOpenVault()`, plus `onMemoryEvent(handler): Promise<() => void>`; `NODE_COLORS: Record<MemoryNodeType, string>` and `NODE_TYPE_LABELS` in memoryTheme.ts; `useMemoryViews` zustand store `{ views: SavedView[], addView(name, filter), removeView(name) }` with `SavedView = { name: string; filter: MemoryGraphFilter }`.

- [ ] **Step 1: types.ts additions** (exact):

```ts
// --- Memory vault ---

export type MemoryNodeType =
  | "project"
  | "goal"
  | "task"
  | "decision"
  | "person"
  | "preference"
  | "fact"
  | "error"
  | "session"
  | "note";

export type MemoryNodeStatus = "candidate" | "confirmed" | "rejected" | "superseded" | "archived";
export type MemorySource =
  | "user_statement"
  | "observed"
  | "inferred"
  | "agent_run"
  | "chat"
  | "manual";
export type MemorySensitivity = "public" | "internal" | "sensitive";

export interface MemoryRelation {
  to: string;
  rel: string;
  confidence: number;
}

export interface MemoryFrontmatter {
  id: string;
  type: MemoryNodeType;
  title: string;
  status: MemoryNodeStatus;
  project?: string | null;
  confidence: number;
  importance: number;
  source: MemorySource;
  source_ref?: string | null;
  sensitivity: MemorySensitivity;
  created: string;
  updated?: string | null;
  last_used?: string | null;
  expires?: string | null;
  supersedes?: string | null;
  superseded_by?: string | null;
  relations?: MemoryRelation[];
  tags?: string[];
}

export interface MemoryNodeSummary {
  id: string;
  slug: string;
  title: string;
  type: MemoryNodeType;
  status: MemoryNodeStatus;
  project: string | null;
  confidence: number;
  importance: number;
  source: MemorySource;
  sensitivity: MemorySensitivity;
  created: string;
  updated: string;
  tags: string[];
  snippet: string | null;
}

export interface MemoryNote {
  frontmatter: MemoryFrontmatter;
  body: string;
  path: string;
  slug: string;
  backlinks: MemoryNodeSummary[];
}

export interface MemoryNoteDraft {
  type: MemoryNodeType;
  title: string;
  body?: string;
  project?: string | null;
  status?: MemoryNodeStatus;
  confidence?: number;
  importance?: number;
  source?: MemorySource;
  source_ref?: string | null;
  sensitivity?: MemorySensitivity;
  relations?: MemoryRelation[];
  tags?: string[];
}

export interface MemoryGraphFilter {
  types?: MemoryNodeType[] | null;
  statuses?: MemoryNodeStatus[] | null;
  project?: string | null;
  query?: string | null;
  updated_since?: string | null;
  updated_until?: string | null;
  limit?: number | null;
}

export interface MemoryGraphNode {
  id: string;
  slug: string;
  title: string;
  type: MemoryNodeType;
  status: MemoryNodeStatus;
  project: string | null;
  confidence: number;
  importance: number;
  created: string;
  updated: string;
}

export interface MemoryGraphEdge {
  from: string;
  to: string;
  rel: string;
  confidence: number;
  origin: "frontmatter" | "body";
}

export interface MemoryGhostNode {
  target: string;
  ref_count: number;
}

export interface MemoryGraphData {
  nodes: MemoryGraphNode[];
  edges: MemoryGraphEdge[];
  ghosts: MemoryGhostNode[];
  truncated: boolean;
}

export type MemoryResolution =
  | { action: "confirm" }
  | { action: "reject" }
  | { action: "supersede"; replaces: string };

export interface MemoryEvent {
  id: number;
  ts: string;
  kind: string;
  text: string;
  project: string | null;
  node_id: string | null;
  ref: string | null;
}

export interface MemoryEventRange {
  since?: string | null;
  until?: string | null;
  limit?: number | null;
}

export interface MemoryQuarantineEntry {
  path: string;
  error: string;
}

export interface MemoryVaultInfo {
  path: string;
  note_count: number;
  counts_by_status: Record<string, number>;
  quarantined: MemoryQuarantineEntry[];
  last_full_index_at: string | null;
  legacy_semantic_present: boolean;
}

export interface MemoryIndexStats {
  indexed: number;
  quarantined: number;
  removed: number;
}

export interface MemoryMigrationReport {
  migrated: number;
  skipped: number;
  errors: string[];
}

export interface MemoryEventPayload {
  kind: "changed" | "rebuilt";
  ids: string[];
}
```

Remove `SemanticFact` and fix the imports in `tauri.ts` (nothing else imports it after the tab rewrite; verify with grep).

- [ ] **Step 2: tauri.ts** — follow the existing `invoke<T>("cmd", args, fallback)` helper pattern in that file exactly (same graceful non-Tauri fallbacks): fallbacks are `{nodes:[],edges:[],ghosts:[],truncated:false}` for graph, `[]` for lists, `null`-rejections for note ops (throw `new Error("Not available outside Tauri")` like other write ops do — mirror whatever `chat_send_message`'s wrapper does), and a no-op unsubscribe for `onMemoryEvent`. `memoryResolveCandidate(id, resolution)` invokes `memory_resolve_candidate` with `{ id, resolution }`.

- [ ] **Step 3: memoryTheme.ts**

```ts
// Canvas palette for the memory graph. The one file allowed to hold raw hex:
// canvas 2D can't consume Tailwind classes. Keep in sync with tailwind tokens.
import type { MemoryNodeType } from "./types";

export const NODE_COLORS: Record<MemoryNodeType, string> = {
  project: "#7c5cff",
  goal: "#5cc8ff",
  task: "#ffd166",
  decision: "#c792ea",
  person: "#7ee787",
  preference: "#64d8cb",
  fact: "#8ab4ff",
  error: "#ff7b72",
  session: "#9a9ab0",
  note: "#c9c9d9",
};

export const NODE_TYPE_LABELS: Record<MemoryNodeType, string> = {
  project: "Project",
  goal: "Goal",
  task: "Task",
  decision: "Decision",
  person: "Person",
  preference: "Preference",
  fact: "Fact",
  error: "Error",
  session: "Session",
  note: "Note",
};

export const GHOST_COLOR = "#55556e";
export const EDGE_COLOR = "rgba(120,120,150,0.25)";
export const LABEL_COLOR = "#b8b8d0";
```

- [ ] **Step 4: memoryViews.ts** — zustand + `persist` middleware (`localStorage`, key `continuum-memory-views`), shape from Interfaces. ~25 lines.

- [ ] **Step 5: Verify** — PowerShell in `apps/desktop`: `corepack pnpm typecheck` (MemoryTab still references removed wrappers → **expected to fail** until Task 12 rewrites it; to keep this task green, do the MemoryTab stub swap now: replace MemoryTab.tsx's entire content with a temporary `export function MemoryTab() { return null; }` — Task 12 replaces it for real). Re-run typecheck → PASS. `corepack pnpm lint` → PASS.

- [ ] **Step 6: Commit** — `git commit -m "feat(desktop): memory vault frontend plumbing — types, api wrappers, theme, saved views"`

---

### Task 12: MemoryGraph component + MemoryTab shell (topbar, filters, legend)

**Files:**
- Modify: `apps/desktop/package.json` (PowerShell: `corepack pnpm add force-graph`)
- Create: `apps/desktop/src/components/memory/MemoryGraph.tsx`
- Modify: `apps/desktop/src/components/tabs/MemoryTab.tsx` (real shell: topbar + graph + legend + quarantine chip + truncated notice)

**Interfaces:**
- Consumes: Task 11 types/API/theme.
- Produces: `MemoryGraph` props contract used by Tasks 13–14:

```ts
interface MemoryGraphProps {
  data: MemoryGraphData;
  selectedId: string | null;
  dimIds?: Set<string> | null;      // timeline scrub: non-members render at 15% alpha
  onSelect(id: string | null): void; // null = background click
  onExpand(id: string): void;        // double-click → overlay editor
  onGhostClick(target: string): void;
}
```

MemoryTab owns all data fetching/state; child components are presentational + callbacks. Internal tab state produced here and extended in 13/14: `filter: MemoryGraphFilter`, `graph`, `selectedId`, `vaultInfo`, plus `refresh()` which refetches graph+info (debounced 300 ms after `onMemoryEvent`).

- [ ] **Step 1: Add dependency** — PowerShell: `cd apps/desktop; corepack pnpm add force-graph` (v1.x lands in package.json).

- [ ] **Step 2: MemoryGraph.tsx** (complete):

```tsx
"use client";

import { useEffect, useRef } from "react";
import type { MemoryGraphData } from "@/lib/types";
import { EDGE_COLOR, GHOST_COLOR, LABEL_COLOR, NODE_COLORS } from "@/lib/memoryTheme";

interface GraphNodeObj {
  id: string;
  label: string;
  color: string;
  radius: number;
  ghost: boolean;
  status: string;
  x?: number;
  y?: number;
  fx?: number;
  fy?: number;
}

interface MemoryGraphProps {
  data: MemoryGraphData;
  selectedId: string | null;
  dimIds?: Set<string> | null;
  onSelect: (id: string | null) => void;
  onExpand: (id: string) => void;
  onGhostClick: (target: string) => void;
}

// force-graph has no bundled types; minimal structural typing.
type ForceGraphInstance = {
  (el: HTMLElement): ForceGraphInstance;
  graphData(d: { nodes: GraphNodeObj[]; links: { source: string; target: string }[] }): ForceGraphInstance;
  nodeId(k: string): ForceGraphInstance;
  nodeCanvasObject(fn: (node: GraphNodeObj, ctx: CanvasRenderingContext2D, scale: number) => void): ForceGraphInstance;
  nodePointerAreaPaint(fn: (node: GraphNodeObj, color: string, ctx: CanvasRenderingContext2D) => void): ForceGraphInstance;
  linkColor(fn: () => string): ForceGraphInstance;
  linkWidth(n: number): ForceGraphInstance;
  onNodeClick(fn: (node: GraphNodeObj) => void): ForceGraphInstance;
  onNodeDragEnd(fn: (node: GraphNodeObj) => void): ForceGraphInstance;
  onBackgroundClick(fn: () => void): ForceGraphInstance;
  width(n: number): ForceGraphInstance;
  height(n: number): ForceGraphInstance;
  autoPauseRedraw(b: boolean): ForceGraphInstance;
  backgroundColor(c: string): ForceGraphInstance;
  centerAt(x?: number, y?: number, ms?: number): ForceGraphInstance;
  zoom(k?: number, ms?: number): ForceGraphInstance;
  _destructor?: () => void;
};

export function MemoryGraph({ data, selectedId, dimIds, onSelect, onExpand, onGhostClick }: MemoryGraphProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const graphRef = useRef<ForceGraphInstance | null>(null);
  const lastClick = useRef<{ id: string; at: number }>({ id: "", at: 0 });
  const propsRef = useRef({ selectedId, dimIds, onSelect, onExpand, onGhostClick });
  propsRef.current = { selectedId, dimIds, onSelect, onExpand, onGhostClick };

  // init once
  useEffect(() => {
    let disposed = false;
    void import("force-graph").then((mod) => {
      if (disposed || !containerRef.current) return;
      const ForceGraph = mod.default as unknown as () => ForceGraphInstance;
      const g = ForceGraph()(containerRef.current)
        .nodeId("id")
        .autoPauseRedraw(false) // candidate pulse needs continuous redraw
        .backgroundColor("rgba(0,0,0,0)")
        .linkColor(() => EDGE_COLOR)
        .linkWidth(1)
        .nodeCanvasObject((node, ctx, scale) => {
          const { selectedId: sel, dimIds: dims } = propsRef.current;
          const dimmed = dims && dims.size > 0 && !dims.has(node.id);
          const faded = node.status === "superseded" || node.status === "archived";
          ctx.globalAlpha = dimmed ? 0.15 : faded ? 0.4 : 1;
          const r = node.radius;
          ctx.beginPath();
          ctx.arc(node.x ?? 0, node.y ?? 0, r, 0, 2 * Math.PI);
          if (node.ghost) {
            ctx.strokeStyle = GHOST_COLOR;
            ctx.lineWidth = 1.5;
            ctx.stroke();
          } else {
            ctx.fillStyle = node.color;
            ctx.fill();
          }
          if (node.status === "candidate") {
            const pulse = 0.5 + 0.5 * Math.sin(Date.now() / 300);
            ctx.beginPath();
            ctx.arc(node.x ?? 0, node.y ?? 0, r + 2 + pulse * 2, 0, 2 * Math.PI);
            ctx.strokeStyle = node.color;
            ctx.globalAlpha = (dimmed ? 0.15 : 0.5) * pulse;
            ctx.stroke();
            ctx.globalAlpha = dimmed ? 0.15 : 1;
          }
          if (node.id === sel) {
            ctx.beginPath();
            ctx.arc(node.x ?? 0, node.y ?? 0, r + 3, 0, 2 * Math.PI);
            ctx.strokeStyle = "#ffffff";
            ctx.lineWidth = 1;
            ctx.stroke();
          }
          if (scale > 1.4 || node.id === sel) {
            ctx.font = `${Math.max(10 / scale, 2)}px sans-serif`;
            ctx.textAlign = "center";
            ctx.fillStyle = LABEL_COLOR;
            ctx.fillText(node.label, node.x ?? 0, (node.y ?? 0) + r + 8 / scale);
          }
          ctx.globalAlpha = 1;
        })
        .nodePointerAreaPaint((node, color, ctx) => {
          ctx.fillStyle = color;
          ctx.beginPath();
          ctx.arc(node.x ?? 0, node.y ?? 0, node.radius + 4, 0, 2 * Math.PI);
          ctx.fill();
        })
        .onNodeClick((node) => {
          const p = propsRef.current;
          if (node.ghost) {
            p.onGhostClick(node.label);
            return;
          }
          const now = Date.now();
          if (lastClick.current.id === node.id && now - lastClick.current.at < 300) {
            p.onExpand(node.id);
          } else {
            p.onSelect(node.id);
          }
          lastClick.current = { id: node.id, at: now };
        })
        .onNodeDragEnd((node) => {
          node.fx = node.x;
          node.fy = node.y;
        })
        .onBackgroundClick(() => propsRef.current.onSelect(null));
      graphRef.current = g;

      const ro = new ResizeObserver(() => {
        if (!containerRef.current || !graphRef.current) return;
        graphRef.current.width(containerRef.current.clientWidth);
        graphRef.current.height(containerRef.current.clientHeight);
      });
      ro.observe(containerRef.current);
      return () => ro.disconnect();
    });
    return () => {
      disposed = true;
      graphRef.current?._destructor?.();
      graphRef.current = null;
    };
  }, []);

  // feed data
  useEffect(() => {
    const g = graphRef.current;
    if (!g) return;
    const nodes: GraphNodeObj[] = [
      ...data.nodes.map((n) => ({
        id: n.id,
        label: n.title,
        color: NODE_COLORS[n.type],
        radius: 3 + n.importance * 6,
        ghost: false,
        status: n.status,
      })),
      ...data.ghosts.map((gh) => ({
        id: `ghost:${gh.target}`,
        label: gh.target,
        color: GHOST_COLOR,
        radius: 3,
        ghost: true,
        status: "ghost",
      })),
    ];
    const ids = new Set(nodes.map((n) => n.id));
    // Ghosts float unlinked: the graph payload deliberately omits per-ghost
    // from-ids (see plan self-review — this is per plan, not an oversight).
    const links = data.edges
      .filter((e) => ids.has(e.from) && ids.has(e.to))
      .map((e) => ({ source: e.from, target: e.to }));
    g.graphData({ nodes, links });
  }, [data]);

  return <div ref={containerRef} className="h-full w-full" />;
}
```

One structural-typing note: force-graph's default export is a factory `ForceGraph()` returning a callable instance — the double call `ForceGraph()(el)` above matches its API.

- [ ] **Step 3: MemoryTab.tsx shell** — full rewrite. Layout skeleton (state + effects here; NotePanel/overlay/stack/timeline slots filled by Tasks 13–14 — render nothing for them yet):

```tsx
"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { FolderOpen, RefreshCw, Search } from "lucide-react";

import { continuum, onMemoryEvent } from "@/lib/tauri";
import { NODE_COLORS, NODE_TYPE_LABELS } from "@/lib/memoryTheme";
import { MemoryGraph } from "@/components/memory/MemoryGraph";
import { Button, Card, EmptyState, SearchInput } from "@/components/ui/primitives";
import type {
  MemoryGraphData,
  MemoryGraphFilter,
  MemoryNodeType,
  MemoryVaultInfo,
} from "@/lib/types";

const EMPTY_GRAPH: MemoryGraphData = { nodes: [], edges: [], ghosts: [], truncated: false };
const ALL_TYPES = Object.keys(NODE_TYPE_LABELS) as MemoryNodeType[];

export function MemoryTab() {
  const [filter, setFilter] = useState<MemoryGraphFilter>({});
  const [graph, setGraph] = useState<MemoryGraphData>(EMPTY_GRAPH);
  const [info, setInfo] = useState<MemoryVaultInfo | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const refreshTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [g, i] = await Promise.all([
        continuum.memoryGraph(filter),
        continuum.memoryVaultInfo(),
      ]);
      setGraph(g);
      setInfo(i);
      setLoadError(null);
    } catch (err) {
      setLoadError(String(err));
    } finally {
      setLoading(false);
    }
  }, [filter]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let disposed = false;
    void onMemoryEvent(() => {
      if (refreshTimer.current) clearTimeout(refreshTimer.current);
      refreshTimer.current = setTimeout(() => void refresh(), 300);
    }).then((u) => {
      if (disposed) u();
      else unlisten = u;
    });
    return () => {
      disposed = true;
      unlisten?.();
      if (refreshTimer.current) clearTimeout(refreshTimer.current);
    };
  }, [refresh]);

  function toggleType(t: MemoryNodeType) {
    setFilter((f) => {
      const cur = f.types ?? null;
      if (!cur) return { ...f, types: [t] };
      const next = cur.includes(t) ? cur.filter((x) => x !== t) : [...cur, t];
      return { ...f, types: next.length === 0 ? null : next };
    });
  }

  function submitSearch() {
    setFilter((f) => ({ ...f, query: query.trim() || null }));
  }

  const showHidden = (filter.statuses?.length ?? 0) > 0;
  function toggleHidden() {
    setFilter((f) => ({
      ...f,
      statuses: showHidden
        ? null
        : ["confirmed", "candidate", "rejected", "superseded", "archived"],
    }));
  }

  return (
    <div className="relative flex h-full min-h-0 flex-col">
      {/* Topbar */}
      <div className="flex flex-wrap items-center gap-2 border-b border-bg-border px-3 py-2">
        <div className="relative w-64">
          <Search size={14} className="pointer-events-none absolute left-2.5 top-2 text-ink-dim" />
          <SearchInput
            value={query}
            onChange={setQuery}
            placeholder="Zoek in geheugen…"
            className="pl-8"
          />
        </div>
        <Button size="sm" variant="primary" onClick={submitSearch}>
          Zoek
        </Button>
        {ALL_TYPES.map((t) => {
          const active = filter.types?.includes(t) ?? false;
          return (
            <button
              key={t}
              onClick={() => toggleType(t)}
              className={
                "flex items-center gap-1.5 rounded-md border px-2 py-1 text-xs transition-colors " +
                (active
                  ? "border-accent-purple/60 bg-accent-purple/15 text-ink"
                  : "border-bg-border bg-bg-elevated text-ink-muted hover:text-ink")
              }
            >
              <span
                className="h-2 w-2 rounded-full"
                style={{ backgroundColor: NODE_COLORS[t] }}
              />
              {NODE_TYPE_LABELS[t]}
            </button>
          );
        })}
        <button
          onClick={toggleHidden}
          className={
            "rounded-md border px-2 py-1 text-xs " +
            (showHidden
              ? "border-accent-purple/60 text-ink"
              : "border-bg-border text-ink-muted hover:text-ink")
          }
        >
          Toon verborgen
        </button>
        <div className="flex-1" />
        {(info?.quarantined.length ?? 0) > 0 && (
          <span className="rounded-md border border-state-warn/40 bg-state-warn/10 px-2 py-1 text-xs text-state-warn">
            {info?.quarantined.length} bestand(en) in quarantaine
          </span>
        )}
        {graph.truncated && (
          <span className="text-xs text-ink-dim">graph afgekapt — verfijn je filters</span>
        )}
        <Button size="sm" variant="ghost" onClick={() => void refresh()}>
          <RefreshCw size={13} />
        </Button>
        <Button size="sm" onClick={() => void continuum.memoryOpenVault()}>
          <FolderOpen size={13} /> Open vault
        </Button>
      </div>

      {/* Graph body */}
      <div className="relative min-h-0 flex-1">
        <MemoryGraph
          data={graph}
          selectedId={selectedId}
          dimIds={null}
          onSelect={setSelectedId}
          onExpand={() => {
            /* Task 13 opens the overlay editor here */
          }}
          onGhostClick={() => {
            /* Task 13 opens the create flow here */
          }}
        />
        {/* Legend */}
        <div className="absolute bottom-3 left-3 flex flex-wrap gap-x-3 gap-y-1 rounded-md border border-bg-border bg-bg-surface/90 px-3 py-2 text-[11px] text-ink-muted">
          {ALL_TYPES.map((t) => (
            <span key={t} className="flex items-center gap-1">
              <span className="h-2 w-2 rounded-full" style={{ backgroundColor: NODE_COLORS[t] }} />
              {NODE_TYPE_LABELS[t]}
            </span>
          ))}
        </div>
        {loadError && (
          <div className="absolute inset-0 flex items-center justify-center">
            <Card title="Memory niet beschikbaar" className="max-w-md">
              <p className="text-sm text-ink-muted">{loadError}</p>
              <div className="mt-3">
                <Button variant="primary" onClick={() => void refresh()}>
                  Opnieuw proberen
                </Button>
              </div>
            </Card>
          </div>
        )}
        {!loading && !loadError && graph.nodes.length === 0 && (
          <div className="absolute inset-0 flex items-center justify-center">
            <Card title="Nog geen memories" className="max-w-md">
              <EmptyState
                title="Je vault is leeg"
                description="Continuum bewaart hier alles wat het over je werk leert — als gewone markdown-bestanden die je zelf mag bewerken."
              />
              <div className="mt-2 flex justify-center gap-2">
                <Button onClick={() => void continuum.memoryOpenVault()}>
                  <FolderOpen size={13} /> Open vault-map
                </Button>
              </div>
            </Card>
          </div>
        )}
      </div>
    </div>
  );
}
```

Overlay/panel state does not exist yet in this task — Task 13 introduces `overlayMode` + `selected` panel rendering and replaces the two no-op callbacks. Height comes from the Shell content area: check how ChatTab achieves full height (`h-full` chain) and reuse that exact approach so the graph fills the tab.

- [ ] **Step 4: Verify** — PowerShell: `corepack pnpm typecheck && corepack pnpm lint && corepack pnpm build` → PASS. Manual smoke optional (`scripts/dev.ps1`).

- [ ] **Step 5: Commit** — `git commit -m "feat(desktop): graph-centric memory tab shell with force-graph canvas"`

---

### Task 13: NotePanel (dock) + NoteEditorOverlay + create flow

**Files:**
- Create: `apps/desktop/src/components/memory/NotePanel.tsx`
- Create: `apps/desktop/src/components/memory/NoteEditorOverlay.tsx`
- Modify: `apps/desktop/src/components/tabs/MemoryTab.tsx` (wire panel, overlay, ghost-create, "Nieuwe memory" button)

**Interfaces:**
- Consumes: Tasks 11–12.
- Produces:

```ts
// NotePanel: docked right panel, resizable via drag handle (min 320px, max 70vw,
// width persisted in localStorage key "continuum-memory-panel-w").
interface NotePanelProps {
  noteId: string;
  onClose(): void;
  onExpand(): void;                  // → overlay
  onChanged(): void;                 // after save/delete → parent refresh()
  onNavigate(id: string): void;      // backlink click
}
// NoteEditorOverlay: full-screen editor. `mode` create vs edit.
type OverlayMode = { kind: "edit"; id: string } | { kind: "create"; draft: MemoryNoteDraft };
interface NoteEditorOverlayProps {
  mode: OverlayMode;
  onClose(): void;
  onSaved(id: string): void;
}
```

- [ ] **Step 1: NotePanel** — fetches `continuum.memoryGetNote(noteId)` on mount/id change. Renders: title, metadata chips (type with color dot, status, source, sensitivity, created/updated), confidence + importance sliders (existing `Slider` primitive, commit on release via `memorySaveNote` with updated frontmatter), tags (comma input), project (TextInput), relations list (each: `to`, `rel` TextInputs + confidence + remove; "add relation" appends empty row; saved with the note), rendered body via `ReactMarkdown` (import pattern from ChatTab), backlinks list (click → `onNavigate`), footer: Expand (⤢), Delete (confirm via `Modal`), Close. Resizable: left-edge drag handle div with `onMouseDown` + window mousemove/mouseup listeners updating width state.
- [ ] **Step 2: NoteEditorOverlay** — fixed inset-0 z-50 over everything (do NOT reuse `Modal` — this is a full-screen surface): header (title TextInput, type Select, status chip, Save/Cancel), split body: textarea (font-mono, flex-1) + live `ReactMarkdown` preview toggle, metadata sidebar (same editors as panel), Esc = onClose (keydown listener with cleanup), Ctrl+S = save. Create mode: `continuum.memoryCreateNote(draft)`; edit mode: fetch + `memorySaveNote`. On save → `onSaved(id)`.
- [ ] **Step 3: Wire MemoryTab** — `selectedId` renders NotePanel (absolute right, full height); `onExpand` sets overlay `{kind:"edit",id}`; ghost click → overlay `{kind:"create", draft:{type:"note", title: target}}`; topbar "Nieuwe memory" button → overlay create with empty title; `onSaved`/`onChanged` → `refresh()`; panel `onNavigate` sets selectedId.
- [ ] **Step 4: Verify** — `corepack pnpm typecheck && corepack pnpm lint && corepack pnpm build` → PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat(desktop): memory note panel and full-screen editor overlay"`

---

### Task 14: CuratorStack, TimelineStrip, saved views, migration + wipe copy

**Files:**
- Create: `apps/desktop/src/components/memory/CuratorStack.tsx`
- Create: `apps/desktop/src/components/memory/TimelineStrip.tsx`
- Modify: `apps/desktop/src/components/tabs/MemoryTab.tsx` (wire both + saved views dropdown + migration button + danger row)

**Interfaces:**
- Consumes: Tasks 11–13; `useMemoryViews` (Task 11).
- Produces:

```ts
interface CuratorStackProps {
  pending: MemoryNodeSummary[];
  onResolve(id: string, resolution: MemoryResolution): void; // parent calls API + refresh
  onOpen(id: string): void; // click title → select node
}
interface TimelineStripProps {
  events: MemoryEvent[];
  window: { since: string; until: string } | null; // null = live ("nu")
  onScrub(w: { since: string; until: string } | null): void;
}
```

- [ ] **Step 1: CuratorStack** — floating top-right (absolute, max-w-sm): shows first 3 of `pending` as cards (title, type chip, confidence, source, snippet), buttons ✓ (`{action:"confirm"}`), ✕ (`{action:"reject"}`), "later" (collapses card into the counter badge for this session — local `Set<string>` of snoozed ids); "+N meer" expands. A candidate whose summary has `relations` proposing supersede is Plan B territory — v1 renders every candidate the same way. Empty pending → render nothing.
- [ ] **Step 2: TimelineStrip** — bottom strip (h-10): fetch handled by parent; renders 48 buckets (visible window = today by default) as density bars (`bg-accent-purple/60` height ∝ count), kind-colored dot for error events (`bg-state-error`); drag/click selects a bucket → `onScrub({since,until})` (bucket bounds); "nu ▶" button clears. Parent MemoryTab: fetches `continuum.memoryEvents({ since: <today 00:00 ISO>, limit: 2000 })` on refresh; `onScrub` sets `dimIds` = ids of nodes whose `created`/`updated` fall inside the window (computed from `graph.nodes`) and passes to `MemoryGraph`; popover listing events in the bucket (simple absolute div above the strip).
- [ ] **Step 3: Saved views** — topbar dropdown (native `select` styled like `Select` primitive or reuse `Select`): lists views from `useMemoryViews`, "+ Huidige weergave opslaan" prompts a name (small `Modal` with TextInput), selecting a view replaces `filter`; delete button (trash icon) beside each entry inside the dropdown panel (use a small popover `Card`, not the native select, if delete-per-row is needed — implementer picks the simpler of the two, both acceptable).
- [ ] **Step 4: Migration + danger row** — in the empty state and in a small "…" topbar menu: when `info.legacy_semantic_present`, show "Importeer oude memory" → `continuum.memoryMigrateLegacy()` → toast/Card with `MigrationReport` counts → `refresh()`. Add "Herbouw index" (→ `memoryRebuildIndex`) and "Wis afgeleide memory-data" (→ existing `wipeMemory` flow moved here from the old tab, same DELETE confirm modal) into that menu; copy states explicitly: **"Vault-markdown wordt nooit verwijderd."**
- [ ] **Step 5: Verify** — `corepack pnpm typecheck && corepack pnpm lint && corepack pnpm build`; PowerShell `corepack pnpm format` (fix with `format:fix` if needed). Manual smoke via `scripts/dev.ps1`: create note → appears in graph; edit externally in editor → live update; scrub timeline (empty is fine).
- [ ] **Step 6: Commit** — `git commit -m "feat(desktop): curator stack, timeline strip, saved views, legacy migration flow"`

---

### Task 15: Docs + full gates

**Files:**
- Create: `docs/memory.md`
- Modify: `ARCHITECTURE.md` (memory section: three stores → vault + index + events; keep raw log/episodic description accurate)
- Modify: `CHANGELOG.md` (`## [Unreleased]` entries: continuum-memory crate, Memory tab rebuild, legacy migration, stub removal)
- Modify: `docs/dashboard.md` (Memory tab section rewrite)

**Steps:**
- [ ] **Step 1: docs/memory.md** — document: vault location + config (`[memory.vault]` keys with defaults), frontmatter schema (copy the spec's canonical block), node types + statuses + lifecycle, wiki-links vs typed relations + recommended `rel` vocabulary (`belongs_to, works_on, blocks, caused_by, decided_in, mentions, prefers, owns`), index rebuildability + quarantine, events retention, migration behavior, Obsidian interop note (vault is plain markdown; safe to open as an Obsidian vault), troubleshooting (delete index.db to force rebuild; quarantine chip meaning).
- [ ] **Step 2: ARCHITECTURE.md + dashboard.md + CHANGELOG.md** edits per Files list. ARCHITECTURE.md must state: vault = source of truth, index derived, desktop links continuum-memory directly, curator lands in Plan B (`docs/superpowers/plans/…-plan-b` when written).
- [ ] **Step 3: Full verification battery** —
  - bash: `export PATH="$HOME/.cargo/bin:$PATH"` then `cargo fmt --all -- --check`, `cargo clippy -p continuum-memory -p continuum-desktop --all-targets -- -D warnings`, `cargo test -p continuum-memory`, `cargo test -p continuum-core --no-default-features --lib`, `cargo test -p continuum-desktop`
  - PowerShell in apps/desktop: `corepack pnpm typecheck; corepack pnpm lint; corepack pnpm build; corepack pnpm format`
- [ ] **Step 4: Commit** — `git commit -m "docs(memory): vault documentation, architecture + changelog updates"`

---

## Plan self-review (done at write time)

- **Spec coverage:** datamodel §→T1/T2; index §→T3/T4; vault façade §→T5; events §→T6; watcher §→T7; migration §→T8; config §→T9; desktop §→T10; UI §§→T11–14; docs/self-healing §→T10 (probe) + T15; testing contract → distributed per task; FTS5 gate → T1. Curator/MCP/wake-context/runtime-health = Plan B by design.
- **Deliberate deviations from spec text (approved rationale):** ghost nodes float unlinked (graph payload has no per-ghost from-ids; noted in T12); `neighbors()` is exposed but the v1 UI does not call it (kept for Plan B wake-context work — do not delete).
- **Type consistency:** `Resolution` serde `{action: …}` matches TS `MemoryResolution`; command names in T10 = wrapper names in T11 = usage in T12–14; `VaultInfoDto` flattens into `MemoryVaultInfo` with `legacy_semantic_present`.
