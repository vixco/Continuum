//! # Repair agent
//!
//! Spawns a dedicated Claude Code (Opus) session with:
//! - Working directory: Kairo install folder (the git root)
//! - A custom system prompt from `prompts/repair-agent-system.md`
//! - Access to the Kairo MCP server (plus repair-specific namespaces once
//!   [`super::repair_tools`] is wired into `kairo-mcp`)
//!
//! The session receives a structured repair context (last 500 log lines,
//! all component statuses, any stack traces, config snapshot) and streams
//! its response back to the dashboard Health tab via the [`RepairEvent`]
//! callback.
//!
//! Writing the context to `~/.kairo-dev/repair-context.md` also gives the
//! user something to look at if the agent spawn itself fails.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::config::KairoConfig;
use crate::logs::{LogBuffer, LogEntry, LogFilter};
use crate::state::{ComponentHealth, StateHandle};

/// Input for a repair run.
pub struct RepairInput<'a> {
    pub dev_dir: &'a Path,
    pub repo_root: &'a Path,
    pub config: &'a KairoConfig,
    pub state: &'a StateHandle,
    pub logs: &'a LogBuffer,
    pub components: Vec<ComponentHealth>,
    pub user_reason: Option<String>,
}

/// Streamed output from the repair session — what the dashboard renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepairEvent {
    Started { ts: DateTime<Utc> },
    ContextWritten { path: String },
    AssistantDelta { text: String },
    ToolCall { name: String },
    ToolResult { name: String, summary: String },
    Stderr { line: String },
    Finished { ts: DateTime<Utc>, success: bool, cost_usd: Option<f64> },
    Error { message: String },
}

/// Run the repair agent. Streams events via `on_event`; blocks until the
/// Claude session exits or fails to spawn.
pub async fn run_repair<F>(input: RepairInput<'_>, on_event: F) -> Result<()>
where
    F: Fn(RepairEvent) + Send + Sync + 'static,
{
    let on_event = Arc::new(on_event);
    (on_event)(RepairEvent::Started { ts: Utc::now() });

    let context_path = write_repair_context(&input).context("write repair context")?;
    (on_event)(RepairEvent::ContextWritten {
        path: context_path.display().to_string(),
    });

    let prompt_path = find_prompt(input.repo_root, input.dev_dir)
        .context("locate repair-agent-system.md")?;

    input.state.set_repair_running(true).await;

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
        .arg("claude-opus-4-6")
        .arg("--append-system-prompt-file")
        .arg(&prompt_path)
        .current_dir(input.repo_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            input.state.set_repair_running(false).await;
            (on_event)(RepairEvent::Error {
                message: format!("failed to spawn `{}`: {e}", claude.display()),
            });
            return Err(e.into());
        }
    };

    // Send the repair context as the user message.
    let user_message = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": format!(
                "Repair context has been written to {}. Read it, diagnose the root cause, \
and apply fixes. Follow the system prompt.{reason}",
                context_path.display(),
                reason = input.user_reason
                    .as_deref()
                    .map(|r| format!(" User said: {r}"))
                    .unwrap_or_default(),
            )
        }
    });

    if let Some(mut stdin) = child.stdin.take() {
        let msg = format!("{}\n", serde_json::to_string(&user_message)?);
        if let Err(e) = stdin.write_all(msg.as_bytes()).await {
            tracing::warn!(
                layer = "health",
                component = "repair",
                error = %e,
                "stdin write failed"
            );
        }
        drop(stdin);
    }

    // Stream stdout (newline-delimited JSON).
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let state_clone = input.state.clone();

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

    let status = child.wait().await.context("wait for claude")?;
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    state_clone.set_repair_running(false).await;
    (on_event)(RepairEvent::Finished {
        ts: Utc::now(),
        success: status.success(),
        cost_usd: None,
    });
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
    out.push_str("# Kairo repair context\n\n");
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
    out.push_str(&toml::to_string_pretty(input.config).unwrap_or_default());
    out.push_str("```\n\n");

    out.push_str("## Last 500 log lines\n\n```\n");
    let filter = LogFilter {
        limit: Some(500),
        ..LogFilter::default()
    };
    let mut entries: Vec<LogEntry> = input.logs.query(&filter);
    entries.reverse();
    for e in entries {
        out.push_str(&format!(
            "{ts} {level} {layer}/{comp} {msg}\n",
            ts = e.ts.format("%H:%M:%S%.3f"),
            level = e.level.to_uppercase(),
            layer = e.layer.as_deref().unwrap_or("-"),
            comp = e.component.as_deref().unwrap_or("-"),
            msg = e.message
        ));
    }
    out.push_str("```\n");

    let path = input.dev_dir.join("repair-context.md");
    std::fs::create_dir_all(input.dev_dir).ok();
    std::fs::write(&path, out).context("write repair context file")?;
    Ok(path)
}

fn which_claude() -> PathBuf {
    // On Windows, `claude` is either a .cmd shim in %LOCALAPPDATA% or on PATH.
    PathBuf::from("claude")
}

fn find_prompt(repo_root: &Path, dev_dir: &Path) -> Result<PathBuf> {
    let candidates = [
        dev_dir.join("repair-agent-system.md"),
        repo_root.join("prompts").join("repair-agent-system.md"),
        Path::new("prompts/repair-agent-system.md").to_path_buf(),
    ];
    for p in candidates.iter() {
        if p.exists() {
            return Ok(p.clone());
        }
    }
    anyhow::bail!(
        "repair-agent-system.md not found in {}, {} or cwd",
        dev_dir.display(),
        repo_root.display()
    )
}

// --- Repair-specific action helpers ---
//
// These are the concrete operations the repair agent can invoke via its
// MCP tools (`kairo-mcp` exposes thin wrappers around them). Keeping them
// here — not in `kairo-mcp` — means the desktop app can also call them
// directly via Tauri commands for the "Restart component" button.

/// Rollback the config file from a dated backup. Returns the restored
/// path.
pub fn rollback_config(
    dev_dir: &Path,
    backups_dir: &Path,
    date: &str,
) -> Result<PathBuf> {
    let zip_path = backups_dir
        .join(date)
        .join(format!("kairo-{date}.zip"));
    if !zip_path.exists() {
        anyhow::bail!("backup {} not found", zip_path.display());
    }
    let file = std::fs::File::open(&zip_path)
        .with_context(|| format!("open {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("read zip")?;
    let mut entry = archive
        .by_name("config.toml")
        .context("config.toml not in backup")?;
    let restored = dev_dir.join("config.toml");
    let mut out = std::fs::File::create(&restored)
        .with_context(|| format!("write {}", restored.display()))?;
    std::io::copy(&mut entry, &mut out).context("copy config")?;
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
    ) -> (PathBuf, PathBuf, KairoConfig, StateHandle, LogBuffer, Vec<ComponentHealth>) {
        let dev = tmp.path().join("dev");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let cfg = KairoConfig::default();
        let state = StateHandle::new();
        let logs = LogBuffer::new(100);
        let components = vec![ComponentHealth {
            name: "vision".into(),
            status: ComponentStatus::Error,
            last_check_ts: None,
            last_error: Some("onnx crashed".into()),
            error_count_24h: 2,
            avg_response_ms: Some(2000),
            log_path: Some("~/.kairo-dev/logs/vision.log".into()),
            recovery_note: Some("re-download SmolVLM".into()),
        }];
        (dev, repo, cfg, state, logs, components)
    }

    #[test]
    fn write_repair_context_includes_all_sections() {
        let tmp = TempDir::new().unwrap();
        let (dev, repo, cfg, state, logs, components) = input_for(&tmp);

        let input = RepairInput {
            dev_dir: &dev,
            repo_root: &repo,
            config: &cfg,
            state: &state,
            logs: &logs,
            components,
            user_reason: Some("voice keeps cutting out".into()),
        };

        let path = write_repair_context(&input).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("voice keeps cutting out"));
        assert!(text.contains("vision"));
        assert!(text.contains("onnx crashed"));
        assert!(text.contains("Configuration snapshot"));
    }

    #[test]
    fn rollback_config_restores_from_zip() {
        let tmp = TempDir::new().unwrap();
        let dev = tmp.path().join("dev");
        let backups = tmp.path().join("backups");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("config.toml"), "old").unwrap();
        crate::health::backup::run_backup(&dev, &backups).unwrap();
        std::fs::write(dev.join("config.toml"), "newer but broken").unwrap();

        let date = Utc::now().format("%Y-%m-%d").to_string();
        let restored = rollback_config(&dev, &backups, &date).unwrap();
        let contents = std::fs::read_to_string(&restored).unwrap();
        assert_eq!(contents, "old");
    }
}
