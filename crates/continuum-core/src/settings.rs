//! Typed, autonomous access to Continuum's runtime settings.
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
    let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT);
    let settings = current_paths
        .into_iter()
        .filter(|(path, _)| query.is_empty() || path.to_ascii_lowercase().contains(&query))
        .take(limit)
        .map(|(path, value)| {
            let sensitive = is_sensitive_path(&path);
            let default = default_paths.get(&path).cloned().unwrap_or(Value::Null);
            let kind = value_type(&value);
            let current = redact_value(&path, value);
            let default = redact_value(&path, default);
            let ui_location = ui_location(&path);
            json!({
                "path": path,
                "value": current,
                "default": default,
                "value_type": kind,
                "sensitive": sensitive,
                "mutable": true,
                "ui_location": ui_location,
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
    let value = lookup(&current, setting_path).ok_or_else(|| {
        format!("Unknown setting path `{setting_path}`. Use settings_list first.")
    })?;
    let default = lookup(&defaults, setting_path)
        .cloned()
        .unwrap_or(Value::Null);
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
pub fn set(config_path: &Path, setting_path: &str, requested: Value) -> Result<Value, String> {
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
        format!("The updated config could not be reloaded and was restored from backup: {error}")
    })?;
    let verified_json = serde_json::to_value(verified).map_err(|error| error.to_string())?;
    let verified_value = lookup(&verified_json, setting_path)
        .cloned()
        .unwrap_or(Value::Null);
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

fn replace_existing_path(
    root: &mut Value,
    path: &str,
    replacement: Value,
) -> Result<Value, String> {
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
        "events" | "file_watcher" | "process_watcher" | "session_state" | "context_package"
        | "continuation" | "context_tools" => "Context engine",
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
