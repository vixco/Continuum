//! # Repair tools (`mcp__continuum__repair_*`)
//!
//! Repair-session tools. Restart requests are supported only for the file and
//! process watcher supervisors: the runtime revalidates the short-lived grant,
//! restarts the existing single instance, and publishes a separate verification
//! result. Queueing the request is never itself reported as recovery. Model
//! reinstall, rollback and escalation remain separately guarded compatibility
//! boundaries.
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
    FileWatcher,
    ProcessWatcher,
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
            Self::FileWatcher => "file_watcher",
            Self::ProcessWatcher => "process_watcher",
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

/// Queue an authorization-bound restart request for a runtime supervisor.
/// The response proves only that the bounded request was persisted; recovery
/// is established later by the runtime's verification event.
pub fn restart(
    data_dir: &Path,
    target: RepairTarget,
    authorization: &str,
) -> anyhow::Result<IntentResponse> {
    queue_intent(
        data_dir,
        "restart",
        serde_json::json!({
            "component": target.as_str(),
        }),
        Some(authorization),
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
        None,
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
        RepairTarget::Memory => memory_status(data_dir),
        RepairTarget::ContextWatcher => (
            "unknown".into(),
            Some("context watcher requires a live runtime health probe".into()),
        ),
        RepairTarget::FileWatcher => runtime_component_status(data_dir, "file_watcher"),
        RepairTarget::ProcessWatcher => runtime_component_status(data_dir, "process_watcher"),
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
        None,
    )
}

// --- internals ---

fn file_status(path: PathBuf) -> (String, Option<String>) {
    if path.exists() {
        (
            "healthy".into(),
            Some("configured model artifact is present".into()),
        )
    } else {
        (
            "error".into(),
            Some("configured model artifact is missing".into()),
        )
    }
}

fn runtime_component_status(data_dir: &Path, component: &str) -> (String, Option<String>) {
    let path = data_dir.join("state.json");
    let Ok(bytes) = std::fs::read(path) else {
        return (
            "unknown".into(),
            Some("runtime state is unavailable; no live verification is possible".into()),
        );
    };
    let Ok(snapshot) =
        serde_json::from_slice::<continuum_core::runtime_publish::RuntimeSnapshot>(&bytes)
    else {
        return (
            "error".into(),
            Some("runtime state could not be parsed".into()),
        );
    };
    let summary = snapshot
        .context_engine
        .as_ref()
        .and_then(|engine| match component {
            "file_watcher" => engine.file_watcher.as_ref(),
            "process_watcher" => engine.process_watcher.as_ref(),
            _ => None,
        });
    let Some(summary) = summary else {
        return (
            "unknown".into(),
            Some("component has not published a live health observation".into()),
        );
    };
    let status = if summary.diagnostic.is_none() {
        // Rolling desktop/runtime upgrades can read a snapshot written before
        // the typed state field existed. Preserve the legacy health meaning
        // instead of treating the serde default (`unavailable`) as a new fault.
        if summary.should_restart || !summary.healthy {
            "error"
        } else {
            "healthy"
        }
    } else if summary.state == continuum_core::operational_state::OperationalState::Degraded {
        "degrading"
    } else if summary.state.faulted() {
        "error"
    } else {
        "healthy"
    };
    let note = summary
        .diagnostic
        .as_ref()
        .map(|diagnostic| diagnostic.explanation.clone())
        .or_else(|| Some("live runtime health observation available".into()));
    (status.into(), note)
}

/// Health check for `RepairTarget::Memory`: the memory vault is the
/// authoritative store now (see `docs/memory.md`), so this checks the
/// vault directory exists and has a derived index (`.continuum/index.db`)
/// — both are created by `Vault::open`/`open_with` on first use, so their
/// absence means the vault has never been opened against this data dir (or
/// was deleted). The legacy `semantic.sqlite` file (still read as a
/// fallback by `memory_get_fact`/`memory_list_facts` — see `tools/memory.rs`)
/// is folded in as a cheap secondary note rather than affecting `status`;
/// it is no longer the primary thing being health-checked here.
fn memory_status(data_dir: &Path) -> (String, Option<String>) {
    let vault_dir = data_dir.join("vault");
    let index_path = vault_dir.join(".continuum").join("index.db");
    let (status, mut note) = if !vault_dir.exists() {
        (
            "error".to_string(),
            Some("memory vault directory is missing".to_string()),
        )
    } else if !index_path.exists() {
        (
            "degrading".to_string(),
            Some("memory vault index is missing and can be rebuilt".to_string()),
        )
    } else {
        (
            "healthy".to_string(),
            Some("memory vault and derived index are present".to_string()),
        )
    };

    if data_dir.join("semantic.sqlite").exists() {
        note = Some(format!(
            "{}; legacy semantic store is also present",
            note.unwrap_or_default()
        ));
    }

    (status, note)
}

fn queue_intent(
    data_dir: &Path,
    kind: &str,
    body: serde_json::Value,
    authorization: Option<&str>,
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
        "authorization": authorization,
        "body": body,
    });
    std::fs::write(&temp, serde_json::to_string_pretty(&payload)?)?;
    std::fs::rename(&temp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    Ok(IntentResponse {
        // Public tool responses never reveal the local Continuum data path.
        intent_file: filename,
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
        let token = uuid::Uuid::new_v4().to_string();
        let resp = restart(tmp.path(), RepairTarget::FileWatcher, &token).unwrap();
        let contents =
            std::fs::read_to_string(tmp.path().join("repair-intents").join(&resp.intent_file))
                .unwrap();
        assert!(contents.contains("\"kind\": \"restart\""));
        assert!(contents.contains("\"component\": \"file_watcher\""));
        assert!(contents.contains(&token));
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
    fn memory_status_errors_when_vault_dir_missing() {
        let tmp = TempDir::new().unwrap();
        let resp = test(tmp.path(), RepairTarget::Memory);
        assert_eq!(resp.status, "error");
        assert!(resp.note.unwrap().contains("missing"));
    }

    #[test]
    fn memory_status_degrading_when_index_missing() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("vault")).unwrap();
        let resp = test(tmp.path(), RepairTarget::Memory);
        assert_eq!(resp.status, "degrading");
    }

    #[test]
    fn memory_status_healthy_when_vault_and_index_present() {
        let tmp = TempDir::new().unwrap();
        let continuum_dir = tmp.path().join("vault").join(".continuum");
        std::fs::create_dir_all(&continuum_dir).unwrap();
        std::fs::write(continuum_dir.join("index.db"), b"").unwrap();
        let resp = test(tmp.path(), RepairTarget::Memory);
        assert_eq!(resp.status, "healthy");
    }

    #[test]
    fn memory_status_notes_legacy_semantic_sqlite_without_changing_status() {
        let tmp = TempDir::new().unwrap();
        let continuum_dir = tmp.path().join("vault").join(".continuum");
        std::fs::create_dir_all(&continuum_dir).unwrap();
        std::fs::write(continuum_dir.join("index.db"), b"").unwrap();
        std::fs::write(tmp.path().join("semantic.sqlite"), b"").unwrap();
        let resp = test(tmp.path(), RepairTarget::Memory);
        assert_eq!(resp.status, "healthy");
        assert!(resp.note.unwrap().contains("semantic.sqlite"));
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
        let contents =
            std::fs::read_to_string(tmp.path().join("repair-intents").join(&resp.intent_file))
                .unwrap();
        assert!(contents.contains("user must reinstall models"));
        assert!(contents.contains("\"kind\": \"escalate\""));
    }
}
