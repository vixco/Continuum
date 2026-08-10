//! Desktop wrappers around the shared typed settings backend.

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
    serialize(continuum_core::settings::set(&config_path(), path, value)?)
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
