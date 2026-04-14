//! # Repair tools (`mcp__kairo__repair_*`)
//!
//! Exposed exclusively to the repair agent session. Each tool writes an
//! **intent file** under `~/.kairo-dev/repair-intents/` that the running
//! Kairo runtime watches for. The runtime then executes the intent (for
//! example, tearing down and re-spawning the TTS subsystem) and writes an
//! outcome file back. This indirection lets the repair agent stay inside
//! its Claude Code sandbox without the runtime giving the agent direct
//! process-control primitives.
//!
//! For `rollback_config` and `test_component` we can answer directly from
//! the MCP process because the work is pure disk I/O.
//!
//! `escalate` writes a user-visible notification file that the dashboard
//! Health tab surfaces as a red banner until acknowledged.
//!
//! The Kairo runtime picks intents up every 2 s; the dashboard reports
//! their status via the normal health stream.

use std::path::{Path, PathBuf};

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Components the repair agent may target. Kept in sync with the
/// components registered in `apps/desktop/src-tauri/src/components.rs`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepairTarget {
    Vision,
    Triage,
    Audio,
    Stt,
    Tts,
    Orchestrator,
    Mcp,
    Memory,
    ContextWatcher,
}

impl RepairTarget {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Vision => "vision",
            Self::Triage => "triage",
            Self::Audio => "audio",
            Self::Stt => "stt",
            Self::Tts => "tts",
            Self::Orchestrator => "orchestrator",
            Self::Mcp => "mcp",
            Self::Memory => "memory",
            Self::ContextWatcher => "context_watcher",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RestartRequest {
    pub component: RepairTarget,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReinstallRequest {
    pub component: RepairTarget,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RollbackRequest {
    /// Backup folder name under `~/.kairo-backups/` — format `YYYY-MM-DD`.
    pub date: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TestRequest {
    pub component: RepairTarget,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct EscalateRequest {
    pub message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IntentResponse {
    pub intent_file: String,
    pub queued_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RollbackResponse {
    pub restored_path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TestResponse {
    pub component: String,
    pub status: String,
    pub note: Option<String>,
}

/// Queue a restart intent for the runtime to pick up.
pub fn restart(data_dir: &Path, target: RepairTarget) -> anyhow::Result<IntentResponse> {
    queue_intent(
        data_dir,
        "restart",
        serde_json::json!({
            "component": target.as_str(),
        }),
    )
}

/// Queue a model reinstall intent.
pub fn reinstall(data_dir: &Path, target: RepairTarget) -> anyhow::Result<IntentResponse> {
    queue_intent(
        data_dir,
        "reinstall",
        serde_json::json!({
            "component": target.as_str(),
        }),
    )
}

/// Rollback immediately — pure disk I/O.
pub fn rollback(data_dir: &Path, date: &str) -> anyhow::Result<RollbackResponse> {
    let backups_dir = backups_dir_for(data_dir);
    let restored = kairo_core::health::repair::rollback_config(data_dir, &backups_dir, date)?;
    Ok(RollbackResponse {
        restored_path: restored.display().to_string(),
    })
}

/// Run the component's light-weight file-presence check. For the full
/// health probe the repair agent should prefer `restart_component`
/// (which triggers a re-spawn + probe), but this gives Claude a quick
/// sanity check that doesn't require runtime round-trip.
pub fn test(data_dir: &Path, target: RepairTarget) -> TestResponse {
    let (status, note) = match target {
        RepairTarget::Vision => file_status(data_dir.join("models/vision")),
        RepairTarget::Triage => file_status(data_dir.join("models/triage")),
        RepairTarget::Stt | RepairTarget::Audio => file_status(data_dir.join("models/stt")),
        RepairTarget::Tts => file_status(data_dir.join("models/tts")),
        RepairTarget::Orchestrator => (
            if which_claude().is_some() {
                "healthy".into()
            } else {
                "error".into()
            },
            Some("checks only whether the `claude` CLI is on PATH".into()),
        ),
        RepairTarget::Mcp => (
            if std::path::Path::new("target/release/kairo-mcp.exe").exists()
                || std::path::Path::new("target/release/kairo-mcp").exists()
            {
                "healthy".into()
            } else {
                "degrading".into()
            },
            Some("binary on disk check".into()),
        ),
        RepairTarget::Memory => file_status(data_dir.join("semantic.sqlite")),
        RepairTarget::ContextWatcher => (
            "unknown".into(),
            Some("context_watcher has no file to probe; call restart instead".into()),
        ),
    };
    TestResponse {
        component: target.as_str().to_string(),
        status,
        note,
    }
}

/// Write an escalation file the dashboard surfaces as a banner.
pub fn escalate(data_dir: &Path, message: &str) -> anyhow::Result<IntentResponse> {
    queue_intent(
        data_dir,
        "escalate",
        serde_json::json!({
            "message": message,
            "ts": Utc::now().to_rfc3339(),
        }),
    )
}

// --- internals ---

fn file_status(path: PathBuf) -> (String, Option<String>) {
    if path.exists() {
        ("healthy".into(), Some(path.display().to_string()))
    } else {
        ("error".into(), Some(format!("missing: {}", path.display())))
    }
}

fn queue_intent(
    data_dir: &Path,
    kind: &str,
    body: serde_json::Value,
) -> anyhow::Result<IntentResponse> {
    let intents_dir = data_dir.join("repair-intents");
    std::fs::create_dir_all(&intents_dir)?;
    let ts = Utc::now().format("%Y%m%dT%H%M%S%3f").to_string();
    let filename = format!("{ts}-{kind}.json");
    let path = intents_dir.join(&filename);
    let payload = serde_json::json!({
        "kind": kind,
        "queued_at": Utc::now().to_rfc3339(),
        "body": body,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&payload)?)?;
    Ok(IntentResponse {
        intent_file: path.display().to_string(),
        queued_at: Utc::now().to_rfc3339(),
    })
}

fn backups_dir_for(data_dir: &Path) -> PathBuf {
    data_dir
        .parent()
        .map(|p| p.join(".kairo-backups"))
        .unwrap_or_else(|| data_dir.join(".kairo-backups"))
}

fn which_claude() -> Option<PathBuf> {
    // Best-effort: check PATH for claude / claude.cmd.
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for name in ["claude.cmd", "claude.exe", "claude"] {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn restart_writes_intent_file() {
        let tmp = TempDir::new().unwrap();
        let resp = restart(tmp.path(), RepairTarget::Tts).unwrap();
        let contents = std::fs::read_to_string(&resp.intent_file).unwrap();
        assert!(contents.contains("\"kind\": \"restart\""));
        assert!(contents.contains("\"component\": \"tts\""));
    }

    #[test]
    fn rollback_restores_from_backup() {
        let tmp = TempDir::new().unwrap();
        let dev = tmp.path().join("dev");
        let backups = tmp
            .path()
            .join("dev")
            .parent()
            .unwrap()
            .join(".kairo-backups");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("config.toml"), "original").unwrap();
        kairo_core::health::backup::run_backup(&dev, &backups).unwrap();
        std::fs::write(dev.join("config.toml"), "corrupted").unwrap();

        let date = Utc::now().format("%Y-%m-%d").to_string();
        let resp = rollback(&dev, &date).unwrap();
        assert!(resp.restored_path.ends_with("config.toml"));
        let contents = std::fs::read_to_string(dev.join("config.toml")).unwrap();
        assert_eq!(contents, "original");
    }

    #[test]
    fn test_missing_file_is_error() {
        let tmp = TempDir::new().unwrap();
        let resp = test(tmp.path(), RepairTarget::Vision);
        assert_eq!(resp.status, "error");
        assert_eq!(resp.component, "vision");
    }

    #[test]
    fn escalate_writes_message() {
        let tmp = TempDir::new().unwrap();
        let resp = escalate(tmp.path(), "user must reinstall models").unwrap();
        let contents = std::fs::read_to_string(&resp.intent_file).unwrap();
        assert!(contents.contains("user must reinstall models"));
        assert!(contents.contains("\"kind\": \"escalate\""));
    }
}
