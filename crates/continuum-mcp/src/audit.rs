//! # Tool-call audit
//!
//! Every MCP tool invocation records a compact [`EventKind::ToolCall`] event.
//! Audit is useful for continuity, but it is not a second copy of user data:
//! payload-bearing fields are reduced to shape metadata before persistence and
//! privacy-sensitive tool families are marked `LocalOnly` so cloud-bound memory
//! retrieval cannot replay them.

use chrono::Utc;
use continuum_core::memory::episodic::{EpisodicEvent, EventKind};
use serde_json::{Map, Value};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::server::ContinuumMcpServer;

const MAX_DEPTH: usize = 8;
const MAX_ITEMS: usize = 80;
const MAX_STRING_CHARS: usize = 300;

/// Records a tool call to episodic memory without blocking the tool response.
/// The persisted payload is deliberately lossy and never contains full message
/// bodies, clipboard text, worker prompts, memory note bodies, or URL queries.
pub(crate) fn record_tool_call(
    server: &ContinuumMcpServer,
    tool: &str,
    args: &Value,
    result_summary: &str,
) {
    let server = server.clone();
    let tool = tool.to_string();
    let sanitized = sanitize_for_tool(&tool, args);
    let args_str = serde_json::to_string(&sanitized).unwrap_or_else(|_| "{}".into());
    let summary_trunc = truncate(result_summary, 160);
    let sensitivity = audit_sensitivity(&tool);

    tokio::spawn(async move {
        let content = format!("tool={tool} args={args_str} result={summary_trunc}");
        let event = EpisodicEvent {
            id: Uuid::new_v4().to_string(),
            ts: Utc::now(),
            kind: EventKind::ToolCall,
            summary: content,
            importance: 0.3,
            tags: vec!["tool_call".to_string(), tool.clone()],
            source_frame_id: None,
            project: None,
            sensitivity,
        };

        match server.episodic().await {
            Ok(mut episodic) => {
                if let Err(error) = episodic.insert_event(&event).await {
                    warn!(
                        layer = "mcp",
                        component = "audit",
                        tool = %tool,
                        error = %error,
                        "Failed to persist tool-call audit event"
                    );
                } else {
                    debug!(
                        layer = "mcp",
                        component = "audit",
                        tool = %tool,
                        "Tool-call audit persisted with minimized arguments"
                    );
                }
            }
            Err(error) => {
                warn!(
                    layer = "mcp",
                    component = "audit",
                    tool = %tool,
                    error = %error,
                    "Episodic store unavailable — audit skipped"
                );
            }
        }
    });
}

fn audit_sensitivity(tool: &str) -> continuum_core::memory::events::EventSensitivity {
    use continuum_core::memory::events::EventSensitivity;

    if tool.starts_with("memory_")
        || tool.starts_with("fs_")
        || tool.starts_with("git_")
        || tool.starts_with("terminal_")
        || tool.starts_with("ide_")
        || tool.starts_with("browser_")
        || tool.starts_with("windows_ui_")
        || tool.starts_with("github_")
        || tool.starts_with("task_")
        || tool.starts_with("evidence_")
        || tool.starts_with("workers_")
        || matches!(
            tool,
            "system_clipboard_get" | "system_notification" | "web_fetch"
        )
    {
        EventSensitivity::LocalOnly
    } else {
        EventSensitivity::CloudAllowed
    }
}

fn sanitize_for_tool(tool: &str, value: &Value) -> Value {
    let mut sanitized = sanitize(value, 0);
    match tool {
        "memory_set_fact" => redact_fields(&mut sanitized, &["value", "source"]),
        "memory_vault_save" => {
            redact_fields(&mut sanitized, &["body", "source_ref", "relations", "tags"])
        }
        "memory_vault_search" | "memory_query_episodic" | "context_search" => {
            redact_fields(&mut sanitized, &["query"])
        }
        "system_notification" => redact_fields(&mut sanitized, &["body"]),
        "workers_spawn_worker" => redact_fields(
            &mut sanitized,
            &["task", "cwd", "allowed_tools", "requested_by", "skills"],
        ),
        "terminal_run" => redact_fields(&mut sanitized, &["args", "cwd", "env"]),
        "fs_create_file" => redact_fields(&mut sanitized, &["content"]),
        "fs_apply_patch" => redact_fields(&mut sanitized, &["old_text", "new_text"]),
        "browser_fill" | "windows_ui_set_focused_value" => {
            redact_fields(&mut sanitized, &["value"])
        }
        "github_create_issue" | "github_comment_issue" | "github_create_pull_request" => {
            redact_fields(&mut sanitized, &["body"])
        }
        "fs_read_file" | "fs_list_dir" => redact_path_field(&mut sanitized),
        "web_fetch" => minimize_url_field(&mut sanitized),
        _ => {}
    }
    sanitized
}

fn sanitize(value: &Value, depth: usize) -> Value {
    if depth >= MAX_DEPTH {
        return Value::String("[depth-limited]".to_string());
    }
    match value {
        Value::Object(map) => {
            let mut output = Map::new();
            for (index, (key, child)) in map.iter().enumerate() {
                if index >= MAX_ITEMS {
                    output.insert(
                        "_truncated".to_string(),
                        Value::String(format!("{} more fields", map.len() - MAX_ITEMS)),
                    );
                    break;
                }
                output.insert(
                    key.clone(),
                    if is_sensitive_key(key) {
                        redacted_shape(child)
                    } else {
                        sanitize(child, depth + 1)
                    },
                );
            }
            Value::Object(output)
        }
        Value::Array(items) => {
            let mut output = items
                .iter()
                .take(MAX_ITEMS)
                .map(|child| sanitize(child, depth + 1))
                .collect::<Vec<_>>();
            if items.len() > MAX_ITEMS {
                output.push(Value::String(format!(
                    "[{} more items]",
                    items.len() - MAX_ITEMS
                )));
            }
            Value::Array(output)
        }
        Value::String(value) => Value::String(truncate(value, MAX_STRING_CHARS)),
        other => other.clone(),
    }
}

fn redact_fields(value: &mut Value, fields: &[&str]) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    for field in fields {
        if let Some(current) = object.remove(*field) {
            object.insert((*field).to_string(), redacted_shape(&current));
        }
    }
}

fn redact_path_field(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if let Some(path) = object.remove("path") {
        let file_name = path
            .as_str()
            .and_then(|value| std::path::Path::new(value).file_name())
            .map(|value| value.to_string_lossy().into_owned());
        object.insert(
            "path".to_string(),
            serde_json::json!({
                "redacted": true,
                "file_name": file_name,
                "chars": path.as_str().map(str::len).unwrap_or_default()
            }),
        );
    }
}

fn minimize_url_field(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let Some(url) = object.remove("url") else {
        return;
    };
    let minimized = url
        .as_str()
        .and_then(|raw| url::Url::parse(raw).ok())
        .map(|parsed| {
            serde_json::json!({
                "scheme": parsed.scheme(),
                "host": parsed.host_str(),
                "port": parsed.port_or_known_default(),
                "path_segments": parsed.path_segments().map(|segments| segments.count()).unwrap_or_default(),
                "query_present": parsed.query().is_some()
            })
        })
        .unwrap_or_else(|| redacted_shape(&url));
    object.insert("url".to_string(), minimized);
}

fn redacted_shape(value: &Value) -> Value {
    match value {
        Value::String(value) => {
            serde_json::json!({"redacted": true, "kind": "string", "chars": value.chars().count()})
        }
        Value::Array(items) => {
            serde_json::json!({"redacted": true, "kind": "array", "items": items.len()})
        }
        Value::Object(object) => {
            let mut keys = object.keys().take(40).cloned().collect::<Vec<_>>();
            keys.sort();
            serde_json::json!({
                "redacted": true,
                "kind": "object",
                "fields": object.len(),
                "keys": keys
            })
        }
        Value::Null => serde_json::json!({"redacted": true, "kind": "null"}),
        Value::Bool(_) => serde_json::json!({"redacted": true, "kind": "boolean"}),
        Value::Number(_) => serde_json::json!({"redacted": true, "kind": "number"}),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "password",
        "secret",
        "token",
        "apikey",
        "api_key",
        "auth",
        "authorization",
        "credential",
        "cookie",
        "private_key",
        "oauth",
        "signed_url",
        "session_id",
        "mcp_url",
        "body",
        "content",
        "old_text",
        "new_text",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle))
}

fn truncate(value: &str, max: usize) -> String {
    let mut output: String = value.chars().take(max).collect();
    if value.chars().count() > max {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn memory_body_is_never_persisted() {
        let value = json!({
            "title": "Private note",
            "body": "highly private body",
            "source_ref": "mail:private@example.com"
        });
        let sanitized = sanitize_for_tool("memory_vault_save", &value);
        let rendered = serde_json::to_string(&sanitized).expect("json");
        assert!(!rendered.contains("highly private body"));
        assert!(!rendered.contains("private@example.com"));
        assert_eq!(sanitized["body"]["redacted"], true);
    }

    #[test]
    fn worker_prompt_and_cwd_are_minimized() {
        let value = json!({
            "task": "fix the confidential customer issue",
            "cwd": "C:/Users/Alice/Customers/Secret",
            "model": "power"
        });
        let sanitized = sanitize_for_tool("workers_spawn_worker", &value);
        let rendered = serde_json::to_string(&sanitized).expect("json");
        assert!(!rendered.contains("confidential customer"));
        assert!(!rendered.contains("Customers/Secret"));
        assert_eq!(sanitized["model"], "power");
    }

    #[test]
    fn web_urls_drop_paths_and_queries() {
        let value = json!({"url":"https://example.com/private/customer?token=abc"});
        let sanitized = sanitize_for_tool("web_fetch", &value);
        assert_eq!(sanitized["url"]["host"], "example.com");
        assert_eq!(sanitized["url"]["query_present"], true);
        let rendered = serde_json::to_string(&sanitized).expect("json");
        assert!(!rendered.contains("customer"));
        assert!(!rendered.contains("abc"));
    }

    #[test]
    fn sensitive_tool_families_are_local_only() {
        use continuum_core::memory::events::EventSensitivity;
        assert_eq!(
            audit_sensitivity("memory_vault_get"),
            EventSensitivity::LocalOnly
        );
        assert_eq!(
            audit_sensitivity("fs_read_file"),
            EventSensitivity::LocalOnly
        );
        assert_eq!(
            audit_sensitivity("context_session"),
            EventSensitivity::CloudAllowed
        );
    }
}
