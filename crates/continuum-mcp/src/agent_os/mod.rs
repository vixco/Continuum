//! Continuum Agent OS: a provider-neutral action broker for computer use,
//! Composio SaaS tools, resumable plans, independent approvals and evidence.
//!
//! The server deliberately lives beside (rather than inside) the context MCP
//! server. It can be registered through Continuum's existing user-managed MCP
//! registry as `agent-os`, giving every supported orchestrator the same action
//! surface without coupling execution privileges to observation privileges.

pub mod composio;
pub mod computer;
pub mod evidence;
pub mod policy;
pub mod runs;
pub mod types;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result as AnyResult};
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

use composio::{classify_meta_tool, ComposioClient};
use computer::ComputerBackend;
use evidence::{EvidenceDraft, EvidenceStore};
use policy::{PolicyEngine, PolicyError};
use runs::{validate_run_id, RunStore};
use types::*;

type McpResult<T> = std::result::Result<T, McpError>;

const SERVER_INSTRUCTIONS: &str = r#"Continuum Agent OS is the execution plane for a persistent personal AI.

Use an observe -> plan -> act -> verify loop:
1. Observe the current computer or search Composio before choosing a tool.
2. Prefer semantic UI Automation selectors over raw coordinates.
3. Use agent_run_plan for multi-step work so progress is persisted and resumable.
4. Treat native approval dialogs as a hard user-consent boundary. Never retry a denied action by changing tools.
5. Inspect each returned verification block and recover when the expected state did not appear.
6. Use Composio search before executing unfamiliar app tools; connection-management tools may return an OAuth link for the user.
7. Destructive SaaS actions are denied by default. Do not attempt to bypass policy.
8. Never claim an action succeeded without a successful tool result and evidence id.
9. For requests such as "go back to the app I was in", use Continuum's context_window/context_timeline/context_search tools to resolve the prior app and title, then list windows, focus the closest exact live match, observe, act, and verify. Never guess a destination from app popularity.
10. The distinct amber AI pointer is the user's visible action boundary. Prefer semantic element targeting and keep actions fast, but never skip required approval or post-action verification.
"#;

struct AgentState {
    data_dir: PathBuf,
    root: PathBuf,
    policy: PolicyEngine,
    evidence: EvidenceStore,
    computer: ComputerBackend,
    composio: ComposioClient,
    runs: RunStore,
}

#[derive(Clone)]
pub struct AgentOsServer {
    state: Arc<AgentState>,
    #[allow(dead_code)]
    tool_router: ToolRouter<AgentOsServer>,
}

#[derive(Debug, Serialize)]
struct ActionExecution {
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_id: Option<String>,
    authorization: AuthorizationReport,
    result: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_warning: Option<String>,
}

#[derive(Debug)]
struct ActionFailure {
    message: String,
    evidence_id: Option<String>,
}

impl ActionFailure {
    fn new(message: impl Into<String>, evidence_id: Option<String>) -> Self {
        Self {
            message: message.into(),
            evidence_id,
        }
    }

    fn into_mcp(self) -> McpError {
        let data = self
            .evidence_id
            .map(|evidence_id| serde_json::json!({ "evidence_id": evidence_id }));
        McpError::invalid_request(self.message, data)
    }
}

#[tool_router]
impl AgentOsServer {
    pub fn new(data_dir: PathBuf) -> AnyResult<Self> {
        std::fs::create_dir_all(&data_dir).with_context(|| {
            format!(
                "Failed to create Continuum data directory at {}",
                data_dir.display()
            )
        })?;
        let root = data_dir.join("agent-os");
        std::fs::create_dir_all(&root)
            .with_context(|| format!("Failed to create {}", root.display()))?;
        let policy = PolicyEngine::load(&root)?;
        let evidence = EvidenceStore::new(&root.join("evidence"))?;
        let config_path = data_dir.join("config.toml");
        let ux = continuum_core::config::load_config(&config_path)
            .map(|config| config.agent_os)
            .unwrap_or_else(|error| {
                tracing::warn!(
                    layer = "agent_os",
                    component = "computer_use",
                    error = %error,
                    "Agent OS UX config could not be loaded; using safe defaults"
                );
                continuum_core::config::AgentOsConfig::default()
            });
        let computer = ComputerBackend::new(&root.join("computer"), ux)?;
        let composio = ComposioClient::new(&root)?;
        let runs = RunStore::new(&root)?;
        Ok(Self {
            state: Arc::new(AgentState {
                data_dir,
                root,
                policy,
                evidence,
                computer,
                composio,
                runs,
            }),
            tool_router: Self::tool_router(),
        })
    }

    // ---------------------------------------------------------------------
    // Agent control plane
    // ---------------------------------------------------------------------

    #[tool(
        description = "Return the Agent OS capability, platform, policy, Composio and run status. Read this before attempting computer or SaaS actions."
    )]
    async fn agent_status(
        &self,
        Parameters(request): Parameters<EmptyRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "agent_status",
            "agent.status",
            RiskLevel::Read,
            "Read Agent OS status",
            None,
            &request,
            None,
            |_| async {
                let policy = self.state.policy.snapshot().await;
                let composio = self.state.composio.status().await;
                let recent_runs = self.state.runs.recent(5)?;
                Ok(serde_json::json!({
                    "name": "Continuum Agent OS",
                    "version": env!("CARGO_PKG_VERSION"),
                    "data_dir": self.state.data_dir,
                    "root": self.state.root,
                    "evidence_log": self.state.evidence.active_path(),
                    "computer": self.state.computer.status(),
                    "composio": composio,
                    "policy": policy,
                    "recent_runs": recent_runs,
                    "architecture": {
                        "loop": ["observe", "plan", "act", "verify", "recover"],
                        "resumable_plans": true,
                        "independent_native_approvals": true,
                        "append_only_evidence": true,
                        "provider_neutral_mcp": true
                    }
                }))
            },
        )
        .await
    }

    #[tool(description = "Read the full persistent Agent OS permission policy.")]
    async fn agent_policy_get(
        &self,
        Parameters(request): Parameters<EmptyRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "agent_policy_get",
            "agent.policy.read",
            RiskLevel::Read,
            "Read Agent OS policy",
            None,
            &request,
            None,
            |_| async { Ok(serde_json::to_value(self.state.policy.snapshot().await)?) },
        )
        .await
    }

    #[tool(
        description = "Change one persistent capability policy. Tightening is immediate; relaxing a policy requires an independent native user approval dialog."
    )]
    async fn agent_policy_set(
        &self,
        Parameters(request): Parameters<PolicySetRequest>,
    ) -> McpResult<CallToolResult> {
        let input = serde_json::to_value(&request)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let started = Instant::now();
        match self
            .state
            .policy
            .set_policy(&request.capability, request.mode)
            .await
        {
            Ok(config) => {
                let authorization = AuthorizationReport {
                    capability: "agent.policy.write".to_string(),
                    configured_mode: PolicyMode::Ask,
                    allowed: true,
                    source: "policy_engine_guard".to_string(),
                };
                let result = serde_json::to_value(config)
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                let evidence = self
                    .state
                    .evidence
                    .record(EvidenceDraft {
                        run_id: None,
                        tool: "agent_policy_set",
                        capability: "agent.policy.write",
                        risk: RiskLevel::Destructive,
                        authorization: Some(&authorization),
                        outcome: "success",
                        duration: started.elapsed(),
                        input: &input,
                        result_summary: result.clone(),
                        error: None,
                    })
                    .await;
                let (evidence_id, evidence_warning) = evidence_parts(evidence);
                success_result(&ActionExecution {
                    evidence_id,
                    authorization,
                    result,
                    evidence_warning,
                })
            }
            Err(error) => {
                let message = error.to_string();
                let evidence_id = self
                    .state
                    .evidence
                    .record(EvidenceDraft {
                        run_id: None,
                        tool: "agent_policy_set",
                        capability: "agent.policy.write",
                        risk: RiskLevel::Destructive,
                        authorization: None,
                        outcome: "denied",
                        duration: started.elapsed(),
                        input: &input,
                        result_summary: Value::Null,
                        error: Some(&message),
                    })
                    .await
                    .ok();
                Err(ActionFailure::new(message, evidence_id).into_mcp())
            }
        }
    }

    #[tool(
        description = "Query the append-only Agent OS evidence log. Inputs are secret-redacted and typed text is never stored verbatim."
    )]
    async fn agent_evidence_query(
        &self,
        Parameters(request): Parameters<EvidenceQueryRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "agent_evidence_query",
            "agent.evidence.read",
            RiskLevel::Read,
            "Read Agent OS evidence",
            None,
            &request,
            None,
            |_| async {
                Ok(serde_json::to_value(
                    self.state.evidence.query(&request).await?,
                )?)
            },
        )
        .await
    }

    #[tool(description = "List up to 100 recently updated resumable Agent OS runs.")]
    async fn agent_recent_runs(
        &self,
        Parameters(request): Parameters<EmptyRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "agent_recent_runs",
            "agent.evidence.read",
            RiskLevel::Read,
            "List recent Agent OS runs",
            None,
            &request,
            None,
            |_| async { Ok(serde_json::to_value(self.state.runs.recent(25)?)?) },
        )
        .await
    }

    #[tool(description = "Fetch one persisted Agent OS run by run_id.")]
    async fn agent_get_run(
        &self,
        Parameters(request): Parameters<GetRunRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "agent_get_run",
            "agent.evidence.read",
            RiskLevel::Read,
            "Read one Agent OS run",
            None,
            &request,
            None,
            |_| async {
                let record = self
                    .state
                    .runs
                    .load(&request.run_id)?
                    .ok_or_else(|| anyhow::anyhow!("Unknown run_id {}", request.run_id))?;
                Ok(serde_json::to_value(record)?)
            },
        )
        .await
    }

    #[tool(
        description = "Execute or resume a persisted multi-step plan. The exact plan can be approved once in a native dialog; every step is independently policy-checked, verified and evidenced. Set dry_run=true to inspect risk without acting."
    )]
    async fn agent_run_plan(
        &self,
        Parameters(request): Parameters<RunPlanRequest>,
    ) -> McpResult<CallToolResult> {
        if request.goal.trim().is_empty() || request.goal.chars().count() > 4_000 {
            return Err(McpError::invalid_params(
                "goal must contain between 1 and 4,000 characters",
                None,
            ));
        }
        let max_steps = self.state.policy.max_plan_steps().await;
        if request.steps.is_empty() || request.steps.len() > max_steps {
            return Err(McpError::invalid_params(
                format!("steps must contain between 1 and {max_steps} entries"),
                None,
            ));
        }
        let classifications = classify_plan(&request.steps)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        if request.dry_run {
            return success_result(&serde_json::json!({
                "dry_run": true,
                "goal": request.goal,
                "steps": classifications,
                "message": "No policy prompt was shown and no action was executed"
            }));
        }
        let risk = classifications
            .iter()
            .filter_map(|entry| entry.get("risk").and_then(Value::as_str))
            .map(parse_risk)
            .max()
            .unwrap_or(RiskLevel::Write);
        let summary = plan_approval_summary(&request, &classifications);
        self.tool_action(
            "agent_run_plan",
            "agent.plan",
            risk,
            &summary,
            request.run_id.as_deref(),
            &request,
            None,
            |plan_authorization| async {
                self.execute_plan(&request, plan_authorization, classifications)
                    .await
            },
        )
        .await
    }

    // ---------------------------------------------------------------------
    // Computer use
    // ---------------------------------------------------------------------

    #[tool(
        description = "Return Windows computer-use backend capabilities without taking a screenshot."
    )]
    async fn computer_status(
        &self,
        Parameters(request): Parameters<EmptyRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_status",
            "computer.observe",
            RiskLevel::Read,
            "Read computer-use capabilities",
            None,
            &request,
            None,
            |_| async { Ok(self.state.computer.status()) },
        )
        .await
    }

    #[tool(
        description = "Observe the foreground window, cursor, monitors and optionally top-level windows, the UI Automation tree and a screenshot. Prefer this before acting."
    )]
    async fn computer_observe(
        &self,
        Parameters(request): Parameters<ObserveRequest>,
    ) -> McpResult<CallToolResult> {
        let capability = if request.include_screenshot {
            "computer.screenshot"
        } else {
            "computer.observe"
        };
        self.tool_action(
            "computer_observe",
            capability,
            RiskLevel::Read,
            "Observe the current Windows desktop state",
            None,
            &request,
            None,
            |_| async { self.state.computer.observe(&request).await },
        )
        .await
    }

    #[tool(
        description = "List visible top-level Windows applications and their handles, titles and bounds."
    )]
    async fn computer_list_windows(
        &self,
        Parameters(request): Parameters<EmptyRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_list_windows",
            "computer.observe",
            RiskLevel::Read,
            "List top-level windows",
            None,
            &request,
            None,
            |_| async { self.state.computer.list_windows().await },
        )
        .await
    }

    #[tool(
        description = "Read a bounded Windows UI Automation tree for the foreground window or a supplied window handle. Use names and automation ids for robust targeting."
    )]
    async fn computer_accessibility(
        &self,
        Parameters(request): Parameters<AccessibilityRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_accessibility",
            "computer.accessibility",
            RiskLevel::Read,
            "Read the Windows accessibility tree",
            None,
            &request,
            None,
            |_| async { self.state.computer.accessibility(&request).await },
        )
        .await
    }

    #[tool(
        description = "Capture the foreground window or full virtual desktop to a local PNG and return its path and exact bounds."
    )]
    async fn computer_screenshot(
        &self,
        Parameters(request): Parameters<ScreenshotRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_screenshot",
            "computer.screenshot",
            RiskLevel::Read,
            "Capture a Windows screenshot",
            None,
            &request,
            None,
            |_| async { self.state.computer.screenshot(&request).await },
        )
        .await
    }

    #[tool(
        description = "Find the best visible, enabled UI Automation element by name, automation_id, control_type and/or class_name. Does not click."
    )]
    async fn computer_find_element(
        &self,
        Parameters(request): Parameters<FindElementRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_find_element",
            "computer.accessibility",
            RiskLevel::Read,
            "Find a semantic UI element",
            None,
            &request,
            None,
            |_| async { self.state.computer.find_element(&request).await },
        )
        .await
    }

    #[tool(
        description = "Click an absolute virtual-screen coordinate, then return a before/after verification snapshot."
    )]
    async fn computer_click(
        &self,
        Parameters(request): Parameters<ClickRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_click",
            "computer.input",
            RiskLevel::Write,
            &format!(
                "Click {} at ({}, {})",
                mouse_button_name(request.button),
                request.x,
                request.y
            ),
            None,
            &request,
            None,
            |_| async {
                self.verified_computer(self.state.computer.click(&request), false)
                    .await
            },
        )
        .await
    }

    #[tool(
        description = "Find a UI element semantically and click the center of its current bounds, then return the matched element plus before/after verification. Prefer this over coordinate clicks."
    )]
    async fn computer_click_element(
        &self,
        Parameters(request): Parameters<ClickElementRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_click_element",
            "computer.input",
            RiskLevel::Write,
            &format!(
                "Click semantic UI element {}",
                selector_summary(&request.selector)
            ),
            None,
            &request,
            None,
            |_| async {
                self.verified_computer(self.state.computer.click_element(&request), false)
                    .await
            },
        )
        .await
    }

    #[tool(
        description = "Type Unicode text into the focused control using a temporary clipboard payload that is deleted and the user's previous clipboard is restored. Typed text is redacted from evidence."
    )]
    async fn computer_type(
        &self,
        Parameters(request): Parameters<TypeRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_type",
            "computer.input",
            RiskLevel::Write,
            &format!(
                "Type {} characters into the focused control{}",
                request.text.chars().count(),
                if request.replace_existing {
                    " and replace existing content"
                } else {
                    ""
                }
            ),
            None,
            &request,
            None,
            |_| async {
                self.verified_computer(self.state.computer.type_text(&request), false)
                    .await
            },
        )
        .await
    }

    #[tool(
        description = "Send a bounded keyboard shortcut or key sequence, then verify the resulting UI state."
    )]
    async fn computer_key(
        &self,
        Parameters(request): Parameters<KeyRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_key",
            "computer.input",
            RiskLevel::Write,
            &format!("Send keyboard sequence {}", request.keys.join("+")),
            None,
            &request,
            None,
            |_| async {
                self.verified_computer(self.state.computer.key(&request), false)
                    .await
            },
        )
        .await
    }

    #[tool(
        description = "Scroll vertically or horizontally at the current cursor position, then verify UI state."
    )]
    async fn computer_scroll(
        &self,
        Parameters(request): Parameters<ScrollRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_scroll",
            "computer.input",
            RiskLevel::Write,
            &format!(
                "Scroll {} by {} wheel units",
                if request.horizontal {
                    "horizontally"
                } else {
                    "vertically"
                },
                request.amount
            ),
            None,
            &request,
            None,
            |_| async {
                self.verified_computer(self.state.computer.scroll(&request), false)
                    .await
            },
        )
        .await
    }

    #[tool(
        description = "Focus a top-level window by handle, title fragment or process name and verify the foreground window changed."
    )]
    async fn computer_focus_window(
        &self,
        Parameters(request): Parameters<FocusWindowRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_focus_window",
            "computer.window",
            RiskLevel::Write,
            "Change the foreground Windows application",
            None,
            &request,
            None,
            |_| async {
                self.verified_computer(self.state.computer.focus_window(&request), false)
                    .await
            },
        )
        .await
    }

    #[tool(
        description = "Open an http/https URL with the user's default browser and verify the resulting foreground state."
    )]
    async fn computer_open_url(
        &self,
        Parameters(request): Parameters<OpenUrlRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_open_url",
            "computer.navigation",
            RiskLevel::Write,
            &format!("Open URL {}", request.url),
            None,
            &request,
            None,
            |_| async {
                self.verified_computer(self.state.computer.open_url(&request), false)
                    .await
            },
        )
        .await
    }

    #[tool(
        description = "Wait for a bounded duration without changing state. Useful between asynchronous UI transitions."
    )]
    async fn computer_wait(
        &self,
        Parameters(request): Parameters<WaitRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_wait",
            "computer.observe",
            RiskLevel::Read,
            &format!("Wait {} ms", request.milliseconds),
            None,
            &request,
            None,
            |_| async { self.state.computer.wait(&request).await },
        )
        .await
    }

    #[tool(
        description = "Poll the Windows UI Automation tree until a semantic element appears or a bounded timeout expires."
    )]
    async fn computer_wait_for_element(
        &self,
        Parameters(request): Parameters<WaitForElementRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "computer_wait_for_element",
            "computer.accessibility",
            RiskLevel::Read,
            &format!(
                "Wait for semantic UI element {}",
                selector_summary(&request.selector)
            ),
            None,
            &request,
            None,
            |_| async { self.state.computer.wait_for_element(&request).await },
        )
        .await
    }

    // ---------------------------------------------------------------------
    // Composio
    // ---------------------------------------------------------------------

    #[tool(
        description = "Return Composio configuration, API-key availability, session and enabled toolkit status without exposing credentials."
    )]
    async fn composio_status(
        &self,
        Parameters(request): Parameters<EmptyRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "composio_status",
            "composio.read",
            RiskLevel::Read,
            "Read Composio integration status",
            None,
            &request,
            None,
            |_| async { Ok(self.state.composio.status().await) },
        )
        .await
    }

    #[tool(
        description = "Configure the persistent Composio user id, optional toolkit allowlist and API base. API keys are never accepted through this tool."
    )]
    async fn composio_configure(
        &self,
        Parameters(request): Parameters<ComposioConfigureRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "composio_configure",
            "composio.connect",
            RiskLevel::Write,
            &format!("Configure Composio for user {}", request.user_id),
            None,
            &request,
            None,
            |_| async { self.state.composio.configure(&request).await },
        )
        .await
    }

    #[tool(
        description = "Create or reuse a Composio Tool Router session. The session exposes discovery, OAuth connection management and execution across enabled apps."
    )]
    async fn composio_create_session(
        &self,
        Parameters(request): Parameters<ComposioCreateSessionRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "composio_create_session",
            "composio.connect",
            RiskLevel::Write,
            "Create a Composio Tool Router session",
            None,
            &request,
            None,
            |_| async { self.state.composio.create_session(&request).await },
        )
        .await
    }

    #[tool(
        description = "Search Composio by natural-language use case (up to seven queries). Returns matching slugs, schemas, connection state and workflow guidance. Call this before unfamiliar tools."
    )]
    async fn composio_search(
        &self,
        Parameters(request): Parameters<ComposioSearchRequest>,
    ) -> McpResult<CallToolResult> {
        self.tool_action(
            "composio_search",
            "composio.read",
            RiskLevel::Read,
            "Search Composio tools",
            None,
            &request,
            None,
            |_| async { self.state.composio.search(&request).await },
        )
        .await
    }

    #[tool(
        description = "Execute one discovered Composio app tool. Risk is inferred from the concrete tool slug; reads may run automatically, writes ask, and destructive actions are denied by default."
    )]
    async fn composio_execute(
        &self,
        Parameters(request): Parameters<ComposioExecuteRequest>,
    ) -> McpResult<CallToolResult> {
        let risk = composio::classify_execute_request(&request);
        let capability = composio_capability(risk);
        let summary = request
            .intent
            .clone()
            .unwrap_or_else(|| format!("Execute Composio tool {}", request.tool_slug));
        self.tool_action(
            "composio_execute",
            capability,
            risk,
            &summary,
            None,
            &request,
            None,
            |_| async { self.state.composio.execute(&request).await },
        )
        .await
    }

    #[tool(
        description = "Execute one official COMPOSIO_* meta-tool for schemas, multi-execution, OAuth connections, waiting or workbench. Risk is derived from the meta-tool and arguments."
    )]
    async fn composio_execute_meta(
        &self,
        Parameters(request): Parameters<ComposioMetaExecuteRequest>,
    ) -> McpResult<CallToolResult> {
        let risk = classify_meta_tool(&request.meta_tool, &request.arguments);
        let capability = if request
            .meta_tool
            .eq_ignore_ascii_case("COMPOSIO_MANAGE_CONNECTIONS")
        {
            if risk == RiskLevel::Destructive {
                "composio.destructive"
            } else {
                "composio.connect"
            }
        } else {
            composio_capability(risk)
        };
        let summary = request
            .intent
            .clone()
            .unwrap_or_else(|| format!("Execute Composio meta-tool {}", request.meta_tool));
        self.tool_action(
            "composio_execute_meta",
            capability,
            risk,
            &summary,
            None,
            &request,
            None,
            |_| async { self.state.composio.execute_meta(&request).await },
        )
        .await
    }

    // ---------------------------------------------------------------------
    // Internal execution helpers
    // ---------------------------------------------------------------------

    // This boundary intentionally carries the complete policy/evidence context.
    #[allow(clippy::too_many_arguments)]
    async fn tool_action<I, F, Fut>(
        &self,
        tool_name: &'static str,
        capability: &'static str,
        risk: RiskLevel,
        summary: &str,
        run_id: Option<&str>,
        input: &I,
        inherited_authorization: Option<&AuthorizationReport>,
        body: F,
    ) -> McpResult<CallToolResult>
    where
        I: Serialize + ?Sized,
        F: FnOnce(AuthorizationReport) -> Fut,
        Fut: Future<Output = AnyResult<Value>>,
    {
        match self
            .execute_action(
                tool_name,
                capability,
                risk,
                summary,
                run_id,
                input,
                inherited_authorization,
                body,
            )
            .await
        {
            Ok(execution) => success_result(&execution),
            Err(failure) => Err(failure.into_mcp()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_action<I, F, Fut>(
        &self,
        tool_name: &'static str,
        capability: &'static str,
        risk: RiskLevel,
        summary: &str,
        run_id: Option<&str>,
        input: &I,
        inherited_authorization: Option<&AuthorizationReport>,
        body: F,
    ) -> std::result::Result<ActionExecution, ActionFailure>
    where
        I: Serialize + ?Sized,
        F: FnOnce(AuthorizationReport) -> Fut,
        Fut: Future<Output = AnyResult<Value>>,
    {
        let input_value = serde_json::to_value(input)
            .map_err(|error| ActionFailure::new(error.to_string(), None))?;
        let started = Instant::now();
        let authorization = match self
            .authorize_capability(capability, risk, summary, inherited_authorization)
            .await
        {
            Ok(report) => report,
            Err(error) => {
                let message = error.to_string();
                let evidence_id = self
                    .state
                    .evidence
                    .record(EvidenceDraft {
                        run_id,
                        tool: tool_name,
                        capability,
                        risk,
                        authorization: None,
                        outcome: "denied",
                        duration: started.elapsed(),
                        input: &input_value,
                        result_summary: Value::Null,
                        error: Some(&message),
                    })
                    .await
                    .ok();
                return Err(ActionFailure::new(message, evidence_id));
            }
        };

        match body(authorization.clone()).await {
            Ok(result) => {
                let evidence = self
                    .state
                    .evidence
                    .record(EvidenceDraft {
                        run_id,
                        tool: tool_name,
                        capability,
                        risk,
                        authorization: Some(&authorization),
                        outcome: "success",
                        duration: started.elapsed(),
                        input: &input_value,
                        result_summary: result.clone(),
                        error: None,
                    })
                    .await;
                let (evidence_id, evidence_warning) = evidence_parts(evidence);
                Ok(ActionExecution {
                    evidence_id,
                    authorization,
                    result,
                    evidence_warning,
                })
            }
            Err(error) => {
                let message = error.to_string();
                let evidence_id = self
                    .state
                    .evidence
                    .record(EvidenceDraft {
                        run_id,
                        tool: tool_name,
                        capability,
                        risk,
                        authorization: Some(&authorization),
                        outcome: "error",
                        duration: started.elapsed(),
                        input: &input_value,
                        result_summary: Value::Null,
                        error: Some(&message),
                    })
                    .await
                    .ok();
                Err(ActionFailure::new(message, evidence_id))
            }
        }
    }

    async fn authorize_capability(
        &self,
        capability: &str,
        risk: RiskLevel,
        summary: &str,
        inherited_authorization: Option<&AuthorizationReport>,
    ) -> std::result::Result<AuthorizationReport, PolicyError> {
        let configured_mode = self.state.policy.configured_mode(capability).await;
        match configured_mode {
            PolicyMode::Deny => Err(PolicyError::Denied {
                capability: capability.to_string(),
            }),
            PolicyMode::Allow => Ok(AuthorizationReport {
                capability: capability.to_string(),
                configured_mode,
                allowed: true,
                source: "persistent_policy".to_string(),
            }),
            PolicyMode::Ask => {
                if let Some(plan) = inherited_authorization
                    .filter(|report| report.allowed && report.source.starts_with("approved_plan:"))
                {
                    Ok(AuthorizationReport {
                        capability: capability.to_string(),
                        configured_mode,
                        allowed: true,
                        source: plan.source.clone(),
                    })
                } else {
                    self.state.policy.authorize(capability, risk, summary).await
                }
            }
        }
    }

    async fn verified_computer<Fut>(&self, action: Fut, force_verify: bool) -> AnyResult<Value>
    where
        Fut: Future<Output = AnyResult<Value>>,
    {
        let verify = force_verify || self.state.policy.verify_after_mutation().await;
        let before = if verify {
            capture_state(self.state.computer.verification_state()).await
        } else {
            StateCapture::disabled()
        };
        let action_result = action.await?;
        let after = if verify {
            capture_state(self.state.computer.verification_state()).await
        } else {
            StateCapture::disabled()
        };
        let state_changed = match (&before.value, &after.value) {
            (Some(before), Some(after)) => {
                Some(normalize_capture(before) != normalize_capture(after))
            }
            _ => None,
        };
        Ok(serde_json::json!({
            "action": action_result,
            "verification": {
                "requested": verify,
                "before": before.value,
                "after": after.value,
                "before_error": before.error,
                "after_error": after.error,
                "state_changed": state_changed,
                "interpretation": if verify {
                    "Compare the post-action foreground/accessibility state with the expected outcome; state_changed=false is not by itself proof of failure for text input."
                } else {
                    "Post-action verification is disabled by policy and was not performed."
                }
            }
        }))
    }

    async fn execute_plan(
        &self,
        request: &RunPlanRequest,
        plan_authorization: AuthorizationReport,
        classifications: Vec<Value>,
    ) -> AnyResult<Value> {
        let run_id = request
            .run_id
            .clone()
            .unwrap_or_else(|| format!("run_{}", uuid::Uuid::new_v4().simple()));
        validate_run_id(&run_id)?;
        let now = chrono::Utc::now();
        let mut record = match self.state.runs.load(&run_id)? {
            Some(existing) => {
                if existing.goal != request.goal || existing.steps != request.steps {
                    bail!(
                        "run_id {run_id} already exists with a different immutable goal or step list"
                    );
                }
                if existing.status == "completed" || existing.status == "completed_with_errors" {
                    return Ok(serde_json::json!({
                        "resumed": false,
                        "already_finished": true,
                        "classification": classifications,
                        "run": existing
                    }));
                }
                existing
            }
            None => RunRecord {
                run_id: run_id.clone(),
                goal: request.goal.clone(),
                status: "running".to_string(),
                created_at: now,
                updated_at: now,
                steps: request.steps.clone(),
                results: Vec::new(),
            },
        };
        record.status = "running".to_string();
        record.updated_at = chrono::Utc::now();
        self.state.runs.save(&record)?;

        let inherited_plan = if plan_authorization.source == "native_user_approval" {
            Some(AuthorizationReport {
                capability: "agent.plan".to_string(),
                configured_mode: PolicyMode::Ask,
                allowed: true,
                source: format!("approved_plan:{run_id}"),
            })
        } else {
            None
        };

        let mut had_errors = false;
        for (index, step) in request.steps.iter().enumerate() {
            if record
                .results
                .iter()
                .any(|result| result.index == index && result.status == "success")
            {
                continue;
            }
            record.results.retain(|result| result.index != index);
            let step_id = step
                .id
                .clone()
                .unwrap_or_else(|| format!("step_{}", index + 1));
            match self
                .execute_plan_step(
                    &run_id,
                    step,
                    request.verify_each_step,
                    inherited_plan.as_ref(),
                )
                .await
            {
                Ok(execution) => {
                    record.results.push(PlanStepResult {
                        index,
                        id: step_id,
                        action: step.action.clone(),
                        status: "success".to_string(),
                        evidence_id: execution.evidence_id.clone(),
                        result: serde_json::json!({
                            "authorization": execution.authorization,
                            "result": execution.result,
                            "evidence_warning": execution.evidence_warning
                        }),
                        error: None,
                    });
                }
                Err(failure) => {
                    had_errors = true;
                    record.results.push(PlanStepResult {
                        index,
                        id: step_id,
                        action: step.action.clone(),
                        status: "error".to_string(),
                        evidence_id: failure.evidence_id,
                        result: Value::Null,
                        error: Some(failure.message),
                    });
                    record.status = "failed".to_string();
                    record.updated_at = chrono::Utc::now();
                    self.state.runs.save(&record)?;
                    if !step.continue_on_error {
                        return Ok(serde_json::json!({
                            "resumed": request.run_id.is_some(),
                            "halted_at_step": index,
                            "classification": classifications,
                            "run": record
                        }));
                    }
                }
            }
            record.updated_at = chrono::Utc::now();
            self.state.runs.save(&record)?;
        }
        record.status = if had_errors {
            "completed_with_errors".to_string()
        } else {
            "completed".to_string()
        };
        record.updated_at = chrono::Utc::now();
        self.state.runs.save(&record)?;
        Ok(serde_json::json!({
            "resumed": request.run_id.is_some(),
            "classification": classifications,
            "run": record
        }))
    }

    async fn execute_plan_step(
        &self,
        run_id: &str,
        step: &PlanStep,
        verify: bool,
        inherited: Option<&AuthorizationReport>,
    ) -> std::result::Result<ActionExecution, ActionFailure> {
        let action = step.action.trim();
        macro_rules! parse {
            ($ty:ty) => {
                serde_json::from_value::<$ty>(step.arguments.clone()).map_err(|error| {
                    ActionFailure::new(
                        format!("Invalid arguments for {}: {error}", step.action),
                        None,
                    )
                })?
            };
        }
        match action {
            "computer_observe" => {
                let request = parse!(ObserveRequest);
                let capability = if request.include_screenshot {
                    "computer.screenshot"
                } else {
                    "computer.observe"
                };
                self.execute_action(
                    "computer_observe",
                    capability,
                    RiskLevel::Read,
                    "Observe computer state as part of an approved plan",
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async { self.state.computer.observe(&request).await },
                )
                .await
            }
            "computer_list_windows" => {
                let request = parse!(EmptyRequest);
                self.execute_action(
                    "computer_list_windows",
                    "computer.observe",
                    RiskLevel::Read,
                    "List windows as part of an approved plan",
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async { self.state.computer.list_windows().await },
                )
                .await
            }
            "computer_accessibility" => {
                let request = parse!(AccessibilityRequest);
                self.execute_action(
                    "computer_accessibility",
                    "computer.accessibility",
                    RiskLevel::Read,
                    "Read accessibility state as part of an approved plan",
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async { self.state.computer.accessibility(&request).await },
                )
                .await
            }
            "computer_screenshot" => {
                let request = parse!(ScreenshotRequest);
                self.execute_action(
                    "computer_screenshot",
                    "computer.screenshot",
                    RiskLevel::Read,
                    "Capture a screenshot as part of an approved plan",
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async { self.state.computer.screenshot(&request).await },
                )
                .await
            }
            "computer_find_element" => {
                let request = parse!(FindElementRequest);
                self.execute_action(
                    "computer_find_element",
                    "computer.accessibility",
                    RiskLevel::Read,
                    "Find a semantic UI element as part of an approved plan",
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async { self.state.computer.find_element(&request).await },
                )
                .await
            }
            "computer_click" => {
                let request = parse!(ClickRequest);
                self.execute_action(
                    "computer_click",
                    "computer.input",
                    RiskLevel::Write,
                    &format!("Plan click at ({}, {})", request.x, request.y),
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async {
                        self.verified_computer(self.state.computer.click(&request), verify)
                            .await
                    },
                )
                .await
            }
            "computer_click_element" => {
                let request = parse!(ClickElementRequest);
                self.execute_action(
                    "computer_click_element",
                    "computer.input",
                    RiskLevel::Write,
                    &format!(
                        "Plan click semantic element {}",
                        selector_summary(&request.selector)
                    ),
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async {
                        self.verified_computer(self.state.computer.click_element(&request), verify)
                            .await
                    },
                )
                .await
            }
            "computer_type" => {
                let request = parse!(TypeRequest);
                self.execute_action(
                    "computer_type",
                    "computer.input",
                    RiskLevel::Write,
                    &format!("Plan type {} characters", request.text.chars().count()),
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async {
                        self.verified_computer(self.state.computer.type_text(&request), verify)
                            .await
                    },
                )
                .await
            }
            "computer_key" => {
                let request = parse!(KeyRequest);
                self.execute_action(
                    "computer_key",
                    "computer.input",
                    RiskLevel::Write,
                    &format!("Plan keyboard sequence {}", request.keys.join("+")),
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async {
                        self.verified_computer(self.state.computer.key(&request), verify)
                            .await
                    },
                )
                .await
            }
            "computer_scroll" => {
                let request = parse!(ScrollRequest);
                self.execute_action(
                    "computer_scroll",
                    "computer.input",
                    RiskLevel::Write,
                    "Plan scroll",
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async {
                        self.verified_computer(self.state.computer.scroll(&request), verify)
                            .await
                    },
                )
                .await
            }
            "computer_focus_window" => {
                let request = parse!(FocusWindowRequest);
                self.execute_action(
                    "computer_focus_window",
                    "computer.window",
                    RiskLevel::Write,
                    "Plan focus window",
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async {
                        self.verified_computer(self.state.computer.focus_window(&request), verify)
                            .await
                    },
                )
                .await
            }
            "computer_open_url" => {
                let request = parse!(OpenUrlRequest);
                self.execute_action(
                    "computer_open_url",
                    "computer.navigation",
                    RiskLevel::Write,
                    &format!("Plan open URL {}", request.url),
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async {
                        self.verified_computer(self.state.computer.open_url(&request), verify)
                            .await
                    },
                )
                .await
            }
            "computer_wait" => {
                let request = parse!(WaitRequest);
                self.execute_action(
                    "computer_wait",
                    "computer.observe",
                    RiskLevel::Read,
                    &format!("Plan wait {} ms", request.milliseconds),
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async { self.state.computer.wait(&request).await },
                )
                .await
            }
            "computer_wait_for_element" => {
                let request = parse!(WaitForElementRequest);
                self.execute_action(
                    "computer_wait_for_element",
                    "computer.accessibility",
                    RiskLevel::Read,
                    "Plan wait for semantic UI element",
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async { self.state.computer.wait_for_element(&request).await },
                )
                .await
            }
            "composio_create_session" => {
                let request = parse!(ComposioCreateSessionRequest);
                self.execute_action(
                    "composio_create_session",
                    "composio.connect",
                    RiskLevel::Write,
                    "Create Composio session as part of an approved plan",
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async { self.state.composio.create_session(&request).await },
                )
                .await
            }
            "composio_search" => {
                let request = parse!(ComposioSearchRequest);
                self.execute_action(
                    "composio_search",
                    "composio.read",
                    RiskLevel::Read,
                    "Search Composio as part of an approved plan",
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async { self.state.composio.search(&request).await },
                )
                .await
            }
            "composio_execute" => {
                let request = parse!(ComposioExecuteRequest);
                let risk = composio::classify_execute_request(&request);
                self.execute_action(
                    "composio_execute",
                    composio_capability(risk),
                    risk,
                    request
                        .intent
                        .as_deref()
                        .unwrap_or("Execute Composio app tool as part of an approved plan"),
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async { self.state.composio.execute(&request).await },
                )
                .await
            }
            "composio_execute_meta" => {
                let request = parse!(ComposioMetaExecuteRequest);
                let risk = classify_meta_tool(&request.meta_tool, &request.arguments);
                let capability = if request
                    .meta_tool
                    .eq_ignore_ascii_case("COMPOSIO_MANAGE_CONNECTIONS")
                {
                    if risk == RiskLevel::Destructive {
                        "composio.destructive"
                    } else {
                        "composio.connect"
                    }
                } else {
                    composio_capability(risk)
                };
                self.execute_action(
                    "composio_execute_meta",
                    capability,
                    risk,
                    request
                        .intent
                        .as_deref()
                        .unwrap_or("Execute Composio meta-tool as part of an approved plan"),
                    Some(run_id),
                    &request,
                    inherited,
                    |_| async { self.state.composio.execute_meta(&request).await },
                )
                .await
            }
            _ => Err(ActionFailure::new(
                format!("Unsupported Agent OS plan action {action:?}"),
                None,
            )),
        }
    }
}

#[tool_handler]
impl ServerHandler for AgentOsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(SERVER_INSTRUCTIONS)
    }
}

#[derive(Debug)]
struct StateCapture {
    value: Option<Value>,
    error: Option<String>,
}

impl StateCapture {
    fn disabled() -> Self {
        Self {
            value: None,
            error: None,
        }
    }
}

async fn capture_state<Fut>(future: Fut) -> StateCapture
where
    Fut: Future<Output = AnyResult<Value>>,
{
    match tokio::time::timeout(Duration::from_secs(25), future).await {
        Ok(Ok(value)) => StateCapture {
            value: Some(value),
            error: None,
        },
        Ok(Err(error)) => StateCapture {
            value: None,
            error: Some(error.to_string()),
        },
        Err(_) => StateCapture {
            value: None,
            error: Some("verification snapshot timed out".to_string()),
        },
    }
}

fn normalize_capture(value: &Value) -> Value {
    let mut normalized = value.clone();
    if let Some(object) = normalized.as_object_mut() {
        object.remove("captured_at");
        if let Some(accessibility) = object
            .get_mut("accessibility")
            .and_then(Value::as_object_mut)
        {
            accessibility.remove("captured_at");
        }
    }
    normalized
}

fn success_result(value: &impl Serialize) -> McpResult<CallToolResult> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

fn evidence_parts(result: AnyResult<String>) -> (Option<String>, Option<String>) {
    match result {
        Ok(id) => (Some(id), None),
        Err(error) => (
            None,
            Some(format!(
                "The action completed, but evidence persistence failed: {error}"
            )),
        ),
    }
}

fn composio_capability(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::Read => "composio.read",
        RiskLevel::Write => "composio.write",
        RiskLevel::Destructive => "composio.destructive",
    }
}

fn mouse_button_name(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
}

fn selector_summary(selector: &FindElementRequest) -> String {
    let fields = [
        selector
            .name
            .as_deref()
            .map(|value| format!("name={value:?}")),
        selector
            .automation_id
            .as_deref()
            .map(|value| format!("automation_id={value:?}")),
        selector
            .control_type
            .as_deref()
            .map(|value| format!("control_type={value:?}")),
        selector
            .class_name
            .as_deref()
            .map(|value| format!("class_name={value:?}")),
    ];
    fields.into_iter().flatten().collect::<Vec<_>>().join(", ")
}

fn classify_plan(steps: &[PlanStep]) -> AnyResult<Vec<Value>> {
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let (capability, risk, description) = classify_step(step)?;
            Ok(serde_json::json!({
                "index": index,
                "id": step.id.clone().unwrap_or_else(|| format!("step_{}", index + 1)),
                "action": step.action,
                "capability": capability,
                "risk": risk.as_str(),
                "description": description,
                "continue_on_error": step.continue_on_error,
                "expectation": step.expectation
            }))
        })
        .collect()
}

fn classify_step(step: &PlanStep) -> AnyResult<(&'static str, RiskLevel, String)> {
    let action = step.action.trim();
    let classification = match action {
        "computer_observe" => {
            let request: ObserveRequest = serde_json::from_value(step.arguments.clone())?;
            if request.include_screenshot {
                (
                    "computer.screenshot",
                    RiskLevel::Read,
                    "Observe the desktop including a screenshot".to_string(),
                )
            } else {
                (
                    "computer.observe",
                    RiskLevel::Read,
                    "Observe the desktop".to_string(),
                )
            }
        }
        "computer_list_windows" => (
            "computer.observe",
            RiskLevel::Read,
            "List top-level windows".to_string(),
        ),
        "computer_accessibility" | "computer_find_element" | "computer_wait_for_element" => (
            "computer.accessibility",
            RiskLevel::Read,
            "Read or wait on semantic UI state".to_string(),
        ),
        "computer_screenshot" => (
            "computer.screenshot",
            RiskLevel::Read,
            "Capture a screenshot".to_string(),
        ),
        "computer_click"
        | "computer_click_element"
        | "computer_type"
        | "computer_key"
        | "computer_scroll" => (
            "computer.input",
            RiskLevel::Write,
            redact_plan_description(step),
        ),
        "computer_focus_window" => (
            "computer.window",
            RiskLevel::Write,
            "Change the foreground window".to_string(),
        ),
        "computer_open_url" => (
            "computer.navigation",
            RiskLevel::Write,
            "Open a browser URL".to_string(),
        ),
        "computer_wait" => (
            "computer.observe",
            RiskLevel::Read,
            "Wait for an asynchronous transition".to_string(),
        ),
        "composio_create_session" => (
            "composio.connect",
            RiskLevel::Write,
            "Create a Composio Tool Router session".to_string(),
        ),
        "composio_search" => (
            "composio.read",
            RiskLevel::Read,
            "Discover Composio tools".to_string(),
        ),
        "composio_execute" => {
            let request: ComposioExecuteRequest = serde_json::from_value(step.arguments.clone())?;
            let risk = composio::classify_execute_request(&request);
            (
                composio_capability(risk),
                risk,
                request
                    .intent
                    .unwrap_or_else(|| format!("Execute {}", request.tool_slug)),
            )
        }
        "composio_execute_meta" => {
            let request: ComposioMetaExecuteRequest =
                serde_json::from_value(step.arguments.clone())?;
            let risk = classify_meta_tool(&request.meta_tool, &request.arguments);
            let capability = if request
                .meta_tool
                .eq_ignore_ascii_case("COMPOSIO_MANAGE_CONNECTIONS")
            {
                if risk == RiskLevel::Destructive {
                    "composio.destructive"
                } else {
                    "composio.connect"
                }
            } else {
                composio_capability(risk)
            };
            (
                capability,
                risk,
                request
                    .intent
                    .unwrap_or_else(|| format!("Execute {}", request.meta_tool)),
            )
        }
        _ => bail!("Unsupported Agent OS plan action {action:?}"),
    };
    Ok(classification)
}

fn redact_plan_description(step: &PlanStep) -> String {
    if step.action == "computer_type" {
        let chars = step
            .arguments
            .get("text")
            .and_then(Value::as_str)
            .map(|text| text.chars().count())
            .unwrap_or_default();
        format!("Type {chars} redacted characters")
    } else {
        step.action.replace('_', " ")
    }
}

fn plan_approval_summary(request: &RunPlanRequest, classifications: &[Value]) -> String {
    let mut lines = vec![format!("Goal: {}", request.goal.trim())];
    for item in classifications.iter().take(40) {
        lines.push(format!(
            "{}. {} [{} / {}] — {}",
            item.get("index").and_then(Value::as_u64).unwrap_or(0) + 1,
            item.get("action")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            item.get("capability")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            item.get("risk")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            item.get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
        ));
    }
    lines.join("\n")
}

fn parse_risk(value: &str) -> RiskLevel {
    match value {
        "destructive" => RiskLevel::Destructive,
        "write" => RiskLevel::Write,
        _ => RiskLevel::Read,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_steps_are_redacted_in_plan_summary() {
        let step = PlanStep {
            id: None,
            action: "computer_type".into(),
            arguments: serde_json::json!({"text":"private value"}),
            expectation: None,
            continue_on_error: false,
        };
        let classifications = classify_plan(&[step]).expect("classify");
        let rendered = serde_json::to_string(&classifications).expect("json");
        assert!(!rendered.contains("private value"));
        assert!(rendered.contains("13 redacted characters"));
    }

    #[test]
    fn destructive_composio_step_is_detected() {
        let step = PlanStep {
            id: None,
            action: "composio_execute".into(),
            arguments: serde_json::json!({
                "tool_slug":"SLACK_DELETE_MESSAGE",
                "arguments": {}
            }),
            expectation: None,
            continue_on_error: false,
        };
        let (_, risk, _) = classify_step(&step).expect("classify");
        assert_eq!(risk, RiskLevel::Destructive);
    }
}
