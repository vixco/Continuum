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
use continuum_core::config::{
    ContextPackageConfig, ContextToolsConfig, GitContextConfig, ObservationToggles,
};
use continuum_core::health::repair::RepairSessionGrant;
use continuum_core::memory::{episodic::EpisodicStore, semantic::SemanticStore};
use continuum_core::permissions::{PermissionDecision, PermissionGateway};
use continuum_core::senses::privacy::PrivacyFilter;
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

use crate::tools::context::{
    self as ctxtool, ContextFilesRequest, ContextGitRequest, ContextPackageRequest,
    ContextProcessesRequest, ContextSearchRequest, ContextTimelineRequest, ContextWindowRequest,
    PackageMemory,
};
use crate::tools::file_actions::{
    FsApplyPatchRequest, FsCreateFileRequest, FsDeleteRequest, FsMoveRequest,
};
use crate::tools::fs::{FsListDirRequest, FsReadFileRequest};
use crate::tools::git::{
    GitCheckpointListRequest, GitCheckpointRequest, GitDiffRequest, GitRollbackRequest,
};
use crate::tools::github::{
    GitHubCommentRequest, GitHubCreateIssueRequest, GitHubCreatePullRequest, GitHubGetFileRequest,
    GitHubListIssuesRequest, GitHubListReposRequest, GitHubRepoRequest,
};
use crate::tools::memory::{
    self as memtool, EpisodicHit, FactView, MemoryGetFactRequest, MemoryListFactsRequest,
    MemoryQueryEpisodicRequest, MemorySetFactRequest, MemoryVaultDeleteRequest,
    MemoryVaultGetRequest, MemoryVaultResolveRequest, MemoryVaultSaveRequest,
    MemoryVaultSearchRequest, MemoryWipeAllRequest, SetFactResponse,
};
use crate::tools::repair::{
    self as repairtool, EscalateRequest, ReinstallRequest, RestartRequest, RollbackRequest,
    TestRequest,
};
use crate::tools::system::{self as systool, NotificationRequest};
use crate::tools::terminal::TerminalRunRequest;
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
    pub(crate) fs_max_write_bytes: usize,
    pub(crate) fs_max_patch_replacements: usize,
    pub(crate) terminal_config: crate::config::McpTerminalConfig,
    pub(crate) github_config: continuum_core::config::GitHubConfig,
    pub(crate) semantic: OnceCell<SemanticStore>,
    pub(crate) episodic: OnceCell<Mutex<EpisodicStore>>,
    pub(crate) vault: OnceCell<Vault>,
    pub(crate) repair_grant: Option<RepairSessionGrant>,
    pub(crate) permissions: PermissionGateway,
    /// The privacy choke point every observation tool routes through
    /// (context engine spec §5.1). Built from the **same** `config.toml`
    /// the runtime loads, so a zone the user configured binds this process
    /// identically — that equivalence is the point of the retrofit.
    pub(crate) privacy: PrivacyFilter,
    /// `[context_tools]` switches (spec §5.1/§6), incl. the clipboard
    /// kill-switch and the `context_*` family master switch.
    pub(crate) context_tools: ContextToolsConfig,
    /// `[privacy.toggles]` — the spec §4.1 "honest toggles". The context
    /// tool family (Task C3) checks them per source, so a user who turned
    /// the microphone or the screen off does not get that source back
    /// through an MCP tool.
    pub(crate) toggles: ObservationToggles,
    /// `[storage] db_path` — the raw-log SQLite database the context
    /// tools open **read-only** (spec §5.2). Same value the runtime
    /// writes to, so both processes agree on which file is the log.
    pub(crate) raw_log_path: PathBuf,
    /// `[git_context]` — the collector switch and the subprocess timeout
    /// `context_git`'s named-project probe honours (Task C4, spec §4.4).
    pub(crate) git_context: GitContextConfig,
    /// `[context_package]` — budget + per-section caps for
    /// `context_package`'s mcp-published profile (Task C4, spec §4.9).
    pub(crate) package_config: ContextPackageConfig,
    /// `[process_watcher].enabled` consent boundary. A stale snapshot must
    /// not remain readable after the collector is switched off.
    pub(crate) process_watcher_enabled: bool,
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

        // Load the FULL config for the privacy boundary (context engine
        // spec §5.1). Same file, same loader, same defaults as the runtime
        // — anything else would let the two processes disagree about which
        // windows are excluded. A parse failure must not stop the server
        // (the orchestrator spawns us on every wake), and defaults are the
        // safe direction here: the built-in `never_observe` /
        // private-browsing rules and all three scrubber families are on by
        // default, so a broken config filters more, never less.
        let full_cfg = continuum_core::config::load_config(&data_dir.join("config.toml"))
            .unwrap_or_else(|error| {
                tracing::warn!(
                    layer = "mcp",
                    component = "privacy",
                    error = %error,
                    "Failed to load config.toml for the privacy filter — using defaults"
                );
                continuum_core::config::ContinuumConfig::default()
            });
        let privacy = PrivacyFilter::from_config(&full_cfg.context, &full_cfg.privacy);
        let context_tools = full_cfg.context_tools.clone();
        let toggles = full_cfg.privacy.toggles.clone();
        let git_context = full_cfg.git_context.clone();
        let package_config = full_cfg.context_package.clone();
        let process_watcher_enabled = full_cfg.process_watcher.enabled;
        let github_config = full_cfg.github.clone();
        // Honour the configured `[storage] db_path` when there is a real
        // config file to honour; otherwise fall back to this process's
        // data dir, because the config *defaults* point at the standard
        // dev dir, which need not be the `--data-dir` we were given.
        let raw_log_path = if data_dir.join("config.toml").exists() {
            PathBuf::from(&full_cfg.storage.db_path)
        } else {
            data_dir.join("raw_log.sqlite")
        };
        let repair_grant = std::env::var("CONTINUUM_REPAIR_TOKEN")
            .ok()
            .and_then(|token| {
                match continuum_core::health::repair::authorize_repair_session(&data_dir, &token) {
                    Ok(grant) => Some(grant),
                    Err(error) => {
                        tracing::warn!(
                            layer = "mcp",
                            component = "repair_auth",
                            error = %error,
                            "Rejected invalid repair session capability"
                        );
                        None
                    }
                }
            });
        let permission_session_id = std::env::var("CONTINUUM_SESSION_ID")
            .unwrap_or_else(|_| format!("mcp-{}", uuid::Uuid::new_v4()));
        let permissions = PermissionGateway::new(
            data_dir.clone(),
            permission_session_id,
            include_str!("../../../config/default-permissions.toml"),
        );

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
                fs_max_write_bytes: mcp_cfg.fs.max_write_bytes,
                fs_max_patch_replacements: mcp_cfg.fs.max_patch_replacements,
                terminal_config: mcp_cfg.terminal,
                github_config,
                semantic: OnceCell::new(),
                episodic: OnceCell::new(),
                vault: OnceCell::new(),
                repair_grant,
                permissions,
                privacy,
                context_tools,
                toggles,
                raw_log_path,
                git_context,
                package_config,
                process_watcher_enabled,
            }),
            tool_router: Self::tool_router(),
        })
    }

    fn require_repair_session(&self) -> Result<RepairSessionGrant, McpError> {
        let grant = self.state.repair_grant.as_ref().ok_or_else(|| {
            self.audit_repair_event(
                "repair_session_denied",
                serde_json::json!({ "reason": "missing_capability" }),
            );
            McpError::invalid_request(
                "repair tools require a valid Health-tab repair session",
                None,
            )
        })?;
        continuum_core::health::repair::authorize_repair_session(&self.state.data_dir, &grant.token)
            .map_err(|error| {
                self.audit_repair_event(
                    "repair_session_denied",
                    serde_json::json!({ "reason": "invalid_or_expired_capability" }),
                );
                McpError::invalid_request(
                    format!("repair session is no longer valid: {error}"),
                    None,
                )
            })
    }

    fn require_repair_component(&self, component: &str) -> Result<RepairSessionGrant, McpError> {
        let grant = self.require_repair_session()?;
        if !grant
            .allowed_components
            .iter()
            .any(|allowed| allowed == component)
        {
            self.audit_repair_event(
                "repair_component_denied",
                serde_json::json!({ "component": component, "operation": "test" }),
            );
            return Err(McpError::invalid_request(
                format!("component {component:?} was not allowlisted by the live repair preview"),
                None,
            ));
        }
        Ok(grant)
    }

    fn require_repair_restart_component(
        &self,
        component: &str,
    ) -> Result<RepairSessionGrant, McpError> {
        let grant = self.require_repair_session()?;
        if !grant
            .allowed_restart_components
            .iter()
            .any(|allowed| allowed == component)
        {
            self.audit_repair_event(
                "repair_component_denied",
                serde_json::json!({ "component": component, "operation": "restart" }),
            );
            return Err(McpError::invalid_request(
                format!(
                    "restart for component {component:?} is not supported by this repair session"
                ),
                None,
            ));
        }
        Ok(grant)
    }

    fn require_repair_escalation(&self) -> Result<RepairSessionGrant, McpError> {
        let grant = self.require_repair_session()?;
        if !grant.allow_escalation_intent {
            self.audit_repair_event(
                "repair_operation_denied",
                serde_json::json!({ "operation": "escalation_intent" }),
            );
            return Err(McpError::invalid_request(
                "escalation intents are not consumed in this repair session; report the manual action in assistant output",
                None,
            ));
        }
        Ok(grant)
    }

    fn audit_repair_event(&self, event: &str, detail: serde_json::Value) {
        let _ = continuum_core::health::repair::append_repair_audit(
            &self.state.data_dir,
            event,
            detail,
        );
    }

    fn backup_before_repair_mutation(&self, operation: &str) -> Result<(), McpError> {
        let backups_dir = repairtool::backups_dir_for(&self.state.data_dir);
        let backup = continuum_core::health::backup::run_backup(&self.state.data_dir, &backups_dir)
            .map_err(|error| {
                McpError::internal_error(
                    format!("pre-{operation} backup failed; mutation blocked: {error}"),
                    None,
                )
            })?;
        continuum_core::health::backup::verify_backup(&backup.path).map_err(|error| {
            McpError::internal_error(
                format!("pre-{operation} backup verification failed; mutation blocked: {error}"),
                None,
            )
        })?;
        let config_path = self.state.data_dir.join("config.toml");
        let retention = continuum_core::config::load_config(&config_path)
            .unwrap_or_default()
            .health
            .backup_retention
            .max(1);
        continuum_core::health::backup::prune_backups(&backups_dir, retention).map_err(
            |error| {
                McpError::internal_error(
                    format!("pre-{operation} backup retention failed; mutation blocked: {error}"),
                    None,
                )
            },
        )?;
        continuum_core::health::backup::verify_backup(&backup.path).map_err(|error| {
            McpError::internal_error(
                format!(
                    "pre-{operation} backup was not retained after pruning; mutation blocked: {error}"
                ),
                None,
            )
        })?;
        continuum_core::health::repair::append_repair_audit(
            &self.state.data_dir,
            "mutation_backup_created",
            serde_json::json!({
                "operation": operation,
                "path": backup.path,
                "bytes": backup.bytes,
                "verified": true,
            }),
        )
        .map_err(|error| {
            McpError::internal_error(
                format!("pre-{operation} audit failed; mutation blocked: {error}"),
                None,
            )
        })?;
        Ok(())
    }

    /// Builds the per-call gate every `context_*` tool reads through
    /// (spec §5.2): where the published state lives, the §5.1 privacy
    /// filter, the observation toggles, and the family master switch.
    pub(crate) fn context_gate(&self) -> crate::tools::context::Gate<'_> {
        crate::tools::context::Gate {
            data_dir: &self.state.data_dir,
            db_path: &self.state.raw_log_path,
            filter: &self.state.privacy,
            toggles: &self.state.toggles,
            git_context: &self.state.git_context,
            package_config: &self.state.package_config,
            process_watcher_enabled: self.state.process_watcher_enabled,
            enabled: self.state.context_tools.enabled,
        }
    }

    /// Fills `context_package`'s memory sections from **this process's
    /// own** vault and episodic stores (spec §4.9 mcp-published profile).
    ///
    /// Both opens are lazy and both failures are non-fatal: a package with
    /// no memories is still worth having, and a wake must not die because
    /// LanceDB could not open. `stale` reports the difference.
    async fn package_memory(&self, query: &str) -> PackageMemory {
        let caps = self.state.package_config.caps();
        let mut memory = PackageMemory::default();
        let now = chrono::Utc::now();

        match self.episodic().await {
            Ok(mut store) => match store.search_similar(query, caps.memories, None).await {
                Ok(hits) => {
                    memory.memories = hits
                        .into_iter()
                        .map(|hit| continuum_core::context::package::MemoryLine {
                            age: continuum_core::orchestrator::wake_context::render_age(
                                now,
                                hit.event.ts,
                            ),
                            text: self.state.privacy.scrub_text(&hit.event.summary),
                            // Fixwave 3a (C1): the same gate the wake
                            // packager applies. `context_package`'s
                            // response is cloud-bound too, so a distilled
                            // `local_only` memory must be withheld here
                            // for exactly the reason it is withheld there.
                            local_only: hit.event.sensitivity
                                == continuum_core::memory::events::EventSensitivity::LocalOnly,
                        })
                        .collect();
                }
                Err(error) => {
                    tracing::warn!(
                        layer = "mcp",
                        component = "context_tools",
                        error = %error,
                        "Episodic search failed — context_package renders without memories"
                    );
                    memory.stale = true;
                }
            },
            Err(error) => {
                tracing::warn!(
                    layer = "mcp",
                    component = "context_tools",
                    error = %error,
                    "Episodic store unavailable — context_package renders without memories"
                );
                memory.stale = true;
            }
        }

        match self.vault().await {
            Ok(vault) => match vault
                .search(query, (caps.vault_notes.saturating_mul(2).max(4)) as u32)
                .await
            {
                Ok(hits) => {
                    memory.vault_notes = hits
                        .into_iter()
                        .filter(|note| note.status == continuum_memory::NodeStatus::Confirmed)
                        .take(caps.vault_notes)
                        .map(|note| continuum_core::context::package::VaultNoteLine {
                            node_type: note.node_type.as_str().to_string(),
                            title: self.state.privacy.scrub_text(&note.title),
                            snippet: note
                                .snippet
                                .as_deref()
                                .map(|text| self.state.privacy.scrub_text(text)),
                            importance: note.importance,
                            // Unlike the wake profile (where retrieval has
                            // already applied `include_sensitive_in_context`),
                            // nothing upstream filtered this list — so a
                            // note the user marked sensitive is tagged
                            // local_only and the cloud gate drops it.
                            local_only: note.sensitivity
                                == continuum_memory::Sensitivity::Sensitive,
                        })
                        .collect();
                }
                Err(error) => {
                    tracing::warn!(
                        layer = "mcp",
                        component = "context_tools",
                        error = %error,
                        "Vault search failed — context_package renders without vault notes"
                    );
                    memory.stale = true;
                }
            },
            Err(error) => {
                tracing::warn!(
                    layer = "mcp",
                    component = "context_tools",
                    error = %error.message,
                    "Vault unavailable — context_package renders without vault notes"
                );
                memory.stale = true;
            }
        }
        memory
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

    fn git_timeout_ms(&self) -> u64 {
        self.state
            .git_context
            .command_timeout_secs
            .saturating_mul(1_000)
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
        let repair_capability_authorized =
            name.starts_with("repair_") && self.state.repair_grant.is_some();
        if !repair_capability_authorized {
            let resource = permission_resource(&args_json);
            match self.state.permissions.check(
                name,
                resource.as_deref(),
                &format!("Continuum wants to run {name}"),
            ) {
                PermissionDecision::Allow => {}
                PermissionDecision::Ask { request } => {
                    return Err(McpError::invalid_request(
                        format!(
                            "permission_required: request_id={} action={}{}; approve it in Continuum Settings > Tools and retry",
                            request.id,
                            request.action,
                            request
                                .resource
                                .as_deref()
                                .map(|value| format!(" resource={value}"))
                                .unwrap_or_default()
                        ),
                        Some(serde_json::json!({ "permission_request": request })),
                    ));
                }
                PermissionDecision::Deny { reason } => {
                    return Err(McpError::invalid_request(
                        format!("permission_denied: {name}: {reason}"),
                        Some(serde_json::json!({ "action": name, "reason": reason })),
                    ));
                }
            }
        }
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
                .search_similar(&req.query, limit, None)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            // Fixwave 3a (C1): this response goes straight into a
            // cloud-bound model's context, so it is an egress point like
            // any package section. Now that distilled memories carry the
            // §4.1 zone of the text they summarize, `local_only` ones are
            // dropped here rather than rendered. Schema-stable: content
            // filtering only, per §5.1's precedent.
            let results: Vec<_> = results
                .into_iter()
                .filter(|r| {
                    r.event.sensitivity
                        != continuum_core::memory::events::EventSensitivity::LocalOnly
                })
                .collect();
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
                    expires: None,
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
                    expires: None,
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
        description = "Permanently delete a vault note: removes its markdown file from disk and its index entry. Unlike memory_vault_resolve's reject/archive-style transitions, this is irreversible — the note is gone, not just re-statused. Other notes with a wiki-link (`[[...]]`) pointing at the deleted id keep their link text, but it becomes an unresolved/ghost reference; their own content and relations are otherwise untouched. Errors if `id` doesn't exist."
    )]
    async fn memory_vault_delete(
        &self,
        Parameters(req): Parameters<MemoryVaultDeleteRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_vault_delete", &req, || async {
            let vault = self.vault().await?;
            vault
                .delete(&req.id)
                .await
                .map_err(|e| memtool::vault_err_to_mcp(&e))?;
            Ok(memtool::VaultDeleteResponse {
                deleted: true,
                id: req.id.clone(),
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
        description = "Return the title and process name of the currently focused (foreground) window. Both fields are empty strings when no window is focused or the lookup fails. Content is privacy-filtered: a window in a never_observe zone comes back as the `[excluded]` sentinel with an empty title, a local_only window keeps its process but not its title, and secrets in ordinary titles are redacted."
    )]
    async fn system_active_window(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("system_active_window", &Value::Null, || async {
            Ok::<_, McpError>(systool::active_window(&self.state.privacy))
        })
        .await
    }

    #[tool(
        description = "Return Continuum's compact, ordered, source-attributed local live world-state: every connected monitor's latest local vision summary plus safe foreground-window, coarse input-activity, terminal, project, and degradation context. No raw screenshots, key values, pointer coordinates, or clipboard content. Both the compact text and the structured state are privacy-filtered: excluded zones return sentinels, local-only content is replaced by a redaction marker, and secrets are scrubbed."
    )]
    async fn system_live_context(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("system_live_context", &Value::Null, || async {
            systool::live_context(&self.state.data_dir, &self.state.privacy)
                .map_err(|error| McpError::internal_error(error, None))
        })
        .await
    }

    #[tool(
        description = "Read the current Windows clipboard text. Returns null if the clipboard is empty, holds non-text data, is locked by another app, the foreground window is in a never_observe privacy zone, or the tool is disabled via [context_tools] clipboard_tool_enabled. Returned text is scrubbed for secrets. Best-effort — never blocks."
    )]
    async fn system_clipboard_get(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("system_clipboard_get", &Value::Null, || async {
            Ok::<_, McpError>(systool::clipboard_get(
                &self.state.privacy,
                self.state.context_tools.clipboard_tool_enabled,
            ))
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Context tools (context engine spec §5.2) — read-only, privacy-gated,
    // degrade to empty/stale answers instead of failing a wake. See
    // `crate::tools::context` for the four rules the whole family obeys.
    // -----------------------------------------------------------------------

    #[tool(
        description = "Continuum's live session state: the current project, the inferred goal and task (with a confidence), the foreground app/window, recently-touched files, and the last observed error/success/user command. Read this FIRST when the user says something like 'continue' or 'what was I doing' — it is what Continuum believes is happening right now. Returns `available: false` before the runtime has published anything and `stale: true` when the runtime stopped publishing (>30 s). Privacy-gated: a session marked local_only reports generalized goal/task, excluded windows report the sentinel, secrets are scrubbed."
    )]
    async fn context_session(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("context_session", &Value::Null, || async {
            Ok::<_, McpError>(ctxtool::session(&self.context_gate()))
        })
        .await
    }

    #[tool(
        description = "The foreground window right now (process, title, zone, pid, exe path, monitor, seconds focused) plus the most recent focus switches with their dwell times. `limit` caps the switch list (default 10, max 50). Switches come from the deduped context-events log; an event database that does not exist yet returns an empty list with `stale: true` rather than an error. Privacy-gated: a never_observe window is the `[excluded]` sentinel with no pid/exe/monitor, a local_only title is redacted."
    )]
    async fn context_window(
        &self,
        Parameters(req): Parameters<ContextWindowRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("context_window", &req, || async {
            Ok::<_, McpError>(ctxtool::window(&self.context_gate(), &req).await)
        })
        .await
    }

    #[tool(
        description = "What is on each screen: every connected monitor with its latest local-vision caption and privacy zone, plus the same compact source-attributed world render `system_live_context` returns. Excluded monitors are still listed — with an empty caption and a never_observe zone marker — so you can see a screen exists and is deliberately not described. `stale: true` once the published snapshot is older than 10 s. Returns nothing when `[privacy.toggles] screen` is off."
    )]
    async fn context_screen(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("context_screen", &Value::Null, || async {
            Ok::<_, McpError>(ctxtool::screen(&self.context_gate()))
        })
        .await
    }

    #[tool(
        description = "The voice pipeline's latest published transcript with its timestamp, plus whether the user appears to be in a call and whether ambient mute is suppressing voice output. The transcript is scrubbed for secrets. Returns `available: false` when the microphone toggle (`[privacy.toggles] mic`, or `pause_all`) is off — an honest toggle silences this tool too — or when the runtime is not publishing."
    )]
    async fn context_audio(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("context_audio", &Value::Null, || async {
            Ok::<_, McpError>(ctxtool::audio(&self.context_gate()))
        })
        .await
    }

    #[tool(
        description = "Every project Continuum knows about: configured entries, user-confirmed ones, and auto-discovery proposals (status `discovered`, which never participate in project resolution until confirmed). Active project first, discovery proposals last. Root paths are home/username-scrubbed. Returns `available: false` when the runtime has never booted against this data directory."
    )]
    async fn context_projects(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("context_projects", &Value::Null, || async {
            Ok::<_, McpError>(ctxtool::projects(&self.context_gate()).await)
        })
        .await
    }

    #[tool(
        description = "Meaningful active background processes from Continuum's opt-in process collector. Returns executable basename, coarse category, PID, CPU, memory, start time and a scrubbed executable path; never command lines, environment variables or process memory. Filter by `category`; `limit` defaults to 20 and is capped at 100. Lifecycle, stop and sustained-pressure history is available through context_timeline with source `process`. Returns `available: false` while [process_watcher].enabled is off."
    )]
    async fn context_processes(
        &self,
        Parameters(req): Parameters<ContextProcessesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("context_processes", &req, || async {
            Ok::<_, McpError>(ctxtool::processes(&self.context_gate(), &req).await)
        })
        .await
    }

    #[tool(
        description = "Continuum's deduped event timeline: what happened, when, how often. Filter with `since`/`until` (RFC 3339), `types` (registry tokens like \"error\", \"commit\", \"focus_switch\"), `project` (slug), and `source` (window|git|file|process|screen|audio|system|voice); `limit` defaults to 50 and is capped at 200. Events are COLLAPSED — one row with `count: 14` means the same thing happened 14 times between `ts_first` and `ts_last`, which is a far stronger signal than a single occurrence. Rows marked local_only are never returned; `omitted_private` tells you how many were withheld. A cold event database returns an empty list with `stale: true`, never an error."
    )]
    async fn context_timeline(
        &self,
        Parameters(req): Parameters<ContextTimelineRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("context_timeline", &req, || async {
            Ok::<_, McpError>(ctxtool::timeline(&self.context_gate(), &req).await)
        })
        .await
    }

    #[tool(
        description = "Full-text search over Continuum's event log (summaries and window titles), best match first. Use this to answer \"when did I last see X\" without scanning the timeline. Punctuation is stripped and each word becomes a prefix term, so \"build fail\" matches \"build failed\". `limit` defaults to 20 and is capped at 50. Same privacy contract as context_timeline: local_only rows are withheld and counted in `omitted_private`."
    )]
    async fn context_search(
        &self,
        Parameters(req): Parameters<ContextSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("context_search", &req, || async {
            Ok::<_, McpError>(ctxtool::search(&self.context_gate(), &req).await)
        })
        .await
    }

    #[tool(
        description = "Recent file activity the file watcher recorded: created, modified, deleted, renamed, and storm-collapsed bulk-change events, oldest first. Optional `project` (slug) filter; `limit` defaults to 20 and is capped at 100. Paths are root-relative and home/username-scrubbed. Returns `available: false` when `[privacy.toggles] files` is off or the file watcher was never enabled."
    )]
    async fn context_files(
        &self,
        Parameters(req): Parameters<ContextFilesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("context_files", &req, || async {
            Ok::<_, McpError>(ctxtool::files(&self.context_gate(), &req).await)
        })
        .await
    }

    #[tool(
        description = "Git state: branch, dirty/staged/untracked counts, ahead/behind, conflicts, and the HEAD commit. With no arguments it returns the ACTIVE project's already-published state — no subprocess, no filesystem access. With `project` (a slug from context_projects) it runs one timeout-bounded `git status` against that project's root, but ONLY for a configured or confirmed project: a `discovered` auto-discovery proposal is refused with an explanatory `reason`, because Continuum never runs commands in a directory the user has not adopted. Ask the user to confirm the project instead of retrying. Root paths are scrubbed; the commit id is a structured identifier and is returned verbatim."
    )]
    async fn context_git(
        &self,
        Parameters(req): Parameters<ContextGitRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("context_git", &req, || async {
            Ok::<_, McpError>(ctxtool::git(&self.context_gate(), &req).await)
        })
        .await
    }

    #[tool(
        description = "The whole picture in one call: Continuum's context package rendered as markdown — current moment (screen captions + foreground window + compact world state), session state, what happened just before, relevant memories and vault notes, recent file/git changes, failed attempts, and the last success. `token_budget` defaults to the configured 1000 and is clamped to [200, 8000]; sections are dropped from a documented ladder when the budget binds. Two sections are structurally absent in this profile and listed in `sections_omitted`: `why_woken` (there is no wake to explain) and `trigger_frame_moment` (the live snapshot replaces it). `per_section_stale` says which of the four sources went cold."
    )]
    async fn context_package(
        &self,
        Parameters(req): Parameters<ContextPackageRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("context_package", &req, || async {
            let gate = self.context_gate();
            // The memory query is the live world render — so "relevant"
            // means relevant to what is on screen right now. No published
            // state means no query, and we skip the store opens entirely
            // rather than paying the fastembed load for a blank search.
            let memory = match ctxtool::package_memory_query(&gate) {
                Some(query) => self.package_memory(&query).await,
                None => PackageMemory::default(),
            };
            Ok::<_, McpError>(ctxtool::package(&gate, &req, memory).await)
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

    #[tool(
        description = "Atomically create a new UTF-8 file inside an allowlisted existing directory. Refuses overwrite, denied secret names, symlink escapes, and content above mcp.fs.max_write_bytes."
    )]
    async fn fs_create_file(
        &self,
        Parameters(req): Parameters<FsCreateFileRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("fs_create_file", &req, || async {
            let allowlist = self.compute_fs_allowlist().await;
            crate::tools::file_actions::create_file(&req, &allowlist, self.state.fs_max_write_bytes)
                .await
                .map_err(file_action_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "Patch an existing allowlisted UTF-8 file by exact old_text precondition. A non-replace-all patch must match exactly once. The original is preserved under Continuum recovery storage before replacement."
    )]
    async fn fs_apply_patch(
        &self,
        Parameters(req): Parameters<FsApplyPatchRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("fs_apply_patch", &req, || async {
            let allowlist = self.compute_fs_allowlist().await;
            crate::tools::file_actions::apply_patch(
                &req,
                &allowlist,
                &self.state.data_dir.join("recovery").join("files"),
                self.state.fs_max_write_bytes,
                self.state.fs_max_patch_replacements,
            )
            .await
            .map_err(file_action_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "Move an allowlisted file or directory to a new allowlisted destination. The destination parent must exist and overwrite is never allowed."
    )]
    async fn fs_move(
        &self,
        Parameters(req): Parameters<FsMoveRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("fs_move", &req, || async {
            let allowlist = self.compute_fs_allowlist().await;
            crate::tools::file_actions::move_path(&req, &allowlist)
                .await
                .map_err(file_action_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "Recoverably delete an allowlisted file or directory by moving it under <data_dir>/recovery/files. Returns the recovery path; no recursive erase command is used."
    )]
    async fn fs_delete_to_trash(
        &self,
        Parameters(req): Parameters<FsDeleteRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("fs_delete_to_trash", &req, || async {
            let allowlist = self.compute_fs_allowlist().await;
            crate::tools::file_actions::delete_to_trash(
                &req,
                &allowlist,
                &self.state.data_dir.join("recovery").join("files"),
            )
            .await
            .map_err(file_action_err_to_mcp)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Git checkpoint tools
    // -----------------------------------------------------------------------

    #[tool(
        description = "Create a durable checkpoint under refs/continuum/checkpoints without changing the user's real index or worktree. Captures tracked and untracked non-secret files; hard-denied paths such as .env and private keys are excluded."
    )]
    async fn git_checkpoint(
        &self,
        Parameters(req): Parameters<GitCheckpointRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("git_checkpoint", &req, || async {
            let allowlist = self.compute_fs_allowlist().await;
            crate::tools::git::checkpoint(&req, &allowlist, self.git_timeout_ms())
                .await
                .map_err(git_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "Show porcelain status and a bounded unified diff for an allowlisted repository. Optionally compare the working tree against an exact Continuum checkpoint id. Read-only; untracked names appear in status but their contents are not returned."
    )]
    async fn git_diff(
        &self,
        Parameters(req): Parameters<GitDiffRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("git_diff", &req, || async {
            let allowlist = self.compute_fs_allowlist().await;
            crate::tools::git::diff(&req, &allowlist, self.git_timeout_ms())
                .await
                .map_err(git_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "List durable Continuum checkpoints for an allowlisted repository, newest first."
    )]
    async fn git_checkpoint_list(
        &self,
        Parameters(req): Parameters<GitCheckpointListRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("git_checkpoint_list", &req, || async {
            let allowlist = self.compute_fs_allowlist().await;
            crate::tools::git::list_checkpoints(&req, &allowlist, self.git_timeout_ms())
                .await
                .map_err(git_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "Restore an exact Continuum checkpoint. Always creates a pre-rollback safety checkpoint, moves untracked files into .git/continuum-recovery, and copies modified sensitive tracked files there before reset. Requires confirmation every call."
    )]
    async fn git_rollback(
        &self,
        Parameters(req): Parameters<GitRollbackRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("git_rollback", &req, || async {
            let allowlist = self.compute_fs_allowlist().await;
            crate::tools::git::rollback(&req, &allowlist, self.git_timeout_ms())
                .await
                .map_err(git_err_to_mcp)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Restricted terminal broker
    // -----------------------------------------------------------------------

    #[tool(
        description = "Run a configured executable as program + literal args in an allowlisted cwd. No shell is involved, stdin is closed, credential-like args are rejected, sensitive environment variables are stripped, and timeout/output are capped. Requires confirmation every call."
    )]
    async fn terminal_run(
        &self,
        Parameters(req): Parameters<TerminalRunRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("terminal_run", &req, || async {
            let allowlist = self.compute_fs_allowlist().await;
            crate::tools::terminal::run(&req, &allowlist, &self.state.terminal_config)
                .await
                .map_err(terminal_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "Run a configured verification command under the restricted terminal broker and persist its exit code, bounded output, duration, cwd, and command as durable JSON evidence."
    )]
    async fn terminal_verify(
        &self,
        Parameters(req): Parameters<TerminalRunRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("terminal_verify", &req, || async {
            let allowlist = self.compute_fs_allowlist().await;
            crate::tools::terminal::verify(
                &req,
                &allowlist,
                &self.state.terminal_config,
                &self.state.data_dir,
            )
            .await
            .map_err(terminal_err_to_mcp)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Optional read-only GitHub integration
    // -----------------------------------------------------------------------

    #[tool(
        description = "Return the active GitHub account and scopes through the official gh CLI. Tokens are never returned and environment-token overrides are ignored; only OS-keyring-backed auth is accepted."
    )]
    async fn github_status(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("github_status", &Value::Null, || async {
            if !self.state.github_config.enabled {
                return Ok(continuum_core::github_cli::GitHubAuthStatus {
                    detail: "GitHub integration is disabled in config".to_string(),
                    ..continuum_core::github_cli::GitHubAuthStatus::default()
                });
            }
            Ok::<_, McpError>(
                continuum_core::github_cli::status(self.state.github_config.api_timeout_secs).await,
            )
        })
        .await
    }

    #[tool(
        description = "Return the connected GitHub user's profile. Read-only and keyring-authenticated through the official gh CLI."
    )]
    async fn github_me(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("github_me", &Value::Null, || async {
            crate::tools::github::me(&self.state.github_config)
                .await
                .map_err(github_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "List repositories visible to the connected GitHub account, with bounded page size and optional visibility filter."
    )]
    async fn github_list_repos(
        &self,
        Parameters(req): Parameters<GitHubListReposRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("github_list_repos", &req, || async {
            crate::tools::github::list_repos(&req, &self.state.github_config)
                .await
                .map_err(github_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "Read metadata for one owner/repository visible to the connected GitHub account."
    )]
    async fn github_get_repo(
        &self,
        Parameters(req): Parameters<GitHubRepoRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("github_get_repo", &req, || async {
            crate::tools::github::get_repo(&req, &self.state.github_config)
                .await
                .map_err(github_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "List issues and pull requests for one GitHub repository. Read-only; state and page size are validated and bounded."
    )]
    async fn github_list_issues(
        &self,
        Parameters(req): Parameters<GitHubListIssuesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("github_list_issues", &req, || async {
            crate::tools::github::list_issues(&req, &self.state.github_config)
                .await
                .map_err(github_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "Read one UTF-8 file or list one directory at an optional GitHub branch, tag, or commit. Traversal and oversized/binary content are rejected."
    )]
    async fn github_get_file(
        &self,
        Parameters(req): Parameters<GitHubGetFileRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("github_get_file", &req, || async {
            crate::tools::github::get_file(&req, &self.state.github_config)
                .await
                .map_err(github_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "Create a GitHub issue in one exact repository. Title/body are bounded and body is redacted from local tool audit details. Requires a fresh user confirmation every call."
    )]
    async fn github_create_issue(
        &self,
        Parameters(req): Parameters<GitHubCreateIssueRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("github_create_issue", &req, || async {
            crate::tools::github::create_issue(&req, &self.state.github_config)
                .await
                .map_err(github_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "Post a bounded GitHub comment to one issue or pull request. Comment body is redacted from local tool audit details. Requires a fresh user confirmation every call."
    )]
    async fn github_comment_issue(
        &self,
        Parameters(req): Parameters<GitHubCommentRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("github_comment_issue", &req, || async {
            crate::tools::github::comment_issue(&req, &self.state.github_config)
                .await
                .map_err(github_err_to_mcp)
        })
        .await
    }

    #[tool(
        description = "Open a GitHub pull request from an existing remote head branch to a base branch. Does not push code. Title/body/refs are validated and bounded. Requires a fresh user confirmation every call."
    )]
    async fn github_create_pull_request(
        &self,
        Parameters(req): Parameters<GitHubCreatePullRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("github_create_pull_request", &req, || async {
            crate::tools::github::create_pull_request(&req, &self.state.github_config)
                .await
                .map_err(github_err_to_mcp)
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
        description = "Compatibility tool for a future component restart consumer. Denied unless a repair capability explicitly authorizes an executable restart path; the safe Health flow does not."
    )]
    async fn repair_restart_component(
        &self,
        Parameters(req): Parameters<RestartRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.require_repair_restart_component(req.component.as_str())?;
        self.backup_before_repair_mutation("restart_component")?;
        self.run_tool("repair_restart_component", &req, || async {
            repairtool::restart(&self.state.data_dir, req.component)
                .map_err(|e| McpError::internal_error(e.to_string(), None))
        })
        .await
    }

    #[tool(
        description = "Compatibility tool for a future model reinstall consumer. Denied unless a repair capability explicitly authorizes it; the safe Health flow does not."
    )]
    async fn repair_reinstall_model(
        &self,
        Parameters(req): Parameters<ReinstallRequest>,
    ) -> Result<CallToolResult, McpError> {
        let grant = self.require_repair_component(req.component.as_str())?;
        if !grant.allow_model_reinstall {
            self.audit_repair_event(
                "repair_operation_denied",
                serde_json::json!({
                    "operation": "reinstall_model",
                    "component": req.component.as_str(),
                }),
            );
            return Err(McpError::invalid_request(
                "model reinstall was not explicitly authorized for this repair session",
                None,
            ));
        }
        self.backup_before_repair_mutation("reinstall_model")?;
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
        let grant = self.require_repair_session()?;
        if !grant.allow_config_rollback {
            self.audit_repair_event(
                "repair_operation_denied",
                serde_json::json!({ "operation": "rollback_config" }),
            );
            return Err(McpError::invalid_request(
                "config rollback was not explicitly authorized for this repair session",
                None,
            ));
        }
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
        self.require_repair_component(req.component.as_str())?;
        self.run_tool("repair_test_component", &req, || async {
            Ok::<_, McpError>(repairtool::test(&self.state.data_dir, req.component))
        })
        .await
    }

    #[tool(
        description = "Compatibility tool for a future escalation-intent consumer. The safe Health flow reports manual next steps in streamed assistant output instead."
    )]
    async fn repair_escalate(
        &self,
        Parameters(req): Parameters<EscalateRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.require_repair_escalation()?;
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

fn permission_resource(args: &Value) -> Option<String> {
    const RESOURCE_KEYS: &[&str] = &[
        "path",
        "source",
        "destination",
        "cwd",
        "repo",
        "repository",
        "url",
        "component",
    ];
    let object = args.as_object()?;
    if let (Some(owner), Some(repo)) = (
        object.get("owner").and_then(Value::as_str),
        object.get("repo").and_then(Value::as_str),
    ) {
        return Some(format!("github:{owner}/{repo}"));
    }
    for key in RESOURCE_KEYS {
        if let Some(value) = object.get(*key).and_then(Value::as_str) {
            return Some(value.chars().take(500).collect());
        }
    }
    None
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

fn file_action_err_to_mcp(error: crate::tools::file_actions::FileActionError) -> McpError {
    use crate::tools::file_actions::FileActionError;
    match error {
        FileActionError::Io(_) => McpError::internal_error(error.to_string(), None),
        _ => McpError::invalid_params(error.to_string(), None),
    }
}

fn git_err_to_mcp(error: crate::tools::git::GitToolError) -> McpError {
    use crate::tools::git::GitToolError;
    match error {
        GitToolError::Denied(_)
        | GitToolError::Git(_)
        | GitToolError::NonUtf8
        | GitToolError::InvalidCheckpointId
        | GitToolError::UnknownCheckpoint
        | GitToolError::UnsafePath => McpError::invalid_params(error.to_string(), None),
        GitToolError::Timeout | GitToolError::Io(_) => {
            McpError::internal_error(error.to_string(), None)
        }
    }
}

fn terminal_err_to_mcp(error: crate::tools::terminal::TerminalError) -> McpError {
    use crate::tools::terminal::TerminalError;
    match error {
        TerminalError::Spawn(_) | TerminalError::Evidence(_) | TerminalError::EvidencePath => {
            McpError::internal_error(error.to_string(), None)
        }
        _ => McpError::invalid_params(error.to_string(), None),
    }
}

fn github_err_to_mcp(error: crate::tools::github::GitHubToolError) -> McpError {
    use crate::tools::github::GitHubToolError;
    match error {
        GitHubToolError::Cli(_) | GitHubToolError::InvalidResponse(_) => {
            McpError::internal_error(error.to_string(), None)
        }
        _ => McpError::invalid_params(error.to_string(), None),
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
                 safe filesystem actions, recoverable Git checkpoints, web fetch, and notifications. \
                 Every tool call passes the local permission gateway and is audited.",
            )
    }
}

#[cfg(test)]
mod repair_authorization_tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn server_with_grant(
        data_dir: PathBuf,
        repair_grant: Option<RepairSessionGrant>,
    ) -> ContinuumMcpServer {
        if let Some(grant) = repair_grant.as_ref() {
            let grants_dir = data_dir.join("repair-grants");
            std::fs::create_dir_all(&grants_dir).unwrap();
            std::fs::write(
                grants_dir.join(format!("{}.json", grant.token)),
                serde_json::to_vec(grant).unwrap(),
            )
            .unwrap();
        }
        let raw_log_path = data_dir.join("raw_log.sqlite");
        let permissions = PermissionGateway::new(
            data_dir.clone(),
            "repair-test",
            include_str!("../../../config/default-permissions.toml"),
        );
        ContinuumMcpServer {
            state: Arc::new(ServerState {
                data_dir,
                http: reqwest::Client::new(),
                fs_extra_paths: Vec::new(),
                fs_max_write_bytes: 1024 * 1024,
                fs_max_patch_replacements: 100,
                terminal_config: crate::config::McpTerminalConfig::default(),
                github_config: continuum_core::config::GitHubConfig::default(),
                semantic: OnceCell::new(),
                episodic: OnceCell::new(),
                vault: OnceCell::new(),
                repair_grant,
                permissions,
                privacy: PrivacyFilter::from_config(
                    &continuum_core::config::ContextConfig::default(),
                    &continuum_core::config::PrivacyConfig::default(),
                ),
                context_tools: ContextToolsConfig::default(),
                toggles: ObservationToggles::default(),
                raw_log_path,
                git_context: GitContextConfig::default(),
                package_config: ContextPackageConfig::default(),
                process_watcher_enabled: true,
            }),
            tool_router: ContinuumMcpServer::tool_router(),
        }
    }

    #[test]
    fn repair_tools_default_to_denied_without_session_grant() {
        let tmp = tempfile::tempdir().unwrap();
        let server = server_with_grant(tmp.path().to_path_buf(), None);
        assert!(server.require_repair_session().is_err());
        assert!(server.require_repair_component("vision").is_err());
    }

    #[test]
    fn repair_grant_is_scoped_to_previewed_components() {
        let tmp = tempfile::tempdir().unwrap();
        let grant = RepairSessionGrant {
            token: uuid::Uuid::new_v4().to_string(),
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::minutes(1),
            allowed_components: vec!["vision".into()],
            allowed_restart_components: Vec::new(),
            allow_escalation_intent: false,
            allow_model_reinstall: false,
            allow_config_rollback: false,
        };
        let server = server_with_grant(tmp.path().to_path_buf(), Some(grant));
        assert!(server.require_repair_component("vision").is_ok());
        assert!(server.require_repair_component("memory").is_err());
        assert!(server.require_repair_restart_component("vision").is_err());
        assert!(server.require_repair_escalation().is_err());
    }

    #[test]
    fn restart_requires_a_distinct_execution_allowlist() {
        let tmp = tempfile::tempdir().unwrap();
        let grant = RepairSessionGrant {
            token: uuid::Uuid::new_v4().to_string(),
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::minutes(1),
            allowed_components: vec!["vision".into()],
            allowed_restart_components: vec!["vision".into()],
            allow_escalation_intent: false,
            allow_model_reinstall: false,
            allow_config_rollback: false,
        };
        let server = server_with_grant(tmp.path().to_path_buf(), Some(grant));
        assert!(server.require_repair_restart_component("vision").is_ok());
        assert!(server.require_repair_restart_component("triage").is_err());
    }

    #[test]
    fn permission_resource_uses_only_known_scope_fields() {
        assert_eq!(
            permission_resource(&serde_json::json!({ "path": "C:/repo/file.rs", "token": "no" })),
            Some("C:/repo/file.rs".to_string())
        );
        assert_eq!(
            permission_resource(&serde_json::json!({ "query": "hello" })),
            None
        );
    }
}
