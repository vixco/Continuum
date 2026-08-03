//! # Orchestrator process spawning
//!
//! Manages the lifecycle of Claude Code child processes for the orchestrator.
//! Each wake spawns a fresh `claude -p` process (see ADR 005).
//!
//! ## Lifecycle per wake
//!
//! 1. Spawn `claude -p --output-format stream-json ...`
//! 2. Write one user message JSON to stdin, close stdin
//! 3. Read events from stdout until `result` event
//! 4. Process exits naturally
//!
//! ## Error handling
//!
//! - If the process fails to spawn: return error, orchestrator enters observe-only mode
//! - If the process exits with non-zero: log error, return partial results
//! - If stdout stream ends without a result event: timeout, kill process

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use super::events::ClaudeEvent;

use crate::config::{continuum_dev_dir, env_or_legacy};

/// Configuration for the orchestrator subprocess.
#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    /// Model to use (default: "claude-opus-4-6").
    pub model: String,
    /// Path to the system prompt file.
    pub system_prompt_path: String,
    /// Maximum time to wait for a response (default: 60 seconds).
    pub timeout_secs: u64,
    /// Whether to use --bare mode (skip hooks, plugins, etc).
    pub bare_mode: bool,
    /// Phase 4: enable the Continuum MCP server. When true, generates an
    /// mcp-config.json at wake time and passes --mcp-config /
    /// --strict-mcp-config to the claude CLI, switches allowedTools to
    /// `mcp__continuum__*`, and lifts permission-mode from "plan" to "default"
    /// (plan mode blocks tool execution). Defaults to true.
    pub mcp_enabled: bool,
    /// Path to the `continuum-mcp` binary. If None, the orchestrator resolves it
    /// at wake time from `CONTINUUM_MCP_BIN` env var, sibling-of-current-exe, or
    /// a PATH lookup.
    pub mcp_server_path: Option<PathBuf>,
    /// Where to write the generated mcp-config.json file. If None, uses
    /// `<data_dir>/mcp-config.json` where data_dir is derived from the
    /// mcp_data_dir field.
    pub mcp_config_path: Option<PathBuf>,
    /// CONTINUUM_DATA_DIR env var passed to the MCP server (for opening semantic
    /// / episodic stores). If None, the MCP server falls back to its own
    /// default (~/.continuum-dev/).
    pub mcp_data_dir: Option<PathBuf>,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-4-6".to_string(),
            system_prompt_path: String::new(),
            timeout_secs: 60,
            bare_mode: true,
            mcp_enabled: true,
            mcp_server_path: None,
            mcp_config_path: None,
            mcp_data_dir: None,
        }
    }
}

/// High-level events emitted by the orchestrator to consumers.
#[derive(Debug, Clone)]
pub enum OrchestratorEvent {
    /// The session has been initialized (process is running).
    SessionReady { session_id: String },
    /// A chunk of text from the assistant's response.
    TextDelta(String),
    /// The assistant's response is complete.
    ResponseComplete {
        full_text: String,
        cost_usd: Option<f64>,
        duration_ms: Option<u64>,
        session_id: Option<String>,
    },
    /// An error occurred.
    Error(String),
}

/// Result of a single orchestrator wake cycle.
#[derive(Debug, Clone)]
pub struct WakeResult {
    /// The full text response from Opus.
    pub response_text: String,
    /// Cost in USD for this wake.
    pub cost_usd: Option<f64>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: Option<u64>,
    /// Session ID (for potential --resume if needed later).
    pub session_id: Option<String>,
    /// Whether the wake completed successfully.
    pub success: bool,
}

/// Spawns a fresh Claude Code process and sends it a single message.
///
/// Returns a stream of [`OrchestratorEvent`]s via the provided callback,
/// and a final [`WakeResult`] when complete.
///
/// This is the core function that implements the "fresh process per wake"
/// lifecycle described in ADR 005.
pub async fn wake_orchestrator(
    config: &OrchestratorConfig,
    user_message: &str,
    mut on_event: impl FnMut(OrchestratorEvent),
) -> Result<WakeResult> {
    info!(
        layer = "orchestrator",
        component = "spawn",
        model = %config.model,
        "Spawning orchestrator process"
    );

    // Build the command.
    let mut cmd = tokio::process::Command::new("claude");
    cmd.arg("--print");
    cmd.arg("--output-format").arg("stream-json");
    cmd.arg("--input-format").arg("stream-json");
    cmd.arg("--verbose");
    cmd.arg("--include-partial-messages");
    cmd.arg("--model").arg(&config.model);
    cmd.arg("--no-session-persistence");

    if config.bare_mode {
        cmd.arg("--bare");
    }

    if !config.system_prompt_path.is_empty() {
        cmd.arg("--append-system-prompt-file")
            .arg(&config.system_prompt_path);
    }

    // Phase 4: attach the Continuum MCP server if enabled.
    //
    // NOTE on --permission-mode: Phase 3 used "plan", which blocks tool
    // execution (it is purely a planning mode). With MCP tools we need
    // "default" so the orchestrator can actually call them. Read the CLI help
    // for --permission-mode before changing this again — a silent switch back
    // to "plan" would make every tool call into a no-op.
    if config.mcp_enabled {
        let mcp_bin = resolve_mcp_binary(config)?;
        let (mcp_config_file, allowed_tools, external_server_count) =
            write_mcp_config(config, &mcp_bin)?;
        cmd.arg("--mcp-config").arg(&mcp_config_file);
        cmd.arg("--strict-mcp-config");
        cmd.arg("--allowedTools").arg(&allowed_tools);
        cmd.arg("--permission-mode").arg("default");
        info!(
            layer = "orchestrator",
            component = "spawn",
            mcp_bin = %mcp_bin.display(),
            mcp_config = %mcp_config_file.display(),
            external_server_count,
            "MCP enabled for this wake"
        );
    } else {
        // Phase 3 compatibility — no tools, plan mode.
        cmd.arg("--allowedTools").arg("");
        cmd.arg("--permission-mode").arg("plan");
    }

    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // If this task is cancelled (e.g. shutdown signal dropped the future)
    // tokio will still try to kill the child rather than leak it as an
    // orphan.
    cmd.kill_on_drop(true);

    // Spawn.
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "claude CLI not found in PATH — install with: npm install -g @anthropic-ai/claude-code"
            )
        } else {
            anyhow::anyhow!("Failed to spawn claude process: {e}")
        }
    })?;

    // Write the user message to stdin and close it.
    {
        let mut stdin = child
            .stdin
            .take()
            .context("Failed to open stdin pipe to claude process")?;

        let message = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": user_message
            }
        });

        let msg_bytes = serde_json::to_vec(&message).context("Failed to serialize user message")?;
        stdin
            .write_all(&msg_bytes)
            .await
            .context("Failed to write to claude stdin")?;
        stdin
            .write_all(b"\n")
            .await
            .context("Failed to write newline")?;
        stdin.flush().await.context("Failed to flush stdin")?;
        // Drop stdin to close the pipe — signals end of input for single-turn.
        drop(stdin);
    }

    debug!(
        layer = "orchestrator",
        component = "spawn",
        "Message sent, reading response stream"
    );

    // Read stdout with timeout.
    let stdout = child.stdout.take().context("Failed to open stdout pipe")?;

    // Capture stderr in background for diagnostics.
    let stderr = child.stderr.take().context("Failed to open stderr pipe")?;
    let stderr_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut output = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            debug!(
                layer = "orchestrator",
                component = "spawn",
                "stderr: {line}"
            );
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&line);
        }
        output
    });

    // Process stdout events.
    let timeout_duration = Duration::from_secs(config.timeout_secs);
    let result = timeout(timeout_duration, async {
        process_event_stream(stdout, &mut on_event).await
    })
    .await;

    let wake_result = match result {
        Ok(Ok(wr)) => wr,
        Ok(Err(e)) => {
            error!(
                layer = "orchestrator",
                component = "spawn",
                error = %e,
                "Event stream processing failed"
            );
            on_event(OrchestratorEvent::Error(e.to_string()));
            WakeResult {
                response_text: String::new(),
                cost_usd: None,
                duration_ms: None,
                session_id: None,
                success: false,
            }
        }
        Err(_) => {
            warn!(
                layer = "orchestrator",
                component = "spawn",
                timeout_secs = config.timeout_secs,
                "Orchestrator timed out — killing process"
            );
            let _ = child.kill().await;
            on_event(OrchestratorEvent::Error(
                "Orchestrator timed out".to_string(),
            ));
            WakeResult {
                response_text: String::new(),
                cost_usd: None,
                duration_ms: None,
                session_id: None,
                success: false,
            }
        }
    };

    // Wait for process exit and stderr.
    let status = child.wait().await;
    let stderr_output = stderr_handle.await.unwrap_or_default();

    if let Ok(status) = status {
        if !status.success() && !wake_result.success {
            warn!(
                layer = "orchestrator",
                component = "spawn",
                exit_code = status.code(),
                stderr = %stderr_output,
                "Claude process exited with error"
            );
        }
    }

    if wake_result.success {
        info!(
            layer = "orchestrator",
            component = "spawn",
            cost_usd = wake_result.cost_usd,
            duration_ms = wake_result.duration_ms,
            response_len = wake_result.response_text.len(),
            "Wake cycle complete"
        );
    }

    Ok(wake_result)
}

/// Locates the `continuum-mcp` binary by checking, in order:
/// (1) explicit `OrchestratorConfig.mcp_server_path`,
/// (2) `CONTINUUM_MCP_BIN` environment variable,
/// (3) sibling of the current executable (`target/debug/continuum-mcp[.exe]`),
/// (4) the bare name `continuum-mcp` (relying on PATH).
fn resolve_mcp_binary(config: &OrchestratorConfig) -> Result<PathBuf> {
    if let Some(p) = &config.mcp_server_path {
        if p.exists() {
            return Ok(p.clone());
        }
        return Err(anyhow::anyhow!(
            "Configured mcp_server_path does not exist: {}",
            p.display()
        ));
    }

    if let Some(env_bin) = env_or_legacy("CONTINUUM_MCP_BIN", "KAIRO_MCP_BIN") {
        let p = PathBuf::from(env_bin);
        if p.exists() {
            return Ok(p);
        }
    }

    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let candidate = dir.join(if cfg!(windows) {
                "continuum-mcp.exe"
            } else {
                "continuum-mcp"
            });
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    // Fall back to the bare name — claude CLI will fail at spawn time if it's
    // not on PATH, which surfaces a clearer error than anything we'd invent.
    Ok(PathBuf::from(if cfg!(windows) {
        "continuum-mcp.exe"
    } else {
        "continuum-mcp"
    }))
}

/// Writes an mcp-config.json pointing at the given continuum-mcp binary and
/// returns its path. A unique suffix is appended per call so parallel wakes
/// cannot clobber each other's config file (e.g. a triage-triggered wake
/// landing at the same millisecond as a hotkey wake).
fn write_mcp_config(
    config: &OrchestratorConfig,
    mcp_bin: &std::path::Path,
) -> Result<(PathBuf, String, usize)> {
    let base_dir = config
        .mcp_config_path
        .as_ref()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .or_else(|| config.mcp_data_dir.clone())
        .or_else(|| Some(continuum_dev_dir()))
        .context("Could not determine directory for mcp-config.json")?;

    std::fs::create_dir_all(&base_dir).with_context(|| {
        format!(
            "Failed to create MCP config parent dir: {}",
            base_dir.display()
        )
    })?;

    // Unique-per-wake filename: pid + monotonic counter + epoch nanos. No
    // cryptographic randomness needed — we just need distinct names for
    // concurrent wakes from the same process.
    use std::sync::atomic::{AtomicU64, Ordering};
    static WAKE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let suffix = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let n = WAKE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        format!("{pid:x}-{n:x}-{nanos:x}")
    };
    let filename = if let Some(explicit) = config.mcp_config_path.as_ref() {
        let stem = explicit
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("mcp-config");
        format!("{stem}-{suffix}.json")
    } else {
        format!("mcp-config-{suffix}.json")
    };
    let config_path = base_dir.join(filename);

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let env_block = if let Some(dir) = &config.mcp_data_dir {
        serde_json::json!({ "CONTINUUM_DATA_DIR": dir.to_string_lossy() })
    } else {
        serde_json::json!({})
    };

    let mut servers = serde_json::Map::new();
    servers.insert(
        "continuum".to_string(),
        serde_json::json!({
            "type": "stdio",
            "command": mcp_bin.to_string_lossy(),
            "args": [],
            "env": env_block,
        }),
    );

    let registry_dir = config
        .mcp_data_dir
        .clone()
        .unwrap_or_else(continuum_dev_dir);
    let mut allowed_tools = vec!["mcp__continuum__*".to_string()];
    let external_allowed = crate::mcp_registry::append_server_configs(&registry_dir, &mut servers)
        .with_context(|| {
            format!(
                "Failed to load installed MCP servers from {}",
                registry_dir.display()
            )
        })?;
    let external_server_count = external_allowed.len();
    allowed_tools.extend(external_allowed);

    let doc = serde_json::json!({ "mcpServers": servers });

    let text = serde_json::to_string_pretty(&doc).context("Failed to serialize mcp-config.json")?;
    std::fs::write(&config_path, text).with_context(|| {
        format!(
            "Failed to write mcp-config.json at {}",
            config_path.display()
        )
    })?;

    Ok((config_path, allowed_tools.join(","), external_server_count))
}

/// Processes the event stream from stdout, collecting text and emitting events.
async fn process_event_stream(
    stdout: tokio::process::ChildStdout,
    on_event: &mut impl FnMut(OrchestratorEvent),
) -> Result<WakeResult> {
    let mut reader = BufReader::new(stdout).lines();
    let mut full_text = String::new();
    let mut session_id = None;
    let mut cost_usd = None;
    let mut duration_ms = None;
    let mut got_result = false;

    while let Some(line) = reader
        .next_line()
        .await
        .context("Failed to read from claude stdout")?
    {
        if line.is_empty() {
            continue;
        }

        let event: ClaudeEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(err) => {
                debug!(
                    layer = "orchestrator",
                    component = "spawn",
                    error = %err,
                    "Skipping unparseable event line"
                );
                continue;
            }
        };

        match &event {
            ClaudeEvent::System(sys) => {
                if let Some(ref sid) = sys.session_id {
                    session_id = Some(sid.clone());
                    on_event(OrchestratorEvent::SessionReady {
                        session_id: sid.clone(),
                    });
                }
            }
            ClaudeEvent::StreamEvent(_) => {
                if let Some(text) = event.as_text_delta() {
                    full_text.push_str(text);
                    on_event(OrchestratorEvent::TextDelta(text.to_string()));
                }
            }
            ClaudeEvent::Result(r) => {
                cost_usd = r.total_cost_usd;
                duration_ms = r.duration_ms;
                if let Some(ref sid) = r.session_id {
                    session_id = Some(sid.clone());
                }
                // Use result text if we didn't accumulate from deltas.
                if full_text.is_empty() {
                    if let Some(ref result_text) = r.result {
                        full_text = result_text.clone();
                    }
                }
                got_result = true;

                on_event(OrchestratorEvent::ResponseComplete {
                    full_text: full_text.clone(),
                    cost_usd,
                    duration_ms,
                    session_id: session_id.clone(),
                });
                break;
            }
            ClaudeEvent::RateLimit(rl) => {
                if let Some(ref info) = rl.rate_limit_info {
                    debug!(
                        layer = "orchestrator",
                        component = "spawn",
                        status = ?info.status,
                        "Rate limit event"
                    );
                }
            }
            _ => {
                // Assistant snapshots, user events, unknown — skip.
            }
        }
    }

    if !got_result {
        warn!(
            layer = "orchestrator",
            component = "spawn",
            "Stream ended without result event"
        );
    }

    Ok(WakeResult {
        response_text: full_text,
        cost_usd,
        duration_ms,
        session_id,
        success: got_result,
    })
}
