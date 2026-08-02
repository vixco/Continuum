//! # Worker types
//!
//! Shared value types for the worker pool, supervisor, and intent-file
//! protocol. The goal is that a single `WorkerSpec` describes everything
//! the supervisor needs to spawn a Claude Code session, and a single
//! `WorkerSnapshot` captures everything an observer (dashboard, MCP
//! server, repair agent) needs to know about it.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::WorkersConfig;

/// Opaque identifier for a worker. A plain UUID string so it round-trips
/// through JSON unchanged and can be used as a filename.
pub type WorkerId = String;

/// Priority used by the queue. Higher variants pre-empt lower ones when the
/// pool picks the next queued worker to start.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPriority {
    /// Kicked off by a scheduler or automation — lowest priority.
    Scheduled = 0,
    /// Spawned during an orchestrator wake — the common case.
    #[default]
    OrchestratorSpawned = 1,
    /// Directly requested by the user via the dashboard — wins over the rest.
    UserRequested = 2,
}

impl WorkerPriority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::OrchestratorSpawned => "orchestrator",
            Self::UserRequested => "user",
        }
    }
}

/// Which model tier the caller requested. The pool's model selector turns
/// this into an explicit model id at spawn time.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerModelTier {
    /// Let the heuristic decide based on the task text + config mode.
    #[default]
    Auto,
    /// Force the budget model (Sonnet by default).
    Budget,
    /// Force the power model (Opus by default).
    Power,
    /// Use an explicit model id (e.g. `"claude-opus-4-6"`).
    Explicit(String),
}

impl WorkerModelTier {
    /// Parse the orchestrator's string hint into a tier. Unknown strings fall
    /// back to `Auto`, which is safer than guessing.
    pub fn from_hint(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "auto" | "" => Self::Auto,
            "budget" | "sonnet" => Self::Budget,
            "power" | "opus" => Self::Power,
            other if other.starts_with("claude-") => Self::Explicit(other.to_string()),
            _ => Self::Auto,
        }
    }
}

/// Inputs for a new worker. Mirrors the `spawn_worker` MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSpec {
    /// The prompt sent to the worker as its user message.
    pub task: String,
    /// Working directory for the worker. Must exist at spawn time.
    pub cwd: PathBuf,
    /// Model tier to use.
    #[serde(default)]
    pub model: WorkerModelTier,
    /// Tool allowlist (CSV, claude CLI format). Empty = pool default.
    #[serde(default)]
    pub allowed_tools: Option<String>,
    /// Priority hint for the queue.
    #[serde(default)]
    pub priority: WorkerPriority,
    /// Wall-clock timeout in seconds. `None` → pool default.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Free-form label (e.g. `"orchestrator"`, `"daily-briefing"`, `"user"`).
    #[serde(default)]
    pub requested_by: Option<String>,
    /// Active skill names to inject into the worker's system prompt. Empty
    /// when no skill matched or the skills system is disabled.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Optional tag list threaded through to episodic memory for audit filters.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Free-form metadata the caller wants echoed back through the snapshot.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl WorkerSpec {
    /// Convenience constructor.
    pub fn new(task: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            task: task.into(),
            cwd: cwd.into(),
            model: WorkerModelTier::Auto,
            allowed_tools: None,
            priority: WorkerPriority::default(),
            timeout_secs: None,
            requested_by: None,
            skills: Vec::new(),
            tags: Vec::new(),
            metadata: HashMap::new(),
        }
    }
}

/// Lifecycle status of a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Queued,
    Starting,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl WorkerStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }
}

/// A compact, publicly observable view of a worker at a moment in time.
/// Written atomically to `~/.continuum-dev/workers/<id>.json` by the runtime and
/// read by the MCP server + dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSnapshot {
    pub id: WorkerId,
    pub task: String,
    pub cwd: String,
    pub model: String,
    pub model_reason: String,
    pub status: WorkerStatus,
    pub priority: WorkerPriority,
    pub requested_by: Option<String>,
    pub skills: Vec<String>,
    pub tags: Vec<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub queued_at: DateTime<Utc>,
    pub elapsed_ms: u64,
    pub progress: f32,
    pub last_line: String,
    pub tool_calls: u32,
    pub cost_usd: Option<f64>,
    pub session_id: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
}

impl WorkerSnapshot {
    /// Initial queued snapshot — everything else fills in as the pool moves
    /// the worker through its lifecycle.
    pub fn queued(id: WorkerId, spec: &WorkerSpec, model: String, model_reason: String) -> Self {
        Self {
            id,
            task: spec.task.clone(),
            cwd: spec.cwd.to_string_lossy().into_owned(),
            model,
            model_reason,
            status: WorkerStatus::Queued,
            priority: spec.priority,
            requested_by: spec.requested_by.clone(),
            skills: spec.skills.clone(),
            tags: spec.tags.clone(),
            started_at: None,
            finished_at: None,
            queued_at: Utc::now(),
            elapsed_ms: 0,
            progress: 0.0,
            last_line: String::new(),
            tool_calls: 0,
            cost_usd: None,
            session_id: None,
            result: None,
            error: None,
        }
    }
}

/// Final outcome returned by the supervisor. The pool uses this to decide
/// whether to mark the worker `completed` or `failed`, and to stamp cost /
/// result fields in the snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerOutcome {
    pub status: WorkerStatus,
    pub result_text: String,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub tool_calls: u32,
    pub error: Option<String>,
}

impl WorkerOutcome {
    pub fn failed(err: impl Into<String>) -> Self {
        Self {
            status: WorkerStatus::Failed,
            result_text: String::new(),
            session_id: None,
            cost_usd: None,
            duration_ms: None,
            tool_calls: 0,
            error: Some(err.into()),
        }
    }
}

/// Fresh UUID, hyphen-preserved so filenames and logs stay identical.
pub fn new_worker_id() -> WorkerId {
    Uuid::new_v4().to_string()
}

/// Aggregated pool stats the dashboard renders on the Home tab.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkerPoolStats {
    pub active: usize,
    pub queued: usize,
    pub completed_today: u64,
    pub failed_today: u64,
    pub total_cost_usd_today: f64,
    pub max_concurrent: usize,
}

impl WorkerPoolStats {
    pub fn from_config(cfg: &WorkersConfig) -> Self {
        Self {
            max_concurrent: cfg.max_concurrent,
            ..Default::default()
        }
    }
}
