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
use chrono::Utc;
use continuum_core::health::repair::RepairSessionGrant;
use continuum_core::memory::{
    episodic::EpisodicStore,
    semantic::{Fact, SemanticStore},
};
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
    MemoryQueryEpisodicRequest, MemorySetFactRequest, SetFactResponse,
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
    pub(crate) repair_grant: Option<RepairSessionGrant>,
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
                repair_grant,
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
        description = "List semantic facts — stable knowledge about the user, projects, preferences. Pass an optional key prefix (e.g. 'project.') to narrow results. Facts are stored at dotted keys like 'user.name' or 'project.simcharts.dir'."
    )]
    async fn memory_list_facts(
        &self,
        Parameters(req): Parameters<MemoryListFactsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_list_facts", &req, || async {
            let limit = req.limit.unwrap_or(50).min(200);
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
        description = "Fetch a single semantic fact by its dotted key. Returns null if not found. Prefer this over memory_list_facts when you know the exact key."
    )]
    async fn memory_get_fact(
        &self,
        Parameters(req): Parameters<MemoryGetFactRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_get_fact", &req, || async {
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
        description = "Store or update a semantic fact Continuum has learned. Keys starting with 'system.' or 'continuum.' are reserved and rejected. Confidence is clamped by source: inferred ≤0.7, observed ≤0.8, user_stated ≤0.9."
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

            let fact = Fact {
                key: req.key.clone(),
                value: req.value.clone(),
                confidence,
                source,
                source_frame_id: None,
                updated_at: Utc::now(),
            };

            let s = self
                .semantic()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            s.upsert_fact(&fact)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            Ok(SetFactResponse {
                key: req.key.clone(),
                stored: true,
                confidence,
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
        description = "Return Continuum's compact, ordered, source-attributed local live world-state: every connected monitor's latest local vision summary plus safe foreground-window, coarse input-activity, terminal, project, and degradation context. No raw screenshots, key values, pointer coordinates, or clipboard content."
    )]
    async fn system_live_context(&self) -> Result<CallToolResult, McpError> {
        self.run_tool("system_live_context", &Value::Null, || async {
            systool::live_context(&self.state.data_dir)
                .map_err(|error| McpError::internal_error(error, None))
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
                "Continuum MCP server — exposes memory, system info, filesystem (read-only), \
                 web fetch, and notification tools to the orchestrator. Every tool call \
                 is audited to episodic memory.",
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
        ContinuumMcpServer {
            state: Arc::new(ServerState {
                data_dir,
                http: reqwest::Client::new(),
                fs_extra_paths: Vec::new(),
                semantic: OnceCell::new(),
                episodic: OnceCell::new(),
                repair_grant,
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
}
