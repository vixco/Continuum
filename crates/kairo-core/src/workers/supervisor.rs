//! # Worker supervisor
//!
//! Spawns a single `claude --print --output-format stream-json` subprocess,
//! feeds it the worker's task, and parses the streamed events into a
//! sequence of [`WorkerEvent`]s. The caller (usually [`super::pool::WorkerPool`])
//! turns those events into snapshot updates, progress reports, and an audit
//! trail in episodic memory.
//!
//! ## What the supervisor does *not* do
//!
//! - It does not decide which model to use — the pool passes an explicit
//!   model id (see [`super::model_select`]).
//! - It does not retry on failure — if claude exits non-zero, the worker is
//!   marked `Failed` and it is up to the orchestrator to decide whether to
//!   retry.
//! - It does not write to disk directly — every observable output goes
//!   through [`WorkerEvent`] callbacks, so the pool stays in control of
//!   where state ends up.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio::time::{timeout_at, Instant};
use tracing::{debug, info, warn};

use crate::orchestrator::events::{ApiEvent, ClaudeEvent, ContentBlock, Delta};

use super::types::{WorkerOutcome, WorkerSpec, WorkerStatus};

/// Inputs the supervisor needs to spawn one worker. Built by the pool so the
/// supervisor does not have to touch config or the skills loader directly.
#[derive(Debug, Clone)]
pub struct SupervisorInput {
    pub id: String,
    pub spec: WorkerSpec,
    pub model: String,
    pub claude_bin: String,
    pub system_prompt_path: Option<PathBuf>,
    pub mcp_config_path: Option<PathBuf>,
    pub allowed_tools: String,
    pub timeout_secs: u64,
    /// When true, the supervisor skips the actual claude spawn and synthesises
    /// a benign stream-json transcript from the task text. Used by
    /// `worker_demo` and integration tests where a real Anthropic API round
    /// trip is undesirable.
    pub dry_run: bool,
}

/// Streaming events the supervisor surfaces to the pool.
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    SessionReady { session_id: String },
    TextDelta(String),
    ToolCall { name: String, input: Value },
    Progress { fraction: f32, note: String },
    Log(String),
    Finished(WorkerOutcome),
}

/// Runs one worker to completion.
///
/// `cancel` can be triggered at any time — the supervisor kills the child
/// process and returns a `Cancelled` outcome.
pub async fn run_worker(
    input: SupervisorInput,
    mut on_event: impl FnMut(WorkerEvent),
    mut cancel: oneshot::Receiver<()>,
) -> WorkerOutcome {
    if input.dry_run {
        return run_dry(input, &mut on_event).await;
    }

    let outcome = match spawn_and_stream(&input, &mut on_event, &mut cancel).await {
        Ok(o) => o,
        Err(e) => {
            on_event(WorkerEvent::Log(format!("worker spawn failed: {e}")));
            WorkerOutcome::failed(e.to_string())
        }
    };

    on_event(WorkerEvent::Finished(outcome.clone()));
    outcome
}

async fn spawn_and_stream(
    input: &SupervisorInput,
    on_event: &mut impl FnMut(WorkerEvent),
    cancel: &mut oneshot::Receiver<()>,
) -> Result<WorkerOutcome> {
    info!(
        layer = "workers",
        component = "supervisor",
        worker_id = %input.id,
        model = %input.model,
        cwd = %input.spec.cwd.display(),
        "Spawning worker"
    );

    std::fs::create_dir_all(&input.spec.cwd).with_context(|| {
        format!(
            "Worker cwd does not exist and could not be created: {}",
            input.spec.cwd.display()
        )
    })?;

    let mut cmd = tokio::process::Command::new(&input.claude_bin);
    cmd.arg("--print");
    cmd.arg("--output-format").arg("stream-json");
    cmd.arg("--input-format").arg("stream-json");
    cmd.arg("--verbose");
    cmd.arg("--include-partial-messages");
    cmd.arg("--model").arg(&input.model);
    cmd.arg("--no-session-persistence");

    if let Some(prompt) = &input.system_prompt_path {
        cmd.arg("--append-system-prompt-file").arg(prompt);
    }
    if let Some(mcp) = &input.mcp_config_path {
        cmd.arg("--mcp-config").arg(mcp);
        cmd.arg("--strict-mcp-config");
    }
    cmd.arg("--allowedTools").arg(&input.allowed_tools);
    cmd.arg("--permission-mode").arg("default");
    cmd.current_dir(&input.spec.cwd);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "Failed to spawn `{}` for worker {} — is the claude CLI installed and on PATH?",
            input.claude_bin, input.id
        )
    })?;

    // Feed the task to stdin in stream-json format.
    {
        let mut stdin = child
            .stdin
            .take()
            .context("Failed to open stdin pipe to worker process")?;
        let msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": input.spec.task }
        });
        let bytes = serde_json::to_vec(&msg).context("Failed to serialize worker user message")?;
        stdin.write_all(&bytes).await.context("stdin write")?;
        stdin.write_all(b"\n").await.context("stdin newline")?;
        stdin.flush().await.context("stdin flush")?;
    }

    let stdout = child
        .stdout
        .take()
        .context("Failed to open stdout pipe from worker")?;
    let stderr = child
        .stderr
        .take()
        .context("Failed to open stderr pipe from worker")?;

    // Drain stderr in parallel to avoid backpressure; collect for diagnostics.
    let stderr_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut out = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&line);
        }
        out
    });

    let deadline = Instant::now() + Duration::from_secs(input.timeout_secs.max(1));

    let mut state = StreamState::default();
    let mut reader = BufReader::new(stdout).lines();

    loop {
        tokio::select! {
            // User cancellation — kill the process and exit.
            _ = &mut *cancel => {
                warn!(
                    layer = "workers",
                    component = "supervisor",
                    worker_id = %input.id,
                    "Worker cancelled by caller"
                );
                let _ = child.kill().await;
                let _ = stderr_handle.await;
                return Ok(WorkerOutcome {
                    status: WorkerStatus::Cancelled,
                    result_text: state.text.clone(),
                    session_id: state.session_id.clone(),
                    cost_usd: state.cost_usd,
                    duration_ms: state.duration_ms,
                    tool_calls: state.tool_calls,
                    error: Some("cancelled".into()),
                });
            }
            // Timeout — kill and report.
            line = timeout_at(deadline, reader.next_line()) => {
                let line = match line {
                    Ok(Ok(Some(line))) => line,
                    Ok(Ok(None)) => {
                        // stdout closed cleanly without a result event.
                        debug!(
                            layer = "workers",
                            component = "supervisor",
                            worker_id = %input.id,
                            "stdout EOF without result event"
                        );
                        let _ = child.wait().await;
                        let _ = stderr_handle.await;
                        let outcome = state.into_outcome(
                            WorkerStatus::Failed,
                            Some("Worker stream ended without a result event".into()),
                        );
                        return Ok(outcome);
                    }
                    Ok(Err(e)) => {
                        let _ = child.kill().await;
                        let _ = stderr_handle.await;
                        return Ok(state.into_outcome(
                            WorkerStatus::Failed,
                            Some(format!("stdout read error: {e}")),
                        ));
                    }
                    Err(_elapsed) => {
                        warn!(
                            layer = "workers",
                            component = "supervisor",
                            worker_id = %input.id,
                            timeout_secs = input.timeout_secs,
                            "Worker timed out — killing process"
                        );
                        let _ = child.kill().await;
                        let _ = stderr_handle.await;
                        return Ok(state.into_outcome(
                            WorkerStatus::TimedOut,
                            Some(format!("timed out after {} s", input.timeout_secs)),
                        ));
                    }
                };

                if line.is_empty() {
                    continue;
                }

                let event: ClaudeEvent = match serde_json::from_str(&line) {
                    Ok(e) => e,
                    Err(e) => {
                        debug!(
                            layer = "workers",
                            component = "supervisor",
                            worker_id = %input.id,
                            error = %e,
                            "Skipping unparseable stream-json line"
                        );
                        continue;
                    }
                };

                if handle_event(event, &mut state, on_event, &input.id).await? {
                    // Result event seen — read any trailing lines but don't block forever.
                    // Child will exit on its own once stdout drains.
                    break;
                }
            }
        }
    }

    let _ = child.wait().await;
    let _ = stderr_handle.await;
    Ok(state.into_outcome(WorkerStatus::Completed, None))
}

/// Handles one parsed event. Returns true when the final `result` event has
/// been seen, signalling the loop to exit.
async fn handle_event(
    event: ClaudeEvent,
    state: &mut StreamState,
    on_event: &mut impl FnMut(WorkerEvent),
    worker_id: &str,
) -> Result<bool> {
    match event {
        ClaudeEvent::System(sys) => {
            if let Some(sid) = sys.session_id {
                state.session_id = Some(sid.clone());
                on_event(WorkerEvent::SessionReady { session_id: sid });
            }
        }
        ClaudeEvent::StreamEvent(se) => match se.event {
            ApiEvent::ContentBlockStart {
                content_block: Some(ContentBlock::ToolUse { name, input, .. }),
                ..
            } => {
                state.tool_calls += 1;
                on_event(WorkerEvent::ToolCall {
                    name: name.clone(),
                    input: input.clone(),
                });
                on_event(WorkerEvent::Log(format!("tool: {name}")));
            }
            ApiEvent::ContentBlockDelta {
                delta: Some(Delta::TextDelta { text }),
                ..
            } => {
                if !text.is_empty() {
                    state.text.push_str(&text);
                    // A rough progress heuristic so the dashboard spinner
                    // moves: scale text length into [0, 0.9]. The real
                    // "done" signal is the result event flipping to 1.0.
                    let approx = (state.text.len().min(4000) as f32) / 4000.0 * 0.9;
                    on_event(WorkerEvent::TextDelta(text.clone()));
                    if approx > state.last_progress + 0.05 {
                        state.last_progress = approx;
                        on_event(WorkerEvent::Progress {
                            fraction: approx,
                            note: "streaming".into(),
                        });
                    }
                }
            }
            _ => {}
        },
        ClaudeEvent::Assistant(_) | ClaudeEvent::User(_) | ClaudeEvent::RateLimit(_) => {}
        ClaudeEvent::Result(r) => {
            state.cost_usd = r.total_cost_usd;
            state.duration_ms = r.duration_ms;
            if state.session_id.is_none() {
                state.session_id = r.session_id.clone();
            }
            if state.text.is_empty() {
                if let Some(t) = r.result.clone() {
                    state.text = t;
                }
            }
            state.is_error = r.is_error;
            state.error_message = r
                .extra
                .get("error")
                .and_then(|v| v.as_str().map(str::to_string));
            debug!(
                layer = "workers",
                component = "supervisor",
                worker_id = %worker_id,
                cost_usd = r.total_cost_usd,
                duration_ms = r.duration_ms,
                "Worker result event"
            );
            on_event(WorkerEvent::Progress {
                fraction: 1.0,
                note: if r.is_error { "error" } else { "done" }.into(),
            });
            return Ok(true);
        }
        ClaudeEvent::Unknown => {}
    }
    Ok(false)
}

#[derive(Default)]
struct StreamState {
    text: String,
    session_id: Option<String>,
    cost_usd: Option<f64>,
    duration_ms: Option<u64>,
    tool_calls: u32,
    last_progress: f32,
    is_error: bool,
    error_message: Option<String>,
}

impl StreamState {
    fn into_outcome(self, status: WorkerStatus, fallback_error: Option<String>) -> WorkerOutcome {
        let (final_status, error) = if self.is_error {
            (
                WorkerStatus::Failed,
                Some(
                    self.error_message
                        .unwrap_or_else(|| "claude reported is_error".into()),
                ),
            )
        } else {
            (status, fallback_error)
        };
        WorkerOutcome {
            status: final_status,
            result_text: self.text,
            session_id: self.session_id,
            cost_usd: self.cost_usd,
            duration_ms: self.duration_ms,
            tool_calls: self.tool_calls,
            error,
        }
    }
}

/// Dry-run path used by tests and `worker_demo`: synthesises a fake transcript
/// so the rest of the pipeline can be exercised without spawning claude.
async fn run_dry(input: SupervisorInput, on_event: &mut impl FnMut(WorkerEvent)) -> WorkerOutcome {
    let session = format!("dryrun-{}-{}", input.id, Utc::now().timestamp_millis());
    on_event(WorkerEvent::SessionReady {
        session_id: session.clone(),
    });
    let preview = input.spec.task.chars().take(80).collect::<String>();
    let response = format!(
        "[dry-run] Worker would run: {preview}\nModel selected: {}\n",
        input.model
    );
    for chunk in response.split_inclusive(' ') {
        on_event(WorkerEvent::TextDelta(chunk.to_string()));
        // Yield so the pool's snapshot updater has room to run.
        tokio::task::yield_now().await;
    }
    on_event(WorkerEvent::Progress {
        fraction: 1.0,
        note: "dry-run done".into(),
    });

    let outcome = WorkerOutcome {
        status: WorkerStatus::Completed,
        result_text: response,
        session_id: Some(session),
        cost_usd: Some(0.0),
        duration_ms: Some(1),
        tool_calls: 0,
        error: None,
    };
    on_event(WorkerEvent::Finished(outcome.clone()));
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workers::types::{WorkerSpec, WorkerStatus};
    use std::path::PathBuf;

    fn demo_input() -> SupervisorInput {
        SupervisorInput {
            id: "w1".into(),
            spec: WorkerSpec::new("Summarize docs/workers.md", PathBuf::from(".")),
            model: "claude-sonnet-4-6".into(),
            claude_bin: "claude".into(),
            system_prompt_path: None,
            mcp_config_path: None,
            allowed_tools: "Read".into(),
            timeout_secs: 30,
            dry_run: true,
        }
    }

    #[tokio::test]
    async fn dry_run_emits_events_and_completes() {
        let mut events = Vec::new();
        let (_tx, rx) = oneshot::channel();
        let outcome = run_worker(demo_input(), |e| events.push(format!("{e:?}")), rx).await;
        assert_eq!(outcome.status, WorkerStatus::Completed);
        assert!(outcome.result_text.contains("dry-run"));
        assert!(events.iter().any(|s| s.contains("SessionReady")));
        assert!(events.iter().any(|s| s.contains("Finished")));
    }

    #[tokio::test]
    async fn cancel_before_spawn_returns_cancelled_when_not_dry() {
        // Claude won't exist in test env — spawn will fail with NotFound,
        // which should surface as Failed (not Cancelled) since the error
        // happens before we reach the select loop.
        let mut input = demo_input();
        input.dry_run = false;
        input.claude_bin = "definitely-not-a-real-binary".into();
        let (_tx, rx) = oneshot::channel();
        let outcome = run_worker(input, |_| {}, rx).await;
        assert_eq!(outcome.status, WorkerStatus::Failed);
        assert!(outcome.error.is_some());
    }
}
