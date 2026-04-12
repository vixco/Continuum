//! # Tool-call audit
//!
//! Every MCP tool invocation records an [`EventKind::ToolCall`] episodic event
//! so future wakes can retrieve what the orchestrator has already asked for.
//! Audit failures are logged but never propagated — a failing audit must not
//! mask a successful (or failed) tool call.
//!
//! ## Sanitization rules
//!
//! - Map keys matching `/password|secret|token|apikey|auth/i` have their
//!   values replaced with `[REDACTED]` before logging.
//! - String values longer than 500 chars are truncated with a marker.
//! - The final summary is capped at 200 chars.

use chrono::Utc;
use kairo_core::memory::episodic::{EpisodicEvent, EventKind};
use serde_json::{Map, Value};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::server::KairoMcpServer;

/// Records a tool call to episodic memory.
///
/// Spawned on the current tokio runtime as a **detached** task so the tool
/// call can return to the client immediately; opening the episodic store for
/// the first time triggers fastembed model loading (200 ms–30 s depending on
/// cache state) and must not block the tool response. Audit writes may be
/// lost if the MCP server process exits before the task completes — audit is
/// intentionally best-effort.
pub(crate) fn record_tool_call(
    server: &KairoMcpServer,
    tool: &str,
    args: &Value,
    result_summary: &str,
) {
    let server = server.clone();
    let tool = tool.to_string();
    let sanitized = sanitize(args);
    let args_str = serde_json::to_string(&sanitized).unwrap_or_else(|_| "{}".into());
    let summary_trunc = truncate(result_summary, 200);

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
        };

        match server.episodic().await {
            Ok(mut ep) => {
                if let Err(e) = ep.insert_event(&event).await {
                    warn!(
                        layer = "mcp",
                        component = "audit",
                        tool = %tool,
                        error = %e,
                        "Failed to persist tool-call audit event"
                    );
                } else {
                    debug!(
                        layer = "mcp",
                        component = "audit",
                        tool = %tool,
                        "Tool-call audited"
                    );
                }
            }
            Err(e) => {
                warn!(
                    layer = "mcp",
                    component = "audit",
                    tool = %tool,
                    error = %e,
                    "Episodic store unavailable — audit skipped"
                );
            }
        }
    });
}

/// Recursively sanitizes a JSON value: redacts sensitive keys and truncates
/// over-long strings.
fn sanitize(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(sanitize_map(m)),
        Value::Array(a) => Value::Array(a.iter().map(sanitize).collect()),
        Value::String(s) if s.chars().count() > 500 => {
            let truncated: String = s.chars().take(500).collect();
            let rest = s.chars().count() - 500;
            Value::String(format!("{truncated}...[+{rest} chars]"))
        }
        other => other.clone(),
    }
}

fn sanitize_map(m: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::with_capacity(m.len());
    for (k, val) in m {
        if is_sensitive_key(k) {
            out.insert(k.clone(), Value::String("[REDACTED]".to_string()));
        } else {
            out.insert(k.clone(), sanitize(val));
        }
    }
    out
}

fn is_sensitive_key(k: &str) -> bool {
    let lower = k.to_lowercase();
    // Match on substring because keys like "api_key", "accessToken", "authHeader" should all catch.
    const NEEDLES: &[&str] = &["password", "secret", "token", "apikey", "api_key", "auth"];
    NEEDLES.iter().any(|n| lower.contains(n))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_redacts_password_keys() {
        let v = json!({"username": "alice", "password": "hunter2"});
        let s = sanitize(&v);
        assert_eq!(s["username"], json!("alice"));
        assert_eq!(s["password"], json!("[REDACTED]"));
    }

    #[test]
    fn sanitize_redacts_nested_auth_keys() {
        let v = json!({"headers": {"Authorization": "Bearer xyz"}, "body": {"msg": "ok"}});
        let s = sanitize(&v);
        assert_eq!(s["headers"]["Authorization"], json!("[REDACTED]"));
        assert_eq!(s["body"]["msg"], json!("ok"));
    }

    #[test]
    fn sanitize_redacts_mixed_case_token_keys() {
        let v = json!({"accessToken": "abc", "api_key": "def", "Normal": "keep"});
        let s = sanitize(&v);
        assert_eq!(s["accessToken"], json!("[REDACTED]"));
        assert_eq!(s["api_key"], json!("[REDACTED]"));
        assert_eq!(s["Normal"], json!("keep"));
    }

    #[test]
    fn sanitize_truncates_long_strings() {
        let long = "a".repeat(700);
        let v = json!({"text": long});
        let s = sanitize(&v);
        let out = s["text"].as_str().unwrap();
        assert!(out.len() < 700);
        assert!(out.contains("[+200 chars]"));
    }

    #[test]
    fn truncate_handles_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_caps_long_string() {
        let t = truncate(&"x".repeat(300), 100);
        assert_eq!(t.chars().count(), 101); // 100 + ellipsis
    }
}
