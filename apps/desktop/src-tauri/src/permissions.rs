use std::collections::{BTreeMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::Serialize;
use serde_json::Value;
use tauri::State;
use tokio::sync::Mutex;

use crate::AppState;

const DEFAULTS: &str = include_str!("../../../../config/default-permissions.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeToolPermission {
    Auto,
    SessionApproved,
    AlwaysConfirm,
    Blocked,
}

impl NativeToolPermission {
    fn parse(value: &str) -> Result<Self, String> {
        match canonical_permission(value)? {
            "auto" => Ok(Self::Auto),
            "session-approved" => Ok(Self::SessionApproved),
            "always-confirm" => Ok(Self::AlwaysConfirm),
            _ => Ok(Self::Blocked),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolPermissionView {
    pub tool: String,
    pub permission: String,
    pub source: String,
}

#[tauri::command]
pub async fn list_tool_permissions(
    app: State<'_, Arc<AppState>>,
) -> Result<Vec<ToolPermissionView>, String> {
    let path = permission_path(&app);
    effective_permissions(&path)
}

#[tauri::command]
pub async fn set_tool_permission(
    app: State<'_, Arc<AppState>>,
    tool: String,
    permission: String,
) -> Result<Vec<ToolPermissionView>, String> {
    let canonical = canonical_permission(&permission)?;
    let defaults = parse_permissions(DEFAULTS, "bundled defaults")?;
    let section = defaults
        .iter()
        .find_map(|(section, tools)| tools.contains_key(&tool).then_some(section.clone()))
        .ok_or_else(|| {
            format!(
                "Unknown MCP tool {tool:?}. New tools must be classified in config/default-permissions.toml before the UI may change them."
            )
        })?;

    let path = permission_path(&app);
    let mut document = load_override_document(&path)?;
    let root = document
        .as_table_mut()
        .ok_or_else(|| "permissions override must be a TOML table".to_string())?;
    let section_value = root
        .entry(section)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let section_table = section_value
        .as_table_mut()
        .ok_or_else(|| "permissions override section must be a TOML table".to_string())?;
    section_table.insert(tool, toml::Value::String(canonical.to_string()));
    persist_document(&path, &document)?;
    effective_permissions(&path)
}

/// The HTTP-provider chat path executes tools inside the Tauri process rather
/// than through `continuum-mcp`. This is its mandatory authorization choke
/// point so neither memory nor live-context tools can bypass the policy shown
/// in the Tools tab.
#[cfg(not(test))]
pub(crate) async fn authorize_in_process_tool(
    local_tool: &str,
    arguments: &Value,
) -> Result<(), String> {
    let canonical_tool = canonical_chat_tool(local_tool).ok_or_else(|| {
        format!("Unknown in-process chat tool {local_tool:?}; permission denied by default")
    })?;
    let data_dir = continuum_core::config::continuum_dev_dir();
    let permission =
        effective_permission_for_tool(&data_dir.join("permissions.toml"), canonical_tool)?;

    match permission {
        NativeToolPermission::Auto => Ok(()),
        NativeToolPermission::Blocked => Err(format!(
            "Tool {canonical_tool:?} is blocked by the enforced local permission policy"
        )),
        NativeToolPermission::SessionApproved => {
            let grants = in_process_session_grants();
            if grants.lock().await.contains(canonical_tool) {
                return Ok(());
            }
            require_native_approval(canonical_tool, arguments, "Approve for this chat session")
                .await?;
            grants.lock().await.insert(canonical_tool.to_string());
            Ok(())
        }
        NativeToolPermission::AlwaysConfirm => {
            require_native_approval(canonical_tool, arguments, "Approve this tool call once").await
        }
    }
}

/// Unit tests exercise the vault/context behavior without opening native UI.
/// Permission parsing and fail-closed mapping are tested separately below.
#[cfg(test)]
pub(crate) async fn authorize_in_process_tool(
    local_tool: &str,
    _arguments: &Value,
) -> Result<(), String> {
    canonical_chat_tool(local_tool)
        .map(|_| ())
        .ok_or_else(|| format!("Unknown in-process chat tool {local_tool:?}"))
}

fn permission_path(app: &AppState) -> PathBuf {
    app.runtime.dev_dir().join("permissions.toml")
}

fn effective_permissions(path: &Path) -> Result<Vec<ToolPermissionView>, String> {
    let defaults = parse_permissions(DEFAULTS, "bundled defaults")?;
    let overrides = if path.exists() {
        let body = std::fs::read_to_string(path)
            .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
        parse_permissions(&body, &path.display().to_string())?
    } else {
        BTreeMap::new()
    };

    let mut output = Vec::new();
    for (section, tools) in defaults {
        for (tool, default) in tools {
            let overridden = overrides
                .get(&section)
                .and_then(|tools| tools.get(&tool))
                .cloned();
            output.push(ToolPermissionView {
                tool,
                permission: ui_permission(overridden.as_deref().unwrap_or(&default)).to_string(),
                source: if overridden.is_some() {
                    "user_override"
                } else {
                    "bundled_default"
                }
                .to_string(),
            });
        }
    }
    output.sort_by(|left, right| left.tool.cmp(&right.tool));
    Ok(output)
}

fn effective_permission_for_tool(path: &Path, tool: &str) -> Result<NativeToolPermission, String> {
    let defaults = parse_permissions(DEFAULTS, "bundled defaults")?;
    let default = defaults
        .values()
        .find_map(|tools| tools.get(tool))
        .ok_or_else(|| {
            format!(
                "Tool {tool:?} is absent from config/default-permissions.toml and is denied by default"
            )
        })?;

    if path.exists() {
        let body = std::fs::read_to_string(path)
            .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
        let overrides = parse_permissions(&body, &path.display().to_string())?;
        if let Some(value) = overrides.values().find_map(|tools| tools.get(tool)) {
            return NativeToolPermission::parse(value);
        }
    }
    NativeToolPermission::parse(default)
}

fn parse_permissions(
    body: &str,
    source: &str,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    let document: toml::Value = toml::from_str(body)
        .map_err(|error| format!("Invalid permissions TOML in {source}: {error}"))?;
    let root = document
        .as_table()
        .ok_or_else(|| format!("Permissions file {source} is not a TOML table"))?;
    let mut output = BTreeMap::new();
    for (section, value) in root {
        let table = value.as_table().ok_or_else(|| {
            format!("Permission section {section:?} in {source} is not a TOML table")
        })?;
        let mut tools = BTreeMap::new();
        for (tool, value) in table {
            let permission = value
                .as_str()
                .ok_or_else(|| format!("Permission for {tool:?} in {source} must be a string"))?;
            canonical_permission(permission)?;
            tools.insert(tool.clone(), permission.to_string());
        }
        output.insert(section.clone(), tools);
    }
    Ok(output)
}

fn canonical_permission(value: &str) -> Result<&'static str, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok("auto"),
        "session" | "session-approved" => Ok("session-approved"),
        "confirm" | "always-confirm" => Ok("always-confirm"),
        "blocked" | "deny" => Ok("blocked"),
        other => Err(format!(
            "Unsupported permission {other:?}; expected auto, session, confirm or blocked"
        )),
    }
}

fn ui_permission(value: &str) -> &'static str {
    match canonical_permission(value).unwrap_or("blocked") {
        "auto" => "auto",
        "session-approved" => "session",
        "always-confirm" => "confirm",
        _ => "blocked",
    }
}

fn canonical_chat_tool(local_tool: &str) -> Option<&'static str> {
    match local_tool {
        "memory_search" => Some("memory_vault_search"),
        "memory_get" => Some("memory_vault_get"),
        "memory_save" => Some("memory_vault_save"),
        "memory_delete" => Some("memory_vault_delete"),
        "context_screen" => Some("context_screen"),
        "context_window" => Some("context_window"),
        _ => None,
    }
}

fn in_process_session_grants() -> &'static Mutex<HashSet<String>> {
    static GRANTS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    GRANTS.get_or_init(|| Mutex::new(HashSet::new()))
}

#[cfg(not(test))]
async fn require_native_approval(tool: &str, arguments: &Value, scope: &str) -> Result<(), String> {
    if std::env::var("CONTINUUM_MCP_HEADLESS")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
    {
        return Err(format!(
            "Tool {tool:?} requires native approval, but Continuum is running headless"
        ));
    }
    let summary = approval_summary(tool, arguments, scope);
    if native_approval(&summary).await? {
        Ok(())
    } else {
        Err(format!("The user denied tool {tool:?}"))
    }
}

#[cfg(not(test))]
fn approval_summary(tool: &str, arguments: &Value, scope: &str) -> String {
    let safe = minimize_arguments(arguments, 0);
    let rendered = serde_json::to_string_pretty(&safe).unwrap_or_else(|_| "{}".to_string());
    sanitize_dialog_text(&format!(
        "Continuum chat wants to call: {tool}\n\n{scope}\n\nArguments (private payloads minimized):\n{rendered}"
    ))
}

#[cfg(not(test))]
fn minimize_arguments(value: &Value, depth: usize) -> Value {
    if depth > 5 {
        return Value::String("[depth-limited]".to_string());
    }
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .take(40)
                .map(|(key, child)| {
                    let lowered = key.to_ascii_lowercase();
                    let private = [
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
                        if private {
                            redacted_shape(child)
                        } else {
                            minimize_arguments(child, depth + 1)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(20)
                .map(|child| minimize_arguments(child, depth + 1))
                .collect(),
        ),
        Value::String(value) => Value::String(value.chars().take(160).collect()),
        other => other.clone(),
    }
}

#[cfg(not(test))]
fn redacted_shape(value: &Value) -> Value {
    match value {
        Value::String(value) => {
            serde_json::json!({"redacted": true, "chars": value.chars().count()})
        }
        Value::Array(items) => serde_json::json!({"redacted": true, "items": items.len()}),
        Value::Object(object) => {
            serde_json::json!({"redacted": true, "fields": object.len()})
        }
        Value::Null => serde_json::json!({"redacted": true, "kind": "null"}),
        Value::Bool(_) => serde_json::json!({"redacted": true, "kind": "boolean"}),
        Value::Number(_) => serde_json::json!({"redacted": true, "kind": "number"}),
    }
}

#[cfg(not(test))]
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

#[cfg(all(not(test), windows))]
async fn native_approval(summary: &str) -> Result<bool, String> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
$result = [System.Windows.Forms.MessageBox]::Show(
  $env:CONTINUUM_APPROVAL_SUMMARY,
  'Continuum chat tool permission',
  [System.Windows.Forms.MessageBoxButtons]::YesNo,
  [System.Windows.Forms.MessageBoxIcon]::Warning,
  [System.Windows.Forms.MessageBoxDefaultButton]::Button2
)
if ($result -eq [System.Windows.Forms.DialogResult]::Yes) { 'allow' } else { 'deny' }
"#;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
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
    .map_err(|_| "Chat tool approval timed out".to_string())?
    .map_err(|error| format!("Could not open chat tool approval: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Chat tool approval failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .eq_ignore_ascii_case("allow"))
}

#[cfg(all(not(test), target_os = "macos"))]
async fn native_approval(summary: &str) -> Result<bool, String> {
    const SCRIPT: &str = r#"
set promptText to system attribute "CONTINUUM_APPROVAL_SUMMARY"
try
  display dialog promptText with title "Continuum chat tool permission" buttons {"Deny", "Allow"} default button "Deny" with icon caution
  if button returned of result is "Allow" then return "allow"
on error number -128
end try
return "deny"
"#;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::process::Command::new("osascript")
            .args(["-e", SCRIPT])
            .env("CONTINUUM_APPROVAL_SUMMARY", summary)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "Chat tool approval timed out".to_string())?
    .map_err(|error| format!("Could not open chat tool approval: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Chat tool approval failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .eq_ignore_ascii_case("allow"))
}

#[cfg(all(not(test), not(any(windows, target_os = "macos"))))]
async fn native_approval(_summary: &str) -> Result<bool, String> {
    Err("Native chat-tool approvals are supported on Windows and macOS only".to_string())
}

fn load_override_document(path: &Path) -> Result<toml::Value, String> {
    if !path.exists() {
        return Ok(toml::Value::Table(toml::Table::new()));
    }
    let body = std::fs::read_to_string(path)
        .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
    toml::from_str(&body)
        .map_err(|error| format!("Invalid permissions TOML in {}: {error}", path.display()))
}

fn persist_document(path: &Path, document: &toml::Value) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "permissions path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(
        ".permissions-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let payload = toml::to_string_pretty(document)
        .map_err(|error| format!("Could not serialize permissions: {error}"))?;

    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("Could not create {}: {error}", temporary.display()))?;
    if let Err(error) = (|| -> std::io::Result<()> {
        file.write_all(payload.as_bytes())?;
        file.sync_all()
    })() {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("Could not persist permissions: {error}"));
    }
    drop(file);

    let backup = path.with_extension("toml.backup");
    if backup.exists() {
        std::fs::remove_file(&backup)
            .map_err(|error| format!("Could not remove stale {}: {error}", backup.display()))?;
    }
    if path.exists() {
        std::fs::rename(path, &backup)
            .map_err(|error| format!("Could not preserve {}: {error}", path.display()))?;
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        if backup.exists() && !path.exists() {
            let _ = std::fs::rename(&backup, path);
        }
        return Err(format!("Could not activate {}: {error}", path.display()));
    }
    if backup.exists() {
        let _ = std::fs::remove_file(backup);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_permissions_are_valid() {
        let parsed = parse_permissions(DEFAULTS, "test").expect("parse defaults");
        assert_eq!(parsed["memory"]["memory_query_episodic"], "auto");
        assert_eq!(parsed["workers"]["workers_spawn_worker"], "always-confirm");
    }

    #[test]
    fn ui_and_file_names_roundtrip() {
        assert_eq!(canonical_permission("session").unwrap(), "session-approved");
        assert_eq!(canonical_permission("confirm").unwrap(), "always-confirm");
        assert_eq!(ui_permission("session-approved"), "session");
        assert_eq!(ui_permission("always-confirm"), "confirm");
    }

    #[test]
    fn in_process_tools_map_to_the_same_mcp_policy() {
        assert_eq!(
            canonical_chat_tool("memory_search"),
            Some("memory_vault_search")
        );
        assert_eq!(
            canonical_chat_tool("memory_delete"),
            Some("memory_vault_delete")
        );
        assert_eq!(
            canonical_chat_tool("context_screen"),
            Some("context_screen")
        );
        assert_eq!(
            canonical_chat_tool("context_window"),
            Some("context_window")
        );
        assert_eq!(canonical_chat_tool("unknown"), None);
    }

    #[test]
    fn effective_permission_uses_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("permissions.toml");
        std::fs::write(&path, "[memory]\nmemory_vault_get = \"blocked\"\n")
            .expect("write override");
        assert_eq!(
            effective_permission_for_tool(&path, "memory_vault_get").expect("permission"),
            NativeToolPermission::Blocked
        );
    }

    #[test]
    fn atomic_override_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("permissions.toml");
        let document: toml::Value =
            toml::from_str("[memory]\nmemory_vault_get = \"blocked\"\n").expect("document");
        persist_document(&path, &document).expect("persist");
        let effective = effective_permissions(&path).expect("effective");
        let selected = effective
            .iter()
            .find(|item| item.tool == "memory_vault_get")
            .expect("tool");
        assert_eq!(selected.permission, "blocked");
        assert_eq!(selected.source, "user_override");
    }
}
