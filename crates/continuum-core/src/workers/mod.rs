//! # Layer 4 — Workers
//!
//! Workers are independent Claude Code sessions spawned by the orchestrator
//! to perform actual work. Each worker gets its own working directory, tool
//! allowlist, model selection, session ID, and log file.
//!
//! The orchestrator does not spawn workers directly — it calls the
//! `mcp__continuum__workers_*` MCP tools. The Continuum MCP server writes an intent
//! file under `~/.continuum-dev/worker-intents/`; the running continuum runtime
//! owns the [`pool::WorkerPool`] and picks the intent up on its next tick.
//!
//! ## Module map
//!
//! - [`types`] — value objects shared across the worker layer (no heavy deps)
//! - [`intent`] — disk protocol between continuum-mcp and the pool (no heavy deps)
//! - [`model_select`] — Auto-mode heuristic for picking Opus vs Sonnet
//! - [`supervisor`] — spawns one claude subprocess, parses stream-json
//!   (runtime feature: needs the orchestrator event types)
//! - [`pool`] — queue, concurrency limit, snapshot publishing, audit hooks
//!   (runtime feature)

pub mod intent;
pub mod model_select;
pub mod types;

#[cfg(feature = "runtime")]
pub mod pool;
#[cfg(feature = "runtime")]
pub mod supervisor;

pub use intent::{WorkerIntent, INTENTS_SUBDIR, SNAPSHOTS_SUBDIR};
pub use model_select::{choose_model, ModelChoice};
pub use types::{
    new_worker_id, WorkerId, WorkerModelTier, WorkerOutcome, WorkerPoolStats, WorkerPriority,
    WorkerSnapshot, WorkerSpec, WorkerStatus,
};

#[cfg(feature = "runtime")]
pub use pool::{EventSink, FinishSink, WorkerPool, WorkerPoolOptions};
#[cfg(feature = "runtime")]
pub use supervisor::{run_worker, SupervisorInput, WorkerEvent};
