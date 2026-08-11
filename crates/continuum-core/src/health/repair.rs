//! # Repair agent
//!
//! Spawns a dedicated Claude Code (Opus) session with:
//! - Working directory: Continuum install folder (the git root)
//! - A custom system prompt from `prompts/repair-agent-system.md`
//! - Access to the Continuum MCP server (plus repair-specific namespaces once
//!   [`super::repair_tools`] is wired into `continuum-mcp`)
//!
//! The session receives a structured repair context (last 500 log lines,
//! all component statuses, any stack traces, config snapshot) and streams
//! its response back to the dashboard Health tab via the [`RepairEvent`]
//! callback.
//!
//! Writing the context to `~/.continuum-dev/repair-context.md` also gives the
//! user something to look at if the agent spawn itself fails.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::ContinuumConfig;
use crate::hardware;
use crate::logs::{LogBuffer, LogEntry, LogFilter};
use crate::state::{ComponentHealth, StateHandle};
use uuid::Uuid;

const REPAIR_GRANTS_DIR: &str = "repair-grants";
const REPAIR_AUDIT_FILE: &str = "repair-audit.ndjson";

/// Input for a repair run.
pub struct RepairInput<'a> {
    pub dev_dir: &'a Path,
    pub backups_dir: &'a Path,
    pub repo_root: &'a Path,
    pub config: &'a ContinuumConfig,
    pub state: &'a StateHandle,
    pub logs: &'a LogBuffer,
    pub components: Vec<ComponentHealth>,
    /// Exact component names authorized by the user's live preview.
    pub allowed_components: Vec<String>,
    pub user_reason: Option<String>,
}

/// Short-lived capability consumed by a dedicated repair MCP process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairSessionGrant {
    pub token: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub allowed_components: Vec<String>,
    /// Components for which the session may queue a restart. Kept separate
    /// from read-only tests so unsupported restart paths fail closed.
    #[serde(default)]
    pub allowed_restart_components: Vec<String>,
    /// Whether the legacy escalation-intent tool is usable. The Health flow
    /// keeps this false because no dashboard consumer exists yet.
    #[serde(default)]
    pub allow_escalation_intent: bool,
    pub allow_model_reinstall: bool,
    pub allow_config_rollback: bool,
}

/// Streamed output from the repair session — what the dashboard renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepairEvent {
    Started {
        ts: DateTime<Utc>,
    },
    ContextWritten {
        path: String,
    },
    BackupCreated {
        path: String,
        bytes: u64,
        verified: bool,
    },
    ActionResult {
        action: String,
        success: bool,
        detail: String,
    },
    AssistantDelta {
        text: String,
    },
    ToolCall {
        name: String,
    },
    ToolResult {
        name: String,
        summary: String,
    },
    Stderr {
        line: String,
    },
    Finished {
        ts: DateTime<Utc>,
        success: bool,
        cost_usd: Option<f64>,
    },
    Verification {
        checked_at: DateTime<Utc>,
        unresolved: Vec<ComponentHealth>,
    },
    Error {
        message: String,
    },
}

/// Run the repair agent. Streams events via `on_event`; blocks until the
/// Claude session exits or fails to spawn.
pub async fn run_repair<F>(input: RepairInput<'_>, on_event: F) -> Result<()>
where
    F: Fn(RepairEvent) + Send + Sync + 'static,
{
    let state = input.state.clone();
    state.set_repair_running(true).await;
    let result = run_repair_active(input, Arc::new(on_event)).await;
    state.set_repair_running(false).await;
    result
}

async fn run_repair_active<F>(input: RepairInput<'_>, on_event: Arc<F>) -> Result<()>
where
    F: Fn(RepairEvent) + Send + Sync + 'static,
{
    (on_event)(RepairEvent::Started { ts: Utc::now() });

    append_repair_audit(
        input.dev_dir,
        "repair_started",
        serde_json::json!({ "allowed_components": input.allowed_components }),
    )?;

    let backup = match super::backup::run_backup(input.dev_dir, input.backups_dir) {
        Ok(backup) => backup,
        Err(error) => {
            (on_event)(RepairEvent::Error {
                message: format!("pre-repair backup failed; no fixes were attempted: {error}"),
            });
            append_repair_audit(
                input.dev_dir,
                "repair_blocked",
                serde_json::json!({ "reason": "backup_failed", "error": error.to_string() }),
            )?;
            return Err(error.context("create pre-repair backup"));
        }
    };
    super::backup::prune_backups(
        input.backups_dir,
        input.config.health.backup_retention.max(1),
    )?;
    super::backup::verify_backup(&backup.path)
        .context("pre-repair backup was not retained after pruning")?;
    (on_event)(RepairEvent::BackupCreated {
        path: backup.path.display().to_string(),
        bytes: backup.bytes,
        verified: true,
    });
    append_repair_audit(
        input.dev_dir,
        "backup_created",
        serde_json::json!({ "path": backup.path, "bytes": backup.bytes, "verified": true }),
    )?;

    let context_path = write_repair_context(&input).context("write repair context")?;
    (on_event)(RepairEvent::ContextWritten {
        path: context_path.display().to_string(),
    });

    let prompt_path =
        find_prompt(input.repo_root, input.dev_dir).context("locate repair-agent-system.md")?;

    // Prepare the complete input before publishing a capability or spawning
    // the subprocess. No fallible setup may orphan a child with a live grant.
    let context_text = std::fs::read_to_string(&context_path)
        .with_context(|| format!("read {}", context_path.display()))?;
    let user_message = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": format!(
                "Use only the authorized repair context below. Diagnose the root cause and \
                 apply only the allowlisted safe tools. Follow the system prompt.\n\n<context>\n{context_text}\n</context>",
            )
        }
    });
    let user_message_line = format!(
        "{}\n",
        serde_json::to_string(&user_message).context("serialize repair request")?
    );
    let session = create_repair_session(&input)?;

    let claude = which_claude();
    let mut cmd = Command::new(&claude);
    cmd.arg("--print")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--include-partial-messages")
        .arg("--model")
        .arg(&input.config.orchestrator.model_id)
        .arg("--append-system-prompt-file")
        .arg(&prompt_path)
        .arg("--mcp-config")
        .arg(&session.mcp_config_path)
        .arg("--strict-mcp-config")
        .arg("--setting-sources")
        .arg("")
        .arg("--tools")
        .arg("")
        .arg("--disable-slash-commands")
        .arg("--no-session-persistence")
        .arg("--allowedTools")
        .arg("mcp__continuum__repair_test_component")
        .arg("--permission-mode")
        .arg("default")
        .current_dir(input.repo_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            (on_event)(RepairEvent::Error {
                message: format!("failed to spawn `{}`: {e}", claude.display()),
            });
            return Err(e.into());
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(user_message_line.as_bytes()).await {
            tracing::error!(
                layer = "health",
                component = "repair",
                error = %e,
                "stdin write failed"
            );
            terminate_child(&mut child).await;
            (on_event)(RepairEvent::Error {
                message: format!("failed to send repair request; subprocess stopped: {e}"),
            });
            return Err(e.into());
        }
        drop(stdin);
    } else {
        terminate_child(&mut child).await;
        anyhow::bail!("repair subprocess did not expose stdin; subprocess stopped");
    }

    // Stream stdout (newline-delimited JSON).
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stderr_cb = Arc::clone(&on_event);
    let stderr_task = tokio::spawn(async move {
        if let Some(stream) = stderr {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                (stderr_cb)(RepairEvent::Stderr { line });
            }
        }
    });

    let stdout_cb = Arc::clone(&on_event);
    let stdout_task = tokio::spawn(async move {
        if let Some(stream) = stdout {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<JsonValue>(&line) {
                    Ok(v) => handle_stream_event(&v, &stdout_cb),
                    Err(_) => (stdout_cb)(RepairEvent::Stderr { line }),
                }
            }
        }
    });

    // Cap the repair run: a hung claude subprocess would otherwise pin
    // `repair_running=true` forever, blocking future repairs. Kill on expiry.
    let repair_timeout =
        Duration::from_secs(input.config.health.repair_timeout_secs.clamp(30, 30 * 60));
    let wait_result = tokio::time::timeout(repair_timeout, child.wait()).await;
    let status_opt = match wait_result {
        Ok(Ok(status)) => Some(status),
        Ok(Err(e)) => {
            tracing::warn!(
                layer = "health",
                component = "repair",
                error = %e,
                "claude subprocess wait failed"
            );
            terminate_child(&mut child).await;
            None
        }
        Err(_elapsed) => {
            tracing::error!(
                layer = "health",
                component = "repair",
                timeout_secs = repair_timeout.as_secs(),
                "Repair-agent subprocess exceeded hard timeout — killing"
            );
            terminate_child(&mut child).await;
            (on_event)(RepairEvent::Error {
                message: format!(
                    "repair run exceeded {} minute hard timeout and was killed",
                    repair_timeout.as_secs() / 60
                ),
            });
            None
        }
    };
    await_stream_task(stdout_task).await;
    await_stream_task(stderr_task).await;

    let success = status_opt.map(|s| s.success()).unwrap_or(false);
    append_repair_audit(
        input.dev_dir,
        "repair_finished",
        serde_json::json!({ "success": success }),
    )?;
    (on_event)(RepairEvent::Finished {
        ts: Utc::now(),
        success,
        cost_usd: None,
    });
    Ok(())
}

async fn terminate_child(child: &mut tokio::process::Child) {
    if let Err(error) = child.kill().await {
        tracing::warn!(
            layer = "health",
            component = "repair",
            error = %error,
            "Failed to kill repair subprocess"
        );
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

async fn await_stream_task(mut task: tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(5), &mut task)
        .await
        .is_err()
    {
        task.abort();
    }
}

struct RepairSessionFiles {
    grant_path: PathBuf,
    mcp_config_path: PathBuf,
}

impl RepairSessionFiles {
    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.mcp_config_path);
        let _ = std::fs::remove_file(&self.grant_path);
    }
}

impl Drop for RepairSessionFiles {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn create_repair_session(input: &RepairInput<'_>) -> Result<RepairSessionFiles> {
    // Resolve every non-secret dependency before publishing the capability.
    // A missing MCP binary must not leave a usable orphan grant behind.
    let mcp_bin = find_mcp_binary(input.repo_root)?;
    let token = Uuid::new_v4().to_string();
    let grants_dir = input.dev_dir.join(REPAIR_GRANTS_DIR);
    std::fs::create_dir_all(&grants_dir)
        .with_context(|| format!("create {}", grants_dir.display()))?;
    let grant_path = grants_dir.join(format!("{token}.json"));
    let temp_grant = grants_dir.join(format!(".{token}.tmp"));
    let now = Utc::now();
    let ttl = input
        .config
        .health
        .repair_session_ttl_secs
        .clamp(30, 60 * 60);
    let grant = RepairSessionGrant {
        token: token.clone(),
        created_at: now,
        expires_at: now + chrono::Duration::seconds(ttl as i64),
        allowed_components: input.allowed_components.clone(),
        // The runtime supervisor (see `continuum_core::supervisor`) consumes
        // `restart` intent files for the components it manages and respawns
        // them. An authorised repair session may therefore queue a restart
        // for exactly that supervised set; anything else stays fail-closed so
        // the agent cannot promise a restart no consumer will act on.
        allowed_restart_components: crate::supervisor::SUPERVISED_REPAIR_TARGETS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        allow_escalation_intent: false,
        // The Health-tab safe flow intentionally cannot authorize downloads or
        // config rollback. Those require a separate, explicit user workflow.
        allow_model_reinstall: false,
        allow_config_rollback: false,
    };
    if let Err(error) = std::fs::write(
        &temp_grant,
        serde_json::to_vec_pretty(&grant).context("serialize repair grant")?,
    ) {
        let _ = std::fs::remove_file(&temp_grant);
        return Err(error).with_context(|| format!("write {}", temp_grant.display()));
    }
    if let Err(error) = std::fs::rename(&temp_grant, &grant_path) {
        let _ = std::fs::remove_file(&temp_grant);
        return Err(error).with_context(|| {
            format!(
                "publish repair grant {} -> {}",
                temp_grant.display(),
                grant_path.display()
            )
        });
    }

    let mcp_config_path = input.dev_dir.join(format!("repair-mcp-{token}.json"));
    let doc = serde_json::json!({
        "mcpServers": {
            "continuum": {
                "type": "stdio",
                "command": mcp_bin,
                "args": [],
                "env": {
                    "CONTINUUM_DATA_DIR": input.dev_dir,
                    "CONTINUUM_REPAIR_TOKEN": token,
                },
            }
        }
    });
    if let Err(error) = std::fs::write(
        &mcp_config_path,
        serde_json::to_vec_pretty(&doc).context("serialize repair MCP config")?,
    ) {
        let _ = std::fs::remove_file(&grant_path);
        let _ = std::fs::remove_file(&mcp_config_path);
        return Err(error).context("write repair MCP config");
    }
    let session = RepairSessionFiles {
        grant_path,
        mcp_config_path,
    };
    if let Err(error) = append_repair_audit(
        input.dev_dir,
        "repair_grant_created",
        serde_json::json!({
            "expires_at": grant.expires_at,
            "allowed_components": grant.allowed_components,
        }),
    ) {
        session.cleanup();
        return Err(error);
    }
    Ok(session)
}

fn find_mcp_binary(repo_root: &Path) -> Result<PathBuf> {
    let executable = if cfg!(windows) {
        "continuum-mcp.exe"
    } else {
        "continuum-mcp"
    };
    let mut candidates = Vec::new();
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            candidates.push(dir.join(executable));
        }
    }
    candidates.push(repo_root.join("target/release").join(executable));
    candidates.push(repo_root.join("target/debug").join(executable));
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .with_context(|| format!("{executable} not found; build or reinstall Continuum MCP"))
}

/// Validate the capability handed to a dedicated repair MCP process.
pub fn authorize_repair_session(dev_dir: &Path, token: &str) -> Result<RepairSessionGrant> {
    Uuid::parse_str(token).context("repair token is not a UUID")?;
    let path = dev_dir
        .join(REPAIR_GRANTS_DIR)
        .join(format!("{token}.json"));
    let bytes =
        std::fs::read(&path).with_context(|| format!("read repair grant {}", path.display()))?;
    let grant: RepairSessionGrant =
        serde_json::from_slice(&bytes).context("parse repair session grant")?;
    if grant.token != token {
        anyhow::bail!("repair grant token mismatch");
    }
    if grant.expires_at <= Utc::now() {
        anyhow::bail!("repair grant expired");
    }
    Ok(grant)
}

/// Append a local, redaction-safe repair audit record.
pub fn append_repair_audit(dev_dir: &Path, event: &str, detail: JsonValue) -> Result<()> {
    std::fs::create_dir_all(dev_dir).with_context(|| format!("create {}", dev_dir.display()))?;
    let path = dev_dir.join(REPAIR_AUDIT_FILE);
    let record = serde_json::json!({
        "ts": Utc::now(),
        "event": event,
        "detail": detail,
    });
    let mut bytes = serde_json::to_vec(&record).context("serialize repair audit record")?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.write_all(&bytes)
        .context("write repair audit record")?;
    file.sync_data().context("sync repair audit record")?;
    Ok(())
}

fn handle_stream_event<F>(v: &JsonValue, on_event: &Arc<F>)
where
    F: Fn(RepairEvent) + Send + Sync + ?Sized,
{
    // `stream_event` with a `content_block_delta.text_delta` is the hot path.
    if v.get("type").and_then(|t| t.as_str()) == Some("stream_event") {
        if let Some(ev) = v.get("event") {
            if ev.get("type").and_then(|t| t.as_str()) == Some("content_block_delta") {
                if let Some(delta) = ev.get("delta") {
                    if delta.get("type").and_then(|t| t.as_str()) == Some("text_delta") {
                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            (on_event)(RepairEvent::AssistantDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                }
            }
        }
    } else if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
        if let Some(blocks) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let name = block
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    (on_event)(RepairEvent::ToolCall { name });
                }
            }
        }
    } else if v.get("type").and_then(|t| t.as_str()) == Some("user") {
        if let Some(blocks) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    let name = block
                        .get("tool_use_id")
                        .and_then(|n| n.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let summary = block
                        .get("content")
                        .and_then(|c| c.as_str())
                        .map(|s| s.chars().take(200).collect())
                        .unwrap_or_default();
                    (on_event)(RepairEvent::ToolResult { name, summary });
                }
            }
        }
    }
}

/// Assemble the repair context file. Structure is designed to be readable
/// by both Claude and a human eyeballing the file directly.
pub fn write_repair_context(input: &RepairInput<'_>) -> Result<PathBuf> {
    let mut out = String::new();
    out.push_str("# Continuum repair context\n\n");
    out.push_str(&format!("Generated at: {}\n\n", Utc::now().to_rfc3339()));

    if let Some(ref reason) = input.user_reason {
        out.push_str("## User-reported problem\n\n");
        out.push_str(reason);
        out.push_str("\n\n");
    }

    out.push_str("## Components\n\n");
    for comp in &input.components {
        out.push_str(&format!(
            "- **{name}** — status=`{status:?}`, errors_24h={errs}, avg_ms={avg}\n",
            name = comp.name,
            status = comp.status,
            errs = comp.error_count_24h,
            avg = comp.avg_response_ms.unwrap_or(0)
        ));
        if let Some(ref err) = comp.last_error {
            out.push_str(&format!("  - last_error: {err}\n"));
        }
        if let Some(ref log) = comp.log_path {
            out.push_str(&format!("  - log_path: {log}\n"));
        }
        if let Some(ref note) = comp.recovery_note {
            out.push_str(&format!("  - recovery: {note}\n"));
        }
    }

    out.push_str("\n## Configuration snapshot\n\n```toml\n");
    let mut safe_config = input.config.clone();
    if !safe_config.tts.elevenlabs.api_key.is_empty() {
        safe_config.tts.elevenlabs.api_key = "<redacted>".into();
    }
    out.push_str(
        &toml::to_string_pretty(&safe_config).context("serialize redacted repair config")?,
    );
    out.push_str("```\n\n");

    out.push_str(&system_resources_block(input.config));
    out.push('\n');

    out.push_str("## Last 500 log lines\n\n```\n");
    let filter = LogFilter {
        limit: Some(500),
        ..LogFilter::default()
    };
    let mut entries: Vec<LogEntry> = input.logs.query(&filter);
    entries.reverse();
    for e in entries {
        let message = e.message.chars().take(2_000).collect::<String>();
        out.push_str(&format!(
            "{ts} {level} {layer}/{comp} {msg}\n",
            ts = e.ts.format("%H:%M:%S%.3f"),
            level = e.level.to_uppercase(),
            layer = e.layer.as_deref().unwrap_or("-"),
            comp = e.component.as_deref().unwrap_or("-"),
            msg = message
        ));
        if out.len() >= 256 * 1024 {
            out.push_str("[repair context truncated at 256 KiB]\n");
            break;
        }
    }
    out.push_str("```\n");

    // The in-memory `LogBuffer` above only carries events emitted inside the
    // desktop process. The standalone `continuum` runtime is a *separate*
    // process whose logs never reach that buffer — it writes them to
    // `~/.continuum-dev/logs/continuum.log` (tracing-appender rolling file,
    // see `bin/continuum.rs`). Tail that file so the repair agent can see what
    // the perception/triage/orchestrator loops actually logged, including the
    // last lines before a crash (which the in-memory buffer loses when the
    // runtime process dies).
    if let Some(tail) = runtime_log_tail(input.dev_dir) {
        out.push_str("\n## Runtime log tail (from disk)\n\n```\n");
        out.push_str(&tail);
        out.push_str("```\n");
    }

    let path = input.dev_dir.join("repair-context.md");
    std::fs::create_dir_all(input.dev_dir).ok();
    std::fs::write(&path, out).context("write repair context file")?;
    Ok(path)
}

/// Read the tail of the runtime's on-disk log (`<dev_dir>/logs/continuum.log`)
/// for the repair context. Returns `None` when the file is absent (the runtime
/// has not run yet, or file logging is not enabled) so the repair context
/// degrades gracefully rather than failing. Keeps the last ~500 lines and
/// caps the output so a runaway log cannot blow up the context file.
fn runtime_log_tail(dev_dir: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let path = dev_dir.join("logs").join("continuum.log");
    let file = std::fs::File::open(&path).ok()?;
    let reader = BufReader::new(file);
    const TAIL_LINES: usize = 500;
    const MAX_BYTES: usize = 64 * 1024;
    let mut ring: std::collections::VecDeque<String> =
        std::collections::VecDeque::with_capacity(TAIL_LINES);
    let mut total_bytes: usize = 0;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        total_bytes = total_bytes.saturating_add(line.len() + 1);
        if ring.len() == TAIL_LINES {
            if let Some(old) = ring.pop_front() {
                total_bytes = total_bytes.saturating_sub(old.len() + 1);
            }
        }
        ring.push_back(line);
        if total_bytes >= MAX_BYTES {
            break;
        }
    }
    if ring.is_empty() {
        return None;
    }
    let mut out = String::new();
    for line in ring {
        out.push_str(&line);
        out.push('\n');
    }
    Some(out)
}

/// Build the `## System resources` block for the repair context.
///
/// Probes the host fresh (cheap — sysinfo + a couple of Win32 calls + an
/// optional `nvidia-smi`) and samples a live CPU/RAM reading so the repair
/// agent can correlate component failures with the actual machine state
/// (e.g. "triage model load OOM'd → 4 GB laptop, vision should be off →
/// lower `cpu_core_fraction` / `workers_max_concurrent`"). The resolved
/// plan shows the knobs the runtime is currently applying.
fn system_resources_block(config: &ContinuumConfig) -> String {
    let specs = hardware::probe_hardware();
    let plan = hardware::resolve_resource_policy(&specs, &config.resources);

    // Live CPU/RAM sample. CPU% reflects the delta since the previous
    // refresh, so prime once and read on a second refresh.
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    sys.refresh_memory();
    // A zero-sleep second refresh gives a near-instant delta; good enough
    // for a repair-context snapshot (not a precise benchmark).
    std::thread::sleep(std::time::Duration::from_millis(100));
    sys.refresh_cpu_all();
    sys.refresh_memory();
    let cpu_pct = {
        let cpus = sys.cpus();
        if cpus.is_empty() {
            0.0f32
        } else {
            cpus.iter().map(|c| c.cpu_usage()).sum::<f32>() / cpus.len() as f32
        }
    };
    let ram_total_mb = sys.total_memory() / 1024 / 1024;
    let ram_used_mb = sys.used_memory() / 1024 / 1024;
    let ram_pct = if ram_total_mb > 0 {
        (ram_used_mb as f32 / ram_total_mb as f32) * 100.0
    } else {
        0.0
    };

    let mut s = String::new();
    s.push_str("## System resources\n\n");
    s.push_str("Detected at repair time (probed fresh — not the boot snapshot):\n\n");
    s.push_str(&format!(
        "- CPU: {brand} — {phys} physical / {log} logical cores\n",
        brand = specs.cpu_brand,
        phys = specs.physical_cores,
        log = specs.logical_cores,
    ));
    s.push_str(&format!(
        "- RAM: {used} / {total} MB used ({ram_pct:.0}%)\n",
        used = ram_used_mb,
        total = ram_total_mb,
    ));
    s.push_str(&format!(
        "- GPU: cuda={cuda}, vram={vram_mb} MB\n",
        cuda = specs.has_cuda,
        vram_mb = specs.vram_mb.unwrap_or(0),
    ));
    s.push_str(&format!(
        "- Power: on_battery={batt}, is_laptop={laptop}\n",
        batt = specs.on_battery,
        laptop = specs.is_laptop,
    ));
    s.push_str(&format!(
        "- Live load: cpu={cpu_pct:.0}%, ram={ram_pct:.0}%\n\n",
    ));
    s.push_str("Resolved resource plan (applied at boot):\n\n");
    s.push_str(&format!(
        "- triage_threads = {tt}\n",
        tt = plan.triage_threads
    ));
    s.push_str(&format!(
        "- triage_gpu_layers = {gl}\n",
        gl = plan.triage_gpu_layers
    ));
    s.push_str(&format!(
        "- vision_enabled = {vis}, vision_gpu = {vg}\n",
        vis = plan.vision_enabled,
        vg = plan.vision_gpu,
    ));
    s.push_str(&format!(
        "- whisper_threads = {wt}\n",
        wt = plan.whisper_threads
    ));
    s.push_str(&format!(
        "- workers_max_concurrent = {w}\n",
        w = plan.workers_max_concurrent
    ));
    s.push_str(&format!(
        "- screen_interval = {si}s, context_interval = {ci}s\n",
        si = plan.screen_interval_secs,
        ci = plan.context_interval_secs,
    ));
    s
}

fn which_claude() -> PathBuf {
    // On Windows, `claude` is either a .cmd shim in %LOCALAPPDATA% or on PATH.
    PathBuf::from("claude")
}

fn find_prompt(repo_root: &Path, dev_dir: &Path) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // Also try next to the running executable — covers packaged installs
    // where `prompts/` is bundled alongside continuum-desktop.exe and the cwd
    // is wherever the user launched from.
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        candidates.push(exe_dir.join("prompts").join("repair-agent-system.md"));
    }
    candidates.push(repo_root.join("prompts").join("repair-agent-system.md"));
    candidates.push(Path::new("prompts/repair-agent-system.md").to_path_buf());
    for p in candidates.iter() {
        if p.is_file()
            && std::fs::metadata(p)
                .map(|metadata| metadata.len() <= 64 * 1024)
                .unwrap_or(false)
        {
            return Ok(p.clone());
        }
    }
    anyhow::bail!(
        "repair-agent-system.md not found (or exceeds 64 KiB) in {}, cwd, or install dir; refusing an untrusted data-directory prompt ({})",
        repo_root.display(),
        dev_dir.display()
    )
}

// --- Repair-specific action helpers ---
//
// These are the concrete operations the repair agent can invoke via its
// MCP tools (`continuum-mcp` exposes thin wrappers around them). Keeping them
// here — not in `continuum-mcp` — means the desktop app can also call them
// directly via Tauri commands for the "Restart component" button.

/// Rollback the config file from a dated backup. Returns the restored
/// path.
pub fn rollback_config(dev_dir: &Path, backups_dir: &Path, date: &str) -> Result<PathBuf> {
    let zip_path = super::backup::latest_backup_for_date(backups_dir, date)?;
    let file =
        std::fs::File::open(&zip_path).with_context(|| format!("open {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("read zip")?;
    let mut entry = archive
        .by_name("config.toml")
        .context("config.toml not in backup")?;
    if entry.size() > super::backup::MAX_CONFIG_BYTES {
        anyhow::bail!("backup config exceeds the 10 MiB rollback safety limit");
    }
    let mut restored_bytes = Vec::new();
    std::io::copy(&mut entry, &mut restored_bytes).context("read config from backup")?;
    drop(entry);
    drop(archive);

    let restored_text =
        std::str::from_utf8(&restored_bytes).context("backup config is not UTF-8")?;
    let restored_config: ContinuumConfig =
        toml::from_str(restored_text).context("backup config is not valid Continuum TOML")?;
    restored_config
        .resources
        .validate()
        .context("backup config has invalid resource limits")?;

    // Backup the current state immediately before the mutating rollback. The
    // source archive was already selected and verified, so this new backup
    // cannot accidentally become the rollback source.
    let safety_backup = super::backup::run_backup(dev_dir, backups_dir)
        .context("create pre-rollback safety backup")?;
    super::backup::prune_backups(backups_dir, restored_config.health.backup_retention.max(1))
        .context("apply backup retention before rollback")?;
    super::backup::verify_backup(&safety_backup.path)
        .context("pre-rollback safety backup was not retained")?;
    append_repair_audit(
        dev_dir,
        "rollback_backup_created",
        serde_json::json!({ "path": safety_backup.path, "bytes": safety_backup.bytes }),
    )?;

    let restored = dev_dir.join("config.toml");
    std::fs::create_dir_all(dev_dir).with_context(|| format!("create {}", dev_dir.display()))?;
    let nonce = Uuid::new_v4();
    let staged = dev_dir.join(format!(".config-{nonce}.tmp"));
    let previous = dev_dir.join(format!(".config-{nonce}.previous"));
    {
        let mut out = std::fs::File::create(&staged)
            .with_context(|| format!("write {}", staged.display()))?;
        out.write_all(&restored_bytes)
            .context("write staged rollback config")?;
        out.sync_all().context("sync staged rollback config")?;
    }
    let had_previous = restored.exists();
    if had_previous {
        std::fs::rename(&restored, &previous).with_context(|| {
            format!(
                "stage current config {} -> {}",
                restored.display(),
                previous.display()
            )
        })?;
    }
    if let Err(error) = std::fs::rename(&staged, &restored) {
        if had_previous {
            let _ = std::fs::rename(&previous, &restored);
        }
        let _ = std::fs::remove_file(&staged);
        return Err(error).context("atomically publish rollback config");
    }
    if let Err(audit_error) = append_repair_audit(
        dev_dir,
        "config_rolled_back",
        serde_json::json!({ "source": zip_path, "restored": restored }),
    ) {
        let revert_result = if had_previous {
            std::fs::remove_file(&restored).and_then(|_| std::fs::rename(&previous, &restored))
        } else {
            std::fs::remove_file(&restored)
        };
        return match revert_result {
            Ok(()) => Err(audit_error.context("audit rollback outcome; config change reverted")),
            Err(revert_error) => Err(anyhow::anyhow!(
                "audit rollback outcome failed ({audit_error}); reverting config also failed ({revert_error}); safety backup: {}",
                safety_backup.path.display()
            )),
        };
    }
    if had_previous {
        if let Err(error) = std::fs::remove_file(&previous) {
            tracing::warn!(
                layer = "health",
                component = "repair",
                error = %error,
                path = %previous.display(),
                "Rollback succeeded but previous-config cleanup failed"
            );
        }
    }
    tracing::info!(
        layer = "health",
        component = "repair",
        date = date,
        "Config rolled back"
    );
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ComponentHealth, ComponentStatus};
    use tempfile::TempDir;

    fn input_for(
        tmp: &TempDir,
    ) -> (
        PathBuf,
        PathBuf,
        ContinuumConfig,
        StateHandle,
        LogBuffer,
        Vec<ComponentHealth>,
    ) {
        let dev = tmp.path().join("dev");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let cfg = ContinuumConfig::default();
        let state = StateHandle::new();
        let logs = LogBuffer::new(100);
        let components = vec![ComponentHealth {
            name: "vision".into(),
            status: ComponentStatus::Error,
            last_check_ts: None,
            last_error: Some("onnx crashed".into()),
            error_count_24h: 2,
            avg_response_ms: Some(2000),
            log_path: Some("~/.continuum-dev/logs/vision.log".into()),
            recovery_note: Some("re-download SmolVLM".into()),
        }];
        (dev, repo, cfg, state, logs, components)
    }

    #[test]
    fn write_repair_context_includes_all_sections() {
        let tmp = TempDir::new().unwrap();
        let (dev, repo, mut cfg, state, logs, components) = input_for(&tmp);
        cfg.tts.elevenlabs.api_key = "secret-test-key".into();
        let backups = tmp.path().join("backups");

        let input = RepairInput {
            dev_dir: &dev,
            backups_dir: &backups,
            repo_root: &repo,
            config: &cfg,
            state: &state,
            logs: &logs,
            components,
            allowed_components: vec!["vision".into()],
            user_reason: Some("voice keeps cutting out".into()),
        };

        let path = write_repair_context(&input).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("voice keeps cutting out"));
        assert!(text.contains("vision"));
        assert!(text.contains("onnx crashed"));
        assert!(text.contains("Configuration snapshot"));
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("secret-test-key"));
    }

    #[test]
    fn runtime_log_tail_reads_disk_log() {
        let tmp = TempDir::new().unwrap();
        let dev = tmp.path().join("dev");
        let logs_dir = dev.join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::write(
            logs_dir.join("continuum.log"),
            "line one\nline two\ntriage decided\n",
        )
        .unwrap();
        let tail = runtime_log_tail(&dev).expect("should read the log");
        assert!(tail.contains("triage decided"));
        assert!(tail.contains("line one"));
    }

    #[test]
    fn runtime_log_tail_none_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let dev = tmp.path().join("dev");
        std::fs::create_dir_all(&dev).unwrap();
        assert!(runtime_log_tail(&dev).is_none());
    }

    #[test]
    fn repair_grant_allows_supervised_restart_targets() {
        // The supervisor-managed set must be authorised for restart so the
        // repair agent can queue a restart the supervisor will consume.
        let targets = crate::supervisor::SUPERVISED_REPAIR_TARGETS;
        assert!(targets.contains(&"vision"));
        assert!(targets.contains(&"audio"));
        assert!(targets.contains(&"context_watcher"));
    }

    #[test]
    fn rollback_config_restores_from_zip() {
        let tmp = TempDir::new().unwrap();
        let dev = tmp.path().join("dev");
        let backups = tmp.path().join("backups");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("config.toml"), "[screen]\ninterval_secs = 4\n").unwrap();
        crate::health::backup::run_backup(&dev, &backups).unwrap();
        std::fs::write(dev.join("config.toml"), "[screen]\ninterval_secs = 8\n").unwrap();

        let date = Utc::now().format("%Y-%m-%d").to_string();
        let restored = rollback_config(&dev, &backups, &date).unwrap();
        let contents = std::fs::read_to_string(&restored).unwrap();
        assert_eq!(contents, "[screen]\ninterval_secs = 4\n");
        assert_eq!(crate::health::backup::count_backups(&backups), 2);
    }

    #[test]
    fn rollback_rejects_invalid_date_without_touching_config() {
        let tmp = TempDir::new().unwrap();
        let dev = tmp.path().join("dev");
        let backups = tmp.path().join("backups");
        std::fs::create_dir_all(&dev).unwrap();
        let current = "[screen]\ninterval_secs = 8\n";
        std::fs::write(dev.join("config.toml"), current).unwrap();
        assert!(rollback_config(&dev, &backups, "../../escape").is_err());
        assert_eq!(
            std::fs::read_to_string(dev.join("config.toml")).unwrap(),
            current
        );
    }

    #[test]
    fn rollback_rejects_invalid_toml_before_mutation() {
        let tmp = TempDir::new().unwrap();
        let dev = tmp.path().join("dev");
        let backups = tmp.path().join("backups");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("config.toml"), "not valid toml").unwrap();
        let source = crate::health::backup::run_backup(&dev, &backups).unwrap();
        let date = source.date.format("%Y-%m-%d").to_string();
        let current = "[screen]\ninterval_secs = 8\n";
        std::fs::write(dev.join("config.toml"), current).unwrap();
        assert!(rollback_config(&dev, &backups, &date).is_err());
        assert_eq!(
            std::fs::read_to_string(dev.join("config.toml")).unwrap(),
            current
        );
        assert_eq!(crate::health::backup::count_backups(&backups), 1);
    }

    #[test]
    fn repair_grant_requires_matching_unexpired_uuid() {
        let tmp = TempDir::new().unwrap();
        let grants = tmp.path().join(REPAIR_GRANTS_DIR);
        std::fs::create_dir_all(&grants).unwrap();
        let token = Uuid::new_v4().to_string();
        let grant = RepairSessionGrant {
            token: token.clone(),
            created_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::minutes(1),
            allowed_components: vec!["vision".into()],
            allowed_restart_components: Vec::new(),
            allow_escalation_intent: false,
            allow_model_reinstall: false,
            allow_config_rollback: false,
        };
        std::fs::write(
            grants.join(format!("{token}.json")),
            serde_json::to_vec(&grant).unwrap(),
        )
        .unwrap();
        assert_eq!(
            authorize_repair_session(tmp.path(), &token)
                .unwrap()
                .allowed_components,
            vec!["vision"]
        );
        assert!(authorize_repair_session(tmp.path(), "../../escape").is_err());

        let expired = RepairSessionGrant {
            expires_at: Utc::now() - chrono::Duration::seconds(1),
            ..grant
        };
        std::fs::write(
            grants.join(format!("{token}.json")),
            serde_json::to_vec(&expired).unwrap(),
        )
        .unwrap();
        assert!(authorize_repair_session(tmp.path(), &token).is_err());
    }
}
