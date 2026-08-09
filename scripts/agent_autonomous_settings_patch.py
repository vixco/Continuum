#!/usr/bin/env python3
from pathlib import Path


def read(path: str) -> str:
    return Path(path).read_text(encoding="utf-8")


def write(path: str, content: str) -> None:
    target = Path(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, found {count} for {old[:80]!r}")
    write(path, text.replace(old, new, 1))


def replace_between(path: str, start_marker: str, end_marker: str, replacement: str) -> None:
    text = read(path)
    start = text.find(start_marker)
    if start < 0:
        raise RuntimeError(f"{path}: start marker not found: {start_marker!r}")
    end = text.find(end_marker, start)
    if end < 0:
        raise RuntimeError(f"{path}: end marker not found: {end_marker!r}")
    write(path, text[:start] + replacement + text[end:])


write(
    "crates/continuum-core/src/settings.rs",
    r'''//! Typed, autonomous access to Continuum's runtime settings.
//!
//! This module is shared by desktop chat providers and the MCP server so every
//! model gets the same discovery, validation, redaction, backup, and write
//! semantics. Mutations are surgical: one existing dotted path is copied from a
//! fully validated candidate config into the user's TOML document. Unknown
//! sibling keys and future top-level sections therefore survive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config::{load_config, ContinuumConfig};

const DEFAULT_LIST_LIMIT: usize = 80;
const MAX_LIST_LIMIT: usize = 250;

/// Discover typed settings, optionally filtered by path keyword.
pub fn list(
    config_path: &Path,
    query: Option<&str>,
    limit: Option<usize>,
) -> Result<Value, String> {
    let current = load_config(config_path)
        .map_err(|error| format!("Could not load {}: {error}", config_path.display()))?;
    let defaults = ContinuumConfig::default();
    let current = serde_json::to_value(current).map_err(|error| error.to_string())?;
    let defaults = serde_json::to_value(defaults).map_err(|error| error.to_string())?;
    let mut current_paths = BTreeMap::new();
    let mut default_paths = BTreeMap::new();
    flatten("", &current, &mut current_paths);
    flatten("", &defaults, &mut default_paths);

    let query = query.unwrap_or_default().trim().to_ascii_lowercase();
    let limit = limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let settings = current_paths
        .into_iter()
        .filter(|(path, _)| query.is_empty() || path.to_ascii_lowercase().contains(&query))
        .take(limit)
        .map(|(path, value)| {
            let sensitive = is_sensitive_path(&path);
            let default = default_paths.get(&path).cloned().unwrap_or(Value::Null);
            json!({
                "path": path,
                "value": redact_value(&path, value),
                "default": redact_value(&path, default),
                "value_type": value_type(&value),
                "sensitive": sensitive,
                "mutable": true,
                "ui_location": ui_location(&path),
                "restart_recommended": true,
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "config_path": config_path.display().to_string(),
        "count": settings.len(),
        "settings": settings,
        "notes": [
            "Use settings_get before settings_set when the exact path or current value is uncertain.",
            "Provider credentials stay in the OS credential store and are intentionally absent.",
            "Most runtime components read config at startup, so a runtime restart is recommended after a change."
        ]
    }))
}

/// Read one exact setting and its default.
pub fn get(config_path: &Path, setting_path: &str) -> Result<Value, String> {
    validate_setting_path(setting_path)?;
    let current = load_config(config_path)
        .map_err(|error| format!("Could not load {}: {error}", config_path.display()))?;
    let defaults = ContinuumConfig::default();
    let current = serde_json::to_value(current).map_err(|error| error.to_string())?;
    let defaults = serde_json::to_value(defaults).map_err(|error| error.to_string())?;
    let value = lookup(&current, setting_path)
        .ok_or_else(|| format!("Unknown setting path `{setting_path}`. Use settings_list first."))?;
    let default = lookup(&defaults, setting_path).cloned().unwrap_or(Value::Null);
    let sensitive = is_sensitive_path(setting_path);

    Ok(json!({
        "path": setting_path,
        "value": redact_value(setting_path, value.clone()),
        "default": redact_value(setting_path, default),
        "value_type": value_type(value),
        "sensitive": sensitive,
        "mutable": true,
        "config_path": config_path.display().to_string(),
        "ui_location": ui_location(setting_path),
        "restart_recommended": true,
    }))
}

/// Change one existing typed setting.
///
/// The candidate is first deserialized as [`ContinuumConfig`] and semantically
/// validated. Only then is the requested path copied into the raw TOML document,
/// preserving unknown sibling keys. The existing file is backed up and the
/// replacement is written through a same-directory temporary file.
pub fn set(
    config_path: &Path,
    setting_path: &str,
    requested: Value,
) -> Result<Value, String> {
    validate_setting_path(setting_path)?;

    let current_config = load_config(config_path)
        .map_err(|error| format!("Could not load {}: {error}", config_path.display()))?;
    let mut candidate_json =
        serde_json::to_value(&current_config).map_err(|error| error.to_string())?;
    let previous = replace_existing_path(&mut candidate_json, setting_path, requested.clone())?;

    let candidate: ContinuumConfig = serde_json::from_value(candidate_json)
        .map_err(|error| format!("Invalid value for `{setting_path}`: {error}"))?;
    candidate
        .resources
        .validate()
        .map_err(|error| format!("Invalid resource setting: {error}"))?;

    let candidate_toml = toml::Value::try_from(&candidate)
        .map_err(|error| format!("Could not encode validated config: {error}"))?;
    let mut raw = read_document(config_path)?;
    {
        let raw_value = toml::Value::Table(raw);
        let mut updated = raw_value;
        copy_candidate_path(&mut updated, &candidate_toml, &path_segments(setting_path)?)?;
        raw = updated
            .as_table()
            .cloned()
            .ok_or_else(|| "Config root stopped being a TOML table".to_string())?;
    }

    let backup = backup_existing(config_path)?;
    if let Err(error) = write_document_atomic(config_path, &raw) {
        restore_backup(config_path, backup.as_deref());
        return Err(error);
    }

    let verified = load_config(config_path).map_err(|error| {
        restore_backup(config_path, backup.as_deref());
        format!(
            "The updated config could not be reloaded and was restored from backup: {error}"
        )
    })?;
    let verified_json = serde_json::to_value(verified).map_err(|error| error.to_string())?;
    let verified_value = lookup(&verified_json, setting_path).cloned().unwrap_or(Value::Null);
    let candidate_value = lookup(
        &serde_json::to_value(&candidate).map_err(|error| error.to_string())?,
        setting_path,
    )
    .cloned()
    .unwrap_or(Value::Null);
    if verified_value != candidate_value {
        restore_backup(config_path, backup.as_deref());
        return Err(format!(
            "Verification failed for `{setting_path}`; the previous config was restored"
        ));
    }

    let sensitive = is_sensitive_path(setting_path);
    Ok(json!({
        "changed": previous != requested,
        "path": setting_path,
        "previous": redact_value(setting_path, previous),
        "value": redact_value(setting_path, verified_value),
        "value_type": value_type(&candidate_value),
        "sensitive": sensitive,
        "config_path": config_path.display().to_string(),
        "backup_path": backup.map(|path| path.display().to_string()),
        "ui_location": ui_location(setting_path),
        "restart_recommended": true,
        "applies": "The value is persisted now. Restart the Continuum runtime to guarantee every component reloads it."
    }))
}

fn validate_setting_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("`path` must be a non-empty dotted setting path".into());
    }
    if path.starts_with('.') || path.ends_with('.') || path.contains("..") {
        return Err(format!("Invalid dotted setting path `{path}`"));
    }
    Ok(())
}

fn path_segments(path: &str) -> Result<Vec<&str>, String> {
    validate_setting_path(path)?;
    Ok(path.split('.').collect())
}

fn flatten(prefix: &str, value: &Value, output: &mut BTreeMap<String, Value>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, child, output);
            }
        }
        Value::Array(items) => {
            if !prefix.is_empty() {
                output.insert(prefix.to_string(), value.clone());
            }
            for (index, child) in items.iter().enumerate() {
                let path = format!("{prefix}.{index}");
                flatten(&path, child, output);
            }
        }
        _ if !prefix.is_empty() => {
            output.insert(prefix.to_string(), value.clone());
        }
        _ => {}
    }
}

fn lookup<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    for segment in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

fn replace_existing_path(root: &mut Value, path: &str, replacement: Value) -> Result<Value, String> {
    let segments = path_segments(path)?;
    let mut current = root;
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        match current {
            Value::Object(map) => {
                if last {
                    let slot = map.get_mut(*segment).ok_or_else(|| {
                        format!("Unknown setting path `{path}`. Use settings_list first.")
                    })?;
                    return Ok(std::mem::replace(slot, replacement));
                }
                current = map.get_mut(*segment).ok_or_else(|| {
                    format!("Unknown setting path `{path}`. Use settings_list first.")
                })?;
            }
            Value::Array(items) => {
                let item_index = segment.parse::<usize>().map_err(|_| {
                    format!("`{segment}` is not a valid array index in `{path}`")
                })?;
                if last {
                    let slot = items.get_mut(item_index).ok_or_else(|| {
                        format!("Unknown setting path `{path}`. Use settings_list first.")
                    })?;
                    return Ok(std::mem::replace(slot, replacement));
                }
                current = items.get_mut(item_index).ok_or_else(|| {
                    format!("Unknown setting path `{path}`. Use settings_list first.")
                })?;
            }
            _ => {
                return Err(format!(
                    "`{path}` traverses through a scalar value; use settings_list to discover a writable leaf"
                ))
            }
        }
    }
    Err(format!("Unknown setting path `{path}`"))
}

/// Copy one validated leaf from `candidate` into `raw`.
///
/// Missing or type-incompatible intermediate containers are replaced with the
/// corresponding validated candidate subtree. Existing tables/arrays are
/// traversed so unknown siblings survive.
fn copy_candidate_path(
    raw: &mut toml::Value,
    candidate: &toml::Value,
    segments: &[&str],
) -> Result<(), String> {
    let Some((segment, rest)) = segments.split_first() else {
        *raw = candidate.clone();
        return Ok(());
    };
    match candidate {
        toml::Value::Table(candidate_table) => {
            let candidate_child = candidate_table.get(*segment);
            if !raw.is_table() {
                *raw = toml::Value::Table(toml::Table::new());
            }
            let raw_table = raw.as_table_mut().expect("just ensured a table");
            if rest.is_empty() {
                match candidate_child {
                    Some(value) => {
                        raw_table.insert((*segment).to_string(), value.clone());
                    }
                    None => {
                        raw_table.remove(*segment);
                    }
                }
                return Ok(());
            }
            let Some(candidate_child) = candidate_child else {
                return Err(format!(
                    "Validated config does not contain intermediate path segment `{segment}`"
                ));
            };
            let raw_child = raw_table
                .entry((*segment).to_string())
                .or_insert_with(|| candidate_child.clone());
            if !same_container_kind(raw_child, candidate_child) {
                *raw_child = candidate_child.clone();
                return Ok(());
            }
            copy_candidate_path(raw_child, candidate_child, rest)
        }
        toml::Value::Array(candidate_items) => {
            let index = segment
                .parse::<usize>()
                .map_err(|_| format!("`{segment}` is not a valid TOML array index"))?;
            let candidate_child = candidate_items
                .get(index)
                .ok_or_else(|| format!("Array index {index} is outside the validated config"))?;
            if !raw.is_array() {
                *raw = toml::Value::Array(candidate_items.clone());
                return Ok(());
            }
            let raw_items = raw.as_array_mut().expect("just ensured an array");
            if index >= raw_items.len() {
                *raw_items = candidate_items.clone();
                return Ok(());
            }
            if rest.is_empty() {
                raw_items[index] = candidate_child.clone();
                return Ok(());
            }
            if !same_container_kind(&raw_items[index], candidate_child) {
                raw_items[index] = candidate_child.clone();
                return Ok(());
            }
            copy_candidate_path(&mut raw_items[index], candidate_child, rest)
        }
        _ => Err("Setting path continues beyond a scalar candidate value".into()),
    }
}

fn same_container_kind(left: &toml::Value, right: &toml::Value) -> bool {
    matches!(
        (left, right),
        (toml::Value::Table(_), toml::Value::Table(_))
            | (toml::Value::Array(_), toml::Value::Array(_))
    )
}

fn read_document(path: &Path) -> Result<toml::Table, String> {
    match std::fs::read_to_string(path) {
        Ok(body) => body
            .parse::<toml::Table>()
            .map_err(|error| format!("Could not parse {} as TOML: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(error) => Err(format!("Could not read {}: {error}", path.display())),
    }
}

fn backup_existing(path: &Path) -> Result<Option<PathBuf>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let backup = path.with_extension("toml.bak");
    std::fs::copy(path, &backup).map_err(|error| {
        format!(
            "Could not create config backup {}: {error}",
            backup.display()
        )
    })?;
    Ok(Some(backup))
}

fn restore_backup(path: &Path, backup: Option<&Path>) {
    match backup {
        Some(backup) => {
            let _ = std::fs::copy(backup, path);
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn write_document_atomic(path: &Path, document: &toml::Table) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
    }
    let body = toml::to_string_pretty(document)
        .map_err(|error| format!("Could not serialize config: {error}"))?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, body)
        .map_err(|error| format!("Could not write {}: {error}", temporary.display()))?;
    if cfg!(windows) && path.exists() {
        std::fs::remove_file(path)
            .map_err(|error| format!("Could not replace {}: {error}", path.display()))?;
    }
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("Could not finalize {}: {error}", path.display()))
}

fn is_sensitive_path(path: &str) -> bool {
    path.split('.').any(|segment| {
        let segment = segment.to_ascii_lowercase();
        matches!(
            segment.as_str(),
            "password"
                | "secret"
                | "token"
                | "access_token"
                | "refresh_token"
                | "api_key"
                | "apikey"
                | "credential"
                | "credentials"
        ) || segment.ends_with("_password")
            || segment.ends_with("_secret")
            || segment.ends_with("_token")
            || segment.ends_with("_api_key")
    })
}

fn redact_value(path: &str, value: Value) -> Value {
    if is_sensitive_path(path) && !value.is_null() {
        return Value::String("[redacted]".into());
    }
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, child)| {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    (key, redact_value(&child_path, child))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .enumerate()
                .map(|(index, child)| redact_value(&format!("{path}.{index}"), child))
                .collect(),
        ),
        other => other,
    }
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn ui_location(path: &str) -> String {
    let section = path.split('.').next().unwrap_or("advanced");
    let label = match section {
        "health" => "Health & diagnostics",
        "vision" | "screen" | "audio" | "context" | "frame" => "Sensing",
        "storage" | "memory" => "Memory & storage",
        "voice" | "tts" => "Voice",
        "workers" | "skills" | "orchestrator" | "triage" => "Agents & models",
        "resources" | "performance" => "Performance",
        "chat" => "Chat",
        "privacy" => "Privacy",
        "projects" | "git_context" | "github" => "Integrations & projects",
        "events"
        | "file_watcher"
        | "process_watcher"
        | "session_state"
        | "context_package"
        | "continuation"
        | "context_tools" => "Context engine",
        _ => "Advanced",
    };
    format!("Settings > {label} ({section})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn path(temp: &TempDir) -> PathBuf {
        temp.path().join("config.toml")
    }

    #[test]
    fn changes_a_typed_scalar_and_reads_it_back() {
        let temp = TempDir::new().unwrap();
        let config_path = path(&temp);
        let result = set(&config_path, "chat.max_tokens", Value::from(1_234_u64)).unwrap();
        assert_eq!(result["value"], 1_234);
        assert_eq!(
            get(&config_path, "chat.max_tokens").unwrap()["value"],
            1_234
        );
    }

    #[test]
    fn rejects_unknown_paths_and_wrong_types_without_touching_the_file() {
        let temp = TempDir::new().unwrap();
        let config_path = path(&temp);
        std::fs::write(&config_path, "[screen]\nenabled = true\n").unwrap();
        let before = std::fs::read_to_string(&config_path).unwrap();

        assert!(set(&config_path, "screen.not_real", Value::Bool(false)).is_err());
        assert!(set(
            &config_path,
            "chat.max_tokens",
            Value::String("fast".into())
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), before);
    }

    #[test]
    fn preserves_unknown_siblings_and_future_sections() {
        let temp = TempDir::new().unwrap();
        let config_path = path(&temp);
        std::fs::write(
            &config_path,
            "[screen]\nenabled = true\nfuture_knob = 7\n\n[future]\nkept = true\n",
        )
        .unwrap();

        set(&config_path, "screen.enabled", Value::Bool(false)).unwrap();

        let raw = std::fs::read_to_string(&config_path).unwrap();
        let document: toml::Table = raw.parse().unwrap();
        assert_eq!(document["screen"]["future_knob"].as_integer(), Some(7));
        assert_eq!(document["future"]["kept"].as_bool(), Some(true));
    }

    #[test]
    fn creates_a_backup_and_leaves_no_temporary_file() {
        let temp = TempDir::new().unwrap();
        let config_path = path(&temp);
        std::fs::write(&config_path, "[screen]\nenabled = true\n").unwrap();

        set(&config_path, "screen.enabled", Value::Bool(false)).unwrap();

        assert!(config_path.with_extension("toml.bak").exists());
        assert!(!config_path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn max_tokens_is_not_treated_as_a_secret() {
        assert!(!is_sensitive_path("chat.max_tokens"));
        assert!(is_sensitive_path("provider.api_key"));
        assert!(is_sensitive_path("provider.access_token"));
    }

    #[test]
    fn list_reports_locations_and_types() {
        let temp = TempDir::new().unwrap();
        let response = list(&path(&temp), Some("screen.enabled"), Some(10)).unwrap();
        assert_eq!(response["count"], 1);
        assert_eq!(response["settings"][0]["value_type"], "boolean");
        assert_eq!(
            response["settings"][0]["ui_location"],
            "Settings > Sensing (screen)"
        );
    }
}
''',
)

replace_once(
    "crates/continuum-core/src/lib.rs",
    "pub mod config_edit;\npub mod context;",
    "pub mod config_edit;\npub mod settings;\npub mod context;",
)

write(
    "apps/desktop/src-tauri/src/settings_tools.rs",
    r'''//! Desktop wrappers around the shared typed settings backend.

use std::path::PathBuf;

use continuum_core::config::continuum_dev_dir;
use serde_json::Value;

fn config_path() -> PathBuf {
    continuum_dev_dir().join("config.toml")
}

pub fn list(input: &Value) -> Result<String, String> {
    let query = input.get("query").and_then(Value::as_str);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok());
    serialize(continuum_core::settings::list(
        &config_path(),
        query,
        limit,
    )?)
}

pub fn get(input: &Value) -> Result<String, String> {
    let path = required_string(input, "path")?;
    serialize(continuum_core::settings::get(&config_path(), path)?)
}

pub fn set(input: &Value) -> Result<String, String> {
    let path = required_string(input, "path")?;
    let value = input
        .get("value")
        .cloned()
        .ok_or_else(|| "missing required field `value`".to_string())?;
    serialize(continuum_core::settings::set(
        &config_path(),
        path,
        value,
    )?)
}

fn required_string<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing required string field `{key}`"))
}

fn serialize(value: Value) -> Result<String, String> {
    serde_json::to_string(&value).map_err(|error| error.to_string())
}
''',
)

chat_tools = "apps/desktop/src-tauri/src/chat_tools.rs"
replace_once(
    chat_tools,
    '''        result
    }
}

impl VaultToolExecutor {''',
    '''        result
    }
}

/// In-process executor for context and settings tools when the memory vault is
/// disabled or unavailable. Settings autonomy must never depend on memory.
pub struct SettingsToolExecutor;

#[async_trait::async_trait]
impl ToolExecutor for SettingsToolExecutor {
    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<String, String> {
        crate::permissions::authorize_in_process_tool(name, input).await?;

        let result = match name {
            "context_screen" => VaultToolExecutor::context_screen(),
            "context_window" => VaultToolExecutor::context_window(),
            "settings_list" => settings_tools::list(input),
            "settings_get" => settings_tools::get(input),
            "settings_set" => settings_tools::set(input),
            other => Err(format!(
                "unknown base chat tool {other:?} (expected context_screen|context_window|settings_list|settings_get|settings_set)"
            )),
        };
        if let Err(error) = &result {
            tracing::warn!(
                layer = "desktop",
                component = "chat_tools",
                tool = name,
                error = %error,
                "chat tool call failed"
            );
        }
        result
    }
}

impl VaultToolExecutor {''',
)
replace_once(chat_tools, '"context_screen" => self.context_screen(),', '"context_screen" => Self::context_screen(),')
replace_once(chat_tools, '"context_window" => self.context_window(),', '"context_window" => Self::context_window(),')
replace_once(chat_tools, "    fn context_screen(&self) -> Result<String, String> {", "    fn context_screen() -> Result<String, String> {")
replace_once(chat_tools, "    fn context_window(&self) -> Result<String, String> {", "    fn context_window() -> Result<String, String> {")
replace_once(
    chat_tools,
    "/// Claude CLI gets an explicit allowlist rather than a wildcard.",
    r'''
/// Build the tools exposed to one HTTP/Anthropic chat turn.
///
/// Live context and settings are always available. Memory tools are added only
/// when the vault is enabled and opened successfully.
pub fn chat_tool_defs(include_memory: bool) -> Vec<ToolDef> {
    let mut tools = memory_tool_defs();
    if !include_memory {
        tools.retain(|tool| !tool.name.starts_with("memory_"));
    }
    tools
}

/// Claude CLI gets an explicit allowlist rather than a wildcard.''',
)
replace_between(
    chat_tools,
    "/// Claude CLI gets an explicit allowlist rather than a wildcard.",
    "fn resolve_mcp_binary()",
    r'''/// Claude CLI gets an explicit allowlist rather than a wildcard. Adding a new
/// server tool therefore does not implicitly grant chat access to it. The
/// conversation id scopes permission grants to this chat session.
pub fn mcp_spec(
    vault_dir: &Path,
    dev_dir: &Path,
    session_id: &str,
    include_memory: bool,
) -> Option<McpSpec> {
    let server_command = resolve_mcp_binary()?;
    Some(McpSpec {
        server_command,
        env: vec![
            (
                "CONTINUUM_VAULT_DIR".into(),
                vault_dir.to_string_lossy().into_owned(),
            ),
            (
                "CONTINUUM_DATA_DIR".into(),
                dev_dir.to_string_lossy().into_owned(),
            ),
            ("CONTINUUM_SESSION_ID".into(), format!("chat-{session_id}")),
        ],
        allowed_tools: mcp_allowed_tools(include_memory),
    })
}

fn mcp_allowed_tools(include_memory: bool) -> Vec<String> {
    let mut tools = vec![
        "mcp__continuum__context_session",
        "mcp__continuum__context_window",
        "mcp__continuum__context_screen",
        "mcp__continuum__context_audio",
        "mcp__continuum__context_projects",
        "mcp__continuum__context_timeline",
        "mcp__continuum__context_search",
        "mcp__continuum__context_files",
        "mcp__continuum__context_git",
        "mcp__continuum__context_package",
        "mcp__continuum__settings_list",
        "mcp__continuum__settings_get",
        "mcp__continuum__settings_set",
    ];
    if include_memory {
        tools.extend([
            "mcp__continuum__memory_vault_search",
            "mcp__continuum__memory_vault_get",
            "mcp__continuum__memory_vault_save",
            "mcp__continuum__memory_vault_resolve",
            "mcp__continuum__memory_vault_delete",
            "mcp__continuum__memory_get_fact",
            "mcp__continuum__memory_list_facts",
            "mcp__continuum__memory_query_episodic",
        ]);
    }
    tools.into_iter().map(String::from).collect()
}

''',
)
text = read(chat_tools)
closing = text.rfind("\n}")
if closing < 0:
    raise RuntimeError("chat_tools.rs: final test-module closing brace not found")
write(chat_tools, text[:closing] + r'''
    #[test]
    fn base_chat_tools_remain_available_without_memory() {
        let names = chat_tool_defs(false)
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "context_screen",
                "context_window",
                "settings_list",
                "settings_get",
                "settings_set"
            ]
        );
    }

    #[test]
    fn claude_base_allowlist_uses_typed_settings_without_generic_fs_writes() {
        let tools = mcp_allowed_tools(false);
        assert!(tools.contains(&"mcp__continuum__settings_list".to_string()));
        assert!(tools.contains(&"mcp__continuum__settings_get".to_string()));
        assert!(tools.contains(&"mcp__continuum__settings_set".to_string()));
        assert!(!tools.iter().any(|tool| tool.contains("memory_")));
        assert!(!tools.contains(&"mcp__continuum__fs_apply_patch".to_string()));

        let with_memory = mcp_allowed_tools(true);
        assert!(with_memory
            .iter()
            .any(|tool| tool == "mcp__continuum__memory_vault_search"));
    }
''' + text[closing:])

chat_rs = "apps/desktop/src-tauri/src/chat.rs"
replace_between(
    chat_rs,
    "    let mut tools = Vec::new();",
    "\n    // Prompt injection:",
    r'''    let mut tools = Vec::new();
    let mut executor: Option<Arc<dyn ToolExecutor>> = None;
    let mut mcp = None;
    let mut tools_section = None;
    let include_memory = vault.is_some();

    match conn.kind {
        ProviderKind::ClaudeCli => {
            // Settings and live context are independent from the memory
            // vault. Memory tools are added to the MCP allowlist only when
            // the vault was explicitly enabled and opened successfully.
            if let Some(spec) = chat_tools::mcp_spec(
                memory.vault_dir(),
                &dev_dir,
                &conversation_id,
                include_memory,
            ) {
                mcp = Some(spec);
                if include_memory {
                    tools_section = Some(memory_tools_section(conn.kind));
                }
            }
        }
        ProviderKind::OpenAiCompat | ProviderKind::Anthropic => {
            tools = chat_tools::chat_tool_defs(include_memory);
            executor = Some(match &vault {
                Some(vault) => Arc::new(chat_tools::VaultToolExecutor {
                    vault: vault.clone(),
                }) as Arc<dyn ToolExecutor>,
                None => Arc::new(chat_tools::SettingsToolExecutor) as Arc<dyn ToolExecutor>,
            });
            if include_memory {
                tools_section = Some(memory_tools_section(conn.kind));
            }
        }
    }
''',
)
replace_once(
    chat_rs,
    '''    // Memory tools: when enabled and the vault opens, the chat AI can read
    // and write the memory vault — via the in-process executor for HTTP
    // providers, or via an attached continuum-mcp server for the Claude
    // CLI. A vault that fails to open degrades this send to a tool-less
    // chat (warn + continue), never to a failed send.''',
    '''    // Memory is optional. A disabled or unavailable vault removes only the
    // memory tools; typed settings and privacy-filtered live-context tools stay
    // attached so self-configuration never depends on the memory subsystem.''',
)
replace_once(
    chat_rs,
    "        tool_max_rounds: chat_cfg.memory_tool_max_rounds,",
    "        tool_max_rounds: chat_cfg.memory_tool_max_rounds.max(1),",
)

replace_once(
    "crates/continuum-mcp/src/tools/mod.rs",
    "pub mod repair;\npub mod system;",
    "pub mod repair;\npub mod settings;\npub mod system;",
)
write(
    "crates/continuum-mcp/src/tools/settings.rs",
    r'''//! Typed settings request schemas for `mcp__continuum__settings_*`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct SettingsListRequest {
    /// Optional dotted-path keyword, such as `screen`, `voice`, `privacy`, or `chat`.
    pub query: Option<String>,
    /// Maximum matches (default 80, maximum 250).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SettingsGetRequest {
    /// Exact dotted path returned by `settings_list`.
    pub path: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SettingsSetRequest {
    /// Exact dotted path returned by `settings_list` or `settings_get`.
    pub path: String,
    /// New JSON value. It must match the typed config field.
    pub value: serde_json::Value,
}
''',
)

server = "crates/continuum-mcp/src/server.rs"
replace_once(
    server,
    "use crate::tools::system::{self as systool, NotificationRequest};",
    '''use crate::tools::settings::{
    SettingsGetRequest, SettingsListRequest, SettingsSetRequest,
};
use crate::tools::system::{self as systool, NotificationRequest};''',
)
replace_once(
    server,
    '''#[tool_router]
impl ContinuumMcpServer {
    /// Constructs a new server with all tools registered. Stores are opened''',
    '''#[tool_router]
impl ContinuumMcpServer {
    #[tool(
        description = "Discover Continuum runtime settings by typed dotted path. Returns current/default values, value types, Settings UI locations, and the config path. Secret-like values are redacted."
    )]
    async fn settings_list(
        &self,
        Parameters(req): Parameters<SettingsListRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("settings_list", &req, || async {
            continuum_core::settings::list(
                &self.state.data_dir.join("config.toml"),
                req.query.as_deref(),
                req.limit,
            )
            .map_err(|error| McpError::invalid_params(error, None))
        })
        .await
    }

    #[tool(
        description = "Read one exact Continuum setting by dotted path, including its current/default value and Settings UI location. Secret-like values are redacted."
    )]
    async fn settings_get(
        &self,
        Parameters(req): Parameters<SettingsGetRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("settings_get", &req, || async {
            continuum_core::settings::get(
                &self.state.data_dir.join("config.toml"),
                &req.path,
            )
            .map_err(|error| McpError::invalid_params(error, None))
        })
        .await
    }

    #[tool(
        description = "Change one existing typed Continuum setting after an explicit user request. The full candidate config is validated, unknown sibling keys are preserved, and the previous file is backed up before an atomic write."
    )]
    async fn settings_set(
        &self,
        Parameters(req): Parameters<SettingsSetRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("settings_set", &req, || async {
            continuum_core::settings::set(
                &self.state.data_dir.join("config.toml"),
                &req.path,
                req.value.clone(),
            )
            .map_err(|error| McpError::invalid_params(error, None))
        })
        .await
    }

    /// Constructs a new server with all tools registered. Stores are opened''',
)

replace_once(
    "crates/continuum-mcp/tests/protocol.rs",
    '''    "context_git",
    "context_package",
];''',
    '''    "context_git",
    "context_package",
    // Typed autonomous settings
    "settings_list",
    "settings_get",
    "settings_set",
];''',
)

replace_once(
    "apps/desktop/src-tauri/assets/chat-system-prompt.md",
    r'''- On the Claude CLI path, equivalent config control is available through
  `mcp__continuum__fs_read_file` and `mcp__continuum__fs_apply_patch`. Read the
  current `~/.continuum-dev/config.toml` first, patch only the requested keys,
  and preserve valid TOML. Never use a broad filesystem edit when a precise
  config patch will do.''',
    r'''- On the Claude CLI path, use `mcp__continuum__settings_list`,
  `mcp__continuum__settings_get`, and `mcp__continuum__settings_set`. These call
  the same typed backend, validation, backup, and permission gate as the other
  providers. Never fall back to a generic filesystem mutation for settings.''',
)

print("Autonomous settings hardening patch applied.")
