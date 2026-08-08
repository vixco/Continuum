use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, Content, ErrorData as McpError, ListToolsResult,
        PaginatedRequestParams, ServerInfo, Tool,
    },
    service::RequestContext,
    RoleServer, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::agent_os::AgentOsServer;

const JOURNAL_VERSION: u32 = 1;
const MAX_PLAN_STEPS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReliableRisk {
    Read,
    Write,
    Destructive,
}

impl ReliableRisk {
    fn is_mutating(self) -> bool {
        self != Self::Read
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Destructive => "destructive",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReliablePlanRequest {
    goal: String,
    steps: Vec<ReliableStep>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default = "default_true")]
    verify_each_step: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ReliableStep {
    #[serde(default)]
    id: Option<String>,
    action: String,
    #[serde(default)]
    arguments: Value,
    #[serde(default)]
    expectation: Option<String>,
    #[serde(default)]
    continue_on_error: bool,
}

#[derive(Debug, Clone)]
enum Postcondition {
    ResultOk,
    StateChanged,
    JsonPointerExists(String),
    JsonPointerEquals { pointer: String, expected: Value },
    TextContains(String),
    WindowTitleContains(String),
    ElementPresent(Map<String, Value>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StepState {
    Pending,
    Running,
    Dispatched,
    Verified,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalState {
    Running,
    Completed,
    CompletedWithErrors,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerificationRecord {
    status: String,
    contract: String,
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalStep {
    index: usize,
    id: String,
    action: String,
    risk: ReliableRisk,
    state: StepState,
    #[serde(default)]
    started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    verification: Option<VerificationRecord>,
    #[serde(default)]
    result_summary: Value,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReliableJournal {
    version: u32,
    run_id: String,
    plan_hash: String,
    goal_summary: String,
    state: JournalState,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    steps: Vec<JournalStep>,
    #[serde(default)]
    final_result: Option<Value>,
}

#[derive(Clone)]
pub struct ReliableAgentOsServer {
    inner: AgentOsServer,
    root: Arc<PathBuf>,
    process_gate: Arc<Mutex<()>>,
}

impl ReliableAgentOsServer {
    pub fn new(inner: AgentOsServer, data_dir: &Path) -> Result<Self> {
        let root = data_dir.join("agent-os").join("reliable-runs");
        std::fs::create_dir_all(&root)
            .with_context(|| format!("failed to create reliable run directory {}", root.display()))?;
        restrict_directory_permissions(&root)?;
        Ok(Self {
            inner,
            root: Arc::new(root),
            process_gate: Arc::new(Mutex::new(())),
        })
    }

    async fn execute_plan(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let arguments = request
            .arguments
            .clone()
            .map(Value::Object)
            .unwrap_or(Value::Null);
        let mut plan: ReliablePlanRequest = serde_json::from_value(arguments).map_err(|error| {
            McpError::invalid_params(format!("invalid reliable Agent OS plan: {error}"), None)
        })?;

        let classifications = validate_plan(&plan).map_err(|error| {
            McpError::invalid_params(format!("unsafe Agent OS plan: {error}"), None)
        })?;
        let mutating = classifications.iter().any(|(_, risk, _)| risk.is_mutating());
        if mutating && plan.run_id.as_deref().is_none_or(str::is_empty) {
            return Ok(visible_error(
                "Mutating Agent OS plans require a stable run_id. Reuse that exact run_id after reconnects so Continuum can prevent duplicate side effects.",
            ));
        }
        let run_id = plan
            .run_id
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("run_{}", uuid::Uuid::new_v4().simple()));
        validate_run_id(&run_id).map_err(|error| {
            McpError::invalid_params(format!("invalid run_id: {error}"), None)
        })?;
        plan.run_id = Some(run_id.clone());

        if plan.dry_run {
            return Ok(CallToolResult::success(vec![Content::text(
                serde_json::to_string_pretty(&serde_json::json!({
                    "dry_run": true,
                    "reliable": true,
                    "run_id": run_id,
                    "steps": classifications.iter().enumerate().map(|(index, (step, risk, contract))| serde_json::json!({
                        "index": index,
                        "id": effective_step_id(step, index),
                        "action": step.action,
                        "risk": risk.as_str(),
                        "verification_contract": contract,
                        "continue_on_error": step.continue_on_error
                    })).collect::<Vec<_>>()
                }))
                .unwrap_or_else(|_| "{}".to_string()),
            )]));
        }

        let _process_guard = self.process_gate.lock().await;
        let plan_hash = hash_plan(&plan);

        if let Some(existing) = self.load_journal(&run_id).map_err(internal_error)? {
            if existing.plan_hash != plan_hash {
                return Ok(visible_error(
                    "This run_id already belongs to a different immutable goal or step list. Use a new run_id instead of changing an approved plan.",
                ));
            }
            match existing.state {
                JournalState::Completed | JournalState::CompletedWithErrors => {
                    let value = existing.final_result.unwrap_or_else(|| {
                        serde_json::json!({"run_id": run_id, "state": existing.state})
                    });
                    return Ok(CallToolResult::success(vec![Content::text(
                        serde_json::to_string_pretty(&value)
                            .unwrap_or_else(|_| "{}".to_string()),
                    )]));
                }
                JournalState::Unknown => {
                    return Ok(visible_error(
                        "This run has an unknown external outcome. Continuum will not replay it automatically. Inspect the destination and evidence, then start a new corrective plan.",
                    ));
                }
                _ => {}
            }
            if existing.steps.iter().any(|step| {
                matches!(step.state, StepState::Dispatched | StepState::Unknown)
            }) {
                return Ok(visible_error(
                    "This run contains an unresolved dispatched mutation. Automatic retry is blocked to prevent a duplicate side effect.",
                ));
            }
        }

        let _file_lock = match acquire_run_lock(&self.root, &run_id) {
            Ok(lock) => lock,
            Err(error) => return Ok(visible_error(&error.to_string())),
        };

        let now = Utc::now();
        let mut journal = self
            .load_journal(&run_id)
            .map_err(internal_error)?
            .unwrap_or_else(|| ReliableJournal {
                version: JOURNAL_VERSION,
                run_id: run_id.clone(),
                plan_hash: plan_hash.clone(),
                goal_summary: minimize_string(&plan.goal, 240),
                state: JournalState::Running,
                created_at: now,
                updated_at: now,
                steps: plan
                    .steps
                    .iter()
                    .enumerate()
                    .map(|(index, step)| JournalStep {
                        index,
                        id: effective_step_id(step, index),
                        action: step.action.clone(),
                        risk: classify_step(step),
                        state: StepState::Pending,
                        started_at: None,
                        finished_at: None,
                        verification: None,
                        result_summary: Value::Null,
                        error: None,
                    })
                    .collect(),
                final_result: None,
            });
        journal.state = JournalState::Running;
        journal.updated_at = Utc::now();
        self.save_journal(&journal).map_err(internal_error)?;

        let mut had_errors = journal
            .steps
            .iter()
            .any(|step| step.state == StepState::Failed);
        let mut response_steps = Vec::with_capacity(plan.steps.len());

        for (index, step) in plan.steps.iter().enumerate() {
            if journal.steps[index].state == StepState::Verified {
                response_steps.push(serde_json::json!({
                    "index": index,
                    "id": journal.steps[index].id,
                    "action": journal.steps[index].action,
                    "status": "verified",
                    "resumed": true,
                    "verification": journal.steps[index].verification,
                    "result": journal.steps[index].result_summary
                }));
                continue;
            }

            let risk = journal.steps[index].risk;
            let contract = parse_postcondition(step.expectation.as_deref(), risk)
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
            journal.steps[index].state = if risk.is_mutating() {
                StepState::Dispatched
            } else {
                StepState::Running
            };
            journal.steps[index].started_at = Some(Utc::now());
            journal.steps[index].finished_at = None;
            journal.steps[index].error = None;
            journal.updated_at = Utc::now();
            self.save_journal(&journal).map_err(internal_error)?;

            let arguments = match step.arguments.as_object() {
                Some(arguments) => arguments.clone(),
                None => {
                    let message = format!("step {} arguments must be a JSON object", index + 1);
                    mark_failure(&mut journal, index, risk, &message);
                    self.save_journal(&journal).map_err(internal_error)?;
                    return Ok(visible_error(&message));
                }
            };
            let call = CallToolRequestParams::new(step.action.clone()).with_arguments(arguments);
            let call_result = self.inner.call_tool(call, context.clone()).await;

            let result = match call_result {
                Ok(result) if result.is_error != Some(true) => result,
                Ok(result) => {
                    let message = result_text(&result);
                    if risk.is_mutating() && !is_pre_dispatch_failure(&message) {
                        mark_unknown(&mut journal, index, &message);
                        self.save_journal(&journal).map_err(internal_error)?;
                        return Ok(visible_error(&format!(
                            "Step {} returned an ambiguous mutation error and is now unknown. It will not be replayed automatically. {message}",
                            index + 1
                        )));
                    }
                    had_errors = true;
                    mark_failure(&mut journal, index, risk, &message);
                    self.save_journal(&journal).map_err(internal_error)?;
                    if step.continue_on_error {
                        response_steps.push(failed_step_json(&journal.steps[index]));
                        continue;
                    }
                    journal.state = JournalState::Failed;
                    journal.updated_at = Utc::now();
                    self.save_journal(&journal).map_err(internal_error)?;
                    return Ok(visible_error(&message));
                }
                Err(error) => {
                    let message = error.to_string();
                    if risk.is_mutating() && !is_pre_dispatch_failure(&message) {
                        mark_unknown(&mut journal, index, &message);
                        self.save_journal(&journal).map_err(internal_error)?;
                        return Ok(visible_error(&format!(
                            "Step {} has an unknown external outcome and cannot be retried automatically. {message}",
                            index + 1
                        )));
                    }
                    had_errors = true;
                    mark_failure(&mut journal, index, risk, &message);
                    self.save_journal(&journal).map_err(internal_error)?;
                    if step.continue_on_error {
                        response_steps.push(failed_step_json(&journal.steps[index]));
                        continue;
                    }
                    journal.state = JournalState::Failed;
                    journal.updated_at = Utc::now();
                    self.save_journal(&journal).map_err(internal_error)?;
                    return Ok(visible_error(&message));
                }
            };

            let result_value = call_result_value(&result);
            let verification = self
                .verify_postcondition(&contract, &result_value, context.clone())
                .await;
            if verification.status != "verified" {
                let detail = verification.detail.clone();
                journal.steps[index].verification = Some(verification);
                if risk.is_mutating() {
                    mark_unknown(&mut journal, index, &detail);
                    self.save_journal(&journal).map_err(internal_error)?;
                    return Ok(visible_error(&format!(
                        "Step {} executed but its postcondition was not proven. The outcome is unknown and later steps were stopped: {detail}",
                        index + 1
                    )));
                }
                had_errors = true;
                mark_failure(&mut journal, index, risk, &detail);
                self.save_journal(&journal).map_err(internal_error)?;
                if step.continue_on_error {
                    response_steps.push(failed_step_json(&journal.steps[index]));
                    continue;
                }
                journal.state = JournalState::Failed;
                journal.updated_at = Utc::now();
                self.save_journal(&journal).map_err(internal_error)?;
                return Ok(visible_error(&detail));
            }

            journal.steps[index].state = StepState::Verified;
            journal.steps[index].finished_at = Some(Utc::now());
            journal.steps[index].verification = Some(verification.clone());
            journal.steps[index].result_summary = compact_and_redact(&result_value, 0);
            journal.steps[index].error = None;
            journal.updated_at = Utc::now();
            self.save_journal(&journal).map_err(internal_error)?;
            response_steps.push(serde_json::json!({
                "index": index,
                "id": journal.steps[index].id,
                "action": step.action,
                "status": "verified",
                "verification": verification,
                "result": result_value
            }));
        }

        journal.state = if had_errors {
            JournalState::CompletedWithErrors
        } else {
            JournalState::Completed
        };
        journal.updated_at = Utc::now();
        let response = serde_json::json!({
            "reliable": true,
            "run_id": run_id,
            "status": if had_errors { "completed_with_errors" } else { "completed" },
            "steps": response_steps,
            "exactly_once": {
                "cross_process_lock": true,
                "write_ahead_dispatch": true,
                "automatic_replay_on_unknown": false
            }
        });
        journal.final_result = Some(compact_and_redact(&response, 0));
        self.save_journal(&journal).map_err(internal_error)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&response).unwrap_or_else(|_| "{}".to_string()),
        )]))
    }

    async fn verify_postcondition(
        &self,
        condition: &Postcondition,
        result: &Value,
        context: RequestContext<RoleServer>,
    ) -> VerificationRecord {
        match condition {
            Postcondition::ResultOk => VerificationRecord {
                status: "verified".to_string(),
                contract: "result_ok".to_string(),
                detail: "The tool returned a non-error result.".to_string(),
            },
            Postcondition::StateChanged => {
                let changed = result
                    .pointer("/result/verification/state_changed")
                    .or_else(|| result.pointer("/verification/state_changed"))
                    .and_then(Value::as_bool);
                VerificationRecord {
                    status: if changed == Some(true) {
                        "verified"
                    } else if changed == Some(false) {
                        "contradicted"
                    } else {
                        "unverified"
                    }
                    .to_string(),
                    contract: "state_changed".to_string(),
                    detail: match changed {
                        Some(true) => "The verified before/after state changed.".to_string(),
                        Some(false) => "The verified state did not change.".to_string(),
                        None => "No state_changed evidence was returned.".to_string(),
                    },
                }
            }
            Postcondition::JsonPointerExists(pointer) => VerificationRecord {
                status: if result.pointer(pointer).is_some() {
                    "verified"
                } else {
                    "contradicted"
                }
                .to_string(),
                contract: format!("json_pointer_exists:{pointer}"),
                detail: if result.pointer(pointer).is_some() {
                    format!("JSON pointer {pointer:?} exists in the tool result.")
                } else {
                    format!("JSON pointer {pointer:?} was absent from the tool result.")
                },
            },
            Postcondition::JsonPointerEquals { pointer, expected } => {
                let actual = result.pointer(pointer);
                VerificationRecord {
                    status: if actual == Some(expected) {
                        "verified"
                    } else {
                        "contradicted"
                    }
                    .to_string(),
                    contract: format!("json_pointer_equals:{pointer}"),
                    detail: if actual == Some(expected) {
                        format!("JSON pointer {pointer:?} matched the expected value.")
                    } else {
                        format!(
                            "JSON pointer {pointer:?} did not match. actual={}, expected={}",
                            actual.cloned().unwrap_or(Value::Null),
                            expected
                        )
                    },
                }
            }
            Postcondition::TextContains(needle) => {
                let rendered = serde_json::to_string(result).unwrap_or_default();
                let matched = rendered
                    .to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase());
                VerificationRecord {
                    status: if matched { "verified" } else { "contradicted" }.to_string(),
                    contract: "text_contains".to_string(),
                    detail: if matched {
                        format!("The result contained {needle:?}.")
                    } else {
                        format!("The result did not contain {needle:?}.")
                    },
                }
            }
            Postcondition::WindowTitleContains(needle) => {
                let observe = CallToolRequestParams::new("computer_observe").with_arguments(
                    serde_json::json!({
                        "include_windows": true,
                        "include_accessibility": false,
                        "include_screenshot": false
                    })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                );
                match self.inner.call_tool(observe, context).await {
                    Ok(value) if value.is_error != Some(true) => {
                        let observed = call_result_value(&value);
                        let matched = object_key_contains(&observed, "title", needle);
                        VerificationRecord {
                            status: if matched { "verified" } else { "contradicted" }
                                .to_string(),
                            contract: "window_title_contains".to_string(),
                            detail: if matched {
                                format!("The post-action window state contained title {needle:?}.")
                            } else {
                                format!("No post-action window title contained {needle:?}.")
                            },
                        }
                    }
                    Ok(value) => VerificationRecord {
                        status: "unverified".to_string(),
                        contract: "window_title_contains".to_string(),
                        detail: format!("Post-action window observation failed: {}", result_text(&value)),
                    },
                    Err(error) => VerificationRecord {
                        status: "unverified".to_string(),
                        contract: "window_title_contains".to_string(),
                        detail: format!("Post-action window observation failed: {error}"),
                    },
                }
            }
            Postcondition::ElementPresent(selector) => {
                let find = CallToolRequestParams::new("computer_find_element")
                    .with_arguments(selector.clone());
                match self.inner.call_tool(find, context).await {
                    Ok(value) if value.is_error != Some(true) => VerificationRecord {
                        status: "verified".to_string(),
                        contract: "element_present".to_string(),
                        detail: "The semantic UI element was present, visible and enabled.".to_string(),
                    },
                    Ok(value) => VerificationRecord {
                        status: "contradicted".to_string(),
                        contract: "element_present".to_string(),
                        detail: result_text(&value),
                    },
                    Err(error) => VerificationRecord {
                        status: "contradicted".to_string(),
                        contract: "element_present".to_string(),
                        detail: error.to_string(),
                    },
                }
            }
        }
    }

    fn journal_path(&self, run_id: &str) -> PathBuf {
        self.root.join(format!("{run_id}.json"))
    }

    fn load_journal(&self, run_id: &str) -> Result<Option<ReliableJournal>> {
        let path = self.journal_path(run_id);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid reliable run journal {}", path.display()))
                .map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("failed to read reliable run journal {}", path.display())),
        }
    }

    fn save_journal(&self, journal: &ReliableJournal) -> Result<()> {
        let path = self.journal_path(&journal.run_id);
        let temporary = self.root.join(format!(
            ".{}-{}-{}.tmp",
            journal.run_id,
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let payload = serde_json::to_vec_pretty(journal)?;
        write_private_file(&temporary, &payload)?;
        let backup = self.root.join(format!(".{}.backup", journal.run_id));
        if backup.exists() {
            std::fs::remove_file(&backup)
                .with_context(|| format!("failed to remove stale {}", backup.display()))?;
        }
        if path.exists() {
            std::fs::rename(&path, &backup).with_context(|| {
                format!("failed to preserve current reliable journal {}", path.display())
            })?;
        }
        if let Err(error) = std::fs::rename(&temporary, &path) {
            let _ = std::fs::remove_file(&temporary);
            if backup.exists() && !path.exists() {
                let _ = std::fs::rename(&backup, &path);
            }
            return Err(error)
                .with_context(|| format!("failed to activate reliable journal {}", path.display()));
        }
        if backup.exists() {
            let _ = std::fs::remove_file(backup);
        }
        Ok(())
    }
}

impl ServerHandler for ReliableAgentOsServer {
    fn get_info(&self) -> ServerInfo {
        self.inner.get_info()
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.inner.get_tool(name)
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.inner.list_tools(request, context).await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let name = request.name.as_ref();
        if name == "agent_run_plan" {
            return self.execute_plan(request, context).await;
        }
        if name == "agent_get_run" {
            let run_id = request
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("run_id"))
                .and_then(Value::as_str);
            if let Some(run_id) = run_id {
                if let Some(journal) = self.load_journal(run_id).map_err(internal_error)? {
                    return Ok(CallToolResult::success(vec![Content::text(
                        serde_json::to_string_pretty(&journal)
                            .unwrap_or_else(|_| "{}".to_string()),
                    )]));
                }
            }
        }
        if is_direct_mutation(name) {
            return Ok(visible_error(
                "Direct Agent OS mutations are disabled. Put the action in agent_run_plan with a stable run_id and a typed expectation so Continuum can checkpoint and verify it safely.",
            ));
        }
        self.inner.call_tool(request, context).await
    }
}

fn validate_plan(
    plan: &ReliablePlanRequest,
) -> Result<Vec<(&ReliableStep, ReliableRisk, String)>> {
    if plan.goal.trim().is_empty() || plan.goal.chars().count() > 4_000 {
        bail!("goal must contain between 1 and 4,000 characters");
    }
    if plan.steps.is_empty() || plan.steps.len() > MAX_PLAN_STEPS {
        bail!("steps must contain between 1 and {MAX_PLAN_STEPS} entries");
    }
    let mut ids = BTreeSet::new();
    let mut output = Vec::with_capacity(plan.steps.len());
    for (index, step) in plan.steps.iter().enumerate() {
        if !is_supported_plan_action(step.action.trim()) {
            bail!("unsupported step action {:?}", step.action);
        }
        if !step.arguments.is_object() {
            bail!("step {} arguments must be a JSON object", index + 1);
        }
        let id = effective_step_id(step, index);
        validate_step_id(&id)?;
        if !ids.insert(id.clone()) {
            bail!("duplicate step id {id:?}");
        }
        let risk = classify_step(step);
        if risk.is_mutating() && step.continue_on_error {
            bail!(
                "mutating step {id:?} cannot continue_on_error; a dependent mutation must stop on uncertainty"
            );
        }
        let contract = parse_postcondition(step.expectation.as_deref(), risk)?;
        output.push((step, risk, render_contract(&contract)));
    }
    Ok(output)
}

fn parse_postcondition(value: Option<&str>, risk: ReliableRisk) -> Result<Postcondition> {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    let Some(value) = value else {
        if risk == ReliableRisk::Read {
            return Ok(Postcondition::ResultOk);
        }
        bail!(
            "mutating steps require a typed expectation: state_changed, json_pointer_exists:/path, json_pointer_equals:/path=<json>, text_contains:<text>, window_title_contains:<text>, or element_present:<selector-json>"
        );
    };
    if value == "result_ok" {
        return Ok(Postcondition::ResultOk);
    }
    if value == "state_changed" {
        return Ok(Postcondition::StateChanged);
    }
    if let Some(pointer) = value.strip_prefix("json_pointer_exists:") {
        validate_pointer(pointer)?;
        return Ok(Postcondition::JsonPointerExists(pointer.to_string()));
    }
    if let Some(rest) = value.strip_prefix("json_pointer_equals:") {
        let (pointer, expected) = rest.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("json_pointer_equals must use /pointer=<json>")
        })?;
        validate_pointer(pointer)?;
        let expected = serde_json::from_str(expected)
            .with_context(|| "json_pointer_equals expected value is not valid JSON")?;
        return Ok(Postcondition::JsonPointerEquals {
            pointer: pointer.to_string(),
            expected,
        });
    }
    if let Some(needle) = value.strip_prefix("text_contains:") {
        if needle.trim().is_empty() || needle.chars().count() > 500 {
            bail!("text_contains requires 1 to 500 characters");
        }
        return Ok(Postcondition::TextContains(needle.to_string()));
    }
    if let Some(needle) = value.strip_prefix("window_title_contains:") {
        if needle.trim().is_empty() || needle.chars().count() > 300 {
            bail!("window_title_contains requires 1 to 300 characters");
        }
        return Ok(Postcondition::WindowTitleContains(needle.to_string()));
    }
    if let Some(selector) = value.strip_prefix("element_present:") {
        let selector: Value = serde_json::from_str(selector)
            .with_context(|| "element_present selector is not valid JSON")?;
        let selector = selector
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("element_present selector must be a JSON object"))?;
        if selector.is_empty() {
            bail!("element_present selector cannot be empty");
        }
        return Ok(Postcondition::ElementPresent(selector));
    }
    bail!("expectation is descriptive text, not a typed verification contract")
}

fn validate_pointer(pointer: &str) -> Result<()> {
    if !pointer.starts_with('/') || pointer.chars().count() > 1_000 {
        bail!("JSON pointer must start with '/' and be at most 1,000 characters");
    }
    Ok(())
}

fn render_contract(condition: &Postcondition) -> String {
    match condition {
        Postcondition::ResultOk => "result_ok".to_string(),
        Postcondition::StateChanged => "state_changed".to_string(),
        Postcondition::JsonPointerExists(pointer) => {
            format!("json_pointer_exists:{pointer}")
        }
        Postcondition::JsonPointerEquals { pointer, expected } => {
            format!("json_pointer_equals:{pointer}={expected}")
        }
        Postcondition::TextContains(value) => format!("text_contains:{value}"),
        Postcondition::WindowTitleContains(value) => {
            format!("window_title_contains:{value}")
        }
        Postcondition::ElementPresent(selector) => format!(
            "element_present:{}",
            serde_json::to_string(selector).unwrap_or_else(|_| "{}".to_string())
        ),
    }
}

fn classify_step(step: &ReliableStep) -> ReliableRisk {
    match step.action.trim() {
        "computer_observe"
        | "computer_list_windows"
        | "computer_accessibility"
        | "computer_screenshot"
        | "computer_find_element"
        | "computer_wait"
        | "computer_wait_for_element"
        | "composio_search" => ReliableRisk::Read,
        "composio_execute" => classify_composio_slug(
            step.arguments
                .get("tool_slug")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        "composio_execute_meta" => {
            let slug = step
                .arguments
                .get("meta_tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_uppercase();
            if slug.contains("REMOTE_BASH") || slug.contains("REMOTE_WORKBENCH") {
                ReliableRisk::Destructive
            } else {
                ReliableRisk::Write
            }
        }
        _ => ReliableRisk::Write,
    }
}

fn classify_composio_slug(slug: &str) -> ReliableRisk {
    let tokens = slug
        .split(|character: char| !character.is_ascii_alphanumeric())
        .map(str::to_ascii_uppercase)
        .collect::<Vec<_>>();
    if tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "DELETE"
                | "REMOVE"
                | "REVOKE"
                | "CANCEL"
                | "TERMINATE"
                | "PURGE"
                | "DESTROY"
                | "DROP"
                | "TRASH"
                | "REFUND"
                | "CHARGE"
                | "PAY"
                | "PAYOUT"
                | "TRANSFER"
                | "WITHDRAW"
                | "PURCHASE"
                | "BUY"
                | "SELL"
                | "BAN"
                | "SUSPEND"
                | "DEACTIVATE"
                | "RESET"
                | "ROTATE"
        )
    }) {
        ReliableRisk::Destructive
    } else if tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "GET" | "LIST" | "SEARCH" | "READ" | "FETCH" | "QUERY" | "FIND" | "STATUS"
        )
    }) {
        ReliableRisk::Read
    } else {
        ReliableRisk::Write
    }
}

fn is_supported_plan_action(action: &str) -> bool {
    matches!(
        action,
        "computer_observe"
            | "computer_list_windows"
            | "computer_accessibility"
            | "computer_screenshot"
            | "computer_find_element"
            | "computer_click"
            | "computer_click_element"
            | "computer_type"
            | "computer_key"
            | "computer_scroll"
            | "computer_focus_window"
            | "computer_open_url"
            | "computer_wait"
            | "computer_wait_for_element"
            | "composio_create_session"
            | "composio_search"
            | "composio_execute"
            | "composio_execute_meta"
    )
}

fn is_direct_mutation(name: &str) -> bool {
    matches!(
        name,
        "computer_click"
            | "computer_click_element"
            | "computer_type"
            | "computer_key"
            | "computer_scroll"
            | "computer_focus_window"
            | "computer_open_url"
            | "composio_execute"
            | "composio_execute_meta"
    )
}

fn effective_step_id(step: &ReliableStep, index: usize) -> String {
    step.id
        .clone()
        .unwrap_or_else(|| format!("step_{}", index + 1))
}

fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty()
        || run_id.len() > 96
        || !run_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("run_id may contain only letters, numbers, '-' and '_' (maximum 96 characters)");
    }
    Ok(())
}

fn validate_step_id(step_id: &str) -> Result<()> {
    if step_id.is_empty()
        || step_id.len() > 96
        || !step_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("step ids may contain only letters, numbers, '-' and '_' (maximum 96 characters)");
    }
    Ok(())
}

fn hash_plan(plan: &ReliablePlanRequest) -> String {
    let value = serde_json::to_value(serde_json::json!({
        "goal": plan.goal,
        "steps": plan.steps,
        "verify_each_step": plan.verify_each_step
    }))
    .unwrap_or(Value::Null);
    let canonical = canonical_json(&value);
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}:{}", canonical.len())
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let sorted = object.iter().collect::<BTreeMap<_, _>>();
            let entries = sorted
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{entries}}}")
        }
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(canonical_json).collect::<Vec<_>>().join(",")
        ),
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
    }
}

struct RunFileLock {
    path: PathBuf,
}

impl Drop for RunFileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_run_lock(root: &Path, run_id: &str) -> Result<RunFileLock> {
    let path = root.join(format!(".{run_id}.lock"));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "run_id {run_id:?} is already executing in another process; inspect its journal instead of retrying"
            )
        }
        Err(error) => return Err(error).context("failed to create cross-process run lock"),
    };
    let payload = serde_json::json!({
        "pid": std::process::id(),
        "created_at": Utc::now(),
        "owner": uuid::Uuid::new_v4().to_string()
    });
    file.write_all(serde_json::to_string(&payload)?.as_bytes())?;
    file.sync_all()?;
    Ok(RunFileLock { path })
}

fn write_private_file(path: &Path, payload: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(payload)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn restrict_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to restrict {} permissions", path.display()))
}

#[cfg(not(unix))]
fn restrict_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn call_result_value(result: &CallToolResult) -> Value {
    if let Some(value) = &result.structured_content {
        return value.clone();
    }
    let texts = result
        .content
        .iter()
        .filter_map(|content| content.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>();
    if texts.len() == 1 {
        serde_json::from_str(&texts[0]).unwrap_or_else(|_| Value::String(texts[0].clone()))
    } else {
        Value::Array(texts.into_iter().map(Value::String).collect())
    }
}

fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| content.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(2_000)
        .collect()
}

fn is_pre_dispatch_failure(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    [
        "denied by policy",
        "approval required",
        "user denied",
        "invalid params",
        "invalid parameters",
        "unsupported",
        "is blocked",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn mark_failure(journal: &mut ReliableJournal, index: usize, _risk: ReliableRisk, message: &str) {
    journal.steps[index].state = StepState::Failed;
    journal.steps[index].finished_at = Some(Utc::now());
    journal.steps[index].error = Some(minimize_string(message, 2_000));
    journal.updated_at = Utc::now();
}

fn mark_unknown(journal: &mut ReliableJournal, index: usize, message: &str) {
    journal.steps[index].state = StepState::Unknown;
    journal.steps[index].finished_at = Some(Utc::now());
    journal.steps[index].error = Some(minimize_string(message, 2_000));
    journal.state = JournalState::Unknown;
    journal.updated_at = Utc::now();
}

fn failed_step_json(step: &JournalStep) -> Value {
    serde_json::json!({
        "index": step.index,
        "id": step.id,
        "action": step.action,
        "status": "failed",
        "error": step.error,
        "verification": step.verification
    })
}

fn object_key_contains(value: &Value, key: &str, needle: &str) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(candidate, value)| {
            (candidate.eq_ignore_ascii_case(key)
                && value
                    .as_str()
                    .is_some_and(|text| text.to_ascii_lowercase().contains(&needle.to_ascii_lowercase())))
                || object_key_contains(value, key, needle)
        }),
        Value::Array(items) => items
            .iter()
            .any(|value| object_key_contains(value, key, needle)),
        _ => false,
    }
}

fn compact_and_redact(value: &Value, depth: usize) -> Value {
    if depth > 7 {
        return Value::String("[depth-limited]".to_string());
    }
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .take(80)
                .map(|(key, child)| {
                    let lowered = key.to_ascii_lowercase();
                    let private = [
                        "password",
                        "secret",
                        "token",
                        "api_key",
                        "authorization",
                        "credential",
                        "cookie",
                        "body",
                        "content",
                        "text",
                        "message",
                        "account",
                        "recipient",
                    ]
                    .iter()
                    .any(|needle| lowered.contains(needle));
                    (
                        key.clone(),
                        if private {
                            redacted_shape(child)
                        } else {
                            compact_and_redact(child, depth + 1)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(100)
                .map(|child| compact_and_redact(child, depth + 1))
                .collect(),
        ),
        Value::String(value) => Value::String(minimize_string(value, 1_000)),
        other => other.clone(),
    }
}

fn redacted_shape(value: &Value) -> Value {
    match value {
        Value::String(value) => serde_json::json!({"redacted":true,"chars":value.chars().count()}),
        Value::Array(items) => serde_json::json!({"redacted":true,"items":items.len()}),
        Value::Object(object) => serde_json::json!({"redacted":true,"fields":object.len()}),
        Value::Null => serde_json::json!({"redacted":true,"kind":"null"}),
        Value::Bool(_) => serde_json::json!({"redacted":true,"kind":"boolean"}),
        Value::Number(_) => serde_json::json!({"redacted":true,"kind":"number"}),
    }
}

fn minimize_string(value: &str, max_chars: usize) -> String {
    let mut output: String = value
        .chars()
        .filter(|character| {
            !matches!(
                character,
                '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        })
        .take(max_chars)
        .collect();
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

fn visible_error(message: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(minimize_string(message, 4_000))])
}

fn internal_error(error: anyhow::Error) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(action: &str, expectation: Option<&str>) -> ReliableStep {
        ReliableStep {
            id: None,
            action: action.to_string(),
            arguments: serde_json::json!({}),
            expectation: expectation.map(str::to_string),
            continue_on_error: false,
        }
    }

    #[test]
    fn mutations_require_typed_postconditions() {
        assert!(parse_postcondition(Some("the form should be saved"), ReliableRisk::Write).is_err());
        assert!(parse_postcondition(Some("state_changed"), ReliableRisk::Write).is_ok());
        assert!(parse_postcondition(None, ReliableRisk::Read).is_ok());
    }

    #[test]
    fn destructive_continue_on_error_is_rejected() {
        let plan = ReliablePlanRequest {
            goal: "delete once".to_string(),
            steps: vec![ReliableStep {
                id: Some("delete".to_string()),
                action: "composio_execute".to_string(),
                arguments: serde_json::json!({"tool_slug":"SLACK_DELETE_MESSAGE"}),
                expectation: Some("json_pointer_exists:/result/response/data".to_string()),
                continue_on_error: true,
            }],
            run_id: Some("delete_once".to_string()),
            dry_run: false,
            verify_each_step: true,
        };
        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn plan_hash_is_stable_across_object_key_order() {
        let mut left = step("computer_observe", None);
        left.arguments = serde_json::json!({"b":2,"a":1});
        let mut right = step("computer_observe", None);
        right.arguments = serde_json::json!({"a":1,"b":2});
        let plan_left = ReliablePlanRequest {
            goal: "observe".into(),
            steps: vec![left],
            run_id: Some("same".into()),
            dry_run: false,
            verify_each_step: true,
        };
        let plan_right = ReliablePlanRequest {
            steps: vec![right],
            ..plan_left.clone()
        };
        assert_eq!(hash_plan(&plan_left), hash_plan(&plan_right));
    }

    #[test]
    fn write_ahead_lock_is_cross_process_visible() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = acquire_run_lock(temp.path(), "same").expect("first lock");
        assert!(acquire_run_lock(temp.path(), "same").is_err());
        drop(first);
        assert!(acquire_run_lock(temp.path(), "same").is_ok());
    }

    #[test]
    fn private_result_fields_are_not_written_to_journal() {
        let value = serde_json::json!({
            "id":"safe-id",
            "body":"private email body",
            "recipient":"person@example.com"
        });
        let compact = compact_and_redact(&value, 0);
        let rendered = serde_json::to_string(&compact).expect("json");
        assert!(rendered.contains("safe-id"));
        assert!(!rendered.contains("private email"));
        assert!(!rendered.contains("person@example.com"));
    }
}
