//! Official `claude` CLI adapter — the legal path for subscription users
//! (non-negotiable #1: never scrape OAuth tokens). Fresh process per send,
//! mirroring ADR-005; history is replayed inside the single user message.
//!
//! Tool calling works differently here than in the HTTP adapters: when
//! [`ChatRequest::mcp`] is set, the continuum-mcp server is attached via a
//! temporary `--mcp-config` file and the CLI executes tools itself — no
//! [`crate::types::ToolExecutor`] loop. The adapter only translates the
//! CLI's tool traffic (`assistant` snapshots carrying `tool_use` blocks,
//! `user` snapshots carrying `tool_result` blocks) into
//! [`ChatEvent::ToolCall`] / [`ChatEvent::ToolResult`] events — exactly
//! once per tool-use id, the call always before its result. When
//! [`ChatRequest::mcp`] is `None`, behavior is unchanged from the original
//! no-tools design (`--allowedTools ""`, no MCP flags).

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::time::Duration;

use futures_util::stream::BoxStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::error::GatewayError;
use crate::types::*;
use crate::ChatProvider;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Renders the `--mcp-config` document for an [`McpSpec`], mirroring the
/// orchestrator's `write_mcp_config` shape exactly: a single stdio server
/// named `continuum` with the spec's binary, no args, and the spec's env.
pub fn mcp_config_json(spec: &McpSpec) -> serde_json::Value {
    let env: serde_json::Map<String, serde_json::Value> = spec
        .env
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    serde_json::json!({
        "mcpServers": {
            "continuum": {
                "type": "stdio",
                "command": spec.server_command.to_string_lossy(),
                "args": [],
                "env": env,
            }
        }
    })
}

/// Flattens a `tool_result` block's `content` field into the single string
/// carried by [`ChatEvent::ToolResult`]: strings pass through, arrays of
/// `{type:"text",text}` blocks join their texts with newlines (non-text
/// items fall back to their JSON serialization), and anything else —
/// including a missing field, passed as `Null` — serializes to JSON.
pub fn flatten_tool_result_content(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| {
                let is_text = item.get("type").and_then(|t| t.as_str()) == Some("text");
                match item.get("text").and_then(|t| t.as_str()) {
                    Some(text) if is_text => text.to_string(),
                    _ => item.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// Dedupe state for translating the CLI's tool traffic into exactly-once
/// [`ChatEvent`]s. With `--include-partial-messages` the CLI re-emits each
/// `assistant` message as a growing sequence of partial snapshots (a
/// `tool_use` block's `input` may still be empty early on) plus a final
/// snapshot, and may likewise replay `user` tool_result snapshots — so both
/// sides are deduplicated by tool-use id.
///
/// Emission rule for a `tool_use` block (guarantees exactly one ToolCall per
/// id, always before its ToolResult, never dropped):
/// - emit on the first sighting whose `input` is non-null and not an empty
///   object, OR whose enclosing message has `stop_reason == "tool_use"`
///   (the snapshot is final, the input really is `{}`);
/// - otherwise remember the id and name, and if a matching `tool_result`
///   arrives first, emit the ToolCall with `input: {}` right before it.
#[derive(Default)]
struct ToolEventTracker {
    /// Ids whose ToolCall has been emitted.
    emitted_calls: HashSet<String>,
    /// Ids whose ToolResult has been emitted.
    emitted_results: HashSet<String>,
    /// Ids seen only with empty input so far, mapped to their tool name so
    /// a late ToolCall can still be emitted when the result arrives.
    pending_names: HashMap<String, String>,
}

impl ToolEventTracker {
    /// Processes an `assistant` snapshot event, returning the ToolCalls to
    /// emit (in content order).
    fn on_assistant(&mut self, v: &serde_json::Value) -> Vec<ChatEvent> {
        let mut out = Vec::new();
        let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) else {
            return out;
        };
        let is_final_snapshot =
            v.pointer("/message/stop_reason").and_then(|s| s.as_str()) == Some("tool_use");
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let Some(id) = block.get("id").and_then(|i| i.as_str()) else {
                continue;
            };
            if self.emitted_calls.contains(id) {
                continue;
            }
            let name = block
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let input = block
                .get("input")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let input_complete = match &input {
                serde_json::Value::Null => false,
                serde_json::Value::Object(o) => !o.is_empty(),
                _ => true,
            };
            if input_complete || is_final_snapshot {
                self.emitted_calls.insert(id.to_string());
                self.pending_names.remove(id);
                out.push(ChatEvent::ToolCall {
                    id: id.to_string(),
                    name,
                    input: if input.is_null() {
                        serde_json::json!({})
                    } else {
                        input
                    },
                });
            } else {
                // Remember the name so a late emission is still possible.
                let entry = self.pending_names.entry(id.to_string()).or_default();
                if entry.is_empty() {
                    *entry = name;
                }
            }
        }
        out
    }

    /// Processes a `user` snapshot event, returning the events to emit: for
    /// each new `tool_result`, a late ToolCall first if that id never got
    /// one, then the ToolResult itself (`duration_ms` is 0 — the CLI does
    /// not report tool durations; the frontend hides 0).
    fn on_user(&mut self, v: &serde_json::Value) -> Vec<ChatEvent> {
        let mut out = Vec::new();
        let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) else {
            return out;
        };
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str()) else {
                continue;
            };
            if self.emitted_results.contains(id) {
                continue;
            }
            self.emitted_results.insert(id.to_string());
            if !self.emitted_calls.contains(id) {
                if let Some(name) = self.pending_names.remove(id) {
                    self.emitted_calls.insert(id.to_string());
                    out.push(ChatEvent::ToolCall {
                        id: id.to_string(),
                        name,
                        input: serde_json::json!({}),
                    });
                }
            }
            let output = flatten_tool_result_content(
                block.get("content").unwrap_or(&serde_json::Value::Null),
            );
            let is_error = block
                .get("is_error")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            out.push(ChatEvent::ToolResult {
                id: id.to_string(),
                output,
                is_error,
                duration_ms: 0,
            });
        }
        out
    }
}

/// Drives the official `claude` CLI as a subprocess. `binary` is the
/// executable name or path (callers default it to `"claude"`; tests point
/// it at a stub). No OAuth token handling lives here or anywhere else in
/// Continuum — this adapter is the *only* sanctioned way to use a
/// subscription account (see non-negotiable #1 in `CLAUDE.md`).
pub struct ClaudeCliAdapter {
    binary: String,
    /// Idle timeout, not an overall/end-to-end one: it bounds the
    /// `--version` probe outright, and for a streaming send it bounds each
    /// individual `lines.next_line()` read (see the `tokio::time::timeout`
    /// around it in `stream_chat`) — so it resets on every line of CLI
    /// stdout. A long generation that keeps steadily streaming output is
    /// never killed by this; only a CLI that goes silent mid-send is.
    timeout: Duration,
}

impl ClaudeCliAdapter {
    /// Builds an adapter around `binary` (resolved via PATH unless it's a
    /// full path), bounding both `--version` probes and per-line reads from
    /// the streaming child process by `timeout`.
    pub fn new(binary: String, timeout: Duration) -> Self {
        Self { binary, timeout }
    }

    /// Renders the conversation into the single user message the CLI
    /// expects: every turn but the last becomes a `User:`/`Assistant:`
    /// line in a labelled transcript, and the final user message is
    /// appended verbatim as the actual ask.
    fn render_transcript(req: &ChatRequest) -> String {
        let mut out = String::new();
        let n = req.messages.len();
        if n > 1 {
            out.push_str("Previous conversation:\n");
            for m in &req.messages[..n - 1] {
                let label = match m.role {
                    ChatRole::User => "User",
                    ChatRole::Assistant => "Assistant",
                };
                out.push_str(&format!("{label}: {}\n", m.content));
            }
            out.push('\n');
        }
        if let Some(last) = req.messages.last() {
            out.push_str(&last.content);
        }
        out
    }

    /// Maps a spawn/IO error to a [`GatewayError`], recognizing "binary not
    /// on PATH" specifically so callers can surface the install hint.
    fn map_spawn_error(e: std::io::Error) -> GatewayError {
        if e.kind() == std::io::ErrorKind::NotFound {
            GatewayError::CliNotFound
        } else {
            GatewayError::BadResponse {
                detail: e.to_string(),
            }
        }
    }

    /// Builds the `tokio::process::Command` for a streaming send: the base
    /// CLI flags are fixed (non-negotiable #3 governs config surfaced to
    /// the user, not the wire protocol used to talk to the CLI itself),
    /// stdio is piped, and the child is killed if the returned `Child` is
    /// dropped without an explicit wait — this crate never wants an
    /// orphaned `claude` process outliving its stream.
    ///
    /// `mcp_config_path` is the temp file holding [`mcp_config_json`] when
    /// `req.mcp` is set (the caller creates it and keeps it alive for the
    /// child's lifetime): it adds `--mcp-config <path> --strict-mcp-config`
    /// and puts the spec's tool names in `--allowedTools`. Without it, the
    /// flags are exactly the original no-tools set (`--allowedTools ""`).
    fn build_command(
        &self,
        req: &ChatRequest,
        prompt_path: &std::path::Path,
        mcp_config_path: Option<&std::path::Path>,
    ) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.arg("--print")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--include-partial-messages")
            .arg("--model")
            .arg(&req.model)
            .arg("--append-system-prompt-file")
            .arg(prompt_path);
        match (req.mcp.as_ref(), mcp_config_path) {
            (Some(spec), Some(config_path)) => {
                cmd.arg("--mcp-config")
                    .arg(config_path)
                    .arg("--strict-mcp-config")
                    .arg("--allowedTools")
                    .arg(spec.allowed_tools.join(","));
            }
            _ => {
                cmd.arg("--allowedTools").arg("");
            }
        }
        cmd.arg("--permission-mode")
            .arg("default")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
}

#[async_trait::async_trait]
impl ChatProvider for ClaudeCliAdapter {
    async fn test_connection(&self) -> Result<ConnectionTestReport, GatewayError> {
        let started = std::time::Instant::now();
        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        let out = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_| GatewayError::Timeout)?
            .map_err(Self::map_spawn_error)?;
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(ConnectionTestReport {
            ok: out.status.success(),
            latency_ms: started.elapsed().as_millis() as u64,
            models: self.list_models().await.unwrap_or_default(),
            detail: version,
        })
    }

    async fn list_models(&self) -> Result<Vec<String>, GatewayError> {
        // The CLI accepts any model id the account can use; this list is a
        // starting point for the picker, which also allows free-text entry
        // (frontend Task 12).
        Ok(vec![
            "claude-opus-4-6".into(),
            "claude-sonnet-4-6".into(),
            "claude-haiku-4-5".into(),
        ])
    }

    async fn stream_chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ChatEvent>, GatewayError> {
        // System prompt travels via file (the CLI's canonical mechanism —
        // see the `--append-system-prompt-file` pattern in CLAUDE.md).
        let prompt_file = tempfile::NamedTempFile::new()
            .and_then(|f| {
                std::fs::write(f.path(), &req.system)?;
                Ok(f)
            })
            .map_err(|e| GatewayError::BadResponse {
                detail: format!("temp prompt file: {e}"),
            })?;

        // MCP attachment travels the same way: a temp `--mcp-config` file
        // pointing at the continuum-mcp server binary. The CLI executes the
        // tools itself; this adapter only relays the resulting tool events.
        let mcp_config_file = match req.mcp.as_ref() {
            Some(spec) => Some(
                tempfile::NamedTempFile::new()
                    .and_then(|f| {
                        std::fs::write(f.path(), mcp_config_json(spec).to_string())?;
                        Ok(f)
                    })
                    .map_err(|e| GatewayError::BadResponse {
                        detail: format!("temp mcp config file: {e}"),
                    })?,
            ),
            None => None,
        };

        let mut cmd = self.build_command(
            &req,
            prompt_file.path(),
            mcp_config_file.as_ref().map(|f| f.path()),
        );
        let mut child = cmd.spawn().map_err(Self::map_spawn_error)?;

        let payload = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": Self::render_transcript(&req)}
        });
        if let Some(mut stdin) = child.stdin.take() {
            let line = format!("{payload}\n");
            let _ = stdin.write_all(line.as_bytes()).await;
            // stdin drops here → EOF, a single user turn (the CLI still
            // loops internally for MCP tool calls before its final result).
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GatewayError::BadResponse {
                detail: "no stdout".into(),
            })?;
        let timeout = self.timeout;
        let (tx, rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
        tokio::spawn(async move {
            // Keep the temp files alive until the process (and this task)
            // ends — the CLI reads them once at startup but they must
            // still exist on disk while that happens.
            let _keep_alive = (prompt_file, mcp_config_file);
            let mut tracker = ToolEventTracker::default();
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = child.kill().await;
                        let _ = tx.send(ChatEvent::Error {
                            message: GatewayError::Cancelled.user_message(),
                            retryable: false,
                        }).await;
                        return;
                    }
                    res = tokio::time::timeout(timeout, lines.next_line()) => match res {
                        Err(_) => {
                            let _ = child.kill().await;
                            let _ = tx.send(ChatEvent::Error {
                                message: GatewayError::Timeout.user_message(),
                                retryable: true,
                            }).await;
                            return;
                        }
                        Ok(Ok(Some(l))) => l,
                        Ok(Ok(None)) | Ok(Err(_)) => break,
                    }
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("stream_event") => {
                        if v.pointer("/event/type").and_then(|t| t.as_str())
                            == Some("content_block_delta")
                            && v.pointer("/event/delta/type").and_then(|t| t.as_str())
                                == Some("text_delta")
                        {
                            if let Some(text) =
                                v.pointer("/event/delta/text").and_then(|t| t.as_str())
                            {
                                let _ = tx
                                    .send(ChatEvent::Delta {
                                        text: text.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                    Some("assistant") => {
                        // Partial/final snapshots of the assistant message;
                        // the tracker turns new tool_use blocks into
                        // exactly-once ToolCall events.
                        for ev in tracker.on_assistant(&v) {
                            let _ = tx.send(ev).await;
                        }
                    }
                    Some("user") => {
                        // Tool results echoed back by the CLI mid-turn.
                        for ev in tracker.on_user(&v) {
                            let _ = tx.send(ev).await;
                        }
                    }
                    Some("result") => {
                        let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
                        if is_error {
                            let detail = v
                                .get("result")
                                .and_then(|r| r.as_str())
                                .unwrap_or("unknown error");
                            let lowered = detail.to_lowercase();
                            let message = if lowered.contains("logged in")
                                || lowered.contains("authenticat")
                            {
                                GatewayError::CliNotLoggedIn.user_message()
                            } else {
                                format!("Claude CLI error: {detail}")
                            };
                            let _ = tx
                                .send(ChatEvent::Error {
                                    message,
                                    retryable: false,
                                })
                                .await;
                        } else {
                            let usage = TokenUsage {
                                input_tokens: v
                                    .pointer("/usage/input_tokens")
                                    .and_then(|x| x.as_u64()),
                                output_tokens: v
                                    .pointer("/usage/output_tokens")
                                    .and_then(|x| x.as_u64()),
                            };
                            let stop_reason = v
                                .get("stop_reason")
                                .and_then(|s| s.as_str())
                                .map(String::from);
                            let _ = tx.send(ChatEvent::Done { usage, stop_reason }).await;
                        }
                        let _ = child.wait().await;
                        return;
                    }
                    _ => {}
                }
            }
            // stdout closed without a result event.
            let _ = child.wait().await;
            let _ = tx
                .send(ChatEvent::Error {
                    message: "Claude CLI exited without a result".into(),
                    retryable: true,
                })
                .await;
        });
        Ok(super::mpsc_stream(rx))
    }
}
