//! Derived SQLite index over the vault. Disposable and rebuildable; the
//! markdown files are the source of truth. Layer: memory.
//!
//! The index mirrors every markdown note's frontmatter into `nodes`, its
//! wiki-links/typed relations into `links`, and a resolved graph into
//! `edges` (+ `unresolved_links` for dangling `[[targets]]`). It also owns
//! the append-only `events` timeline and the `quarantine` table of notes
//! that failed to parse. `nodes`/`links`/`edges`/`unresolved_links`/
//! `nodes_fts`/`quarantine` are fully derived and cleared by `rebuild()`;
//! `events` is never touched by a rebuild — only a schema-version reset
//! (which drops everything) removes it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

use crate::error::{MemoryError, Result};
use crate::frontmatter::{extract_wiki_links, parse_document, ParsedDoc};
use crate::model::{
    GhostNode, GraphData, GraphEdge, GraphFilter, GraphNode, IndexStats, NodeStatus, NodeSummary,
    NodeType, QuarantineEntry, Sensitivity, Source,
};

/// Bump when the schema changes; mismatch triggers drop + recreate.
const SCHEMA_VERSION: &str = "1";

/// Handle to the vault's derived SQLite index.
pub struct Index {
    pool: SqlitePool,
}

/// Outcome of indexing a single file.
pub enum IndexOutcome {
    /// The note was parsed and its node id upserted.
    Indexed(String),
    /// The note failed to parse; it was recorded in `quarantine`.
    Quarantined,
    /// The file no longer exists; any indexed node for its path was removed.
    Removed,
    /// The path is not an indexable markdown note (non-`.md`, or inside
    /// `.continuum/`) and was ignored.
    Skipped,
}

/// Internal result of `Index::upsert_node`: either the node was written, or
/// its slug is already owned by another node's path (deterministic
/// first-wins; the caller quarantines the loser).
enum UpsertOutcome {
    Indexed(String),
    SlugCollision { slug: String, occupant_path: String },
}

/// A single frontmatter relation or body wiki-link, prior to resolution.
pub(crate) struct RawLink {
    pub from_id: String,
    pub target: String,
    pub rel: String,
    pub confidence: f64,
    pub origin: String,
}

/// A resolved `(from_id, to_id, rel, confidence, origin)` edge, ready to
/// upsert into `edges`.
pub(crate) type ResolvedEdge = (String, String, String, f64, String);

/// An unresolved `(from_id, target)` pair, ready to upsert into
/// `unresolved_links`.
pub(crate) type UnresolvedLink = (String, String);

/// Resolve raw links against the current node set.
///
/// Priority per target: slug (lowercased) -> title (case-insensitive) ->
/// id (exact). Self-edges are dropped. Returns `(resolved edges,
/// unresolved (from_id, target) pairs, deduped)`.
pub(crate) fn resolve_links(
    links: &[RawLink],
    by_slug: &HashMap<String, String>,
    by_title: &HashMap<String, String>,
    by_id: &HashMap<String, String>,
) -> (Vec<ResolvedEdge>, Vec<UnresolvedLink>) {
    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    let mut unresolved_seen: HashSet<(String, String)> = HashSet::new();

    for link in links {
        let target_lower = link.target.to_lowercase();
        let hit = by_slug
            .get(&target_lower)
            .or_else(|| by_title.get(&target_lower))
            .or_else(|| by_id.get(&link.target));

        match hit {
            Some(to_id) if *to_id != link.from_id => {
                resolved.push((
                    link.from_id.clone(),
                    to_id.clone(),
                    link.rel.clone(),
                    link.confidence,
                    link.origin.clone(),
                ));
            }
            Some(_) => {
                // Self-edge: intentionally dropped.
            }
            None => {
                let key = (link.from_id.clone(), link.target.clone());
                if unresolved_seen.insert(key.clone()) {
                    unresolved.push(key);
                }
            }
        }
    }

    (resolved, unresolved)
}

/// snake_case string for `Source` (matches its serde representation).
fn source_str(source: Source) -> &'static str {
    match source {
        Source::UserStatement => "user_statement",
        Source::Observed => "observed",
        Source::Inferred => "inferred",
        Source::AgentRun => "agent_run",
        Source::Chat => "chat",
        Source::Manual => "manual",
    }
}

/// snake_case string for `Sensitivity` (matches its serde representation).
fn sensitivity_str(sensitivity: Sensitivity) -> &'static str {
    match sensitivity {
        Sensitivity::Public => "public",
        Sensitivity::Internal => "internal",
        Sensitivity::Sensitive => "sensitive",
    }
}

/// Inverse of [`source_str`]; `None` for unrecognized strings.
fn parse_source(s: &str) -> Option<Source> {
    match s {
        "user_statement" => Some(Source::UserStatement),
        "observed" => Some(Source::Observed),
        "inferred" => Some(Source::Inferred),
        "agent_run" => Some(Source::AgentRun),
        "chat" => Some(Source::Chat),
        "manual" => Some(Source::Manual),
        _ => None,
    }
}

/// Inverse of [`sensitivity_str`]; `None` for unrecognized strings.
fn parse_sensitivity(s: &str) -> Option<Sensitivity> {
    match s {
        "public" => Some(Sensitivity::Public),
        "internal" => Some(Sensitivity::Internal),
        "sensitive" => Some(Sensitivity::Sensitive),
        _ => None,
    }
}

/// Parse a `nodes.type` value, defaulting to [`NodeType::Note`] and logging
/// a warning for unrecognized values (a corrupted or hand-edited row should
/// never crash a query).
fn parse_node_type_or_warn(s: &str) -> NodeType {
    NodeType::parse(s).unwrap_or_else(|| {
        tracing::warn!(
            layer = "memory",
            component = "index",
            raw = %s,
            "unknown node type in row, defaulting to note"
        );
        NodeType::Note
    })
}

/// Parse a `nodes.status` value, defaulting to [`NodeStatus::Confirmed`]
/// and logging a warning for unrecognized values.
fn parse_node_status_or_warn(s: &str) -> NodeStatus {
    NodeStatus::parse(s).unwrap_or_else(|| {
        tracing::warn!(
            layer = "memory",
            component = "index",
            raw = %s,
            "unknown node status in row, defaulting to confirmed"
        );
        NodeStatus::Confirmed
    })
}

/// Parse a `nodes.source` value, defaulting to [`Source::Manual`] and
/// logging a warning for unrecognized values.
fn parse_source_or_warn(s: &str) -> Source {
    parse_source(s).unwrap_or_else(|| {
        tracing::warn!(
            layer = "memory",
            component = "index",
            raw = %s,
            "unknown source in row, defaulting to manual"
        );
        Source::Manual
    })
}

/// Parse a `nodes.sensitivity` value, defaulting to
/// [`Sensitivity::Internal`] and logging a warning for unrecognized values.
fn parse_sensitivity_or_warn(s: &str) -> Sensitivity {
    parse_sensitivity(s).unwrap_or_else(|| {
        tracing::warn!(
            layer = "memory",
            component = "index",
            raw = %s,
            "unknown sensitivity in row, defaulting to internal"
        );
        Sensitivity::Internal
    })
}

/// Hydrate a `nodes` row (any `SELECT *`-shaped query against it) into a
/// [`NodeSummary`]. Unrecognized enum strings fall back to safe defaults
/// (logged once per call, see the `parse_*_or_warn` helpers) rather than
/// failing the whole query over one corrupted row.
fn summary_from_row(row: &SqliteRow) -> NodeSummary {
    let type_str: String = row.try_get("type").unwrap_or_default();
    let status_str: String = row.try_get("status").unwrap_or_default();
    let source_str: String = row.try_get("source").unwrap_or_default();
    let sensitivity_str: String = row.try_get("sensitivity").unwrap_or_default();
    let tags_json: String = row
        .try_get("tags_json")
        .unwrap_or_else(|_| "[]".to_string());
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    let snippet: String = row.try_get("snippet").unwrap_or_default();

    NodeSummary {
        id: row.try_get("id").unwrap_or_default(),
        slug: row.try_get("slug").unwrap_or_default(),
        title: row.try_get("title").unwrap_or_default(),
        node_type: parse_node_type_or_warn(&type_str),
        status: parse_node_status_or_warn(&status_str),
        project: row.try_get::<Option<String>, _>("project").unwrap_or(None),
        confidence: row.try_get::<f64, _>("confidence").unwrap_or(0.0) as f32,
        importance: row.try_get::<f64, _>("importance").unwrap_or(0.0) as f32,
        source: parse_source_or_warn(&source_str),
        sensitivity: parse_sensitivity_or_warn(&sensitivity_str),
        created: row.try_get("created").unwrap_or_default(),
        updated: row.try_get("updated").unwrap_or_default(),
        tags,
        snippet: if snippet.is_empty() {
            None
        } else {
            Some(snippet)
        },
    }
}

/// Hydrate a `nodes` row into the leaner [`GraphNode`] shape used by
/// `graph`/`neighbors` (no source/sensitivity/tags/snippet).
fn graph_node_from_row(row: &SqliteRow) -> GraphNode {
    let type_str: String = row.try_get("type").unwrap_or_default();
    let status_str: String = row.try_get("status").unwrap_or_default();

    GraphNode {
        id: row.try_get("id").unwrap_or_default(),
        slug: row.try_get("slug").unwrap_or_default(),
        title: row.try_get("title").unwrap_or_default(),
        node_type: parse_node_type_or_warn(&type_str),
        status: parse_node_status_or_warn(&status_str),
        project: row.try_get::<Option<String>, _>("project").unwrap_or(None),
        confidence: row.try_get::<f64, _>("confidence").unwrap_or(0.0) as f32,
        importance: row.try_get::<f64, _>("importance").unwrap_or(0.0) as f32,
        created: row.try_get("created").unwrap_or_default(),
        updated: row.try_get("updated").unwrap_or_default(),
    }
}

/// Normalize a user search string into an FTS5 `MATCH` query: each
/// whitespace-separated token is stripped to alphanumeric characters,
/// empties are dropped, and survivors become `"tok"*` prefix matches
/// joined with spaces. Empty/whitespace-only input yields `""`.
///
/// `pub` (rather than `pub(crate)`) purely so tests can exercise it
/// directly; it is a pure string transform with no trust implications.
pub fn fts_query(user: &str) -> String {
    user.split_whitespace()
        .map(|tok| {
            tok.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|tok| !tok.is_empty())
        .map(|tok| format!("\"{tok}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 64-bit FNV-1a hash of `s`, formatted as lowercase hex. Used as a cheap
/// change-detection fingerprint for note bodies.
fn fnv1a(s: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// First 160 chars of `body` with whitespace runs collapsed to single
/// spaces. Char-boundary safe (operates on `chars()`, never raw bytes).
fn make_snippet(body: &str) -> String {
    let head: String = body.chars().take(160).collect();
    head.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// File modification time as Unix seconds. Missing/unsupported mtimes
/// collapse to 0 rather than failing indexing.
fn mtime_of(path: &Path) -> Result<i64> {
    let meta = std::fs::metadata(path).map_err(|e| MemoryError::io(path, e))?;
    let modified = meta.modified().map_err(|e| MemoryError::io(path, e))?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(secs)
}

/// Path of `path` relative to `vault_dir`, with `\` normalized to `/`.
/// Paths outside the vault are rejected.
fn rel_path(vault_dir: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(vault_dir).map_err(|_| {
        MemoryError::Invalid(format!(
            "path {} is not inside vault dir {}",
            path.display(),
            vault_dir.display()
        ))
    })?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

/// Recursively collect every `.md` file under `dir`, skipping the
/// `.continuum` index directory.
fn collect_md_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| MemoryError::io(dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| MemoryError::io(dir, e))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| MemoryError::io(&path, e))?;
        if file_type.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some(".continuum") {
                continue;
            }
            collect_md_files(&path, out)?;
        } else if file_type.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

impl Index {
    /// Open (creating if missing) the SQLite index at `db_path`, ensuring
    /// the schema exists and matches [`SCHEMA_VERSION`]. A version
    /// mismatch drops and recreates every derived table (including
    /// `events`).
    pub async fn open(db_path: &Path) -> Result<Index> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        let index = Index { pool };
        index.create_schema().await?;
        index.ensure_schema_version().await?;
        Ok(index)
    }

    /// The underlying connection pool, exposed for callers (and tests)
    /// that need raw queries against the index.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    async fn create_schema(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS nodes(
              id TEXT PRIMARY KEY, slug TEXT UNIQUE, path TEXT UNIQUE, type TEXT NOT NULL,
              title TEXT NOT NULL, status TEXT NOT NULL, project TEXT,
              confidence REAL NOT NULL, importance REAL NOT NULL, source TEXT NOT NULL,
              sensitivity TEXT NOT NULL, created TEXT NOT NULL, updated TEXT NOT NULL,
              last_used TEXT, expires TEXT, supersedes TEXT, superseded_by TEXT,
              tags_json TEXT NOT NULL DEFAULT '[]', mtime INTEGER NOT NULL DEFAULT 0,
              body_hash TEXT NOT NULL DEFAULT '', snippet TEXT NOT NULL DEFAULT '')",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS links(
              from_id TEXT NOT NULL, target TEXT NOT NULL, rel TEXT NOT NULL,
              confidence REAL NOT NULL, origin TEXT NOT NULL,
              PRIMARY KEY(from_id, target, rel, origin))",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS edges(
              from_id TEXT NOT NULL, to_id TEXT NOT NULL, rel TEXT NOT NULL,
              confidence REAL NOT NULL, origin TEXT NOT NULL,
              PRIMARY KEY(from_id, to_id, rel, origin))",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS unresolved_links(
              from_id TEXT NOT NULL, target TEXT NOT NULL, PRIMARY KEY(from_id, target))",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(node_id UNINDEXED, title, body, tags)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events(
              id INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, kind TEXT NOT NULL,
              text TEXT NOT NULL, project TEXT, node_id TEXT, \"ref\" TEXT)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS events_ts ON events(ts)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS quarantine(path TEXT PRIMARY KEY, error TEXT NOT NULL, mtime INTEGER NOT NULL)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn ensure_schema_version(&self) -> Result<()> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM meta WHERE key = 'schema_version'")
                .fetch_optional(&self.pool)
                .await?;
        match row {
            Some((v,)) if v == SCHEMA_VERSION => Ok(()),
            _ => self.reset_schema().await,
        }
    }

    /// Drop every table (including `events` and `quarantine`) and
    /// recreate the schema from scratch. Only called on a schema-version
    /// mismatch, never by `rebuild()`.
    async fn reset_schema(&self) -> Result<()> {
        for table in [
            "nodes",
            "links",
            "edges",
            "unresolved_links",
            "nodes_fts",
            "events",
            "quarantine",
            "meta",
        ] {
            sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
                .execute(&self.pool)
                .await?;
        }
        self.create_schema().await?;
        sqlx::query(
            "INSERT INTO meta(key, value) VALUES ('schema_version', ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(SCHEMA_VERSION)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Index (or re-index) a single markdown file. `path` must live under
    /// `vault_dir`. Non-`.md` files and anything under `.continuum/` are
    /// ignored ([`IndexOutcome::Skipped`]); a missing file is treated as a
    /// deletion ([`IndexOutcome::Removed`]); a parse failure quarantines
    /// the file ([`IndexOutcome::Quarantined`]).
    pub async fn index_file(&self, vault_dir: &Path, path: &Path) -> Result<IndexOutcome> {
        self.index_file_inner(vault_dir, path, true).await
    }

    /// Index (or re-index) each of `paths` — e.g. a debounced batch from
    /// the file-watcher — without recomputing the resolved edge graph
    /// after every individual file, then recomputes `edges`/
    /// `unresolved_links` exactly ONCE at the end. Behaviorally equivalent
    /// to calling [`Index::index_file`] on each path in sequence (same
    /// per-file [`IndexOutcome`]s, same final `edges` table), but O(1)
    /// edge recomputations instead of O(paths.len()) for a multi-file
    /// batch — `index_file`'s own per-call recompute is unaffected, this
    /// is purely a batch-shaped path used by `Vault::reindex_paths`.
    pub(crate) async fn index_files(
        &self,
        vault_dir: &Path,
        paths: &[PathBuf],
    ) -> Result<Vec<IndexOutcome>> {
        let mut outcomes = Vec::with_capacity(paths.len());
        for path in paths {
            outcomes.push(self.index_file_inner(vault_dir, path, false).await?);
        }
        self.recompute_edges().await?;
        Ok(outcomes)
    }

    async fn index_file_inner(
        &self,
        vault_dir: &Path,
        path: &Path,
        recompute: bool,
    ) -> Result<IndexOutcome> {
        let rel = rel_path(vault_dir, path)?;
        if rel.starts_with(".continuum/") || !rel.ends_with(".md") {
            return Ok(IndexOutcome::Skipped);
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.remove_path_inner(vault_dir, path, recompute).await?;
                return Ok(IndexOutcome::Removed);
            }
            Err(e) => return Err(MemoryError::io(path, e)),
        };

        let mtime = mtime_of(path)?;

        match parse_document(&content) {
            Ok(parsed) => match self.upsert_node(&rel, path, &parsed, mtime).await? {
                UpsertOutcome::Indexed(id) => {
                    if recompute {
                        self.recompute_edges().await?;
                    }
                    Ok(IndexOutcome::Indexed(id))
                }
                UpsertOutcome::SlugCollision {
                    slug,
                    occupant_path,
                } => {
                    let error = format!("slug '{slug}' already used by {occupant_path}");
                    self.quarantine_path(&rel, &error, mtime).await?;
                    if recompute {
                        self.recompute_edges().await?;
                    }
                    Ok(IndexOutcome::Quarantined)
                }
            },
            Err(parse_err) => {
                self.quarantine_path(&rel, &parse_err.to_string(), mtime)
                    .await?;
                if recompute {
                    self.recompute_edges().await?;
                }
                Ok(IndexOutcome::Quarantined)
            }
        }
    }

    async fn quarantine_path(&self, rel: &str, error: &str, mtime: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let existing: Option<(String,)> = sqlx::query_as("SELECT id FROM nodes WHERE path = ?")
            .bind(rel)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some((id,)) = existing {
            sqlx::query("DELETE FROM links WHERE from_id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM nodes_fts WHERE node_id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM nodes WHERE id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "INSERT INTO quarantine(path, error, mtime) VALUES (?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET error = excluded.error, mtime = excluded.mtime",
        )
        .bind(rel)
        .bind(error)
        .bind(mtime)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn upsert_node(
        &self,
        rel: &str,
        path: &Path,
        parsed: &ParsedDoc,
        mtime: i64,
    ) -> Result<UpsertOutcome> {
        let fm = &parsed.frontmatter;
        let body = &parsed.body;
        let id = fm.id.clone();
        let slug = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_lowercase();

        let mut tx = self.pool.begin().await?;

        // Deterministic first-wins on slug collisions: a UNIQUE constraint
        // violation here would otherwise abort a rebuild mid-flight, after
        // the top-of-rebuild DELETEs already ran. Whoever already owns the
        // slug keeps it; the caller quarantines this file instead. Which
        // file "already owns" the slug is made deterministic by having
        // `rebuild` walk files in sorted relative-path order.
        //
        // Compare by *path*, not id: rewriting this exact file in place
        // with a new frontmatter id must not look like a foreign
        // collision against its own prior row (same path, old id).
        let collision: Option<(String,)> =
            sqlx::query_as("SELECT path FROM nodes WHERE slug = ? AND path <> ?")
                .bind(&slug)
                .bind(rel)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((occupant_path,)) = collision {
            return Ok(UpsertOutcome::SlugCollision {
                slug,
                occupant_path,
            });
        }

        // A rewritten file can carry a new id; purge the old node row(s)
        // at this path *and* their links/nodes_fts rows. Deleting only the
        // `nodes` row (as a prior version of this code did) orphaned
        // `links`/`nodes_fts` rows under the old id forever, since nothing
        // else ever revisits an id that no longer appears in `nodes`.
        let old_ids: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM nodes WHERE path = ? AND id <> ?")
                .bind(rel)
                .bind(&id)
                .fetch_all(&mut *tx)
                .await?;
        for (old_id,) in &old_ids {
            sqlx::query("DELETE FROM links WHERE from_id = ?")
                .bind(old_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM nodes_fts WHERE node_id = ?")
                .bind(old_id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM nodes WHERE path = ? AND id <> ?")
            .bind(rel)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM quarantine WHERE path = ?")
            .bind(rel)
            .execute(&mut *tx)
            .await?;

        let updated = fm.updated.unwrap_or(fm.created);
        let snippet = make_snippet(body);
        let body_hash = fnv1a(body);
        let tags_json = serde_json::to_string(&fm.tags).unwrap_or_else(|_| "[]".to_string());

        sqlx::query(
            "INSERT INTO nodes(
               id, slug, path, type, title, status, project, confidence, importance,
               source, sensitivity, created, updated, last_used, expires, supersedes,
               superseded_by, tags_json, mtime, body_hash, snippet
             ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(id) DO UPDATE SET
               slug = excluded.slug, path = excluded.path, type = excluded.type,
               title = excluded.title, status = excluded.status, project = excluded.project,
               confidence = excluded.confidence, importance = excluded.importance,
               source = excluded.source, sensitivity = excluded.sensitivity,
               created = excluded.created, updated = excluded.updated,
               last_used = excluded.last_used, expires = excluded.expires,
               supersedes = excluded.supersedes, superseded_by = excluded.superseded_by,
               tags_json = excluded.tags_json, mtime = excluded.mtime,
               body_hash = excluded.body_hash, snippet = excluded.snippet",
        )
        .bind(&id)
        .bind(&slug)
        .bind(rel)
        .bind(fm.node_type.as_str())
        .bind(&fm.title)
        .bind(fm.status.as_str())
        .bind(&fm.project)
        .bind(fm.confidence as f64)
        .bind(fm.importance as f64)
        .bind(source_str(fm.source))
        .bind(sensitivity_str(fm.sensitivity))
        .bind(fm.created.to_rfc3339())
        .bind(updated.to_rfc3339())
        .bind(fm.last_used.map(|d| d.to_rfc3339()))
        .bind(fm.expires.map(|d| d.to_rfc3339()))
        .bind(&fm.supersedes)
        .bind(&fm.superseded_by)
        .bind(&tags_json)
        .bind(mtime)
        .bind(&body_hash)
        .bind(&snippet)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM links WHERE from_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        for relation in &fm.relations {
            sqlx::query(
                "INSERT OR REPLACE INTO links(from_id, target, rel, confidence, origin)
                 VALUES (?, ?, ?, ?, 'frontmatter')",
            )
            .bind(&id)
            .bind(&relation.to)
            .bind(&relation.rel)
            .bind(relation.confidence as f64)
            .execute(&mut *tx)
            .await?;
        }
        for target in extract_wiki_links(body) {
            sqlx::query(
                "INSERT OR REPLACE INTO links(from_id, target, rel, confidence, origin)
                 VALUES (?, ?, 'mentions', 1.0, 'body')",
            )
            .bind(&id)
            .bind(&target)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query("DELETE FROM nodes_fts WHERE node_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO nodes_fts(node_id, title, body, tags) VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(&fm.title)
            .bind(body)
            .bind(fm.tags.join(" "))
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(UpsertOutcome::Indexed(id))
    }

    /// Remove any indexed node/quarantine entry for `path` (which need not
    /// still exist on disk) and recompute the resolved edge graph.
    pub async fn remove_path(&self, vault_dir: &Path, path: &Path) -> Result<()> {
        self.remove_path_inner(vault_dir, path, true).await
    }

    async fn remove_path_inner(
        &self,
        vault_dir: &Path,
        path: &Path,
        recompute: bool,
    ) -> Result<()> {
        let rel = rel_path(vault_dir, path)?;
        let mut tx = self.pool.begin().await?;
        let existing: Option<(String,)> = sqlx::query_as("SELECT id FROM nodes WHERE path = ?")
            .bind(&rel)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some((id,)) = existing {
            sqlx::query("DELETE FROM links WHERE from_id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM nodes_fts WHERE node_id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM nodes WHERE id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM quarantine WHERE path = ?")
            .bind(&rel)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if recompute {
            self.recompute_edges().await?;
        }
        Ok(())
    }

    /// Recompute `edges` and `unresolved_links` from the current
    /// `nodes`/`links` tables. Idempotent and safe to call repeatedly;
    /// `rebuild()` and `index_file`/`remove_path` all funnel through this.
    pub async fn recompute_edges(&self) -> Result<()> {
        // ORDER BY id: SQLite gives no ordering guarantee for a TEXT
        // primary key without an explicit ORDER BY, and this table's rows
        // are mutated over time (upserts, deletes) rather than only ever
        // appended — relying on physical/rowid order would silently drift.
        // We need a *stable* order so that the tie-break below always
        // picks the same winner across repeated rebuilds.
        let nodes: Vec<(String, String, String)> =
            sqlx::query_as("SELECT id, slug, title FROM nodes ORDER BY id")
                .fetch_all(&self.pool)
                .await?;

        let mut by_slug: HashMap<String, String> = HashMap::new();
        let mut by_title: HashMap<String, String> = HashMap::new();
        let mut by_id: HashMap<String, String> = HashMap::new();
        for (id, slug, title) in &nodes {
            // Deterministic tie-break for a title shared by multiple nodes
            // (slugs can't collide in practice — collisions are quarantined
            // at index time — but titles legitimately can): nodes are
            // visited in ascending `id` order and the FIRST occurrence of a
            // given slug/title wins, so link resolution always lands on the
            // note with the smallest id, consistently across rebuilds.
            by_slug
                .entry(slug.to_lowercase())
                .or_insert_with(|| id.clone());
            by_title
                .entry(title.to_lowercase())
                .or_insert_with(|| id.clone());
            by_id.insert(id.clone(), id.clone());
        }

        let link_rows: Vec<(String, String, String, f64, String)> =
            sqlx::query_as("SELECT from_id, target, rel, confidence, origin FROM links")
                .fetch_all(&self.pool)
                .await?;
        let links: Vec<RawLink> = link_rows
            .into_iter()
            .map(|(from_id, target, rel, confidence, origin)| RawLink {
                from_id,
                target,
                rel,
                confidence,
                origin,
            })
            .collect();

        let (resolved, unresolved) = resolve_links(&links, &by_slug, &by_title, &by_id);

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM edges").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM unresolved_links")
            .execute(&mut *tx)
            .await?;
        for (from_id, to_id, rel, confidence, origin) in &resolved {
            sqlx::query(
                "INSERT OR REPLACE INTO edges(from_id, to_id, rel, confidence, origin)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(from_id)
            .bind(to_id)
            .bind(rel)
            .bind(confidence)
            .bind(origin)
            .execute(&mut *tx)
            .await?;
        }
        for (from_id, target) in &unresolved {
            sqlx::query("INSERT OR IGNORE INTO unresolved_links(from_id, target) VALUES (?, ?)")
                .bind(from_id)
                .bind(target)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Fully rebuild the index from the markdown files under `vault_dir`.
    /// Clears `nodes`, `links`, `nodes_fts`, `unresolved_links`, `edges`,
    /// and `quarantine` first — `events` is never touched.
    pub async fn rebuild(&self, vault_dir: &Path) -> Result<IndexStats> {
        sqlx::query("DELETE FROM nodes").execute(&self.pool).await?;
        sqlx::query("DELETE FROM links").execute(&self.pool).await?;
        sqlx::query("DELETE FROM nodes_fts")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM unresolved_links")
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM edges").execute(&self.pool).await?;
        sqlx::query("DELETE FROM quarantine")
            .execute(&self.pool)
            .await?;

        let mut files = Vec::new();
        collect_md_files(vault_dir, &mut files)?;

        // Deterministic processing order: filesystem enumeration order is
        // not guaranteed, but "who wins a slug collision" must be stable
        // across rebuilds. Sort by vault-relative path (forward-slash
        // normalized) so the same file always wins.
        let mut files_rel: Vec<(String, PathBuf)> = Vec::with_capacity(files.len());
        for file in files {
            let rel = rel_path(vault_dir, &file)?;
            files_rel.push((rel, file));
        }
        files_rel.sort_by(|a, b| a.0.cmp(&b.0));

        let mut indexed = 0u64;
        let mut quarantined = 0u64;
        for (_, file) in &files_rel {
            match self.index_file_inner(vault_dir, file, false).await? {
                IndexOutcome::Indexed(_) => indexed += 1,
                IndexOutcome::Quarantined => quarantined += 1,
                IndexOutcome::Removed | IndexOutcome::Skipped => {}
            }
        }

        self.recompute_edges().await?;

        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO meta(key, value) VALUES ('last_full_index_at', ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(&now)
        .execute(&self.pool)
        .await?;

        tracing::info!(
            layer = "memory",
            component = "index",
            indexed,
            quarantined,
            "vault reindexed"
        );

        Ok(IndexStats {
            indexed,
            quarantined,
            removed: 0,
        })
    }

    /// Full-text search over titles/bodies/tags. Empty or whitespace-only
    /// `q` returns `Ok(vec![])` without touching the database. Results are
    /// ordered by FTS5 `rank` (best match first).
    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<NodeSummary>> {
        let query = fts_query(q);
        if query.is_empty() {
            return Ok(vec![]);
        }
        let hits: Vec<(String,)> = sqlx::query_as(
            "SELECT node_id FROM nodes_fts WHERE nodes_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )
        .bind(&query)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        if hits.is_empty() {
            return Ok(vec![]);
        }
        let ids: Vec<String> = hits.into_iter().map(|(id,)| id).collect();
        let rows = self.fetch_nodes_by_ids(&ids).await?;
        let mut by_id: HashMap<String, NodeSummary> =
            rows.into_iter().map(|n| (n.id.clone(), n)).collect();
        // `IN (...)` does not preserve the FTS rank order, so re-project
        // through the id list we already have in rank order.
        Ok(ids.into_iter().filter_map(|id| by_id.remove(&id)).collect())
    }

    /// Hydrate `nodes` rows for an explicit id set. `ids` empty short-circuits
    /// to `Ok(vec![])` rather than emitting an invalid `IN ()`.
    async fn fetch_nodes_by_ids(&self, ids: &[String]) -> Result<Vec<NodeSummary>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM nodes WHERE id IN (");
        {
            let mut sep = qb.separated(", ");
            for id in ids {
                sep.push_bind(id);
            }
        }
        qb.push(")");
        let rows = qb.build().fetch_all(&self.pool).await?;
        Ok(rows.iter().map(summary_from_row).collect())
    }

    /// Resolve `f` (default statuses = confirmed + candidate when
    /// `f.statuses` is `None`) into a bounded graph snapshot: nodes ordered
    /// by `importance DESC, id ASC`, capped at `f.limit.unwrap_or(default_limit)`
    /// with `truncated` set when more rows existed; edges and ghost
    /// (unresolved-link) nodes restricted to that node set.
    pub async fn graph(&self, f: &GraphFilter, default_limit: u32) -> Result<GraphData> {
        let statuses: Vec<NodeStatus> = match &f.statuses {
            Some(s) if !s.is_empty() => s.clone(),
            _ => vec![NodeStatus::Confirmed, NodeStatus::Candidate],
        };
        let limit = f.limit.unwrap_or(default_limit);

        let id_filter: Option<Vec<String>> = match &f.query {
            Some(q) if !q.trim().is_empty() => Some(self.fts_ids(q).await?),
            _ => None,
        };

        let (nodes, truncated) = self
            .query_graph_nodes(
                Some(&statuses),
                f.types.as_deref(),
                f.project.as_deref(),
                f.updated_since,
                f.updated_until,
                id_filter.as_deref(),
                limit,
            )
            .await?;

        self.hydrate_edges_and_ghosts(nodes, truncated).await
    }

    /// BFS neighborhood of `id` over `edges`, traversed in both directions
    /// (edge direction is ignored — this answers "what's connected to
    /// this node", not "what does this node point to"). `depth` is capped
    /// to 2. The start node is always included, regardless of status.
    pub async fn neighbors(&self, id: &str, depth: u8, default_limit: u32) -> Result<GraphData> {
        let edge_rows: Vec<(String, String)> = sqlx::query_as("SELECT from_id, to_id FROM edges")
            .fetch_all(&self.pool)
            .await?;
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        for (from, to) in edge_rows {
            adj.entry(from.clone()).or_default().push(to.clone());
            adj.entry(to).or_default().push(from);
        }

        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(id.to_string());
        let mut frontier = vec![id.to_string()];
        for _ in 0..depth.min(2) {
            let mut next = Vec::new();
            for cur in &frontier {
                if let Some(neighbors_of) = adj.get(cur) {
                    for n in neighbors_of {
                        if visited.insert(n.clone()) {
                            next.push(n.clone());
                        }
                    }
                }
            }
            frontier = next;
        }

        let ids: Vec<String> = visited.into_iter().collect();
        let (nodes, truncated) = self
            .query_graph_nodes(None, None, None, None, None, Some(&ids), default_limit)
            .await?;
        self.hydrate_edges_and_ghosts(nodes, truncated).await
    }

    /// Shared node query behind `graph`/`neighbors`: builds `WHERE` clauses
    /// from whichever filters are `Some`, orders by `importance DESC, id
    /// ASC`, and fetches `limit + 1` rows so the caller can detect
    /// truncation by popping the extra row. `id_filter: Some(&[])` (an
    /// empty explicit id set, e.g. a query with no FTS hits) short-circuits
    /// to an empty, non-truncated result rather than emitting `IN ()`.
    #[allow(clippy::too_many_arguments)]
    async fn query_graph_nodes(
        &self,
        statuses: Option<&[NodeStatus]>,
        types: Option<&[NodeType]>,
        project: Option<&str>,
        updated_since: Option<DateTime<Utc>>,
        updated_until: Option<DateTime<Utc>>,
        id_filter: Option<&[String]>,
        limit: u32,
    ) -> Result<(Vec<GraphNode>, bool)> {
        if matches!(id_filter, Some(ids) if ids.is_empty()) {
            return Ok((vec![], false));
        }

        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM nodes WHERE 1=1");
        if let Some(statuses) = statuses {
            qb.push(" AND status IN (");
            {
                let mut sep = qb.separated(", ");
                for s in statuses {
                    sep.push_bind(s.as_str());
                }
            }
            qb.push(")");
        }
        if let Some(types) = types.filter(|t| !t.is_empty()) {
            qb.push(" AND type IN (");
            {
                let mut sep = qb.separated(", ");
                for t in types {
                    sep.push_bind(t.as_str());
                }
            }
            qb.push(")");
        }
        if let Some(project) = project {
            qb.push(" AND project = ").push_bind(project.to_string());
        }
        if let Some(since) = updated_since {
            qb.push(" AND updated >= ").push_bind(since.to_rfc3339());
        }
        if let Some(until) = updated_until {
            qb.push(" AND updated <= ").push_bind(until.to_rfc3339());
        }
        if let Some(ids) = id_filter {
            qb.push(" AND id IN (");
            {
                let mut sep = qb.separated(", ");
                for id in ids {
                    sep.push_bind(id);
                }
            }
            qb.push(")");
        }
        qb.push(" ORDER BY importance DESC, id ASC LIMIT ");
        qb.push_bind(i64::from(limit) + 1);

        let rows = qb.build().fetch_all(&self.pool).await?;
        let mut nodes: Vec<GraphNode> = rows.iter().map(graph_node_from_row).collect();
        let truncated = nodes.len() > limit as usize;
        if truncated {
            nodes.truncate(limit as usize);
        }
        Ok((nodes, truncated))
    }

    /// Ids of nodes whose title/body/tags match `q` via FTS5. `q` that
    /// sanitizes to an empty query (see [`fts_query`]) returns `Ok(vec![])`.
    async fn fts_ids(&self, q: &str) -> Result<Vec<String>> {
        let query = fts_query(q);
        if query.is_empty() {
            return Ok(vec![]);
        }
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT node_id FROM nodes_fts WHERE nodes_fts MATCH ?1")
                .bind(&query)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Fill in `edges`/`ghosts` for a resolved node set (`graph`/`neighbors`
    /// share this tail end).
    async fn hydrate_edges_and_ghosts(
        &self,
        nodes: Vec<GraphNode>,
        truncated: bool,
    ) -> Result<GraphData> {
        let ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
        let edges = self.fetch_edges_within(&ids).await?;
        let ghosts = self.fetch_ghosts_from(&ids).await?;
        Ok(GraphData {
            nodes,
            edges,
            ghosts,
            truncated,
        })
    }

    /// Edges with both endpoints inside `ids`. Empty `ids` short-circuits
    /// to `Ok(vec![])`.
    async fn fetch_edges_within(&self, ids: &[String]) -> Result<Vec<GraphEdge>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT from_id, to_id, rel, confidence, origin FROM edges WHERE from_id IN (",
        );
        {
            let mut sep = qb.separated(", ");
            for id in ids {
                sep.push_bind(id);
            }
        }
        qb.push(") AND to_id IN (");
        {
            let mut sep = qb.separated(", ");
            for id in ids {
                sep.push_bind(id);
            }
        }
        qb.push(")");
        let rows: Vec<(String, String, String, f64, String)> =
            qb.build_query_as().fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|(from, to, rel, confidence, origin)| GraphEdge {
                from,
                to,
                rel,
                confidence: confidence as f32,
                origin,
            })
            .collect())
    }

    /// Unresolved link targets originating inside `ids`, most-referenced
    /// first, capped at 100. Empty `ids` short-circuits to `Ok(vec![])`.
    async fn fetch_ghosts_from(&self, ids: &[String]) -> Result<Vec<GhostNode>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT target, count(*) as cnt FROM unresolved_links WHERE from_id IN (",
        );
        {
            let mut sep = qb.separated(", ");
            for id in ids {
                sep.push_bind(id);
            }
        }
        qb.push(") GROUP BY target ORDER BY cnt DESC LIMIT 100");
        let rows: Vec<(String, i64)> = qb.build_query_as().fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|(target, cnt)| GhostNode {
                target,
                ref_count: cnt as u32,
            })
            .collect())
    }

    /// Candidate nodes awaiting review, oldest-created first.
    pub async fn pending(&self) -> Result<Vec<NodeSummary>> {
        let rows =
            sqlx::query("SELECT * FROM nodes WHERE status = 'candidate' ORDER BY created ASC")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.iter().map(summary_from_row).collect())
    }

    /// Nodes with a resolved edge pointing at `id` (i.e. "what links here"),
    /// ordered by `importance DESC, id ASC`. Deduplicated: a node linking to
    /// `id` via multiple edges (a typed relation and a body mention, say)
    /// appears once.
    pub async fn backlinks(&self, id: &str) -> Result<Vec<NodeSummary>> {
        let rows = sqlx::query(
            "SELECT DISTINCT n.* FROM nodes n JOIN edges e ON e.from_id = n.id
             WHERE e.to_id = ? ORDER BY n.importance DESC, n.id ASC",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(summary_from_row).collect())
    }

    /// Vault-relative path of `id`'s markdown file, if it is indexed.
    pub async fn get_node_path(&self, id: &str) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT path FROM nodes WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(path,)| path))
    }

    /// Resolve `needle` to a node id: exact slug match (case-insensitive)
    /// first, then case-insensitive title match (smallest id wins on
    /// ties, for determinism when multiple notes share a title).
    pub async fn find_by_slug_or_title(&self, needle: &str) -> Result<Option<String>> {
        let needle_lower = needle.to_lowercase();

        if let Some((id,)) = sqlx::query_as::<_, (String,)>("SELECT id FROM nodes WHERE slug = ?")
            .bind(&needle_lower)
            .fetch_optional(&self.pool)
            .await?
        {
            return Ok(Some(id));
        }

        let row: Option<(String,)> =
            sqlx::query_as("SELECT id FROM nodes WHERE lower(title) = ? ORDER BY id ASC LIMIT 1")
                .bind(&needle_lower)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(id,)| id))
    }

    /// Total node count plus a per-status breakdown.
    pub async fn counts(&self) -> Result<(u64, BTreeMap<String, u64>)> {
        let (total,): (i64,) = sqlx::query_as("SELECT count(*) FROM nodes")
            .fetch_one(&self.pool)
            .await?;
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT status, count(*) FROM nodes GROUP BY status")
                .fetch_all(&self.pool)
                .await?;
        let by_status = rows
            .into_iter()
            .map(|(status, count)| (status, count as u64))
            .collect();
        Ok((total as u64, by_status))
    }

    /// Every quarantined (failed-to-parse) note, ordered by path.
    pub async fn quarantined(&self) -> Result<Vec<QuarantineEntry>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT path, error FROM quarantine ORDER BY path ASC")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(path, error)| QuarantineEntry { path, error })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_is_stable_and_sensitive() {
        assert_eq!(fnv1a("hello"), fnv1a("hello"));
        assert_ne!(fnv1a("hello"), fnv1a("hellp"));
    }

    #[test]
    fn snippet_collapses_whitespace_and_truncates() {
        let body = "a".repeat(200);
        assert_eq!(make_snippet(&body).chars().count(), 160);
        assert_eq!(make_snippet("hello   \n\n world"), "hello world");
    }

    #[test]
    fn resolve_links_prioritizes_slug_then_title_then_id() {
        let mut by_slug = HashMap::new();
        by_slug.insert("beta".to_string(), "mem_b".to_string());
        let mut by_title = HashMap::new();
        by_title.insert("gamma title".to_string(), "mem_c".to_string());
        let mut by_id = HashMap::new();
        by_id.insert("mem_d".to_string(), "mem_d".to_string());
        by_id.insert("mem_a".to_string(), "mem_a".to_string());

        let links = vec![
            RawLink {
                from_id: "mem_a".into(),
                target: "Beta".into(),
                rel: "mentions".into(),
                confidence: 1.0,
                origin: "body".into(),
            },
            RawLink {
                from_id: "mem_a".into(),
                target: "Gamma Title".into(),
                rel: "mentions".into(),
                confidence: 1.0,
                origin: "body".into(),
            },
            RawLink {
                from_id: "mem_a".into(),
                target: "mem_d".into(),
                rel: "mentions".into(),
                confidence: 1.0,
                origin: "body".into(),
            },
            RawLink {
                from_id: "mem_a".into(),
                target: "mem_a".into(),
                rel: "mentions".into(),
                confidence: 1.0,
                origin: "body".into(),
            },
            RawLink {
                from_id: "mem_a".into(),
                target: "Ghost".into(),
                rel: "mentions".into(),
                confidence: 1.0,
                origin: "body".into(),
            },
        ];

        let (resolved, unresolved) = resolve_links(&links, &by_slug, &by_title, &by_id);
        let to_ids: Vec<&str> = resolved.iter().map(|(_, to, ..)| to.as_str()).collect();
        assert!(to_ids.contains(&"mem_b"));
        assert!(to_ids.contains(&"mem_c"));
        assert!(to_ids.contains(&"mem_d"));
        assert_eq!(resolved.len(), 3); // self-edge dropped
        assert_eq!(unresolved, vec![("mem_a".to_string(), "Ghost".to_string())]);
    }
}
