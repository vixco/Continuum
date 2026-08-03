//! The `Vault` façade: the single entry point the runtime and dashboard use
//! to read and write the memory vault. It wraps a vault directory
//! (markdown source of truth) and its derived SQLite
//! [`Index`](crate::index::Index) behind a CRUD + graph + candidate
//! resolution API. Every write to a note file is atomic (temp file +
//! rename) so a crash mid-write never corrupts an existing note; the index
//! is kept in sync incrementally by reindexing the exact file just written
//! rather than a full rebuild per call. Layer: memory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ulid::Ulid;

use crate::error::{MemoryError, Result};
use crate::frontmatter::{parse_document, render_document};
use crate::index::{Index, IndexOutcome};
use crate::model::{
    Frontmatter, GraphData, GraphFilter, IndexStats, NodeStatus, NodeSummary, NodeType, Note,
    NoteDraft, Resolution, VaultInfo,
};
use crate::slug::slugify;

/// Tunables for a [`Vault`]. Every field is read from config and
/// overridable via the dashboard (per house rule: never hardcode a
/// threshold).
#[derive(Debug, Clone)]
pub struct VaultOptions {
    /// Debounce window for the file-watcher (Task 7), in milliseconds.
    pub watcher_debounce_ms: u64,
    /// Default cap on nodes returned by `graph`/`neighbors` when the
    /// caller's filter does not specify its own `limit`.
    pub graph_max_nodes: u32,
}

impl Default for VaultOptions {
    fn default() -> Self {
        Self {
            watcher_debounce_ms: 500,
            graph_max_nodes: 1500,
        }
    }
}

/// Façade over a vault directory: markdown notes on disk (source of truth)
/// plus the derived SQLite index that makes them queryable.
pub struct Vault {
    dir: PathBuf,
    index: Index,
    opts: VaultOptions,
}

/// Atomically write `content` to `path`: write to a `.tmp` sibling, then
/// `rename` over the target. `std::fs::rename` replaces the destination on
/// both Windows and Unix, so a reader never observes a partially written
/// file.
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp_path = path.with_extension("md.tmp");
    std::fs::write(&tmp_path, content).map_err(|e| MemoryError::io(&tmp_path, e))?;
    std::fs::rename(&tmp_path, path).map_err(|e| MemoryError::io(path, e))?;
    Ok(())
}

/// Recursively delete any stray `*.tmp` file under `dir`, skipping
/// `.continuum` (the index lives there, never note tmp files). A stray tmp
/// file means a prior process died between the write and the rename; it is
/// always safe to discard since the rename it was waiting for never
/// happened, so the destination file (if any) is still the last good copy.
fn clean_stray_tmp_files(dir: &Path) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| MemoryError::io(dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| MemoryError::io(dir, e))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| MemoryError::io(&path, e))?;
        if file_type.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some(".continuum") {
                continue;
            }
            clean_stray_tmp_files(&path)?;
        } else if file_type.is_file() && path.extension().and_then(|e| e.to_str()) == Some("tmp") {
            std::fs::remove_file(&path).map_err(|e| MemoryError::io(&path, e))?;
            tracing::debug!(
                layer = "memory",
                component = "vault",
                path = %path.display(),
                "removed stray tmp file"
            );
        }
    }
    Ok(())
}

/// Lowercased file stem of `path`, used as the note's slug.
fn slug_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

impl Vault {
    /// Open (creating if missing) the vault at `dir` with default
    /// [`VaultOptions`].
    pub async fn open(dir: &Path) -> Result<Vault> {
        Self::open_with(dir, VaultOptions::default()).await
    }

    /// Open (creating if missing) the vault at `dir`. Creates every
    /// [`NodeType`] folder plus `.continuum`, cleans up any stray `*.tmp`
    /// files left by a crashed write, opens the derived index, and always
    /// performs a full [`Index::rebuild`] (the spec's "index is disposable"
    /// guarantee holds only if every open re-derives it from the markdown).
    ///
    /// `dir` is stored as given, not canonicalized — canonicalizing would
    /// break UNC paths on Windows.
    pub async fn open_with(dir: &Path, opts: VaultOptions) -> Result<Vault> {
        std::fs::create_dir_all(dir).map_err(|e| MemoryError::io(dir, e))?;
        for ty in NodeType::ALL {
            let folder = dir.join(ty.folder());
            std::fs::create_dir_all(&folder).map_err(|e| MemoryError::io(&folder, e))?;
        }
        let continuum_dir = dir.join(".continuum");
        std::fs::create_dir_all(&continuum_dir).map_err(|e| MemoryError::io(&continuum_dir, e))?;

        clean_stray_tmp_files(dir)?;

        let index = Index::open(&continuum_dir.join("index.db")).await?;
        let vault = Vault {
            dir: dir.to_path_buf(),
            index,
            opts,
        };
        let stats = vault.rebuild_index().await?;

        tracing::info!(
            layer = "memory",
            component = "vault",
            dir = %vault.dir.display(),
            indexed = stats.indexed,
            quarantined = stats.quarantined,
            "vault opened"
        );
        Ok(vault)
    }

    /// The vault's root directory, as given to `open`/`open_with`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Fully rebuild the derived index from the markdown files on disk.
    pub async fn rebuild_index(&self) -> Result<IndexStats> {
        self.index.rebuild(&self.dir).await
    }

    /// Reindex exactly the given files (e.g. paths reported by the
    /// file-watcher) rather than the whole vault. Returns the ids of notes
    /// that were successfully (re)indexed; removed/quarantined/skipped
    /// paths are not included.
    pub async fn reindex_paths(&self, paths: &[PathBuf]) -> Result<Vec<String>> {
        let mut changed = Vec::new();
        for path in paths {
            if let IndexOutcome::Indexed(id) = self.index.index_file(&self.dir, path).await? {
                changed.push(id);
            }
        }
        Ok(changed)
    }

    /// Find a slug based on `base` that is not already used by an indexed
    /// node nor by an existing file in `folder`. Mirrors
    /// [`crate::slug::unique_slug`]'s append-a-counter algorithm, but async
    /// so it can check the index's `nodes` table between attempts.
    async fn find_unique_slug(&self, base: &str, folder: &Path) -> Result<String> {
        let mut candidate = base.to_string();
        let mut n = 2u32;
        loop {
            let taken_in_index: Option<(i64,)> =
                sqlx::query_as("SELECT 1 FROM nodes WHERE slug = ?")
                    .bind(&candidate)
                    .fetch_optional(self.index.pool())
                    .await?;
            let file_exists = folder.join(format!("{candidate}.md")).exists();
            if taken_in_index.is_none() && !file_exists {
                return Ok(candidate);
            }
            candidate = format!("{base}-{n}");
            n += 1;
        }
    }

    /// Create a new note from `draft`: validates the title, derives a
    /// unique slug, assigns a fresh `mem_<ulid>` id, writes the markdown
    /// file atomically, and indexes it. `slugify` always maps onto a
    /// filename-safe slug inside the type folder (it strips path
    /// separators and `.`/`..` segments), so the written file can never
    /// land outside the vault.
    pub async fn create(&self, draft: NoteDraft) -> Result<Note> {
        if draft.title.trim().is_empty() {
            return Err(MemoryError::Invalid("title must not be empty".into()));
        }
        let base_slug = slugify(&draft.title);
        if base_slug.is_empty() {
            return Err(MemoryError::Invalid("title must not be empty".into()));
        }

        let folder = self.dir.join(draft.node_type.folder());
        let slug = self.find_unique_slug(&base_slug, &folder).await?;
        let id = format!("mem_{}", Ulid::new().to_string().to_lowercase());
        let now = Utc::now();

        let frontmatter = Frontmatter {
            id: id.clone(),
            node_type: draft.node_type,
            title: draft.title.clone(),
            status: draft.status,
            project: draft.project.clone(),
            confidence: draft.confidence,
            importance: draft.importance,
            source: draft.source,
            source_ref: draft.source_ref.clone(),
            sensitivity: draft.sensitivity,
            created: now,
            updated: None,
            last_used: None,
            expires: None,
            supersedes: None,
            superseded_by: None,
            relations: draft.relations.clone(),
            tags: draft.tags.clone(),
            extra: BTreeMap::new(),
        };

        let path = folder.join(format!("{slug}.md"));
        let content = render_document(&frontmatter, &draft.body)?;
        atomic_write(&path, &content)?;
        self.index.index_file(&self.dir, &path).await?;

        tracing::info!(
            layer = "memory",
            component = "vault",
            id = %id,
            slug = %slug,
            "note created"
        );

        Ok(Note {
            frontmatter,
            body: draft.body,
            path,
            slug,
            backlinks: vec![],
        })
    }

    /// Load a note by id: resolves its path via the index, reads and
    /// parses the markdown file, and attaches its current backlinks.
    pub async fn get(&self, id: &str) -> Result<Note> {
        let rel = self
            .index
            .get_node_path(id)
            .await?
            .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
        let path = self.dir.join(&rel);
        let content = std::fs::read_to_string(&path).map_err(|e| MemoryError::io(&path, e))?;
        let parsed = parse_document(&content)?;
        let backlinks = self.index.backlinks(id).await?;
        Ok(Note {
            frontmatter: parsed.frontmatter,
            body: parsed.body,
            path: path.clone(),
            slug: slug_of(&path),
            backlinks,
        })
    }

    /// Persist `note` back to disk by id. The file stays at its existing
    /// indexed path even if `note.frontmatter.node_type` changed — the
    /// folder is a convention, the frontmatter is truth; moving the file on
    /// a type change is a deliberate non-goal of this call. Always stamps
    /// `updated = now`.
    pub async fn save(&self, note: &Note) -> Result<()> {
        let id = &note.frontmatter.id;
        let rel = self
            .index
            .get_node_path(id)
            .await?
            .ok_or_else(|| MemoryError::NotFound(id.clone()))?;
        let path = self.dir.join(&rel);

        let mut frontmatter = note.frontmatter.clone();
        frontmatter.updated = Some(Utc::now());

        let content = render_document(&frontmatter, &note.body)?;
        atomic_write(&path, &content)?;
        self.index.index_file(&self.dir, &path).await?;
        Ok(())
    }

    /// Delete a note by id: removes the markdown file and its index entry.
    pub async fn delete(&self, id: &str) -> Result<()> {
        let rel = self
            .index
            .get_node_path(id)
            .await?
            .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
        let path = self.dir.join(&rel);
        std::fs::remove_file(&path).map_err(|e| MemoryError::io(&path, e))?;
        self.index.remove_path(&self.dir, &path).await?;
        tracing::info!(layer = "memory", component = "vault", id = %id, "note deleted");
        Ok(())
    }

    /// Full-text search over titles/bodies/tags.
    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<NodeSummary>> {
        self.index.search(q, limit).await
    }

    /// Bounded graph snapshot matching `f`, defaulting the node cap to
    /// `opts.graph_max_nodes` when `f.limit` is unset.
    pub async fn graph(&self, f: &GraphFilter) -> Result<GraphData> {
        self.index.graph(f, self.opts.graph_max_nodes).await
    }

    /// BFS neighborhood of `id` up to `depth` hops (capped at 2).
    pub async fn neighbors(&self, id: &str, depth: u8) -> Result<GraphData> {
        self.index
            .neighbors(id, depth, self.opts.graph_max_nodes)
            .await
    }

    /// Candidate nodes awaiting review.
    pub async fn pending(&self) -> Result<Vec<NodeSummary>> {
        self.index.pending().await
    }

    /// Resolve a candidate note. Errors with [`MemoryError::Invalid`] if
    /// `id` does not currently have [`NodeStatus::Candidate`] status. For
    /// [`Resolution::Supersede`], the superseded partner is loaded (and
    /// must exist — a missing partner errors before anything is written)
    /// before either note is saved.
    pub async fn resolve_candidate(&self, id: &str, r: Resolution) -> Result<()> {
        let mut note = self.get(id).await?;
        if note.frontmatter.status != NodeStatus::Candidate {
            return Err(MemoryError::Invalid(format!(
                "note {id} is not a candidate (status: {})",
                note.frontmatter.status.as_str()
            )));
        }

        match r {
            Resolution::Confirm => {
                note.frontmatter.status = NodeStatus::Confirmed;
                self.save(&note).await?;
            }
            Resolution::Reject => {
                note.frontmatter.status = NodeStatus::Rejected;
                self.save(&note).await?;
            }
            Resolution::Supersede { replaces } => {
                // Load the partner before writing anything: if it does not
                // exist, this call must fail cleanly with no side effects.
                let mut partner = self.get(&replaces).await?;

                note.frontmatter.status = NodeStatus::Confirmed;
                note.frontmatter.supersedes = Some(replaces.clone());
                partner.frontmatter.status = NodeStatus::Superseded;
                partner.frontmatter.superseded_by = Some(id.to_string());

                self.save(&note).await?;
                self.save(&partner).await?;
            }
        }

        tracing::info!(layer = "memory", component = "vault", id = %id, "candidate resolved");
        Ok(())
    }

    /// Vault health/summary for the dashboard: note count, per-status
    /// breakdown, quarantined files, and the last full-index timestamp.
    pub async fn info(&self) -> Result<VaultInfo> {
        let (note_count, counts_by_status) = self.index.counts().await?;
        let quarantined = self.index.quarantined().await?;
        let last_full_index_at: Option<(String,)> =
            sqlx::query_as("SELECT value FROM meta WHERE key = 'last_full_index_at'")
                .fetch_optional(self.index.pool())
                .await?;
        Ok(VaultInfo {
            path: self.dir.display().to_string(),
            note_count,
            counts_by_status,
            quarantined,
            last_full_index_at: last_full_index_at.map(|(v,)| v),
        })
    }

    /// Stamp `last_used = now` on each id, saving each note in turn.
    /// Missing ids are skipped with a `debug!` log rather than failing the
    /// whole batch (a caller touching a batch of recently-referenced ids
    /// shouldn't be tripped up by one that was deleted concurrently).
    pub async fn touch_last_used(&self, ids: &[String]) -> Result<()> {
        let now = Utc::now();
        for id in ids {
            match self.get(id).await {
                Ok(mut note) => {
                    note.frontmatter.last_used = Some(now);
                    self.save(&note).await?;
                }
                Err(MemoryError::NotFound(_)) => {
                    tracing::debug!(
                        layer = "memory",
                        component = "vault",
                        id = %id,
                        "touch_last_used: id not found, skipping"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Archive every confirmed/candidate note whose `expires` timestamp has
    /// passed. Returns the number archived. Timestamps are parsed and
    /// compared as `DateTime<Utc>`, not string-compared — RFC3339 strings
    /// only sort correctly when every note uses the exact same
    /// zero-padding and offset representation, which hand-edited
    /// frontmatter cannot be trusted to preserve.
    pub async fn sweep_expired(&self) -> Result<u64> {
        let now = Utc::now();
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, expires FROM nodes \
             WHERE expires IS NOT NULL AND status IN ('confirmed', 'candidate')",
        )
        .fetch_all(self.index.pool())
        .await?;

        let mut archived = 0u64;
        for (id, expires_raw) in rows {
            let expires = match DateTime::parse_from_rfc3339(&expires_raw) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(e) => {
                    tracing::warn!(
                        layer = "memory",
                        component = "vault",
                        id = %id,
                        error = %e,
                        "sweep_expired: unparsable expires timestamp, skipping"
                    );
                    continue;
                }
            };
            if expires < now {
                let mut note = self.get(&id).await?;
                note.frontmatter.status = NodeStatus::Archived;
                self.save(&note).await?;
                archived += 1;
            }
        }

        if archived > 0 {
            tracing::info!(
                layer = "memory",
                component = "vault",
                archived,
                "swept expired notes"
            );
        }
        Ok(archived)
    }
}
