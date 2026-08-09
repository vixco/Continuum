use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, Content, ErrorData as McpError, ListToolsResult,
        PaginatedRequestParams, ServerInfo, Tool,
    },
    service::RequestContext,
    RoleServer, ServerHandler,
};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::ContinuumMcpServer;

const BUILTIN_PERMISSIONS: &str = include_str!("../../../config/default-permissions.toml");
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPermission {
    Auto,
    SessionApproved,
    AlwaysConfirm,
    Blocked,
}

impl ToolPermission {
    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "session-approved" | "session" => Ok(Self::SessionApproved),
            "always-confirm" | "confirm" => Ok(Self::AlwaysConfirm),
            "blocked" | "deny" => Ok(Self::Blocked),
            other => bail!("unsupported MCP permission tier {other:?}"),
        }
    }
}

#[derive(Debug, Clone)]
struct PermissionConfig {
    tools: BTreeMap<String, ToolPermission>,
    source: PathBuf,
}

impl PermissionConfig {
    fn load(data_dir: &Path) -> Result<Self> {
        let mut tools = BTreeMap::new();
        merge_toml_permissions(BUILTIN_PERMISSIONS, "bundled defaults", &mut tools)?;

        let override_path = std::env::var_os("CONTINUUM_PERMISSIONS_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("permissions.toml"));
        if override_path.exists() {
            let body = std::fs::read_to_string(&override_path).with_context(|| {
                format!(
                    "failed to read MCP permissions at {}",
                    override_path.display()
                )
            })?;
            merge_toml_permissions(&body, &override_path.display().to_string(), &mut tools)?;
        }

        Ok(Self {
            tools,
            source: override_path,
        })
    }

    fn tier(&self, tool: &str) -> ToolPermission {
        self.tools
            .get(tool)
            .copied()
            // Unknown tools must never become silently callable merely because
            // a new handler was added without updating the permission contract.
            .unwrap_or(ToolPermission::AlwaysConfirm)
    }
}

fn merge_toml_permissions(
    body: &str,
    source: &str,
    destination: &mut BTreeMap<String, ToolPermission>,
) -> Result<()> {
    let document: toml::Value = toml::from_str(body)
        .with_context(|| format!("invalid MCP permissions TOML in {source}"))?;
    let root = document
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("MCP permission document {source} is not a TOML table"))?;

    for (section, value) in root {
        let table = value.as_table().ok_or_else(|| {
            anyhow::anyhow!("permission section {section:?} in {source} is not a table")
        })?;
        for (tool, tier) in table {
            let tier = tier.as_str().ok_or_else(|| {
                anyhow::anyhow!("permission for {tool:?} in {source} must be a string")
            })?;
            destination.insert(tool.clone(), ToolPermission::parse(tier)?);
        }
    }
    Ok(())
}

#[derive(Clone)]
struct PermissionBroker {
    config: Arc<PermissionConfig>,
    session_grants: Arc<Mutex<HashSet<String>>>,
}

impl PermissionBroker {
    fn load(data_dir: &Path) -> Result<Self> {
        Ok(Self {
            config: Arc::new(PermissionConfig::load(data_dir)?),
            session_grants: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    async fn authorize(&self, tool: &str, arguments: &Value) -> std::result::Result<(), String> {
        let tier = self.config.tier(tool);
        match tier {
            ToolPermission::Auto => Ok(()),
            ToolPermission::Blocked => {
                // The repair server already validates a short-lived, scoped
                // capability. The ordinary MCP process has no token and stays
                // blocked by default.
                if tool.starts_with("repair_")
                    && std::env::var("CONTINUUM_REPAIR_TOKEN")
                        .is_ok_and(|value| !value.trim().is_empty())
                {
                    Ok(())
                } else {
                    Err(format!(
                        "MCP tool {tool:?} is blocked by the enforced permission policy ({})",
                        self.config.source.display()
                    ))
                }
            }
            ToolPermission::SessionApproved => {
                if self.session_grants.lock().await.contains(tool) {
                    return Ok(());
                }
                self.require_native_approval(tool, arguments, "Approve for this MCP session")
                    .await?;
                self.session_grants.lock().await.insert(tool.to_string());
                Ok(())
            }
            ToolPermission::AlwaysConfirm => {
                self.require_native_approval(tool, arguments, "Approve this call once")
                    .await
            }
        }
    }

    async fn require_native_approval(
        &self,
        tool: &str,
        arguments: &Value,
        scope: &str,
    ) -> std::result::Result<(), String> {
        if std::env::var("CONTINUUM_MCP_HEADLESS")
            .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        {
            return Err(format!(
                "MCP tool {tool:?} requires user approval, but Continuum is running headless"
            ));
        }

        let summary = approval_summary(tool, arguments, scope);
        let approved = native_approval(&summary).await?;
        if approved {
            Ok(())
        } else {
            Err(format!("the user denied MCP tool {tool:?}"))
        }
    }
}

/// Governance wrapper for the legacy/context MCP server. The wrapped server
/// remains responsible for schemas and execution; this layer is the mandatory
/// authorization and cloud-egress choke point.
#[derive(Clone)]
pub struct PermissionedMcpServer {
    inner: ContinuumMcpServer,
    broker: PermissionBroker,
}

impl PermissionedMcpServer {
    pub fn new(inner: ContinuumMcpServer, data_dir: &Path) -> Result<Self> {
        Ok(Self {
            inner,
            broker: PermissionBroker::load(data_dir)?,
        })
    }
}

impl ServerHandler for PermissionedMcpServer {
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
        let tool = request.name.to_string();
        tracing::debug!(tool = %tool, "permission wrapper received tool call");
        let arguments = request
            .arguments
            .as_ref()
            .map(|value| Value::Object(value.clone()))
            .unwrap_or(Value::Null);

        if let Err(message) = self.broker.authorize(&tool, &arguments).await {
            return Ok(CallToolResult::error(vec![Content::text(message)]));
        }
        tracing::debug!(tool = %tool, "permission wrapper authorized tool call");

        let result = self.inner.call_tool(request, context).await?;
        tracing::debug!(tool = %tool, "inner tool handler completed");
        let result = apply_egress_policy(&tool, result);
        tracing::debug!(tool = %tool, "tool response passed egress policy");
        Ok(result)
    }
}

fn apply_egress_policy(tool: &str, mut result: CallToolResult) -> CallToolResult {
    if let Some(value) = result.structured_content.take() {
        if tool == "memory_vault_get" && contains_sensitive_memory(&value) {
            return CallToolResult::error(vec![Content::text(
                "This memory is marked sensitive and was withheld from the cloud-bound MCP result. Open it locally in the Memory tab.",
            )]);
        }
        result.structured_content = Some(sanitize_result_value(tool, value));
    }

    let mut blocked_sensitive = false;
    let mut content = Vec::with_capacity(result.content.len());
    for block in result.content {
        if let Some(text) = block.as_text() {
            match serde_json::from_str::<Value>(&text.text) {
                Ok(value) => {
                    if tool == "memory_vault_get" && contains_sensitive_memory(&value) {
                        blocked_sensitive = true;
                        continue;
                    }
                    let sanitized = sanitize_result_value(tool, value);
                    let rendered = serde_json::to_string_pretty(&sanitized)
                        .unwrap_or_else(|_| "{\"withheld\":true}".to_string());
                    content.push(Content::text(rendered));
                }
                Err(_) => content.push(Content::text(redact_free_text(&text.text))),
            }
        } else {
            content.push(block);
        }
    }

    if blocked_sensitive {
        return CallToolResult::error(vec![Content::text(
            "This memory is marked sensitive and was withheld from the cloud-bound MCP result. Open it locally in the Memory tab.",
        )]);
    }
    result.content = content;
    result
}

fn sanitize_result_value(tool: &str, value: Value) -> Value {
    let value = if tool == "memory_vault_search" {
        filter_sensitive_search_results(value)
    } else {
        value
    };
    redact_secret_keys(value, 0)
}

fn filter_sensitive_search_results(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .filter(|item| !contains_sensitive_memory(item))
                .map(filter_sensitive_search_results)
                .collect(),
        ),
        Value::Object(mut object) => {
            for child in object.values_mut() {
                let current = std::mem::take(child);
                *child = filter_sensitive_search_results(current);
            }
            Value::Object(object)
        }
        other => other,
    }
}

fn contains_sensitive_memory(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            let direct = object
                .get("sensitivity")
                .and_then(Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case("sensitive"));
            let frontmatter = object
                .get("frontmatter")
                .and_then(Value::as_object)
                .and_then(|value| value.get("sensitivity"))
                .and_then(Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case("sensitive"));
            direct || frontmatter || object.values().any(contains_sensitive_memory)
        }
        Value::Array(items) => items.iter().any(contains_sensitive_memory),
        _ => false,
    }
}

fn redact_secret_keys(value: Value, depth: usize) -> Value {
    if depth > 12 {
        return Value::String("[depth-limited]".to_string());
    }
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, child)| {
                    let lowered = key.to_ascii_lowercase();
                    let secret = [
                        "password",
                        "secret",
                        "token",
                        "api_key",
                        "apikey",
                        "authorization",
                        "credential",
                        "cookie",
                        "private_key",
                        "oauth",
                        "signed_url",
                    ]
                    .iter()
                    .any(|needle| lowered.contains(needle));
                    (
                        key,
                        if secret {
                            Value::String("[redacted]".to_string())
                        } else {
                            redact_secret_keys(child, depth + 1)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .take(500)
                .map(|value| redact_secret_keys(value, depth + 1))
                .collect(),
        ),
        Value::String(value) => Value::String(truncate(&value, 8_000)),
        other => other,
    }
}

fn redact_free_text(value: &str) -> String {
    // Non-JSON output should be uncommon. Keep useful diagnostics bounded but
    // do not echo obvious bearer/API-key material into the model context.
    value
        .split_whitespace()
        .map(|part| {
            let lowered = part.to_ascii_lowercase();
            if lowered.starts_with("bearer")
                || lowered.starts_with("sk-")
                || lowered.starts_with("ghp_")
                || lowered.starts_with("github_pat_")
            {
                "[redacted]".to_string()
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(16_000)
        .collect()
}

fn approval_summary(tool: &str, arguments: &Value, scope: &str) -> String {
    let safe = summarize_arguments(arguments, 0);
    let rendered = serde_json::to_string_pretty(&safe).unwrap_or_else(|_| "{}".to_string());
    sanitize_dialog_text(&format!(
        "Continuum wants to call MCP tool: {tool}\n\n{scope}\n\nArguments (sensitive values minimized):\n{rendered}"
    ))
}

fn summarize_arguments(value: &Value, depth: usize) -> Value {
    if depth > 5 {
        return Value::String("[depth-limited]".to_string());
    }
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .take(40)
                .map(|(key, value)| {
                    let lowered = key.to_ascii_lowercase();
                    let sensitive = [
                        "password",
                        "secret",
                        "token",
                        "key",
                        "authorization",
                        "cookie",
                        "body",
                        "content",
                        "text",
                    ]
                    .iter()
                    .any(|needle| lowered.contains(needle));
                    (
                        key.clone(),
                        if sensitive {
                            redacted_shape(value)
                        } else {
                            summarize_arguments(value, depth + 1)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(20)
                .map(|value| summarize_arguments(value, depth + 1))
                .collect(),
        ),
        Value::String(value) => Value::String(truncate(value, 160)),
        other => other.clone(),
    }
}

fn redacted_shape(value: &Value) -> Value {
    match value {
        Value::String(value) => {
            serde_json::json!({"redacted": true, "chars": value.chars().count()})
        }
        Value::Array(items) => serde_json::json!({"redacted": true, "items": items.len()}),
        Value::Object(object) => serde_json::json!({"redacted": true, "fields": object.len()}),
        other => serde_json::json!({"redacted": true, "type": match other {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            _ => "value",
        }}),
    }
}

fn sanitize_dialog_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .filter(|character| {
            !matches!(
                character,
                '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        })
        .take(3_500)
        .collect()
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut output: String = value.chars().take(max_chars).collect();
    if value.chars().count() > max_chars {
        output.push('…');
    }
    output
}

#[cfg(windows)]
async fn native_approval(summary: &str) -> std::result::Result<bool, String> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
$result = [System.Windows.Forms.MessageBox]::Show(
  $env:CONTINUUM_APPROVAL_SUMMARY,
  'Continuum MCP permission',
  [System.Windows.Forms.MessageBoxButtons]::YesNo,
  [System.Windows.Forms.MessageBoxIcon]::Warning,
  [System.Windows.Forms.MessageBoxDefaultButton]::Button2
)
if ($result -eq [System.Windows.Forms.DialogResult]::Yes) { 'allow' } else { 'deny' }
"#;
    let output = tokio::time::timeout(
        APPROVAL_TIMEOUT,
        tokio::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-STA",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                SCRIPT,
            ])
            .env("CONTINUUM_APPROVAL_SUMMARY", summary)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "MCP approval dialog timed out".to_string())?
    .map_err(|error| format!("failed to open MCP approval dialog: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "MCP approval dialog failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .eq_ignore_ascii_case("allow"))
}

#[cfg(target_os = "macos")]
async fn native_approval(summary: &str) -> std::result::Result<bool, String> {
    const SCRIPT: &str = r#"
set promptText to system attribute "CONTINUUM_APPROVAL_SUMMARY"
try
  display dialog promptText with title "Continuum MCP permission" buttons {"Deny", "Allow"} default button "Deny" with icon caution
  if button returned of result is "Allow" then
    return "allow"
  end if
on error number -128
end try
return "deny"
"#;
    let output = tokio::time::timeout(
        APPROVAL_TIMEOUT,
        tokio::process::Command::new("osascript")
            .args(["-e", SCRIPT])
            .env("CONTINUUM_APPROVAL_SUMMARY", summary)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "MCP approval dialog timed out".to_string())?
    .map_err(|error| format!("failed to open MCP approval dialog: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "MCP approval dialog failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .eq_ignore_ascii_case("allow"))
}

#[cfg(not(any(windows, target_os = "macos")))]
async fn native_approval(_summary: &str) -> std::result::Result<bool, String> {
    Err("native MCP approvals are currently supported on Windows and macOS only".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_contract_parses_and_unknown_tools_fail_closed() {
        let mut tools = BTreeMap::new();
        merge_toml_permissions(BUILTIN_PERMISSIONS, "test", &mut tools).expect("permissions");
        assert_eq!(tools["memory_query_episodic"], ToolPermission::Auto);
        let config = PermissionConfig {
            tools,
            source: PathBuf::from("test"),
        };
        assert_eq!(
            config.tier("new_unclassified_tool"),
            ToolPermission::AlwaysConfirm
        );
    }

    #[test]
    fn sensitive_vault_rows_are_removed_from_search_results() {
        let value = serde_json::json!([
            {"title":"safe", "sensitivity":"normal"},
            {"title":"private", "sensitivity":"sensitive"}
        ]);
        let filtered = filter_sensitive_search_results(value);
        assert_eq!(filtered.as_array().expect("array").len(), 1);
        assert_eq!(filtered[0]["title"], "safe");
    }

    #[test]
    fn approval_summary_removes_bidi_controls_and_secret_payloads() {
        let summary = approval_summary(
            "example",
            &serde_json::json!({"body":"private", "token":"secret", "title":"safe\u{202e}"}),
            "once",
        );
        assert!(!summary.contains("private"));
        assert!(!summary.contains("secret"));
        assert!(!summary.contains('\u{202e}'));
    }
}
