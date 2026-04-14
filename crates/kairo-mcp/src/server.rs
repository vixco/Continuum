//! # KairoMcpServer
//!
//! The MCP server handler. Holds shared runtime state (store handles, HTTP
//! client, allowlist config) and routes tool calls via the rmcp
//! `#[tool_router]` macro infrastructure.
//!
//! ## Lazy store initialization
//!
//! Because kairo-mcp is spawned fresh on every orchestrator wake, we avoid
//! opening LanceDB + loading the fastembed model (≈200–300 ms) unless a tool
//! actually needs episodic memory. SQLite is light but is also lazy for
//! symmetry. First tool call that needs a store pays the open cost.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use kairo_core::memory::{
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

/// Shared server state. Held inside an `Arc` so the handler can derive `Clone`
/// cheaply.
pub(crate) struct ServerState {
    pub(crate) data_dir: PathBuf,
    #[allow(dead_code)]
    pub(crate) http: reqwest::Client,
    pub(crate) fs_extra_paths: Vec<PathBuf>,
    pub(crate) semantic: OnceCell<SemanticStore>,
    pub(crate) episodic: OnceCell<Mutex<EpisodicStore>>,
}

/// The main MCP server. Cloneable because rmcp can fan out the handler across
/// concurrent tool invocations.
#[derive(Clone)]
pub struct KairoMcpServer {
    pub(crate) state: Arc<ServerState>,
    #[allow(dead_code)] // populated by the #[tool_router] macro's dispatch table
    tool_router: ToolRouter<KairoMcpServer>,
}

// ---------------------------------------------------------------------------
// Construction + lazy store access
// ---------------------------------------------------------------------------

#[tool_router]
impl KairoMcpServer {
    /// Constructs a new server with all tools registered. Stores are opened
    /// lazily on first use to keep startup under ~20 ms.
    pub async fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&data_dir).with_context(|| {
            format!(
                "Failed to create Kairo data directory at {}",
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
            .user_agent(concat!("kairo-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Failed to build reqwest client")?;

        Ok(Self {
            state: Arc::new(ServerState {
                data_dir,
                http,
                fs_extra_paths: mcp_cfg.fs.extra_paths,
                semantic: OnceCell::new(),
                episodic: OnceCell::new(),
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
        description = "Semantic (vector) search over Kairo's episodic memory. Returns the top-N most similar past events — wakes, responses, remembered moments, and prior tool calls. Use this FIRST before asking the user things Kairo may already have seen."
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
        description = "Store or update a semantic fact Kairo has learned. Keys starting with 'system.' or 'kairo.' are reserved and rejected. Confidence is clamped by source: inferred ≤0.7, observed ≤0.8, user_stated ≤0.9."
    )]
    async fn memory_set_fact(
        &self,
        Parameters(req): Parameters<MemorySetFactRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("memory_set_fact", &req, || async {
            if memtool::is_reserved_key(&req.key) {
                return Err(McpError::invalid_params(
                    format!(
                        "Key '{}' uses a reserved prefix ('system.' / 'kairo.') — those are \
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
        description = "Read up to 100 KB of a UTF-8 text file. The path must be inside the allowlist (Kairo data dir, project.*.dir facts, or configured extra_paths). Denied paths, binary files, and paths matching the hardcoded deny list (.ssh, .env, *.pem, etc.) return an error. Larger files are truncated with a clearly marked prefix."
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
        description = "Restart a Kairo subsystem. Queues a restart intent the running kairo runtime picks up on its next tick. Targets: vision | triage | audio | stt | tts | orchestrator | mcp | memory | context_watcher."
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
        description = "Rollback config.toml from a dated backup under ~/.kairo-backups/. `date` format is `YYYY-MM-DD`. Destructive — the current config is overwritten; confirm before calling."
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
impl ServerHandler for KairoMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Kairo MCP server — exposes memory, system info, filesystem (read-only), \
                 web fetch, and notification tools to the orchestrator. Every tool call \
                 is audited to episodic memory.",
            )
    }
}
