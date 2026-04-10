//! # Phase 0 — Hello World
//!
//! Proves that a Rust process can spawn `claude` in headless mode, send it a
//! prompt via stdin, and parse the streamed JSON response in real time.
//!
//! ## Usage
//!
//! ```bash
//! cargo run --example hello_world -p kairo-core
//! ```
//!
//! ## Prerequisites
//!
//! - Claude Code CLI installed: `npm install -g @anthropic-ai/claude-code`
//! - Authenticated: `claude login`

use std::io::Write as _;
use std::process::Stdio;

use anyhow::{Context, Result};
use kairo_core::orchestrator::events::{ApiEvent, ClaudeEvent, Delta};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info, warn};

/// The prompt we send to Claude Code.
const PROMPT: &str = "What is 2+2? Respond in one sentence.";

/// The model to use.
const MODEL: &str = "claude-opus-4-6";

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing for structured logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    info!(
        layer = "orchestrator",
        component = "hello_world",
        "Phase 0: spawning Claude Code in headless mode"
    );

    // Spawn the claude CLI process.
    let mut child = tokio::process::Command::new("claude")
        .arg("--print")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--include-partial-messages")
        .arg("--model")
        .arg(MODEL)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "claude CLI not found in PATH — install with: \
                     npm install -g @anthropic-ai/claude-code"
                )
            } else {
                anyhow::anyhow!("failed to spawn claude process: {e}")
            }
        })?;

    // Write the JSON user message to stdin, then close it.
    {
        let mut stdin = child
            .stdin
            .take()
            .context("failed to open stdin pipe to claude process")?;

        let message = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": PROMPT
            }
        });

        let msg_bytes = serde_json::to_vec(&message).context("failed to serialize user message")?;
        stdin
            .write_all(&msg_bytes)
            .await
            .context("failed to write to claude stdin")?;
        stdin
            .write_all(b"\n")
            .await
            .context("failed to write newline to claude stdin")?;
        stdin
            .flush()
            .await
            .context("failed to flush claude stdin")?;
        // Drop stdin to close the pipe — signals end of input.
        drop(stdin);
    }

    info!(
        layer = "orchestrator",
        component = "hello_world",
        prompt = PROMPT,
        "sent prompt, reading streamed response..."
    );

    // Read stdout line by line.
    let stdout = child
        .stdout
        .take()
        .context("failed to open stdout pipe from claude process")?;
    let mut reader = BufReader::new(stdout).lines();

    // Also capture stderr for error diagnosis.
    let stderr = child
        .stderr
        .take()
        .context("failed to open stderr pipe from claude process")?;
    let stderr_handle = tokio::spawn(async move {
        let mut stderr_reader = BufReader::new(stderr).lines();
        let mut stderr_output = String::new();
        while let Ok(Some(line)) = stderr_reader.next_line().await {
            debug!(
                layer = "orchestrator",
                component = "hello_world",
                "stderr: {line}"
            );
            if !stderr_output.is_empty() {
                stderr_output.push('\n');
            }
            stderr_output.push_str(&line);
        }
        stderr_output
    });

    // Track whether we've started printing response text (for newline after streaming).
    let mut printed_text = false;

    // Parse and handle each event.
    while let Some(line) = reader
        .next_line()
        .await
        .context("failed to read line from claude stdout")?
    {
        if line.is_empty() {
            continue;
        }

        let event: ClaudeEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(err) => {
                warn!(
                    layer = "orchestrator",
                    component = "hello_world",
                    error = %err,
                    "failed to parse event JSON, skipping line"
                );
                debug!(
                    layer = "orchestrator",
                    component = "hello_world",
                    raw = %line,
                    "unparseable line content"
                );
                continue;
            }
        };

        match &event {
            ClaudeEvent::System(sys) => {
                if let Some(ref session_id) = sys.session_id {
                    info!(
                        layer = "orchestrator",
                        component = "hello_world",
                        session_id = %session_id,
                        model = ?sys.model,
                        version = ?sys.claude_code_version,
                        tools_count = sys.tools.len(),
                        "session initialized"
                    );
                }
            }

            ClaudeEvent::StreamEvent(se) => {
                match &se.event {
                    ApiEvent::ContentBlockDelta {
                        delta: Some(Delta::TextDelta { text }),
                        ..
                    } => {
                        // Print text as it streams — this is the live output.
                        print!("{text}");
                        std::io::stdout()
                            .flush()
                            .context("failed to flush stdout")?;
                        printed_text = true;
                    }
                    ApiEvent::MessageDelta { delta, .. } => {
                        if let Some(d) = delta {
                            if let Some(ref reason) = d.stop_reason {
                                debug!(
                                    layer = "orchestrator",
                                    component = "hello_world",
                                    stop_reason = %reason,
                                    "message completed"
                                );
                            }
                        }
                    }
                    _ => {
                        // Other stream events (message_start, content_block_start/stop,
                        // message_stop) are structural — log at debug level.
                        debug!(
                            layer = "orchestrator",
                            component = "hello_world",
                            event = %event,
                            "stream event"
                        );
                    }
                }
            }

            ClaudeEvent::Assistant(_) => {
                // Partial message snapshots — useful for multi-turn, skip in hello world.
                debug!(
                    layer = "orchestrator",
                    component = "hello_world",
                    "assistant message snapshot"
                );
            }

            ClaudeEvent::RateLimit(rl) => {
                if let Some(ref info) = rl.rate_limit_info {
                    debug!(
                        layer = "orchestrator",
                        component = "hello_world",
                        status = ?info.status,
                        rate_limit_type = ?info.rate_limit_type,
                        "rate limit status"
                    );
                }
            }

            ClaudeEvent::Result(r) => {
                // Ensure we end the streamed text on a new line.
                if printed_text {
                    println!();
                }
                println!();

                // Print the summary.
                println!("--- Session Complete ---");

                if let Some(ref session_id) = r.session_id {
                    println!("Session ID:   {session_id}");
                }
                if let Some(duration) = r.duration_ms {
                    println!("Duration:     {duration} ms");
                }
                if let Some(cost) = r.total_cost_usd {
                    println!("Total cost:   ${cost:.6}");
                }
                if let Some(ref subtype) = r.subtype {
                    println!("Status:       {subtype}");
                }
                if let Some(turns) = r.num_turns {
                    println!("Turns:        {turns}");
                }

                if r.is_error {
                    error!(
                        layer = "orchestrator",
                        component = "hello_world",
                        result = ?r.result,
                        "session ended with error"
                    );
                    // Check for common auth errors.
                    if let Some(ref result_text) = r.result {
                        if result_text.contains("not authenticated")
                            || result_text.contains("login")
                            || result_text.contains("API key")
                        {
                            eprintln!(
                                "\nError: claude CLI is not authenticated — run: claude login"
                            );
                        }
                    }
                } else {
                    info!(
                        layer = "orchestrator",
                        component = "hello_world",
                        duration_ms = r.duration_ms,
                        cost_usd = r.total_cost_usd,
                        "session completed successfully"
                    );
                }
            }

            ClaudeEvent::User(_) => {
                debug!(
                    layer = "orchestrator",
                    component = "hello_world",
                    "user message event"
                );
            }

            ClaudeEvent::Unknown => {
                warn!(
                    layer = "orchestrator",
                    component = "hello_world",
                    "received unknown event type"
                );
            }
        }
    }

    // Wait for the process to exit.
    let status = child
        .wait()
        .await
        .context("failed to wait for claude process")?;

    // Collect stderr for diagnostics.
    let stderr_output = stderr_handle.await.context("stderr reader task panicked")?;

    if !status.success() {
        let code = status.code().unwrap_or(-1);

        // Check for common failure modes.
        if stderr_output.contains("not authenticated") || stderr_output.contains("login") {
            anyhow::bail!("claude CLI is not authenticated — run: claude login");
        }

        anyhow::bail!("claude process exited with code {code}.\nstderr: {stderr_output}");
    }

    Ok(())
}
