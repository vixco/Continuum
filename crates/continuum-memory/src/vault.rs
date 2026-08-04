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
    Event, EventRange, Frontmatter, GraphData, GraphFilter, IndexStats, NewEvent, NodeStatus,
    NodeSummary, NodeType, Note, NoteDraft, Resolution, VaultInfo,
};
use crate::slug::slugify;
use crate::watcher::{self, VaultWatcher};

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
    /// Serializes the multi-step write sequences of `create` (slug-check →
    /// write → index), `save`, `delete`, `resolve_candidate`, and
    /// `sweep_expired` against each other, *within this process*. Without
    /// it, two concurrent callers could both pass a slug-uniqueness check
    /// before either has written, then race to atomically write the same
    /// path — the second write silently destroys the first note (the
    /// index then reads it as an id-rewrite of the same path and purges
    /// the first note's rows with no error surfaced anywhere). Reads
    /// (`get`, `search`, `graph`, `pending`, …) never take this lock and
    /// stay fully concurrent. One coarse lock for the whole vault is
    /// intentional: these are small, rare operations, and a single lock
    /// keeps the reasoning simple. It does NOT cover cross-process races
    /// (e.g. the desktop dashboard and the runtime both writing the same
    /// vault directory at once) — those are instead caught after the fact
    /// by the index's deterministic slug-collision quarantine.
    write_lock: tokio::sync::Mutex<()>,
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

/// Delete `index_path` and its `-wal`/`-shm` journal siblings, if present.
/// Used only by the corrupt-index fail-safe in [`Vault::open_index_with_failsafe`];
/// every path touched is a sibling of `index_path` (which always lives
/// inside `.continuum/`), so this never reaches outside that directory.
fn remove_index_files(index_path: &Path) -> Result<()> {
    let mut candidates = vec![index_path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut name = index_path.as_os_str().to_os_string();
        name.push(suffix);
        candidates.push(PathBuf::from(name));
    }
    for path in candidates {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| MemoryError::io(&path, e))?;
        }
    }
    Ok(())
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

        let index_path = continuum_dir.join("index.db");
        let index = Self::open_index_with_failsafe(&index_path).await?;
        let vault = Vault {
            dir: dir.to_path_buf(),
            index,
            opts,
            write_lock: tokio::sync::Mutex::new(()),
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

    /// Open the derived SQLite index at `index_path`, with a corrupt-index
    /// fail-safe. The index is disposable by design — the vault's markdown
    /// files are the sole source of truth, and the spec's "index is
    /// disposable" guarantee only holds if a broken index.db (truncated by
    /// a crash, corrupted by a full disk, etc.) can never strand a user out
    /// of their own vault.
    ///
    /// If `index_path` already exists on disk and [`Index::open`] fails
    /// against it, this logs a warning, deletes `index.db` and its
    /// `-wal`/`-shm` siblings (never anything outside `.continuum/`), and
    /// retries [`Index::open`] exactly once. The error is only propagated
    /// if that retry also fails. A missing `index_path` (first-ever open)
    /// skips the fail-safe entirely — [`Index::open`] failing there is a
    /// genuine error (e.g. an unwritable `.continuum/` directory), not
    /// corruption to recover from.
    async fn open_index_with_failsafe(index_path: &Path) -> Result<Index> {
        let existed = index_path.exists();
        match Index::open(index_path).await {
            Ok(index) => Ok(index),
            Err(e) if existed => {
                tracing::warn!(
                    layer = "memory",
                    component = "vault",
                    path = %index_path.display(),
                    error = %e,
                    "index open failed against an existing index.db; deleting and rebuilding"
                );
                remove_index_files(index_path)?;
                Index::open(index_path).await
            }
            Err(e) => Err(e),
        }
    }

    /// The vault's root directory, as given to `open`/`open_with`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Resolve `needle` to a node id by exact slug or case-insensitive
    /// title match. `pub(crate)` accessor onto the private `index` field —
    /// used by [`crate::migrate`] for idempotency checks (skip re-migrating
    /// a legacy record whose title already exists) without widening the
    /// public API beyond what the migration task needs.
    pub(crate) async fn find_by_slug_or_title(&self, needle: &str) -> Result<Option<String>> {
        self.index.find_by_slug_or_title(needle).await
    }

    /// Fully rebuild the derived index from the markdown files on disk.
    pub async fn rebuild_index(&self) -> Result<IndexStats> {
        self.index.rebuild(&self.dir).await
    }

    /// Reindex exactly the given files (e.g. paths reported by the
    /// file-watcher) rather than the whole vault. Returns the ids of notes
    /// that were successfully (re)indexed; removed/quarantined/skipped
    /// paths are not included.
    ///
    /// Uses [`Index::index_files`] to recompute the resolved edge graph
    /// once for the whole batch rather than once per file — a full
    /// `recompute_edges` per file in a multi-file batch was wasted work
    /// (`N-1` of the `N` recomputes get immediately superseded by the
    /// next file's), and edge resolution scans every `nodes`/`links` row,
    /// so it scales with vault size, not batch size.
    pub async fn reindex_paths(&self, paths: &[PathBuf]) -> Result<Vec<String>> {
        let outcomes = self.index.index_files(&self.dir, paths).await?;
        Ok(outcomes
            .into_iter()
            .filter_map(|o| match o {
                IndexOutcome::Indexed(id) => Some(id),
                _ => None,
            })
            .collect())
    }

    /// Start a debounced watch of this vault's directory for external
    /// edits (Obsidian, another editor, or a second Continuum process
    /// sharing the same vault). Batches of changed markdown paths arrive on
    /// [`VaultWatcher::rx`]; feed them to [`Vault::reindex_paths`] to keep
    /// the index in sync:
    ///
    /// ```ignore
    /// let mut w = vault.watch()?;
    /// while let Some(paths) = w.rx.recv().await {
    ///     vault.reindex_paths(&paths).await?;
    /// }
    /// ```
    ///
    /// Uses `opts.watcher_debounce_ms` as the debounce window. Dropping the
    /// returned [`VaultWatcher`] stops the watch.
    pub fn watch(&self) -> Result<VaultWatcher> {
        watcher::spawn(self.dir(), self.opts.watcher_debounce_ms)
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
    /// separators and `.`/`..` segments, falling back to `"note"` if
    /// nothing alphanumeric survives — it can never produce an empty
    /// slug), so the written file can never land outside the vault.
    ///
    /// Holds `write_lock` across the entire slug-check → write → index
    /// sequence so two concurrent `create` calls for the same title can
    /// never both pick the same slug (see the field doc on `write_lock`).
    pub async fn create(&self, draft: NoteDraft) -> Result<Note> {
        let _guard = self.write_lock.lock().await;

        if draft.title.trim().is_empty() {
            return Err(MemoryError::Invalid("title must not be empty".into()));
        }
        let base_slug = slugify(&draft.title);

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
        let _guard = self.write_lock.lock().await;
        self.save_locked(note, true).await
    }

    /// Like [`Vault::save`], but leaves `note.frontmatter.updated` exactly
    /// as given instead of re-stamping it to now. `pub(crate)` — the only
    /// legitimate caller today is [`crate::migrate`], which must carry a
    /// legacy record's original `updated_at` into the vault; from the
    /// public API's perspective every other write should always mean "this
    /// changed right now," so this bypass is deliberately not exposed
    /// outside the crate.
    pub(crate) async fn save_preserving_updated(&self, note: &Note) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        self.save_locked(note, false).await
    }

    /// Body of `save`/`save_preserving_updated`, factored out so callers
    /// that already hold `write_lock` for a larger multi-note sequence
    /// (`resolve_candidate`, `sweep_expired`) can persist a note without
    /// re-locking — `write_lock` is not reentrant, so calling either public
    /// entry point from inside one of those would deadlock. `stamp_updated`
    /// selects between the two callers' contracts: `true` re-stamps
    /// `updated = now` (every caller except migration), `false` writes
    /// `note.frontmatter.updated` verbatim.
    async fn save_locked(&self, note: &Note, stamp_updated: bool) -> Result<()> {
        let id = &note.frontmatter.id;
        let rel = self
            .index
            .get_node_path(id)
            .await?
            .ok_or_else(|| MemoryError::NotFound(id.clone()))?;
        let path = self.dir.join(&rel);

        let mut frontmatter = note.frontmatter.clone();
        if stamp_updated {
            frontmatter.updated = Some(Utc::now());
        }

        let content = render_document(&frontmatter, &note.body)?;
        atomic_write(&path, &content)?;
        self.index.index_file(&self.dir, &path).await?;
        Ok(())
    }

    /// Delete a note by id: removes the markdown file and its index entry.
    pub async fn delete(&self, id: &str) -> Result<()> {
        let _guard = self.write_lock.lock().await;
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
    /// before either note is saved, and `replaces` must not equal `id` (a
    /// note cannot supersede itself).
    ///
    /// Holds `write_lock` across the whole read-modify-write sequence
    /// (including both saves in the `Supersede` case) so a concurrent
    /// writer can never observe or interleave with a half-applied
    /// resolution.
    pub async fn resolve_candidate(&self, id: &str, r: Resolution) -> Result<()> {
        let _guard = self.write_lock.lock().await;

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
                self.save_locked(&note, true).await?;
            }
            Resolution::Reject => {
                note.frontmatter.status = NodeStatus::Rejected;
                self.save_locked(&note, true).await?;
            }
            Resolution::Supersede { replaces } => {
                if replaces == id {
                    return Err(MemoryError::Invalid(
                        "a note cannot supersede itself".into(),
                    ));
                }

                // Load the partner before writing anything: if it does not
                // exist, this call must fail cleanly with no side effects.
                let mut partner = self.get(&replaces).await?;

                note.frontmatter.status = NodeStatus::Confirmed;
                note.frontmatter.supersedes = Some(replaces.clone());
                partner.frontmatter.status = NodeStatus::Superseded;
                partner.frontmatter.superseded_by = Some(id.to_string());

                // Write the PARTNER first, then the candidate. If the
                // process dies (or a write errors) between the two, the
                // candidate is still sitting in the pending queue with
                // status `candidate` — visible, and retriable. The
                // opposite order (candidate first) would leave a
                // `confirmed` note pointing at a partner that has no idea
                // it was superseded: an invisible inconsistency nothing
                // would ever surface. A retried call after a partial
                // failure here is safe: it reloads the already-superseded
                // partner and writes back the same status/superseded_by
                // (an idempotent no-op change other than `updated`), then
                // completes by saving the candidate — no special-casing
                // needed, and no error on the "already superseded" partner.
                self.save_locked(&partner, true).await?;
                self.save_locked(&note, true).await?;
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
    ///
    /// Holds `write_lock` for the whole sweep (coarse, but sweeps are rare
    /// and small) so it can't interleave archive-writes with another
    /// writer. A note that vanishes between the query and its archive
    /// write (e.g. concurrently deleted) is skipped with a `debug!` log
    /// rather than aborting the rest of the batch.
    pub async fn sweep_expired(&self) -> Result<u64> {
        let _guard = self.write_lock.lock().await;

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
                match self.get(&id).await {
                    Ok(mut note) => {
                        note.frontmatter.status = NodeStatus::Archived;
                        self.save_locked(&note, true).await?;
                        archived += 1;
                    }
                    Err(MemoryError::NotFound(_)) => {
                        tracing::debug!(
                            layer = "memory",
                            component = "vault",
                            id = %id,
                            "sweep_expired: note vanished mid-sweep, skipping"
                        );
                    }
                    Err(e) => return Err(e),
                }
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

    /// Append a single event to the timeline. If `ts` is `None`, uses the
    /// current UTC time. Timestamps are stored as RFC3339 strings. Events
    /// are append-only and do not require `write_lock`.
    pub async fn append_event(&self, event: NewEvent) -> Result<()> {
        let ts = event.ts.unwrap_or_else(Utc::now).to_rfc3339();
        sqlx::query(
            "INSERT INTO events(ts, kind, text, project, node_id, \"ref\")
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&ts)
        .bind(&event.kind)
        .bind(&event.text)
        .bind(&event.project)
        .bind(&event.node_id)
        .bind(&event.reference)
        .execute(self.index.pool())
        .await?;

        Ok(())
    }

    /// Query events within an optional time range, ordered ascending by
    /// timestamp — *unless* `range.since_id` is set, in which case the
    /// result is ordered ascending by `id` instead (see
    /// [`EventRange::since_id`]'s doc comment for why: a `since_id` caller
    /// is polling a watermark under a `LIMIT`, and needs the fetched batch
    /// to be *contiguous by id* — the lowest ids past the watermark — not
    /// whichever ids happen to sort earliest by `ts`, which can skip a
    /// lower id whose `ts` is backdated ahead of a higher id's. `limit`
    /// defaults to 500 if not specified; `since` and `until` are inclusive
    /// when provided.
    pub async fn events(&self, range: &EventRange) -> Result<Vec<Event>> {
        let limit = range.limit.unwrap_or(500);
        let mut query =
            "SELECT id, ts, kind, text, project, node_id, \"ref\" FROM events WHERE 1=1"
                .to_string();

        if range.since.is_some() {
            query.push_str(" AND ts >= ?");
        }
        if range.until.is_some() {
            query.push_str(" AND ts <= ?");
        }
        if range.since_id.is_some() {
            query.push_str(" AND id > ?");
        }

        if range.since_id.is_some() {
            query.push_str(" ORDER BY id ASC LIMIT ?");
        } else {
            query.push_str(" ORDER BY ts ASC LIMIT ?");
        }

        let mut qry = sqlx::query_as::<
            _,
            (
                i64,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
            ),
        >(&query);
        if let Some(since) = range.since {
            qry = qry.bind(since.to_rfc3339());
        }
        if let Some(until) = range.until {
            qry = qry.bind(until.to_rfc3339());
        }
        if let Some(since_id) = range.since_id {
            qry = qry.bind(since_id);
        }
        qry = qry.bind(i64::from(limit));

        let rows = qry.fetch_all(self.index.pool()).await?;
        Ok(rows
            .into_iter()
            .map(|(id, ts, kind, text, project, node_id, reference)| Event {
                id,
                ts,
                kind,
                text,
                project,
                node_id,
                reference,
            })
            .collect())
    }

    /// Delete events with timestamp older than `now - keep_days` days.
    /// Returns the number of deleted rows. Timestamps are parsed as RFC3339.
    pub async fn prune_events(&self, keep_days: u32) -> Result<u64> {
        let cutoff = (Utc::now() - chrono::Duration::days(i64::from(keep_days))).to_rfc3339();
        let result = sqlx::query("DELETE FROM events WHERE ts < ?")
            .bind(&cutoff)
            .execute(self.index.pool())
            .await?;

        let deleted = result.rows_affected();
        if deleted > 0 {
            tracing::info!(
                layer = "memory",
                component = "vault",
                deleted,
                keep_days,
                "pruned events"
            );
        }

        Ok(deleted)
    }
}
