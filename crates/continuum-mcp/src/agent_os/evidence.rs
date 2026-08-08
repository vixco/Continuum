use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use super::types::{AuthorizationReport, EvidenceQueryRequest, RiskLevel};

const MAX_ACTIVE_LOG_BYTES: u64 = 20 * 1024 * 1024;
const MAX_QUERY_LIMIT: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceEvent {
    pub id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub run_id: Option<String>,
    pub tool: String,
    pub capability: String,
    pub risk: RiskLevel,
    pub authorization: String,
    pub outcome: String,
    pub duration_ms: u64,
    pub input: Value,
    #[serde(default)]
    pub result_summary: Value,
    #[serde(default)]
    pub error: Option<String>,
}

pub struct EvidenceStore {
    root: PathBuf,
    active_path: PathBuf,
    gate: Mutex<()>,
}

pub struct EvidenceDraft<'a> {
    pub run_id: Option<&'a str>,
    pub tool: &'a str,
    pub capability: &'a str,
    pub risk: RiskLevel,
    pub authorization: Option<&'a AuthorizationReport>,
    pub outcome: &'a str,
    pub duration: Duration,
    pub input: &'a Value,
    pub result_summary: Value,
    pub error: Option<&'a str>,
}

impl EvidenceStore {
    pub fn new(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("Failed to create evidence directory {}", root.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
            active_path: root.join("evidence.jsonl"),
            gate: Mutex::new(()),
        })
    }

    pub fn active_path(&self) -> &Path {
        &self.active_path
    }

    pub async fn record(&self, draft: EvidenceDraft<'_>) -> Result<String> {
        let _guard = self.gate.lock().await;
        self.rotate_if_needed()?;
        let id = format!("ev_{}", uuid::Uuid::new_v4().simple());
        let event = EvidenceEvent {
            id: id.clone(),
            timestamp: chrono::Utc::now(),
            run_id: draft.run_id.map(str::to_owned),
            tool: draft.tool.to_string(),
            capability: draft.capability.to_string(),
            risk: draft.risk,
            authorization: draft
                .authorization
                .map(|report| report.source.clone())
                .unwrap_or_else(|| "not_authorized".to_string()),
            outcome: draft.outcome.to_string(),
            duration_ms: draft.duration.as_millis().min(u128::from(u64::MAX)) as u64,
            input: sanitize_value(draft.tool, draft.input),
            result_summary: compact_value(
                &sanitize_value(draft.tool, &draft.result_summary),
                6,
                80,
                4000,
            ),
            error: draft.error.map(|value| truncate(value, 2000)),
        };
        let payload = serde_json::to_vec(&event)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.active_path)
            .with_context(|| {
                format!(
                    "Failed to open evidence log for append at {}",
                    self.active_path.display()
                )
            })?;
        file.write_all(&payload)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_data()?;
        Ok(id)
    }

    pub async fn query(&self, request: &EvidenceQueryRequest) -> Result<Vec<EvidenceEvent>> {
        let _guard = self.gate.lock().await;
        let limit = request.limit.clamp(1, MAX_QUERY_LIMIT);
        let mut paths = std::fs::read_dir(&self.root)
            .with_context(|| format!("Failed to read evidence directory {}", self.root.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name == "evidence.jsonl"
                            || (name.starts_with("evidence-") && name.ends_with(".jsonl"))
                    })
            })
            .collect::<Vec<_>>();
        // Timestamped rotated logs sort before the active file lexically, so
        // explicitly put the active file first and then newest rotations.
        paths.sort_by(|left, right| {
            let left_active = left == &self.active_path;
            let right_active = right == &self.active_path;
            right_active
                .cmp(&left_active)
                .then_with(|| right.file_name().cmp(&left.file_name()))
        });

        let mut matches = Vec::with_capacity(limit);
        for path in paths {
            let body = match std::fs::read_to_string(&path) {
                Ok(body) => body,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("Failed to read {}", path.display()))
                }
            };
            for line in body.lines().rev() {
                if line.trim().is_empty() {
                    continue;
                }
                let event: EvidenceEvent = match serde_json::from_str(line) {
                    Ok(event) => event,
                    Err(error) => {
                        tracing::warn!(
                            layer = "agent_os",
                            component = "evidence",
                            error = %error,
                            path = %path.display(),
                            "Skipping malformed evidence line"
                        );
                        continue;
                    }
                };
                if request
                    .run_id
                    .as_deref()
                    .is_some_and(|run_id| event.run_id.as_deref() != Some(run_id))
                {
                    continue;
                }
                if request
                    .tool
                    .as_deref()
                    .is_some_and(|tool| event.tool != tool)
                {
                    continue;
                }
                if request
                    .outcome
                    .as_deref()
                    .is_some_and(|outcome| event.outcome != outcome)
                {
                    continue;
                }
                matches.push(event);
                if matches.len() >= limit {
                    return Ok(matches);
                }
            }
        }
        Ok(matches)
    }

    fn rotate_if_needed(&self) -> Result<()> {
        let size = self
            .active_path
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        if size < MAX_ACTIVE_LOG_BYTES {
            return Ok(());
        }
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let rotated = self.root.join(format!(
            "evidence-{stamp}-{}.jsonl",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::rename(&self.active_path, &rotated).with_context(|| {
            format!(
                "Failed to rotate evidence log {} to {}",
                self.active_path.display(),
                rotated.display()
            )
        })?;
        Ok(())
    }
}

pub fn sanitize_value(tool: &str, value: &Value) -> Value {
    let mut sanitized = redact_secrets(value, 0);
    if tool == "computer_type" {
        redact_text_field(&mut sanitized);
    }
    redact_nested_type_steps(&mut sanitized);
    sanitized
}

fn redact_nested_type_steps(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let is_type_step = object
                .get("action")
                .and_then(Value::as_str)
                .is_some_and(|action| action == "computer_type");
            if is_type_step {
                if let Some(arguments) = object.get_mut("arguments") {
                    redact_text_field(arguments);
                }
            }
            for child in object.values_mut() {
                redact_nested_type_steps(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_nested_type_steps(item);
            }
        }
        _ => {}
    }
}

fn redact_text_field(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        if let Some(text) = object.remove("text") {
            let chars = text
                .as_str()
                .map(|value| value.chars().count())
                .unwrap_or(0);
            object.insert(
                "text".to_string(),
                serde_json::json!({ "redacted": true, "chars": chars }),
            );
        }
    }
}

fn redact_secrets(value: &Value, depth: usize) -> Value {
    if depth > 12 {
        return Value::String("[depth-limited]".to_string());
    }
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, child) in map {
                let normalized = key.to_ascii_lowercase();
                let secret = [
                    "api_key",
                    "apikey",
                    "token",
                    "secret",
                    "password",
                    "authorization",
                    "credential",
                    "cookie",
                ]
                .iter()
                .any(|needle| normalized.contains(needle));
                out.insert(
                    key.clone(),
                    if secret {
                        Value::String("[redacted]".to_string())
                    } else {
                        redact_secrets(child, depth + 1)
                    },
                );
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(200)
                .map(|item| redact_secrets(item, depth + 1))
                .collect(),
        ),
        Value::String(text) => Value::String(truncate(text, 4000)),
        other => other.clone(),
    }
}

pub fn compact_value(
    value: &Value,
    max_depth: usize,
    max_items: usize,
    max_string_chars: usize,
) -> Value {
    fn visit(
        value: &Value,
        depth: usize,
        max_depth: usize,
        max_items: usize,
        max_string_chars: usize,
    ) -> Value {
        if depth >= max_depth {
            return Value::String("[depth-limited]".to_string());
        }
        match value {
            Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (index, (key, child)) in map.iter().enumerate() {
                    if index >= max_items {
                        out.insert(
                            "_truncated".to_string(),
                            Value::String(format!("{} more fields", map.len() - max_items)),
                        );
                        break;
                    }
                    out.insert(
                        key.clone(),
                        visit(child, depth + 1, max_depth, max_items, max_string_chars),
                    );
                }
                Value::Object(out)
            }
            Value::Array(items) => {
                let mut out: Vec<Value> = items
                    .iter()
                    .take(max_items)
                    .map(|child| visit(child, depth + 1, max_depth, max_items, max_string_chars))
                    .collect();
                if items.len() > max_items {
                    out.push(Value::String(format!(
                        "[{} more items]",
                        items.len() - max_items
                    )));
                }
                Value::Array(out)
            }
            Value::String(text) => Value::String(truncate(text, max_string_chars)),
            other => other.clone(),
        }
    }
    visit(value, 0, max_depth, max_items, max_string_chars)
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut out: String = value.chars().take(max_chars).collect();
    if value.chars().count() > max_chars {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_keys_and_typed_text_are_redacted() {
        let value = serde_json::json!({
            "text": "very private message",
            "nested": { "api_key": "secret", "safe": "value" }
        });
        let sanitized = sanitize_value("computer_type", &value);
        assert_eq!(sanitized["nested"]["api_key"], "[redacted]");
        assert_eq!(sanitized["text"]["redacted"], true);
        assert_eq!(sanitized["text"]["chars"], 20);
    }

    #[test]
    fn nested_plan_type_text_is_redacted() {
        let value = serde_json::json!({
            "steps": [{"action":"computer_type", "arguments":{"text":"secret payload"}}]
        });
        let sanitized = sanitize_value("agent_run_plan", &value);
        assert_eq!(sanitized["steps"][0]["arguments"]["text"]["redacted"], true);
        assert_eq!(sanitized["steps"][0]["arguments"]["text"]["chars"], 14);
        assert!(!serde_json::to_string(&sanitized)
            .expect("json")
            .contains("secret payload"));
    }

    #[test]
    fn compact_value_limits_large_arrays() {
        let value = Value::Array((0..100).map(Value::from).collect());
        let compact = compact_value(&value, 4, 5, 100);
        assert_eq!(compact.as_array().expect("array").len(), 6);
    }
}
