//! # Repair tools (`mcp__continuum__repair_*`)
//!
//! Published compatibility tools for repair sessions. The restart, reinstall,
//! and escalation helpers can write intent files under
//! `~/.continuum-dev/repair-intents/`, but this release has no runtime consumer
//! for those files. The guarded Health flow therefore denies those tools and
//! performs its one supported mutation (starting an offline runtime) directly
//! in the desktop after preview and backup authorization.
//!
//! For `rollback_config` and `test_component` we can answer directly from
//! the MCP process because the work is pure disk I/O.
//!
//! `escalate` also remains only a compatibility intent writer. Manual actions
//! are surfaced truthfully in the repair agent's streamed output.

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
    /// Backup folder name under `~/.continuum-backups/` — format `YYYY-MM-DD`.
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

/// Queue a legacy restart intent. No consumer exists in this release.
pub fn restart(data_dir: &Path, target: RepairTarget) -> anyhow::Result<IntentResponse> {
    queue_intent(
        data_dir,
        "restart",
        serde_json::json!({
            "component": target.as_str(),
        }),
    )
}

/// Queue a legacy model reinstall intent. No consumer exists in this release.
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
    let restored = continuum_core::health::repair::rollback_config(data_dir, &backups_dir, date)?;
    Ok(RollbackResponse {
        restored_path: restored.display().to_string(),
    })
}

/// Run the component's light-weight file-presence check. For the full
/// health probe this gives Claude only a quick sanity check; it is not proof
/// of a live restart or recovery.
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
            if std::path::Path::new("target/release/continuum-mcp.exe").exists()
                || std::path::Path::new("target/release/continuum-mcp").exists()
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

/// Write a legacy escalation intent. No dashboard consumer exists currently.
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
    // Millisecond precision + short nonce — the orchestrator can legitimately
    // fire two intents within the same millisecond, and a collision would
    // silently overwrite the earlier one.
    let ts = Utc::now().format("%Y%m%dT%H%M%S%3f").to_string();
    let nonce = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static INTENT_COUNTER: AtomicU64 = AtomicU64::new(0);
        INTENT_COUNTER.fetch_add(1, Ordering::Relaxed)
    };
    let filename = format!("{ts}-{kind}-{nonce:06x}.json");
    let path = intents_dir.join(&filename);
    let temp = intents_dir.join(format!(".{filename}.tmp"));
    let payload = serde_json::json!({
        "kind": kind,
        "queued_at": Utc::now().to_rfc3339(),
        "body": body,
    });
    std::fs::write(&temp, serde_json::to_string_pretty(&payload)?)?;
    std::fs::rename(&temp, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        error
    })?;
    Ok(IntentResponse {
        intent_file: path.display().to_string(),
        queued_at: Utc::now().to_rfc3339(),
    })
}

pub(crate) fn backups_dir_for(data_dir: &Path) -> PathBuf {
    data_dir
        .parent()
        .map(|p| p.join(".continuum-backups"))
        .unwrap_or_else(|| data_dir.join(".continuum-backups"))
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
            .join(".continuum-backups");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("config.toml"), "[screen]\ninterval_secs = 4\n").unwrap();
        continuum_core::health::backup::run_backup(&dev, &backups).unwrap();
        std::fs::write(dev.join("config.toml"), "[screen]\ninterval_secs = 8\n").unwrap();

        let date = Utc::now().format("%Y-%m-%d").to_string();
        let resp = rollback(&dev, &date).unwrap();
        assert!(resp.restored_path.ends_with("config.toml"));
        let contents = std::fs::read_to_string(dev.join("config.toml")).unwrap();
        assert_eq!(contents, "[screen]\ninterval_secs = 4\n");
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
