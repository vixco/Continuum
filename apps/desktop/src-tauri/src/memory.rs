//! Memory vault state + Tauri commands for the dashboard. The vault opens
//! lazily (first command) so a broken vault degrades the Memory tab, not
//! the whole app. Layer: memory, component: dashboard.

use std::path::PathBuf;
use std::sync::Arc;

use continuum_memory::{
    Event, EventRange, GraphData, GraphFilter, IndexStats, MigrationReport, NodeSummary, Note,
    NoteDraft, Resolution, Vault, VaultOptions,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_opener::OpenerExt;

/// Topic for live vault-change pushes to the frontend.
pub const MEMORY_EVENT_TOPIC: &str = "continuum:memory";

/// Payload for the `continuum:memory` Tauri event. `kind` is `"changed"`
/// after an incremental reindex (watcher batch) or `"rebuilt"` right after
/// the watcher bridge starts (announcing the vault's initial index state).
#[derive(Clone, Serialize)]
pub struct MemoryEventPayload {
    pub kind: String, // "changed" | "rebuilt"
    pub ids: Vec<String>,
}

/// [`continuum_memory::VaultInfo`] plus a flag for whether the pre-vault
/// `semantic.sqlite` key/value store still exists on disk (drives the
/// "Migrate legacy memory" prompt in the dashboard).
#[derive(Serialize)]
pub struct VaultInfoDto {
    #[serde(flatten)]
    pub info: continuum_memory::VaultInfo,
    pub legacy_semantic_present: bool,
}

/// Shared dashboard handle onto the memory vault. The vault itself is
/// opened lazily on first use (see [`MemoryState::vault`]) rather than at
/// construction time, so a broken/locked vault directory surfaces as a
/// per-command error instead of failing app startup.
pub struct MemoryState {
    vault_dir: PathBuf,
    legacy_semantic_db: PathBuf,
    vault: tokio::sync::OnceCell<Arc<Vault>>,
    pub opts: VaultOptions,
}

impl MemoryState {
    /// Builds a new, not-yet-opened vault handle over `vault_dir`, tracking
    /// `legacy_semantic_db` for the migration-available flag. Call
    /// [`MemoryState::with_opts`] to override the default [`VaultOptions`]
    /// from config.
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

    /// Opens the vault on first call and returns the cached handle on every
    /// call after that. Errors are mapped to [`continuum_memory::MemoryError::user_message`]
    /// since every caller (commands, the health probe, the watcher bridge)
    /// only needs a presentable string, never the typed error.
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

// --- Inner functions: the actual logic, free of Tauri types so they're
// directly unit-testable (see the `tests` module below). Every
// `#[tauri::command]` wrapper further down is a thin pass-through onto one
// of these.

pub(crate) async fn graph_inner(
    state: &MemoryState,
    filter: GraphFilter,
) -> Result<GraphData, String> {
    let vault = state.vault().await?;
    vault.graph(&filter).await.map_err(|e| e.user_message())
}

pub(crate) async fn search_inner(
    state: &MemoryState,
    query: &str,
    limit: Option<u32>,
) -> Result<Vec<NodeSummary>, String> {
    let vault = state.vault().await?;
    vault
        .search(query, limit.unwrap_or(20))
        .await
        .map_err(|e| e.user_message())
}

pub(crate) async fn get_note_inner(state: &MemoryState, id: &str) -> Result<Note, String> {
    let vault = state.vault().await?;
    vault.get(id).await.map_err(|e| e.user_message())
}

pub(crate) async fn create_note_inner(
    state: &MemoryState,
    draft: NoteDraft,
) -> Result<Note, String> {
    let vault = state.vault().await?;
    vault.create(draft).await.map_err(|e| e.user_message())
}

/// Persists `note`. Requires `note.frontmatter.id` to be non-empty — an
/// empty id can only mean the caller meant to `create` instead of `save`,
/// and would otherwise surface as a confusing "note not found" from the
/// vault layer.
pub(crate) async fn save_note_inner(state: &MemoryState, note: Note) -> Result<(), String> {
    if note.frontmatter.id.trim().is_empty() {
        return Err("note id must not be empty".into());
    }
    let vault = state.vault().await?;
    vault.save(&note).await.map_err(|e| e.user_message())
}

pub(crate) async fn delete_note_inner(state: &MemoryState, id: &str) -> Result<(), String> {
    let vault = state.vault().await?;
    vault.delete(id).await.map_err(|e| e.user_message())
}

pub(crate) async fn resolve_inner(
    state: &MemoryState,
    id: &str,
    resolution: Resolution,
) -> Result<(), String> {
    let vault = state.vault().await?;
    vault
        .resolve_candidate(id, resolution)
        .await
        .map_err(|e| e.user_message())
}

pub(crate) async fn pending_inner(state: &MemoryState) -> Result<Vec<NodeSummary>, String> {
    let vault = state.vault().await?;
    vault.pending().await.map_err(|e| e.user_message())
}

pub(crate) async fn events_inner(
    state: &MemoryState,
    range: EventRange,
) -> Result<Vec<Event>, String> {
    let vault = state.vault().await?;
    vault.events(&range).await.map_err(|e| e.user_message())
}

pub(crate) async fn vault_info_inner(state: &MemoryState) -> Result<VaultInfoDto, String> {
    let vault = state.vault().await?;
    let info = vault.info().await.map_err(|e| e.user_message())?;
    Ok(VaultInfoDto {
        info,
        legacy_semantic_present: state.legacy_semantic_db.exists(),
    })
}

pub(crate) async fn migrate_inner(state: &MemoryState) -> Result<MigrationReport, String> {
    let vault = state.vault().await?;
    continuum_memory::migrate_legacy_semantic(&vault, &state.legacy_semantic_db)
        .await
        .map_err(|e| e.user_message())
}

pub(crate) async fn rebuild_inner(state: &MemoryState) -> Result<IndexStats, String> {
    let vault = state.vault().await?;
    vault.rebuild_index().await.map_err(|e| e.user_message())
}

// --- Tauri commands ---

/// Bounded graph snapshot of the vault, filtered by `filter`.
#[tauri::command]
pub async fn memory_graph(
    state: State<'_, Arc<MemoryState>>,
    filter: GraphFilter,
) -> Result<GraphData, String> {
    graph_inner(&state, filter).await
}

/// Full-text search over vault titles/bodies/tags.
#[tauri::command]
pub async fn memory_search(
    state: State<'_, Arc<MemoryState>>,
    query: String,
    limit: Option<u32>,
) -> Result<Vec<NodeSummary>, String> {
    search_inner(&state, &query, limit).await
}

/// Loads a single note by id.
#[tauri::command]
pub async fn memory_get_note(
    state: State<'_, Arc<MemoryState>>,
    id: String,
) -> Result<Note, String> {
    get_note_inner(&state, &id).await
}

/// Creates a new note from `draft`.
#[tauri::command]
pub async fn memory_create_note(
    state: State<'_, Arc<MemoryState>>,
    draft: NoteDraft,
) -> Result<Note, String> {
    create_note_inner(&state, draft).await
}

/// Persists edits to an existing note.
#[tauri::command]
pub async fn memory_save_note(
    state: State<'_, Arc<MemoryState>>,
    note: Note,
) -> Result<(), String> {
    save_note_inner(&state, note).await
}

/// Deletes a note by id.
#[tauri::command]
pub async fn memory_delete_note(
    state: State<'_, Arc<MemoryState>>,
    id: String,
) -> Result<(), String> {
    delete_note_inner(&state, &id).await
}

/// Confirms, rejects, or supersedes a candidate note.
#[tauri::command]
pub async fn memory_resolve_candidate(
    state: State<'_, Arc<MemoryState>>,
    id: String,
    resolution: Resolution,
) -> Result<(), String> {
    resolve_inner(&state, &id, resolution).await
}

/// Lists candidate notes awaiting review.
#[tauri::command]
pub async fn memory_pending(
    state: State<'_, Arc<MemoryState>>,
) -> Result<Vec<NodeSummary>, String> {
    pending_inner(&state).await
}

/// Queries the vault's timeline events within an optional range.
#[tauri::command]
pub async fn memory_events(
    state: State<'_, Arc<MemoryState>>,
    range: EventRange,
) -> Result<Vec<Event>, String> {
    events_inner(&state, range).await
}

/// Vault health/summary for the dashboard's Memory tab.
#[tauri::command]
pub async fn memory_vault_info(state: State<'_, Arc<MemoryState>>) -> Result<VaultInfoDto, String> {
    vault_info_inner(&state).await
}

/// One-shot migration of the legacy `semantic.sqlite` fact store into the
/// vault.
#[tauri::command]
pub async fn memory_migrate_legacy(
    state: State<'_, Arc<MemoryState>>,
) -> Result<MigrationReport, String> {
    migrate_inner(&state).await
}

/// Forces a full rebuild of the derived SQLite index from the markdown on
/// disk.
#[tauri::command]
pub async fn memory_rebuild_index(
    state: State<'_, Arc<MemoryState>>,
) -> Result<IndexStats, String> {
    rebuild_inner(&state).await
}

/// Opens the vault directory in the OS file explorer. Opens the vault
/// first (rather than just reading the configured path) so the directory
/// is guaranteed to exist — `Vault::open_with` creates it on first use.
#[tauri::command]
pub async fn memory_open_vault(
    state: State<'_, Arc<MemoryState>>,
    app: AppHandle,
) -> Result<(), String> {
    let vault = state.vault().await?;
    let dir = vault.dir().to_path_buf();
    app.opener()
        .open_path(dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Bridges the vault's file-watcher into the `continuum:memory` Tauri
/// event so the dashboard sees external edits (Obsidian, another editor,
/// or the headless `continuum` runtime writing the same vault) live,
/// without polling. Spawned once at startup after the Tauri app handle
/// exists (see `main.rs`). A vault or watcher that fails to start just
/// means live updates are unavailable this session — every command still
/// works against the vault directly, so this failure is logged and
/// swallowed rather than propagated.
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
        let _ = app.emit(
            MEMORY_EVENT_TOPIC,
            MemoryEventPayload {
                kind: "rebuilt".into(),
                ids: vec![],
            },
        );
        while let Some(paths) = watcher.rx.recv().await {
            match vault.reindex_paths(&paths).await {
                Ok(ids) => {
                    let _ = app.emit(
                        MEMORY_EVENT_TOPIC,
                        MemoryEventPayload {
                            kind: "changed".into(),
                            ids,
                        },
                    );
                }
                Err(e) => tracing::warn!(layer = "memory", component = "dashboard",
                    error = %e.user_message(), "reindex after watch event failed"),
            }
        }
    });
}

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
