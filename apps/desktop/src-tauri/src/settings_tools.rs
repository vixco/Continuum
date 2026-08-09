//! Typed settings access for desktop chat tools.
//!
//! The model never edits arbitrary files through this module. It can only read
//! and replace paths that already exist in the typed `ContinuumConfig`, and the
//! whole candidate config is deserialized + validated before it is persisted.

use std::collections::BTreeMap;

use continuum_core::config::{continuum_dev_dir, load_config, ContinuumConfig};
use serde_json::{json, Value};

const LIST_DEFAULT: usize = 80;
const LIST_MAX: usize = 250;

pub fn list(input: &Value) -> Result<String, String> {
    let path = continuum_dev_dir().join("config.toml");
    let cfg = load_config(&path).map_err(|error| format!("could not load settings: {error}"))?;
    let defaults = ContinuumConfig::default();
    let current = serde_json::to_value(cfg).map_err(|error| error.to_string())?;
    let default_value = serde_json::to_value(defaults).map_err(|error| error.to_string())?;

    let mut current_flat = BTreeMap::new();
    let mut default_flat = BTreeMap::new();
    flatten("", &current, &mut current_flat);
    flatten("", &default_value, &mut default_flat);

    let query = input
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(LIST_DEFAULT)
        .clamp(1, LIST_MAX);

    let mut entries = Vec::new();
    for (setting_path, value) in current_flat {
        if !query.is_empty() && !setting_path.to_ascii_lowercase().contains(&query) {
            continue;
        }
        let default = default_flat.get(&setting_path).cloned().unwrap_or(Value::Null);
        entries.push(json!({
            "path": setting_path,
            "current": redact_if_sensitive(&setting_path, value),
            "default": redact_if_sensitive(&setting_path, default),
        }));
        if entries.len() >= limit {
            break;
        }
    }

    serde_json::to_string(&json!({
        "config_path": path,
        "ui_location": "Settings",
        "count": entries.len(),
        "limit": limit,
        "entries": entries,
        "note": "Use settings_get before settings_set when the intended path or value is not obvious. Most runtime changes apply after the background runtime restarts."
    }))
    .map_err(|error| error.to_string())
}

pub fn get(input: &Value) -> Result<String, String> {
    let setting_path = string_field(input, "path")?;
    let config_path = continuum_dev_dir().join("config.toml");
    let cfg = load_config(&config_path).map_err(|error| format!("could not load settings: {error}"))?;
    let defaults = ContinuumConfig::default();
    let current = serde_json::to_value(cfg).map_err(|error| error.to_string())?;
    let default_value = serde_json::to_value(defaults).map_err(|error| error.to_string())?;
    let value = value_at_path(&current, setting_path)
        .ok_or_else(|| format!("unknown setting path `{setting_path}`; call settings_list to discover valid paths"))?
        .clone();
    let default = value_at_path(&default_value, setting_path)
        .cloned()
        .unwrap_or(Value::Null);

    serde_json::to_string(&json!({
        "path": setting_path,
        "current": redact_if_sensitive(setting_path, value),
        "default": redact_if_sensitive(setting_path, default),
        "config_path": config_path,
        "ui_location": ui_location(setting_path),
        "restart_recommended": true,
    }))
    .map_err(|error| error.to_string())
}

pub fn set(input: &Value) -> Result<String, String> {
    let setting_path = string_field(input, "path")?;
    let requested = input
        .get("value")
        .ok_or_else(|| "missing required field `value`".to_string())?
        .clone();

    let config_path = continuum_dev_dir().join("config.toml");
    let cfg = load_config(&config_path).map_err(|error| format!("could not load settings: {error}"))?;
    let mut candidate_json = serde_json::to_value(cfg).map_err(|error| error.to_string())?;
    let previous = replace_existing_path(&mut candidate_json, setting_path, requested.clone())?;

    let candidate: ContinuumConfig = serde_json::from_value(candidate_json)
        .map_err(|error| format!("invalid value for `{setting_path}`: {error}"))?;
    candidate
        .resources
        .validate()
        .map_err(|error| format!("invalid [resources] settings: {error}"))?;

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create settings directory: {error}"))?;
    }
    if config_path.exists() {
        let backup = config_path.with_extension("toml.bak");
        std::fs::copy(&config_path, &backup)
            .map_err(|error| format!("could not create config backup {}: {error}", backup.display()))?;
    }
    let rendered = toml::to_string_pretty(&candidate)
        .map_err(|error| format!("could not serialize settings: {error}"))?;
    std::fs::write(&config_path, rendered)
        .map_err(|error| format!("could not persist settings: {error}"))?;

    serde_json::to_string(&json!({
        "updated": true,
        "path": setting_path,
        "previous": redact_if_sensitive(setting_path, previous),
        "current": redact_if_sensitive(setting_path, requested),
        "config_path": config_path,
        "ui_location": ui_location(setting_path),
        "restart_recommended": true,
        "backup_path": config_path.with_extension("toml.bak"),
    }))
    .map_err(|error| error.to_string())
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
            if items.is_empty() {
                output.insert(prefix.to_string(), value.clone());
            } else {
                for (index, child) in items.iter().enumerate() {
                    let path = format!("{prefix}.{index}");
                    flatten(&path, child, output);
                }
            }
        }
        _ => {
            output.insert(prefix.to_string(), value.clone());
        }
    }
}

fn value_at_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

fn replace_existing_path(root: &mut Value, path: &str, replacement: Value) -> Result<Value, String> {
    let segments = path
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return Err("setting path cannot be empty".into());
    }

    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        current = match current {
            Value::Object(map) => map.get_mut(*segment),
            Value::Array(items) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get_mut(index)),
            _ => None,
        }
        .ok_or_else(|| format!("unknown setting path `{path}`; call settings_list to discover valid paths"))?;
    }

    let final_segment = segments[segments.len() - 1];
    match current {
        Value::Object(map) => {
            let slot = map
                .get_mut(final_segment)
                .ok_or_else(|| format!("unknown setting path `{path}`; call settings_list to discover valid paths"))?;
            Ok(std::mem::replace(slot, replacement))
        }
        Value::Array(items) => {
            let index = final_segment
                .parse::<usize>()
                .map_err(|_| format!("invalid array index in setting path `{path}`"))?;
            let slot = items
                .get_mut(index)
                .ok_or_else(|| format!("unknown setting path `{path}`; call settings_list to discover valid paths"))?;
            Ok(std::mem::replace(slot, replacement))
        }
        _ => Err(format!("setting path `{path}` does not point to a configurable value")),
    }
}

fn string_field<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing required string field `{key}`"))
}

fn is_sensitive_path(path: &str) -> bool {
    path.split('.').any(|segment| {
        let segment = segment.to_ascii_lowercase();
        segment.contains("secret")
            || segment.contains("password")
            || segment.contains("token")
            || segment == "api_key"
            || segment.ends_with("_api_key")
    })
}

fn redact_if_sensitive(path: &str, value: Value) -> Value {
    if is_sensitive_path(path) && !value.is_null() {
        Value::String("<redacted>".into())
    } else {
        value
    }
}

fn ui_location(path: &str) -> &'static str {
    match path.split('.').next().unwrap_or_default() {
        "resources" | "vision" | "screen" | "audio" | "voice" | "tts" => "Settings → Resources / Voice",
        "github" => "Settings → Integrations → GitHub",
        "chat" => "Chat / Settings",
        "privacy" | "context" | "context_tools" | "file_watcher" | "process_watcher" => "Context / Settings",
        "memory" => "Memory / Settings",
        _ => "Settings",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_scalar_and_rejects_unknown_path() {
        let mut value = json!({"screen":{"enabled":true},"items":[{"x":1}]});
        let old = replace_existing_path(&mut value, "screen.enabled", json!(false)).unwrap();
        assert_eq!(old, json!(true));
        assert_eq!(value["screen"]["enabled"], json!(false));
        assert!(replace_existing_path(&mut value, "screen.nope", json!(1)).is_err());
    }

    #[test]
    fn supports_array_indices() {
        let mut value = json!({"items":[{"x":1}]});
        replace_existing_path(&mut value, "items.0.x", json!(2)).unwrap();
        assert_eq!(value["items"][0]["x"], json!(2));
    }

    #[test]
    fn sensitive_values_are_redacted() {
        assert_eq!(redact_if_sensitive("foo.api_key", json!("abc")), json!("<redacted>"));
        assert_eq!(redact_if_sensitive("screen.enabled", json!(true)), json!(true));
    }
}
