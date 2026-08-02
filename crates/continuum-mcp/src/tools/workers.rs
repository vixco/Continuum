//! # Worker tools (`mcp__continuum__workers_*`)
//!
//! The orchestrator calls these to spawn Claude Code workers, poll their
//! status, wait for completion, and cancel them. The MCP server does NOT
//! spawn workers itself — it translates every call into a disk-backed
//! intent that the running `continuum` runtime picks up on its next tick.
//!
//! The pool on the runtime side publishes `<data_dir>/workers/<id>.json`
//! snapshots, which this module reads for status / list / wait.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::Utc;
use continuum_core::workers::intent::{list_snapshots, read_snapshot, write_intent, WorkerIntent};
use continuum_core::workers::types::{
    new_worker_id, WorkerModelTier, WorkerPriority, WorkerSnapshot, WorkerSpec, WorkerStatus,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Arguments for `spawn_worker`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SpawnWorkerRequest {
    /// The prompt sent to the worker as its user message.
    pub task: String,
    /// Absolute path to the worker's working directory. Must exist.
    pub cwd: String,
    /// Optional model hint: `"auto"`, `"budget"`, `"power"`, or an explicit
    /// `"claude-*"` id. Defaults to `"auto"`.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional allowlist override (CSV). Empty = pool default.
    #[serde(default)]
    pub allowed_tools: Option<String>,
    /// Optional per-spawn timeout in seconds. Empty = pool default.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Optional priority tag: `"scheduled"`, `"orchestrator"`, or `"user"`.
    #[serde(default)]
    pub priority: Option<String>,
    /// Free-form label for the audit trail (`"daily-briefing"`, `"user"`, …).
    #[serde(default)]
    pub requested_by: Option<String>,
    /// Optional skill names to force-include in the worker's prompt.
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    /// Optional tag list threaded through to episodic memory.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

/// Response from `spawn_worker`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct SpawnWorkerResponse {
    pub worker_id: String,
    pub queued_at: String,
    pub note: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WorkerIdRequest {
    pub worker_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WorkerWaitRequest {
    pub worker_id: String,
    /// Maximum seconds to block waiting for a terminal state. The MCP call
    /// returns the current snapshot even if the worker hasn't finished,
    /// clamped to 300 seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WorkerListRequest {
    /// Max snapshots to return. Clamped to 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// If set, filter by status (e.g. `"running"`, `"completed"`).
    #[serde(default)]
    pub status: Option<String>,
}

/// Compact view of a worker snapshot returned to Claude Code.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkerSnapshotView {
    pub id: String,
    pub task: String,
    pub cwd: String,
    pub model: String,
    pub model_reason: String,
    pub status: String,
    pub priority: String,
    pub skills: Vec<String>,
    pub tags: Vec<String>,
    pub elapsed_ms: u64,
    pub progress: f32,
    pub tool_calls: u32,
    pub cost_usd: Option<f64>,
    pub last_line: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub queued_at: String,
}

impl From<WorkerSnapshot> for WorkerSnapshotView {
    fn from(s: WorkerSnapshot) -> Self {
        Self {
            id: s.id,
            task: s.task,
            cwd: s.cwd,
            model: s.model,
            model_reason: s.model_reason,
            status: s.status.as_str().into(),
            priority: s.priority.as_str().into(),
            skills: s.skills,
            tags: s.tags,
            elapsed_ms: s.elapsed_ms,
            progress: s.progress,
            tool_calls: s.tool_calls,
            cost_usd: s.cost_usd,
            last_line: s.last_line,
            result: s.result,
            error: s.error,
            started_at: s.started_at.map(|t| t.to_rfc3339()),
            finished_at: s.finished_at.map(|t| t.to_rfc3339()),
            queued_at: s.queued_at.to_rfc3339(),
        }
    }
}

/// Writes a `Spawn` intent and returns the pre-allocated worker id.
pub fn spawn(data_dir: &Path, req: SpawnWorkerRequest) -> Result<SpawnWorkerResponse> {
    if req.task.trim().is_empty() {
        return Err(anyhow!("task must not be empty"));
    }
    if req.cwd.trim().is_empty() {
        return Err(anyhow!("cwd must not be empty"));
    }
    let cwd = PathBuf::from(&req.cwd);
    let id = new_worker_id();
    let spec = WorkerSpec {
        task: req.task.clone(),
        cwd,
        model: req
            .model
            .as_deref()
            .map(WorkerModelTier::from_hint)
            .unwrap_or(WorkerModelTier::Auto),
        allowed_tools: req.allowed_tools.clone(),
        priority: parse_priority(req.priority.as_deref()),
        timeout_secs: req.timeout_secs,
        requested_by: req.requested_by.clone(),
        skills: req.skills.clone().unwrap_or_default(),
        tags: req.tags.clone().unwrap_or_default(),
        metadata: std::collections::HashMap::new(),
    };
    let path = write_intent(data_dir, &WorkerIntent::spawn(id.clone(), spec))?;
    Ok(SpawnWorkerResponse {
        worker_id: id,
        queued_at: Utc::now().to_rfc3339(),
        note: format!(
            "Spawn intent queued at {}. The runtime will pick it up on its next \
             tick (< 1 s). Poll worker_status or call worker_wait for the result.",
            path.display()
        ),
    })
}

/// Returns the current snapshot for a worker, or a synthetic "unknown"
/// snapshot if the runtime hasn't processed the spawn intent yet.
pub fn status(data_dir: &Path, id: &str) -> Result<WorkerSnapshotView> {
    if let Some(snap) = read_snapshot(data_dir, id)? {
        return Ok(snap.into());
    }
    // Hasn't materialised yet: return a stub so Claude can retry.
    Ok(WorkerSnapshotView {
        id: id.into(),
        task: String::new(),
        cwd: String::new(),
        model: String::new(),
        model_reason: String::new(),
        status: "pending".into(),
        priority: "orchestrator".into(),
        skills: Vec::new(),
        tags: Vec::new(),
        elapsed_ms: 0,
        progress: 0.0,
        tool_calls: 0,
        cost_usd: None,
        last_line: "runtime has not processed the spawn intent yet — retry shortly".into(),
        result: None,
        error: None,
        started_at: None,
        finished_at: None,
        queued_at: Utc::now().to_rfc3339(),
    })
}

/// Writes a cancel intent. Returns the latest snapshot (if any).
pub fn cancel(data_dir: &Path, id: &str) -> Result<WorkerSnapshotView> {
    write_intent(data_dir, &WorkerIntent::Cancel { id: id.to_string() })?;
    status(data_dir, id)
}

/// Blocks until the worker reaches a terminal state or the timeout elapses.
/// Polls the snapshot file at 250 ms intervals — cheap, filesystem-bound.
pub async fn wait(data_dir: &Path, id: &str, timeout_secs: u64) -> Result<WorkerSnapshotView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs.clamp(1, 300));
    let poll = Duration::from_millis(250);
    loop {
        if let Some(snap) = read_snapshot(data_dir, id)? {
            if snap.status.is_terminal() {
                return Ok(snap.into());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return status(data_dir, id);
        }
        tokio::time::sleep(poll).await;
    }
}

pub fn list(data_dir: &Path, req: WorkerListRequest) -> Result<Vec<WorkerSnapshotView>> {
    let limit = req.limit.unwrap_or(25).clamp(1, 100) as usize;
    let status_filter = req.status.as_deref().map(|s| s.to_ascii_lowercase());

    let mut out: Vec<WorkerSnapshotView> = list_snapshots(data_dir)?
        .into_iter()
        .filter(|s| {
            status_filter
                .as_deref()
                .map(|f| s.status.as_str() == f)
                .unwrap_or(true)
        })
        .map(WorkerSnapshotView::from)
        .collect();
    out.truncate(limit);
    Ok(out)
}

fn parse_priority(s: Option<&str>) -> WorkerPriority {
    match s.map(|v| v.to_ascii_lowercase()).as_deref() {
        Some("user") | Some("user_requested") => WorkerPriority::UserRequested,
        Some("scheduled") => WorkerPriority::Scheduled,
        Some("orchestrator") | Some("orchestrator_spawned") | None | Some("") => {
            WorkerPriority::OrchestratorSpawned
        }
        Some(_) => WorkerPriority::OrchestratorSpawned,
    }
}

/// Guard against `worker_status` swallowing unknown status strings.
#[allow(dead_code)]
fn _terminal_invariant() {
    let _: [WorkerStatus; 4] = [
        WorkerStatus::Completed,
        WorkerStatus::Failed,
        WorkerStatus::Cancelled,
        WorkerStatus::TimedOut,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn spawn_writes_intent_file() {
        let tmp = TempDir::new().unwrap();
        let resp = spawn(
            tmp.path(),
            SpawnWorkerRequest {
                task: "do stuff".into(),
                cwd: tmp.path().to_string_lossy().into(),
                model: Some("power".into()),
                allowed_tools: None,
                timeout_secs: None,
                priority: Some("user".into()),
                requested_by: Some("tests".into()),
                skills: None,
                tags: Some(vec!["smoke".into()]),
            },
        )
        .unwrap();
        assert!(!resp.worker_id.is_empty());
        // The intent file lives in the worker-intents subdir.
        let intents = tmp.path().join("worker-intents");
        let files: Vec<_> = std::fs::read_dir(&intents)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn spawn_rejects_empty_task() {
        let tmp = TempDir::new().unwrap();
        let err = spawn(
            tmp.path(),
            SpawnWorkerRequest {
                task: "   ".into(),
                cwd: tmp.path().to_string_lossy().into(),
                model: None,
                allowed_tools: None,
                timeout_secs: None,
                priority: None,
                requested_by: None,
                skills: None,
                tags: None,
            },
        );
        assert!(err.is_err());
    }

    #[test]
    fn status_pending_when_no_snapshot() {
        let tmp = TempDir::new().unwrap();
        let view = status(tmp.path(), "does-not-exist").unwrap();
        assert_eq!(view.status, "pending");
    }

    #[test]
    fn cancel_writes_cancel_intent() {
        let tmp = TempDir::new().unwrap();
        cancel(tmp.path(), "w-1").unwrap();
        let intents = tmp.path().join("worker-intents");
        let files: Vec<_> = std::fs::read_dir(&intents)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        let name = files[0].file_name().unwrap().to_string_lossy().to_string();
        assert!(name.ends_with("-cancel.json"));
    }

    #[test]
    fn list_returns_only_matching_status() {
        let tmp = TempDir::new().unwrap();
        // Seed two snapshots directly.
        let spec = WorkerSpec::new("x", tmp.path());
        let mut a = WorkerSnapshot::queued("a".into(), &spec, "m".into(), "r".into());
        a.status = WorkerStatus::Running;
        let mut b = WorkerSnapshot::queued("b".into(), &spec, "m".into(), "r".into());
        b.status = WorkerStatus::Completed;
        continuum_core::workers::intent::write_snapshot(tmp.path(), &a).unwrap();
        continuum_core::workers::intent::write_snapshot(tmp.path(), &b).unwrap();

        let running = list(
            tmp.path(),
            WorkerListRequest {
                limit: None,
                status: Some("running".into()),
            },
        )
        .unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].status, "running");
    }

    #[tokio::test]
    async fn wait_returns_completed_when_snapshot_exists() {
        let tmp = TempDir::new().unwrap();
        let spec = WorkerSpec::new("x", tmp.path());
        let mut snap = WorkerSnapshot::queued("w-1".into(), &spec, "m".into(), "r".into());
        snap.status = WorkerStatus::Completed;
        continuum_core::workers::intent::write_snapshot(tmp.path(), &snap).unwrap();
        let view = wait(tmp.path(), "w-1", 1).await.unwrap();
        assert_eq!(view.status, "completed");
    }
}
