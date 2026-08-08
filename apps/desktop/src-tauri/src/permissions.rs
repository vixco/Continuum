use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::AppState;

const DEFAULTS: &str = include_str!("../../../../config/default-permissions.toml");

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
            let permission = value.as_str().ok_or_else(|| {
                format!("Permission for {tool:?} in {source} must be a string")
            })?;
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
    fn atomic_override_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("permissions.toml");
        let document: toml::Value = toml::from_str(
            "[memory]\nmemory_vault_get = \"blocked\"\n",
        )
        .expect("document");
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
