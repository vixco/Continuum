//! Official `claude` CLI adapter — the legal path for subscription users
//! (non-negotiable #1: never scrape OAuth tokens). Fresh process per send,
//! mirroring ADR-005; history is replayed inside the single user message.

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

/// Drives the official `claude` CLI as a subprocess. `binary` is the
/// executable name or path (callers default it to `"claude"`; tests point
/// it at a stub). No OAuth token handling lives here or anywhere else in
/// Continuum — this adapter is the *only* sanctioned way to use a
/// subscription account (see non-negotiable #1 in `CLAUDE.md`).
pub struct ClaudeCliAdapter {
    binary: String,
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

    /// Builds the `tokio::process::Command` for a streaming send: the CLI
    /// flags are fixed (non-negotiable #3 governs config surfaced to the
    /// user, not the wire protocol used to talk to the CLI itself), stdio
    /// is piped, and the child is killed if the returned `Child` is
    /// dropped without an explicit wait — this crate never wants an
    /// orphaned `claude` process outliving its stream.
    fn build_command(
        &self,
        req: &ChatRequest,
        prompt_path: &std::path::Path,
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
            .arg(prompt_path)
            .arg("--allowedTools")
            .arg("")
            .arg("--permission-mode")
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

        let mut cmd = self.build_command(&req, prompt_file.path());
        let mut child = cmd.spawn().map_err(Self::map_spawn_error)?;

        let payload = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": Self::render_transcript(&req)}
        });
        if let Some(mut stdin) = child.stdin.take() {
            let line = format!("{payload}\n");
            let _ = stdin.write_all(line.as_bytes()).await;
            // stdin drops here → EOF, single-turn.
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
            // Keep the temp file alive until the process (and this task)
            // ends — the CLI reads it once at startup but the file must
            // still exist on disk while that happens.
            let _keep_alive = prompt_file;
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
