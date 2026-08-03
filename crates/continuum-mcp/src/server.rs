//! # ContinuumMcpServer
//!
//! The MCP server handler. Holds shared runtime state (store handles, HTTP
//! client, allowlist config) and routes tool calls via the rmcp
//! `#[tool_router]` macro infrastructure.
//!
//! ## Lazy store initialization
//!
//! Because continuum-mcp is spawned fresh on every orchestrator wake, we avoid
//! opening LanceDB + loading the fastembed model (≈200–300 ms) unless a tool
//! actually needs episodic memory. SQLite is light but is also lazy for
//! symmetry. First tool call that needs a store pays the open cost.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use continuum_core::memory::{episodic::EpisodicStore, semantic::SemanticStore};
use continuum_memory::Vault;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, ErrorData as McpError, Implementation, ProtocolVersion,
        ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router, ServerHandler,
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Mutex, MutexGuard, OnceCell};

use crate::tools::fs::{FsListDirRequest, FsReadFileRequest};
use crate::tools::memory::{
    self as memtool, EpisodicHit, FactView, MemoryGetFactRequest, MemoryListFactsRequest,
    MemoryQueryEpisodicRequest, MemorySetFactRequest, MemoryVaultGetRequest,
    MemoryVaultResolveRequest, MemoryVaultSaveRequest, MemoryVaultSearchRequest,
    MemoryWipeAllRequest, SetFactResponse,
};
use crate::tools::repair::{
    self as repairtool, EscalateRequest, ReinstallRequest, RestartRequest, RollbackRequest,
    TestRequest,
};
use crate::tools::system::{self as systool, NotificationRequest};
use crate::tools::web::WebFetchRequest;
use crate::tools::workers::{
    self as workertool, SpawnWorkerRequest, WorkerIdRequest, WorkerListRequest, WorkerWaitRequest,
};

/// Shared server state. Held inside an `Arc` so the handler can derive `Clone`
/// cheaply.
pub(crate) struct ServerState {
    pub(crate) data_dir: PathBuf,
    #[allow(dead_code)]
    pub(crate) http: reqwest::Client,
    pub(crate) fs_extra_paths: Vec<PathBuf>,
    pub(crate) semantic: OnceCell<SemanticStore>,
    pub(crate) episodic: OnceCell<Mutex<EpisodicStore>>,
    pub(crate) vault: OnceCell<Vault>,
}

/// The main MCP server. Cloneable because rmcp can fan out the handler across
/// concurrent tool invocations.
#[derive(Clone)]
pub struct ContinuumMcpServer {
    pub(crate) state: Arc<ServerState>,
    #[allow(dead_code)] // populated by the #[tool_router] macro's dispatch table
    tool_router: ToolRouter<ContinuumMcpServer>,
}

// ---------------------------------------------------------------------------
// Construction + lazy store access
// ---------------------------------------------------------------------------

#[tool_router]
impl ContinuumMcpServer {
    /// Constructs a new server with all tools registered. Stores are opened
    /// lazily on first use to keep startup under ~20 ms.
    pub async fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir).with_context(|| {
            format!(
                "Failed to create Continuum data directory at {}",
                data_dir.display()
            )
        })?;

        // Load [mcp] config — non-fatal, falls back to defaults on any error.
        let mcp_cfg = crate::config::load(&data_dir);

        // web_fetch is the only consumer of `http`, and redirect-SSRF is a
        // real concern (a host we DNS-verified as public could redirect us to
        // a private address). Redirects are therefore disabled entirely; the
        // tool surfaces 3xx responses as `Redirected` errors so the caller
        // can re-invoke against the target URL directly.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("continuum-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Failed to build reqwest client")?;

        Ok(Self {
            state: Arc::new(ServerState {
                data_dir,
                http,
                fs_extra_paths: mcp_cfg.fs.extra_paths,
                semantic: OnceCell::new(),
                episodic: OnceCell::new(),
                vault: OnceCell::new(),
            }),
            tool_router: Self::tool_router(),
        })
    }

    /// Builds the filesystem allowlist from all current sources:
    /// (1) `data_dir`, (2) config `[mcp.fs].extra_paths`,
    /// (3) semantic facts under `project.*.dir`. Called on every fs_* tool
    /// invocation so newly-set project facts take effect immediately.
    pub(crate) async fn compute_fs_allowlist(&self) -> crate::allowlist::AllowlistConfig {
        let mut project_dirs: Vec<String> = Vec::new();
        if let Ok(s) = self.semantic().await {
            if let Ok(facts) = s.query_facts_by_prefix("project.").await {
                for f in facts {
                    if f.key.ends_with(".dir") && !f.value.is_empty() {
                        project_dirs.push(f.value);
                    }
                }
            }
        }
        crate::tools::fs::build_allowlist(
            &self.state.data_dir,
            &self.state.fs_extra_paths,
            &project_dirs,
        )
    }

    /// Returns a reference to the semantic store, opening it if not yet opened.
    pub(crate) async fn semantic(&self) -> Result<&SemanticStore> {
        self.state
            .semantic
            .get_or_try_init(|| async {
                let path = self.state.data_dir.join("semantic.sqlite");
                SemanticStore::open(&path.to_string_lossy())
                    .await
                    .with_context(|| format!("Failed to open SemanticStore at {}", path.display()))
            })
            .await
    }

    /// Locks and returns the episodic store, opening it if not yet opened.
    /// The guard releases when dropped — keep the scope short.
    pub(crate) async fn episodic(&self) -> Result<MutexGuard<'_, EpisodicStore>> {
        let cell = self
            .state
            .episodic
            .get_or_try_init(|| async {
                let path = self.state.data_dir.join("episodic_db");
                EpisodicStore::open(&path.to_string_lossy())
                    .await
                    .with_context(|| format!("Failed to open EpisodicStore at {}", path.display()))
                    .map(Mutex::new)
            })
            .await?;
        Ok(cell.lock().await)
    }

    /// Resolves the memory vault directory: the `CONTINUUM_VAULT_DIR`
    /// environment variable if set to a non-blank value, else
    /// `<data_dir>/vault` — matching `MemoryVaultConfig::resolve_vault_dir`'s
    /// own default (empty `vault_dir` -> `<dev_dir>/vault`).
    ///
    /// The MCP server does not load the full `ContinuumConfig` today (only
    /// the `[mcp]` section, via `crate::config::load`), so a non-default
    /// `config.memory.vault.vault_dir` set for the runtime/dashboard is
    /// invisible here. `CONTINUUM_VAULT_DIR` is the escape hatch — set it in
    /// the MCP server's spawn environment to match. Documented as a known
    /// limitation in `docs/mcp-tools.md`.
    fn vault_dir(&self) -> PathBuf {
        match std::env::var("CONTINUUM_VAULT_DIR") {
            Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
            _ => self.state.data_dir.join("vault"),
        }
    }

    /// Returns a reference to the memory vault, opening it if not yet
    /// opened. See [`Self::vault_dir`] for how the directory is resolved.
    pub(crate) async fn vault(&self) -> Result<&Vault, McpError> {
        self.state
            .vault
            .get_or_try_init(|| async {
                let dir = self.vault_dir();
                Vault::open(&dir).await.map_err(|e| {
                    McpError::internal_error(
                        format!("Failed to open vault at {}: {e}", dir.display()),
                        None,
                    )
                })
            })
            .await
    }

    /// Shared wrapper: serialize args for audit, run the body, audit the
    /// outcome, then convert success into a JSON-text `CallToolResult`. Errors
    /// propagate as `McpError`.
    async fn run_tool<T, F, Fut>(
        &self,
        name: &'static str,
        args: impl Serialize,
        body: F,
    ) -> Result<CallToolResult, McpError>
    where
        T: Serialize,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, McpError>>,
    {
        let args_json = serde_json::to_value(&args).unwrap_or(Value::Null);
        let outcome = body().await;
        let summary = match &outcome {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("error: {}", e.message),
        };
        // Fire-and-forget: audit must not block the tool response (lazy
        // episodic-store init triggers fastembed model loading).
        crate::audit::record_tool_call(self, name, &args_json, &summary);
        outcome.map(|v| {
            let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
            CallToolResult::success(vec![Content::text(text)])
        })
    }

    // -----------------------------------------------------------------------
    // Memory tools
    // -----------------------------------------------------------------------

    #[tool(
        description = "Semantic (vector) search over Continuum's episodic memory. Returns the top-N most similar past events — wakes, responses, remembered moments, and prior tool calls. Use this FIRST before asking the user things Continuum may already have seen."
    )]
    async fn memory_query_episodic(
        &self,
        Parameters(req): Parameters<MemoryQueryEpisodicRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_query_episodic", &req, || async {
            let limit = req.limit.unwrap_or(5).min(25) as usize;
            let mut ep = self
                .episodic()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            let results = ep
                .search_similar(&req.query, limit)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            let hits: Vec<EpisodicHit> = results
                .into_iter()
                .map(|r| EpisodicHit {
                    id: r.event.id,
                    ts: r.event.ts,
                    kind: memtool::kind_to_string(r.event.kind),
                    summary: r.event.summary,
                    importance: r.event.importance,
                    tags: r.event.tags,
                    distance: r.distance,
                })
                .collect();
            Ok(hits)
        })
        .await
    }

    #[tool(
        description = "List facts — stable knowledge about the user, projects, preferences. Pass an optional key prefix (e.g. 'project.') to narrow results. Facts are stored at dotted keys like 'user.name' or 'project.simcharts.dir'. Reads the memory vault first; falls back to the legacy semantic store when the vault has no matching facts, or when the vault itself is unavailable (e.g. an unopenable vault directory)."
    )]
    async fn memory_list_facts(
        &self,
        Parameters(req): Parameters<MemoryListFactsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_list_facts", &req, || async {
            let limit = req.limit.unwrap_or(50).min(200);

            // Vault-first, legacy-fallback — on ANY vault miss, not just
            // "no match": a vault that fails to open or read (disk full, a
            // permission change, an unrecoverable index failure) must not
            // hard-fail this read tool when the legacy store can still
            // serve it. Only a genuine legacy-store failure below is
            // allowed to propagate as an error.
            let vault_attempt: Result<Vec<FactView>, McpError> = async {
                let vault = self.vault().await?;
                memtool::vault_list_facts(vault, req.prefix.as_deref(), limit).await
            }
            .await;
            let vault_facts = match vault_attempt {
                Ok(facts) => facts,
                Err(e) => {
                    tracing::warn!(
                        layer = "mcp",
                        component = "memory",
                        error = %e.message,
                        "memory_list_facts: vault read failed, falling back to legacy semantic store"
                    );
                    Vec::new()
                }
            };
            if !vault_facts.is_empty() {
                return Ok(vault_facts);
            }

            let s = self
                .semantic()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            let facts = match &req.prefix {
                Some(p) => {
                    let mut all = s
                        .query_facts_by_prefix(p)
                        .await
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    all.truncate(limit as usize);
                    all
                }
                None => s
                    .list_recent_facts(limit)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?,
            };
            Ok(facts.into_iter().map(FactView::from).collect::<Vec<_>>())
        })
        .await
    }

    #[tool(
        description = "Fetch a single fact by its dotted key. Returns null if not found. Prefer this over memory_list_facts when you know the exact key. Reads the memory vault first; falls back to the legacy semantic store when the vault has no note for that key, or when the vault itself is unavailable (e.g. an unopenable vault directory)."
    )]
    async fn memory_get_fact(
        &self,
        Parameters(req): Parameters<MemoryGetFactRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_get_fact", &req, || async {
            // Same vault-first/legacy-fallback-on-any-miss contract as
            // memory_list_facts above — see that handler's comment.
            let vault_attempt: Result<Option<FactView>, McpError> = async {
                let vault = self.vault().await?;
                memtool::vault_get_fact(vault, &req.key).await
            }
            .await;
            match vault_attempt {
                Ok(Some(view)) => return Ok(Some(view)),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        layer = "mcp",
                        component = "memory",
                        error = %e.message,
                        "memory_get_fact: vault read failed, falling back to legacy semantic store"
                    );
                }
            }

            let s = self
                .semantic()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            let fact = s
                .get_fact(&req.key)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            Ok(fact.map(FactView::from))
        })
        .await
    }

    #[tool(
        description = "Store or update a fact Continuum has learned. Keys starting with 'system.' or 'continuum.' are reserved and rejected. Confidence is clamped by source: inferred ≤0.7, observed ≤0.8, user_stated ≤0.9. Writes a `type: fact` note into the memory vault (title derived from the key, tagged with the key's first segment) — a second call with the same key updates that note instead of creating a duplicate. Does NOT write to the legacy semantic store."
    )]
    async fn memory_set_fact(
        &self,
        Parameters(req): Parameters<MemorySetFactRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_set_fact", &req, || async {
            if memtool::is_reserved_key(&req.key) {
                return Err(McpError::invalid_params(
                    format!(
                        "Key '{}' uses a reserved prefix ('system.' / 'continuum.') — those are \
                         managed by the runtime, not the orchestrator.",
                        req.key
                    ),
                    None,
                ));
            }

            let source = memtool::parse_source(req.source.as_deref());
            // Orchestrator-written facts start at the source ceiling; upstream
            // reinforcement logic can raise them over time.
            let confidence = memtool::clamp_confidence(source, 0.9);

            let title = memtool::title_from_key(&req.key);
            let tag = memtool::tag_from_key(&req.key);
            let source_ref = format!("mcp:set_fact:{}", req.key);

            let vault = self.vault().await?;
            let existing_id = memtool::find_existing_note_id(vault, &title).await?;

            if let Some(id) = existing_id {
                let mut note = vault
                    .get(&id)
                    .await
                    .map_err(|e| memtool::vault_err_to_mcp(&e))?;
                note.frontmatter.status = continuum_memory::NodeStatus::Confirmed;
                note.body = req.value.clone();
                note.frontmatter.confidence = confidence;
                note.frontmatter.source = continuum_memory::Source::AgentRun;
                note.frontmatter.source_ref = Some(source_ref);
                if !note.frontmatter.tags.iter().any(|t| t == &tag) {
                    note.frontmatter.tags.push(tag);
                }
                vault
                    .save(&note)
                    .await
                    .map_err(|e| memtool::vault_err_to_mcp(&e))?;
            } else {
                let draft = continuum_memory::NoteDraft {
                    node_type: continuum_memory::NodeType::Fact,
                    title,
                    body: req.value.clone(),
                    project: None,
                    status: continuum_memory::NodeStatus::Confirmed,
                    confidence,
                    importance: 0.5,
                    source: continuum_memory::Source::AgentRun,
                    source_ref: Some(source_ref),
                    sensitivity: Default::default(),
                    relations: vec![],
                    tags: vec![tag],
                };
                vault
                    .create(draft)
                    .await
                    .map_err(|e| memtool::vault_err_to_mcp(&e))?;
            }

            Ok(SetFactResponse {
                key: req.key.clone(),
                stored: true,
                confidence,
            })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Vault memory tools (memory_vault_*) — direct access to the memory
    // vault's full node model. See docs/memory.md.
    // -----------------------------------------------------------------------

    #[tool(
        description = "Full-text search over the memory vault (titles, bodies, tags). Optional `types` (e.g. [\"fact\",\"decision\"]) and `project` (exact slug) filters are applied after the text match. Returns up to `limit` (default 10, max 100) NodeSummary rows ordered by search rank."
    )]
    async fn memory_vault_search(
        &self,
        Parameters(req): Parameters<MemoryVaultSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_vault_search", &req, || async {
            let vault = self.vault().await?;
            let limit = req.limit.unwrap_or(10).min(100);
            let needs_filter = req.types.is_some() || req.project.is_some();
            let fetch_n = if needs_filter {
                limit.saturating_mul(5).max(50)
            } else {
                limit
            };
            let mut hits = vault
                .search(&req.query, fetch_n)
                .await
                .map_err(|e| memtool::vault_err_to_mcp(&e))?;
            if let Some(types) = &req.types {
                hits.retain(|h| {
                    types
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case(h.node_type.as_str()))
                });
            }
            if let Some(project) = &req.project {
                hits.retain(|h| h.project.as_deref() == Some(project.as_str()));
            }
            hits.truncate(limit as usize);
            Ok(hits)
        })
        .await
    }

    #[tool(
        description = "Fetch a single vault note by id: full frontmatter, body, and backlinks (other notes with a resolved edge pointing at this one). Errors if the id doesn't exist."
    )]
    async fn memory_vault_get(
        &self,
        Parameters(req): Parameters<MemoryVaultGetRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_vault_get", &req, || async {
            let vault = self.vault().await?;
            vault
                .get(&req.id)
                .await
                .map_err(|e| memtool::vault_err_to_mcp(&e))
        })
        .await
    }

    #[tool(
        description = "Create or update a confirmed vault note (status: confirmed, source: agent_run). `type` is one of project|goal|task|decision|person|preference|fact|error|session|note. If a note with the same title already exists (case-insensitive), it is UPDATED in place instead of creating a duplicate — omitted optional fields (confidence, importance, project, relations, tags, source_ref) leave the existing note's values untouched on an update."
    )]
    async fn memory_vault_save(
        &self,
        Parameters(req): Parameters<MemoryVaultSaveRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req_clone = req.clone();
        self.run_tool("memory_vault_save", &req, || async {
            let req = req_clone;
            let node_type = continuum_memory::NodeType::parse(&req.r#type).ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "unknown vault node type \"{}\" (expected one of project|goal|task|\
                         decision|person|preference|fact|error|session|note)",
                        req.r#type
                    ),
                    None,
                )
            })?;
            let relations: Option<Vec<continuum_memory::Relation>> = req.relations.map(|rs| {
                rs.into_iter()
                    .map(|r| continuum_memory::Relation {
                        to: r.to,
                        rel: r.rel,
                        confidence: r.confidence.unwrap_or(1.0),
                    })
                    .collect()
            });

            let vault = self.vault().await?;
            let existing_id = memtool::find_existing_note_id(vault, &req.title).await?;

            if let Some(id) = existing_id {
                let mut note = vault
                    .get(&id)
                    .await
                    .map_err(|e| memtool::vault_err_to_mcp(&e))?;
                note.frontmatter.status = continuum_memory::NodeStatus::Confirmed;
                note.frontmatter.node_type = node_type;
                note.body = req.body;
                note.frontmatter.source = continuum_memory::Source::AgentRun;
                if let Some(project) = req.project {
                    note.frontmatter.project = Some(project);
                }
                if let Some(confidence) = req.confidence {
                    note.frontmatter.confidence = confidence.clamp(0.0, 1.0);
                }
                if let Some(importance) = req.importance {
                    note.frontmatter.importance = importance.clamp(0.0, 1.0);
                }
                if let Some(relations) = relations {
                    note.frontmatter.relations = relations;
                }
                if let Some(tags) = req.tags {
                    note.frontmatter.tags = tags;
                }
                if let Some(source_ref) = req.source_ref {
                    note.frontmatter.source_ref = Some(source_ref);
                }
                vault
                    .save(&note)
                    .await
                    .map_err(|e| memtool::vault_err_to_mcp(&e))?;
                Ok(memtool::VaultSaveResponse { id, updated: true })
            } else {
                let draft = continuum_memory::NoteDraft {
                    node_type,
                    title: req.title,
                    body: req.body,
                    project: req.project,
                    status: continuum_memory::NodeStatus::Confirmed,
                    confidence: req.confidence.unwrap_or(0.5).clamp(0.0, 1.0),
                    importance: req.importance.unwrap_or(0.5).clamp(0.0, 1.0),
                    source: continuum_memory::Source::AgentRun,
                    source_ref: req.source_ref,
                    sensitivity: Default::default(),
                    relations: relations.unwrap_or_default(),
                    tags: req.tags.unwrap_or_default(),
                };
                let note = vault
                    .create(draft)
                    .await
                    .map_err(|e| memtool::vault_err_to_mcp(&e))?;
                Ok(memtool::VaultSaveResponse {
                    id: note.frontmatter.id,
                    updated: false,
                })
            }
        })
        .await
    }

    #[tool(
        description = "Resolve a candidate vault note. `action` is one of confirm|reject|supersede. \"supersede\" requires `replaces` (the id of the node this one supersedes) and confirms this note while flipping the replaced note to status=superseded. Errors if `id` is not currently a candidate."
    )]
    async fn memory_vault_resolve(
        &self,
        Parameters(req): Parameters<MemoryVaultResolveRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_vault_resolve", &req, || async {
            let resolution = memtool::parse_resolution(&req.action, req.replaces.clone())?;
            let vault = self.vault().await?;
            vault
                .resolve_candidate(&req.id, resolution)
                .await
                .map_err(|e| memtool::vault_err_to_mcp(&e))?;
            Ok(memtool::VaultResolveResponse {
                id: req.id.clone(),
                ok: true,
            })
        })
        .await
    }

    #[tool(
        description = "Request a wipe of derived memory data (raw perception log, episodic memory, and the vault's timeline events). Requires `confirm` to equal the literal string \"WIPE\". Writes a request file the running continuum runtime drains at its next boot or daily hygiene tick; this tool only queues the request. Vault markdown notes are NEVER deleted by this or any wipe path."
    )]
    async fn memory_wipe_all(
        &self,
        Parameters(req): Parameters<MemoryWipeAllRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_wipe_all", &req, || async {
            if req.confirm != "WIPE" {
                return Err(McpError::invalid_params(
                    "confirm must equal the literal string \"WIPE\"",
                    None,
                ));
            }
            let path = memtool::write_wipe_request(&self.state.data_dir)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            Ok(memtool::WipeAllResponse {
                path: path.display().to_string(),
            })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // System info tools
    // -----------------------------------------------------------------------

    #[tool(
        description = "Return the current local wall-clock time: ISO-8601 string, timezone offset in minutes, and epoch milliseconds. No arguments."
    )]
    async fn system_current_time(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("system_current_time", &Value::Null, || async {
            Ok::<_, McpError>(systool::current_time())
        })
        .await
    }

    #[tool(
        description = "Return the title and process name of the currently focused (foreground) window. Both fields are empty strings when no window is focused or the lookup fails."
    )]
    async fn system_active_window(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("system_active_window", &Value::Null, || async {
            Ok::<_, McpError>(systool::active_window())
        })
        .await
    }

    #[tool(
        description = "Read the current Windows clipboard text. Returns null if the clipboard is empty, holds non-text data, or is locked by another app. Best-effort — never blocks."
    )]
    async fn system_clipboard_get(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("system_clipboard_get", &Value::Null, || async {
            Ok::<_, McpError>(systool::clipboard_get())
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Filesystem tools (read-only)
    // -----------------------------------------------------------------------

    #[tool(
        description = "Read up to 100 KB of a UTF-8 text file. The path must be inside the allowlist (Continuum data dir, project.*.dir facts, or configured extra_paths). Denied paths, binary files, and paths matching the hardcoded deny list (.ssh, .env, *.pem, etc.) return an error. Larger files are truncated with a clearly marked prefix."
    )]
    async fn fs_read_file(
        &self,
        Parameters(req): Parameters<FsReadFileRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("fs_read_file", &req, || async {
            let cfg = self.compute_fs_allowlist().await;
            crate::tools::fs::read_file(&req.path, &cfg).map_err(|e| fs_err_to_mcp(&e))
        })
        .await
    }

    #[tool(
        description = "List up to 500 entries of a directory. The directory must be inside the allowlist; child entries that would themselves be denied (e.g. .ssh, node_modules) are silently filtered out. Returns name, kind ('file'|'dir'), size, and modified timestamp."
    )]
    async fn fs_list_dir(
        &self,
        Parameters(req): Parameters<FsListDirRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("fs_list_dir", &req, || async {
            let cfg = self.compute_fs_allowlist().await;
            crate::tools::fs::list_dir(&req.path, &cfg).map_err(|e| fs_err_to_mcp(&e))
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Web fetch (GET only, no redirects)
    // -----------------------------------------------------------------------

    #[tool(
        description = "HTTP GET a public URL. Scheme must be http or https. Host must resolve to a public IP — private ranges (10/8, 172.16/12, 192.168/16, 127/8, 169.254/16, ULAs, ::1) are rejected. 5 second timeout, 50 KB response cap with truncation marker. Redirects are NOT followed; a 3xx response returns an error — re-invoke with the target URL directly."
    )]
    async fn web_fetch(
        &self,
        Parameters(req): Parameters<WebFetchRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("web_fetch", &req, || async {
            crate::tools::web::fetch(&self.state.http, &req.url)
                .await
                .map_err(|e| web_err_to_mcp(&e))
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Notification
    // -----------------------------------------------------------------------

    #[tool(
        description = "Show a Windows toast with a title and body. Use sparingly — this is for gently surfacing info to the user, not spam. Rate-limited to one notification per 10 seconds per MCP session. Title truncated at 64 chars, body at 200."
    )]
    async fn system_notification(
        &self,
        Parameters(req): Parameters<NotificationRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("system_notification", &req, || async {
            Ok::<_, McpError>(systool::show_notification(&req.title, &req.body))
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Repair tools — REPAIR AGENT ONLY. These mutate runtime state through
    // the intent file protocol; use them only during an active repair
    // session spawned via `trigger_repair`.
    // -----------------------------------------------------------------------

    #[tool(
        description = "Restart a Continuum subsystem. Queues a restart intent the running continuum runtime picks up on its next tick. Targets: vision | triage | audio | stt | tts | orchestrator | mcp | memory | context_watcher."
    )]
    async fn repair_restart_component(
        &self,
        Parameters(req): Parameters<RestartRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("repair_restart_component", &req, || async {
            repairtool::restart(&self.state.data_dir, req.component)
                .map_err(|e| McpError::internal_error(e.to_string(), None))
        })
        .await
    }

    #[tool(
        description = "Queue a model reinstall for a component. The runtime re-runs scripts/download-models.ps1 for the matching model on its next tick. Destructive — confirm with the user first."
    )]
    async fn repair_reinstall_model(
        &self,
        Parameters(req): Parameters<ReinstallRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("repair_reinstall_model", &req, || async {
            repairtool::reinstall(&self.state.data_dir, req.component)
                .map_err(|e| McpError::internal_error(e.to_string(), None))
        })
        .await
    }

    #[tool(
        description = "Rollback config.toml from a dated backup under ~/.continuum-backups/. `date` format is `YYYY-MM-DD`. Destructive — the current config is overwritten; confirm before calling."
    )]
    async fn repair_rollback_config(
        &self,
        Parameters(req): Parameters<RollbackRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("repair_rollback_config", &req, || async {
            repairtool::rollback(&self.state.data_dir, &req.date)
                .map_err(|e| McpError::internal_error(e.to_string(), None))
        })
        .await
    }

    #[tool(
        description = "Quick file-presence sanity check for a component. Returns a snapshot status (healthy | degrading | error | unknown). Use this before and after applying fixes — it does NOT re-run the full health probe; for that, restart the component."
    )]
    async fn repair_test_component(
        &self,
        Parameters(req): Parameters<TestRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("repair_test_component", &req, || async {
            Ok::<_, McpError>(repairtool::test(&self.state.data_dir, req.component))
        })
        .await
    }

    #[tool(
        description = "Post a user-visible escalation. Writes an intent file the dashboard turns into a red Health-tab banner. Use when the repair requires manual intervention (e.g. re-authenticate claude CLI, free disk space)."
    )]
    async fn repair_escalate(
        &self,
        Parameters(req): Parameters<EscalateRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("repair_escalate", &req, || async {
            repairtool::escalate(&self.state.data_dir, &req.message)
                .map_err(|e| McpError::internal_error(e.to_string(), None))
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Worker tools (Phase 8). Every call writes an intent file that the
    // running continuum runtime picks up. The runtime's WorkerPool publishes
    // per-worker snapshots this server reads back for status / wait / list.
    // -----------------------------------------------------------------------

    #[tool(
        description = "Spawn a new Claude Code worker. Hands the runtime a spawn intent and returns the worker id immediately; poll worker_status (or block with worker_wait) for the result. `cwd` must be an absolute path the worker should run in. `model` accepts \"auto\" (default), \"budget\" (Sonnet), \"power\" (Opus), or an explicit \"claude-*\" id. Workers cannot spawn other workers via MCP — use Claude Code's built-in Task tool if sub-agents are needed."
    )]
    async fn workers_spawn_worker(
        &self,
        Parameters(req): Parameters<SpawnWorkerRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req_clone = req.clone();
        self.run_tool("workers_spawn_worker", &req, || async {
            workertool::spawn(&self.state.data_dir, req_clone)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))
        })
        .await
    }

    #[tool(
        description = "Return the current snapshot for a worker (status, elapsed_ms, progress, last_line, result on completion). Status is one of queued|starting|running|completed|failed|cancelled|timed_out|pending (pending = the runtime hasn't processed the spawn intent yet)."
    )]
    async fn workers_worker_status(
        &self,
        Parameters(req): Parameters<WorkerIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("workers_worker_status", &req, || async {
            workertool::status(&self.state.data_dir, &req.worker_id)
                .map_err(|e| McpError::internal_error(e.to_string(), None))
        })
        .await
    }

    #[tool(
        description = "Cancel a running or queued worker. Always returns immediately with the latest snapshot. The runtime may take up to one tick to kill the claude subprocess."
    )]
    async fn workers_worker_cancel(
        &self,
        Parameters(req): Parameters<WorkerIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("workers_worker_cancel", &req, || async {
            workertool::cancel(&self.state.data_dir, &req.worker_id)
                .map_err(|e| McpError::internal_error(e.to_string(), None))
        })
        .await
    }

    #[tool(
        description = "Block until a worker reaches a terminal state (completed, failed, cancelled, timed_out). `timeout_secs` defaults to 60 and is clamped to [1, 300]. Returns the final snapshot either way — don't assume absence of error means success; check `status`."
    )]
    async fn workers_worker_wait(
        &self,
        Parameters(req): Parameters<WorkerWaitRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("workers_worker_wait", &req, || async {
            let timeout = req.timeout_secs.unwrap_or(60);
            workertool::wait(&self.state.data_dir, &req.worker_id, timeout)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))
        })
        .await
    }

    #[tool(
        description = "List recent worker snapshots. Optional `status` filter (queued|starting|running|completed|failed|cancelled|timed_out). `limit` is clamped to 100."
    )]
    async fn workers_worker_list(
        &self,
        Parameters(req): Parameters<WorkerListRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req_clone = req.clone();
        self.run_tool("workers_worker_list", &req, || async {
            workertool::list(&self.state.data_dir, req_clone)
                .map_err(|e| McpError::internal_error(e.to_string(), None))
        })
        .await
    }
}

fn fs_err_to_mcp(e: &crate::tools::fs::FsError) -> McpError {
    use crate::tools::fs::FsError;
    match e {
        FsError::Denied(_) | FsError::NotAFile | FsError::NotADirectory | FsError::NonUtf8 => {
            McpError::invalid_params(e.to_string(), None)
        }
        FsError::Io(_) => McpError::internal_error(e.to_string(), None),
    }
}

fn web_err_to_mcp(e: &crate::tools::web::WebFetchError) -> McpError {
    use crate::tools::web::WebFetchError;
    match e {
        WebFetchError::InvalidUrl(_)
        | WebFetchError::SchemeNotAllowed(_)
        | WebFetchError::NoHost
        | WebFetchError::PrivateAddress(_)
        | WebFetchError::Redirected(_) => McpError::invalid_params(e.to_string(), None),
        WebFetchError::DnsFailed(_) | WebFetchError::HttpFailed(_) => {
            McpError::internal_error(e.to_string(), None)
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for ContinuumMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Continuum MCP server — exposes memory (facts + the memory vault), system info, \
                 filesystem (read-only), web fetch, and notification tools to the orchestrator. \
                 Every tool call is audited to episodic memory.",
            )
    }
}
