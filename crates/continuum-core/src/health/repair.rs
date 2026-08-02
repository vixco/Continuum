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

/// Hard wall-clock ceiling for a single repair-agent run. A stuck or runaway
/// Claude Opus session shouldn't block future repairs or leave
/// `repair_running` pinned forever. 30 minutes is generous for a complex
/// investigation but finite; the next tick after this fires kills the
/// subprocess and marks the run failed.
const REPAIR_HARD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Input for a repair run.
pub struct RepairInput<'a> {
    pub dev_dir: &'a Path,
    pub repo_root: &'a Path,
    pub config: &'a ContinuumConfig,
    pub state: &'a StateHandle,
    pub logs: &'a LogBuffer,
    pub components: Vec<ComponentHealth>,
    pub user_reason: Option<String>,
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
    let on_event = Arc::new(on_event);
    (on_event)(RepairEvent::Started { ts: Utc::now() });

    let context_path = write_repair_context(&input).context("write repair context")?;
    (on_event)(RepairEvent::ContextWritten {
        path: context_path.display().to_string(),
    });

    let prompt_path =
        find_prompt(input.repo_root, input.dev_dir).context("locate repair-agent-system.md")?;

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
        .arg(&input.config.orchestrator.model_id)
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

    // Cap the repair run: a hung claude subprocess would otherwise pin
    // `repair_running=true` forever, blocking future repairs. Kill on expiry.
    let wait_result = tokio::time::timeout(REPAIR_HARD_TIMEOUT, child.wait()).await;
    let status_opt = match wait_result {
        Ok(Ok(status)) => Some(status),
        Ok(Err(e)) => {
            tracing::warn!(
                layer = "health",
                component = "repair",
                error = %e,
                "claude subprocess wait failed"
            );
            None
        }
        Err(_elapsed) => {
            tracing::error!(
                layer = "health",
                component = "repair",
                timeout_secs = REPAIR_HARD_TIMEOUT.as_secs(),
                "Repair-agent subprocess exceeded hard timeout — killing"
            );
            if let Err(e) = child.kill().await {
                tracing::warn!(
                    layer = "health",
                    component = "repair",
                    error = %e,
                    "Failed to kill stuck claude subprocess"
                );
            }
            (on_event)(RepairEvent::Error {
                message: format!(
                    "repair run exceeded {} minute hard timeout and was killed",
                    REPAIR_HARD_TIMEOUT.as_secs() / 60
                ),
            });
            None
        }
    };
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    state_clone.set_repair_running(false).await;
    (on_event)(RepairEvent::Finished {
        ts: Utc::now(),
        success: status_opt.map(|s| s.success()).unwrap_or(false),
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
    out.push_str(&toml::to_string_pretty(input.config).unwrap_or_default());
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
    let mut candidates: Vec<PathBuf> = vec![
        dev_dir.join("repair-agent-system.md"),
        repo_root.join("prompts").join("repair-agent-system.md"),
        Path::new("prompts/repair-agent-system.md").to_path_buf(),
    ];
    // Also try next to the running executable — covers packaged installs
    // where `prompts/` is bundled alongside continuum-desktop.exe and the cwd
    // is wherever the user launched from.
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        candidates.push(exe_dir.join("prompts").join("repair-agent-system.md"));
    }
    for p in candidates.iter() {
        if p.exists() {
            return Ok(p.clone());
        }
    }
    anyhow::bail!(
        "repair-agent-system.md not found in {}, {}, cwd, or install dir",
        dev_dir.display(),
        repo_root.display()
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
    let zip_path = backups_dir.join(date).join(format!("continuum-{date}.zip"));
    if !zip_path.exists() {
        anyhow::bail!("backup {} not found", zip_path.display());
    }
    let file =
        std::fs::File::open(&zip_path).with_context(|| format!("open {}", zip_path.display()))?;
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
