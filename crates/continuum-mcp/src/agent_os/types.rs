use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Read,
    Write,
    Destructive,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Destructive => "destructive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    Allow,
    Ask,
    Deny,
}

impl PolicyMode {
    pub fn rank(self) -> u8 {
        match self {
            Self::Deny => 0,
            Self::Ask => 1,
            Self::Allow => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicyConfig {
    pub version: u32,
    pub policies: BTreeMap<String, PolicyMode>,
    pub native_approval_dialogs: bool,
    pub approval_timeout_secs: u64,
    pub max_plan_steps: usize,
    pub verify_after_mutation: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AuthorizationReport {
    pub capability: String,
    pub configured_mode: PolicyMode,
    pub allowed: bool,
    pub source: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EmptyRequest {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicySetRequest {
    pub capability: String,
    pub mode: PolicyMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EvidenceQueryRequest {
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default = "default_evidence_limit")]
    pub limit: usize,
}

fn default_evidence_limit() -> usize {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ObserveRequest {
    #[serde(default = "default_true")]
    pub include_windows: bool,
    #[serde(default)]
    pub include_accessibility: bool,
    #[serde(default)]
    pub include_screenshot: bool,
    #[serde(default = "default_accessibility_nodes")]
    pub accessibility_max_nodes: usize,
    #[serde(default = "default_accessibility_depth")]
    pub accessibility_max_depth: usize,
}

fn default_true() -> bool {
    true
}
fn default_accessibility_nodes() -> usize {
    250
}
fn default_accessibility_depth() -> usize {
    8
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccessibilityRequest {
    #[serde(default)]
    pub window_handle: Option<i64>,
    #[serde(default = "default_accessibility_nodes")]
    pub max_nodes: usize,
    #[serde(default = "default_accessibility_depth")]
    pub max_depth: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScreenshotRequest {
    #[serde(default)]
    pub target: ScreenshotTarget,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScreenshotTarget {
    #[default]
    ForegroundWindow,
    VirtualScreen,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindElementRequest {
    #[serde(default)]
    pub window_handle: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub automation_id: Option<String>,
    #[serde(default)]
    pub control_type: Option<String>,
    #[serde(default)]
    pub class_name: Option<String>,
    #[serde(default)]
    pub exact: bool,
    #[serde(default = "default_accessibility_nodes")]
    pub max_nodes: usize,
    #[serde(default = "default_accessibility_depth")]
    pub max_depth: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClickRequest {
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub button: MouseButton,
    #[serde(default = "default_click_count")]
    pub count: u8,
    #[serde(default = "default_post_action_delay")]
    pub post_action_delay_ms: u64,
}

fn default_click_count() -> u8 {
    1
}
fn default_post_action_delay() -> u64 {
    350
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClickElementRequest {
    #[serde(flatten)]
    pub selector: FindElementRequest,
    #[serde(default)]
    pub button: MouseButton,
    #[serde(default = "default_click_count")]
    pub count: u8,
    #[serde(default = "default_post_action_delay")]
    pub post_action_delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TypeRequest {
    pub text: String,
    #[serde(default)]
    pub replace_existing: bool,
    #[serde(default = "default_post_action_delay")]
    pub post_action_delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct KeyRequest {
    pub keys: Vec<String>,
    #[serde(default = "default_post_action_delay")]
    pub post_action_delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScrollRequest {
    pub amount: i32,
    #[serde(default)]
    pub horizontal: bool,
    #[serde(default = "default_post_action_delay")]
    pub post_action_delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FocusWindowRequest {
    #[serde(default)]
    pub handle: Option<i64>,
    #[serde(default)]
    pub title_contains: Option<String>,
    #[serde(default)]
    pub process_name: Option<String>,
    #[serde(default = "default_post_action_delay")]
    pub post_action_delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpenUrlRequest {
    pub url: String,
    #[serde(default = "default_open_url_delay")]
    pub post_action_delay_ms: u64,
}

fn default_open_url_delay() -> u64 {
    800
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WaitRequest {
    pub milliseconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WaitForElementRequest {
    #[serde(flatten)]
    pub selector: FindElementRequest,
    #[serde(default = "default_wait_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u64,
}

fn default_wait_timeout() -> u64 {
    15_000
}
fn default_poll_interval() -> u64 {
    500
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComposioConfigureRequest {
    pub user_id: String,
    #[serde(default)]
    pub enabled_toolkits: Vec<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub reset_session: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComposioCreateSessionRequest {
    #[serde(default)]
    pub force_new: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComposioSearchRequest {
    pub queries: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComposioExecuteRequest {
    pub tool_slug: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub enable_auto_workbench_offload: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComposioMetaExecuteRequest {
    pub meta_tool: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub intent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlanStep {
    #[serde(default)]
    pub id: Option<String>,
    pub action: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub expectation: Option<String>,
    #[serde(default)]
    pub continue_on_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunPlanRequest {
    pub goal: String,
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "default_true")]
    pub verify_each_step: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetRunRequest {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStepResult {
    pub index: usize,
    pub id: String,
    pub action: String,
    pub status: String,
    #[serde(default)]
    pub evidence_id: Option<String>,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub goal: String,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub steps: Vec<PlanStep>,
    pub results: Vec<PlanStepResult>,
}
