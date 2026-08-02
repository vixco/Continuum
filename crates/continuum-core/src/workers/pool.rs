//! # Worker pool
//!
//! Owns every worker Continuum knows about. Enforces `workers.max_concurrent`,
//! orders the waiting list by `WorkerPriority` (then FIFO within a tier),
//! dispatches a supervisor per active slot, and surfaces snapshots to the
//! dashboard + MCP.
//!
//! The pool deliberately keeps all per-worker mutation behind a single
//! `Mutex<PoolInner>` so snapshot writes, queue adjustments, and the cancel
//! dispatcher always see a consistent world. The surface area that needs to
//! be serialisation-fast is small (the tick loop, `submit`, `cancel`,
//! `status`, `list`, `wait`), and inside that surface we never hold the
//! lock across `.await` — every await point takes a fresh lock.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use tokio::sync::{oneshot, watch, Mutex, Notify};
use tracing::{info, warn};

use crate::config::env_or_legacy;

use crate::config::WorkersConfig;
use crate::skills::{MatchContext, Skill, SkillLoader, SkillMatcher};

use super::intent::{self, WorkerIntent};
use super::model_select::choose_model;
use super::supervisor::{run_worker, SupervisorInput, WorkerEvent};
use super::types::{
    new_worker_id, WorkerId, WorkerOutcome, WorkerPoolStats, WorkerSnapshot, WorkerSpec,
    WorkerStatus,
};

/// Callback invoked on every `WorkerEvent` emitted by a supervisor. The
/// runtime passes one of these so it can mirror worker activity into
/// episodic memory without the pool depending on the memory module.
pub type EventSink = Arc<dyn Fn(&WorkerId, WorkerEvent) + Send + Sync + 'static>;

/// Optional hook called whenever a worker reaches a terminal state, so the
/// runtime can write a single audit summary to episodic memory.
pub type FinishSink = Arc<dyn Fn(&WorkerSnapshot) + Send + Sync + 'static>;

/// Shared, cloneable handle to the worker pool.
#[derive(Clone)]
pub struct WorkerPool {
    inner: Arc<Mutex<PoolInner>>,
    tick: Arc<Notify>,
    config: Arc<Mutex<WorkersConfig>>,
    data_dir: PathBuf,
    claude_bin: String,
    event_sink: Option<EventSink>,
    finish_sink: Option<FinishSink>,
    skill_loader: Option<SkillLoader>,
    mcp_config_path: Option<PathBuf>,
    base_system_prompt: Option<String>,
    skill_token_budget: usize,
}

/// Options for configuring a fresh [`WorkerPool`].
#[derive(Clone)]
pub struct WorkerPoolOptions {
    pub config: WorkersConfig,
    pub data_dir: PathBuf,
    pub claude_bin: String,
    /// When set, the pool asks the loader for active skills at launch time
    /// and appends matched skill content to the worker's system prompt.
    pub skill_loader: Option<SkillLoader>,
    /// Approximate token budget for injected skill content. Defaults to 2000.
    pub skill_token_budget: usize,
    /// MCP config file path passed to the worker's `--mcp-config` flag.
    /// `None` disables MCP for workers (useful for tests + worker_demo).
    pub mcp_config_path: Option<PathBuf>,
    /// Text prepended to every worker's system prompt (before skills).
    /// Typically loaded from `prompts/worker-system.md`.
    pub base_system_prompt: Option<String>,
}

impl WorkerPoolOptions {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            config: WorkersConfig::default(),
            data_dir,
            claude_bin: "claude".into(),
            skill_loader: None,
            skill_token_budget: 2000,
            mcp_config_path: None,
            base_system_prompt: None,
        }
    }
}

struct PoolInner {
    queued: VecDeque<QueuedWorker>,
    active: HashMap<WorkerId, ActiveWorker>,
    snapshots: HashMap<WorkerId, WorkerSnapshot>,
    waiters: HashMap<WorkerId, Vec<oneshot::Sender<WorkerSnapshot>>>,
    completed_today: u64,
    failed_today: u64,
    total_cost_today: f64,
    recent_failures: VecDeque<(Instant, String)>,
    shutdown: bool,
}

struct QueuedWorker {
    id: WorkerId,
    spec: WorkerSpec,
    snapshot: WorkerSnapshot,
    seq: u64,
}

struct ActiveWorker {
    cancel_tx: Option<oneshot::Sender<()>>,
    started_at: Instant,
}

impl WorkerPool {
    /// Build a fresh pool. Does not spawn any background tasks — call
    /// [`WorkerPool::spawn_background`] for that.
    pub fn new(opts: WorkerPoolOptions) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PoolInner {
                queued: VecDeque::new(),
                active: HashMap::new(),
                snapshots: HashMap::new(),
                waiters: HashMap::new(),
                completed_today: 0,
                failed_today: 0,
                total_cost_today: 0.0,
                recent_failures: VecDeque::new(),
                shutdown: false,
            })),
            tick: Arc::new(Notify::new()),
            config: Arc::new(Mutex::new(opts.config)),
            data_dir: opts.data_dir,
            claude_bin: opts.claude_bin,
            event_sink: None,
            finish_sink: None,
            skill_loader: opts.skill_loader,
            mcp_config_path: opts.mcp_config_path,
            base_system_prompt: opts.base_system_prompt,
            skill_token_budget: if opts.skill_token_budget == 0 {
                2000
            } else {
                opts.skill_token_budget
            },
        }
    }

    /// Attach a streaming callback (tool calls, text deltas, etc).
    pub fn with_event_sink(mut self, sink: EventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Attach a callback fired once per worker on terminal state.
    pub fn with_finish_sink(mut self, sink: FinishSink) -> Self {
        self.finish_sink = Some(sink);
        self
    }

    /// Update the pool config. The next tick picks up the new limits.
    pub async fn set_config(&self, cfg: WorkersConfig) {
        let mut guard = self.config.lock().await;
        *guard = cfg;
        self.tick.notify_waiters();
    }

    /// Queues a worker immediately. The returned id is allocated now; the
    /// worker may sit in the queue if the pool is at capacity.
    pub async fn submit(&self, spec: WorkerSpec) -> Result<WorkerId> {
        let id = new_worker_id();
        self.submit_with_id(id.clone(), spec).await?;
        Ok(id)
    }

    /// Queues a worker with an explicit id — used when the caller (usually
    /// the MCP server) already returned the id to the orchestrator.
    pub async fn submit_with_id(&self, id: WorkerId, spec: WorkerSpec) -> Result<()> {
        let cfg = self.config.lock().await.clone();
        let choice = choose_model(&cfg, &spec.model, &spec.task);
        let snapshot = WorkerSnapshot::queued(
            id.clone(),
            &spec,
            choice.model.clone(),
            choice.reason.clone(),
        );

        intent::write_snapshot(&self.data_dir, &snapshot).ok();

        let mut guard = self.inner.lock().await;
        if guard.shutdown {
            return Err(anyhow::anyhow!("worker pool has shut down"));
        }
        if guard.snapshots.contains_key(&id) {
            // Re-submit of an already-known id — treat as idempotent.
            return Ok(());
        }
        let seq = guard.completed_today
            + guard.failed_today
            + guard.active.len() as u64
            + guard.queued.len() as u64;

        guard.snapshots.insert(id.clone(), snapshot.clone());
        guard.queued.push_back(QueuedWorker {
            id: id.clone(),
            spec,
            snapshot,
            seq,
        });

        info!(
            layer = "workers",
            component = "pool",
            worker_id = %id,
            model = %choice.model,
            queued = guard.queued.len(),
            active = guard.active.len(),
            "Worker queued"
        );
        drop(guard);
        self.tick.notify_waiters();
        Ok(())
    }

    /// Cancel a worker by id. Returns `true` if the worker was queued or
    /// running (and thus actually cancelled), `false` if unknown or already
    /// terminal.
    pub async fn cancel(&self, id: &str) -> bool {
        let mut guard = self.inner.lock().await;

        // Queued: drop from the queue and mark the snapshot cancelled.
        if let Some(pos) = guard.queued.iter().position(|q| q.id == id) {
            let queued = guard.queued.remove(pos).unwrap();
            let mut snap = queued.snapshot;
            snap.status = WorkerStatus::Cancelled;
            snap.finished_at = Some(Utc::now());
            snap.error = Some("cancelled before start".into());
            guard.snapshots.insert(id.to_string(), snap.clone());
            intent::write_snapshot(&self.data_dir, &snap).ok();
            notify_waiters(&mut guard, id, &snap);
            return true;
        }

        // Active: signal cancel — supervisor flips the snapshot.
        if let Some(active) = guard.active.get_mut(id) {
            if let Some(tx) = active.cancel_tx.take() {
                let _ = tx.send(());
            }
            return true;
        }

        false
    }

    /// Signal shutdown. New submits fail, running workers are cancelled.
    pub async fn shutdown(&self) {
        let (ids, drained): (Vec<WorkerId>, Vec<QueuedWorker>) = {
            let mut guard = self.inner.lock().await;
            guard.shutdown = true;
            let ids = guard.active.keys().cloned().collect::<Vec<_>>();
            let drained: Vec<QueuedWorker> = guard.queued.drain(..).collect();
            (ids, drained)
        };
        for q in drained {
            let mut snap = q.snapshot;
            snap.status = WorkerStatus::Cancelled;
            snap.finished_at = Some(Utc::now());
            snap.error = Some("pool shutdown".into());
            {
                let mut guard = self.inner.lock().await;
                guard.snapshots.insert(q.id.clone(), snap.clone());
                notify_waiters(&mut guard, &q.id, &snap);
            }
            intent::write_snapshot(&self.data_dir, &snap).ok();
        }
        for id in ids {
            self.cancel(&id).await;
        }
        self.tick.notify_waiters();
    }

    /// Current snapshot for one worker.
    pub async fn status(&self, id: &str) -> Option<WorkerSnapshot> {
        let guard = self.inner.lock().await;
        guard.snapshots.get(id).cloned()
    }

    /// All known snapshots (queued + running + recent terminal), newest first.
    pub async fn list(&self) -> Vec<WorkerSnapshot> {
        let guard = self.inner.lock().await;
        let mut out: Vec<_> = guard.snapshots.values().cloned().collect();
        out.sort_by(|a, b| b.queued_at.cmp(&a.queued_at));
        out
    }

    /// Aggregate stats for the dashboard.
    pub async fn stats(&self) -> WorkerPoolStats {
        let cfg = self.config.lock().await.clone();
        let guard = self.inner.lock().await;
        WorkerPoolStats {
            active: guard.active.len(),
            queued: guard.queued.len(),
            completed_today: guard.completed_today,
            failed_today: guard.failed_today,
            total_cost_usd_today: guard.total_cost_today,
            max_concurrent: cfg.max_concurrent.clamp(1, 10),
        }
    }

    /// Wait for a worker to reach a terminal state. Returns immediately if
    /// the worker is already terminal or unknown. Resolves with the final
    /// snapshot, or `None` if no such id exists.
    pub async fn wait(&self, id: &str, timeout: Option<Duration>) -> Option<WorkerSnapshot> {
        let rx = {
            let mut guard = self.inner.lock().await;
            if let Some(snap) = guard.snapshots.get(id) {
                if snap.status.is_terminal() {
                    return Some(snap.clone());
                }
            } else {
                return None;
            }
            let (tx, rx) = oneshot::channel();
            guard.waiters.entry(id.to_string()).or_default().push(tx);
            rx
        };
        match timeout {
            Some(d) => tokio::time::timeout(d, rx).await.ok().and_then(|r| r.ok()),
            None => rx.await.ok(),
        }
    }

    /// Drain any pending intent files and merge them into the pool.
    pub async fn process_intents(&self) -> Result<()> {
        let intents = intent::drain_intents(&self.data_dir)?;
        for (_, intent) in intents {
            match intent {
                WorkerIntent::Spawn { id, spec } => {
                    if let Err(e) = self.submit_with_id(id.clone(), *spec).await {
                        warn!(
                            layer = "workers",
                            component = "pool",
                            worker_id = %id,
                            error = %e,
                            "Failed to accept spawn intent"
                        );
                    }
                }
                WorkerIntent::Cancel { id } => {
                    self.cancel(&id).await;
                }
            }
        }
        Ok(())
    }

    /// Spawn the long-running background task. Returns a shutdown signal
    /// clone-sharable via tokio `watch`.
    pub fn spawn_background(
        &self,
        mut shutdown: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        let pool = self.clone();
        tokio::spawn(async move {
            loop {
                // Drain intent files once per tick.
                let _ = pool.process_intents().await;
                // Try to launch as many workers as there is room for.
                pool.try_launch_loop().await;

                let refresh = {
                    let cfg = pool.config.lock().await;
                    Duration::from_millis(cfg.status_refresh_ms.max(100))
                };

                tokio::select! {
                    _ = pool.tick.notified() => {},
                    _ = tokio::time::sleep(refresh) => {},
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            pool.shutdown().await;
                            break;
                        }
                    }
                }
            }
        })
    }

    /// Launch workers until every available slot is full.
    async fn try_launch_loop(&self) {
        loop {
            let cfg = self.config.lock().await.clone();
            let mut guard = self.inner.lock().await;
            if guard.shutdown {
                return;
            }
            if guard.active.len() >= cfg.max_concurrent.clamp(1, 10) {
                return;
            }
            let Some(queued) = pop_highest_priority(&mut guard.queued) else {
                return;
            };
            drop(guard);
            self.launch(queued, &cfg).await;
        }
    }

    async fn launch(&self, queued: QueuedWorker, cfg: &WorkersConfig) {
        let QueuedWorker {
            id,
            spec,
            mut snapshot,
            ..
        } = queued;

        // Refusal: has this task pattern failed too often recently?
        if self.should_refuse_pattern(&spec.task).await {
            snapshot.status = WorkerStatus::Failed;
            snapshot.error = Some(
                "refused: task pattern has failed repeatedly (failure_streak_limit reached)".into(),
            );
            snapshot.finished_at = Some(Utc::now());
            self.finalise_snapshot(id.clone(), snapshot).await;
            return;
        }

        // Build the per-worker system prompt: base prompt + matched skills.
        let (matched_skill_names, prompt_path) = self.assemble_worker_prompt(&id, &spec).await;
        if !matched_skill_names.is_empty() {
            snapshot.skills = matched_skill_names.clone();
        }

        snapshot.status = WorkerStatus::Starting;
        snapshot.started_at = Some(Utc::now());
        self.store_snapshot(&id, snapshot.clone()).await;

        let timeout_secs = spec.timeout_secs.unwrap_or(cfg.default_timeout_secs);
        let allowed_tools = spec
            .allowed_tools
            .clone()
            .unwrap_or_else(|| cfg.default_allowed_tools.clone());

        let (cancel_tx, cancel_rx) = oneshot::channel();
        {
            let mut guard = self.inner.lock().await;
            guard.active.insert(
                id.clone(),
                ActiveWorker {
                    cancel_tx: Some(cancel_tx),
                    started_at: Instant::now(),
                },
            );
        }

        let input = SupervisorInput {
            id: id.clone(),
            spec: spec.clone(),
            model: snapshot.model.clone(),
            claude_bin: self.claude_bin.clone(),
            system_prompt_path: prompt_path,
            mcp_config_path: self.mcp_config_path.clone(),
            allowed_tools,
            timeout_secs,
            dry_run: env_or_legacy("CONTINUUM_WORKER_DRY_RUN", "KAIRO_WORKER_DRY_RUN")
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false),
        };

        let pool = self.clone();
        let worker_id_for_task = id.clone();
        tokio::spawn(async move {
            let event_pool = pool.clone();
            let event_id = worker_id_for_task.clone();
            let outcome = run_worker(
                input,
                move |evt| {
                    let pool = event_pool.clone();
                    let id = event_id.clone();
                    let sink = pool.event_sink.clone();
                    if let Some(s) = sink.as_ref() {
                        s(&id, evt.clone());
                    }
                    // Fire-and-forget snapshot update so we never block the
                    // supervisor loop on tokio lock contention.
                    tokio::spawn(async move {
                        pool.apply_event(&id, evt).await;
                    });
                },
                cancel_rx,
            )
            .await;

            pool.finish_worker(worker_id_for_task, outcome).await;
        });
    }

    async fn apply_event(&self, id: &str, event: WorkerEvent) {
        let mut guard = self.inner.lock().await;
        let Some(snap) = guard.snapshots.get_mut(id) else {
            return;
        };
        match event {
            WorkerEvent::SessionReady { session_id } => {
                snap.session_id = Some(session_id);
                snap.status = WorkerStatus::Running;
            }
            WorkerEvent::TextDelta(text) => {
                // Keep the last-line preview short so snapshot JSON stays tiny.
                let mut combined = snap.last_line.clone();
                combined.push_str(&text);
                if combined.chars().count() > 200 {
                    combined = combined
                        .chars()
                        .rev()
                        .take(200)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                }
                snap.last_line = combined;
            }
            WorkerEvent::ToolCall { name, .. } => {
                snap.tool_calls += 1;
                snap.last_line = format!("tool: {name}");
            }
            WorkerEvent::Progress { fraction, note } => {
                snap.progress = fraction.clamp(0.0, 1.0);
                if !note.is_empty() {
                    snap.last_line = note;
                }
            }
            WorkerEvent::Log(msg) => {
                snap.last_line = msg;
            }
            WorkerEvent::Finished(_) => { /* final update handled by finish_worker */ }
        }
        if let Some(started) = snap.started_at {
            snap.elapsed_ms = (Utc::now() - started).num_milliseconds().max(0) as u64;
        }
        let clone = snap.clone();
        drop(guard);
        intent::write_snapshot(&self.data_dir, &clone).ok();
    }

    async fn finish_worker(&self, id: WorkerId, outcome: WorkerOutcome) {
        let mut snap = {
            let guard = self.inner.lock().await;
            guard.snapshots.get(&id).cloned()
        }
        .unwrap_or_else(|| {
            WorkerSnapshot::queued(
                id.clone(),
                &WorkerSpec::new("unknown", std::env::temp_dir()),
                "unknown".into(),
                "unknown".into(),
            )
        });

        snap.status = outcome.status;
        snap.finished_at = Some(Utc::now());
        snap.result = Some(outcome.result_text.clone());
        snap.error = outcome.error.clone();
        snap.cost_usd = outcome.cost_usd;
        if let Some(sid) = outcome.session_id.clone() {
            snap.session_id = Some(sid);
        }
        snap.tool_calls = snap.tool_calls.max(outcome.tool_calls);
        snap.progress = if matches!(outcome.status, WorkerStatus::Completed) {
            1.0
        } else {
            snap.progress
        };
        if let Some(started) = snap.started_at {
            snap.elapsed_ms = (Utc::now() - started).num_milliseconds().max(0) as u64;
        }

        {
            let mut guard = self.inner.lock().await;
            guard.active.remove(&id);
            guard.snapshots.insert(id.clone(), snap.clone());
            match outcome.status {
                WorkerStatus::Completed => {
                    guard.completed_today = guard.completed_today.saturating_add(1);
                }
                WorkerStatus::Failed | WorkerStatus::TimedOut => {
                    guard.failed_today = guard.failed_today.saturating_add(1);
                    guard
                        .recent_failures
                        .push_back((Instant::now(), snap.task.clone()));
                }
                _ => {}
            }
            if let Some(cost) = outcome.cost_usd {
                guard.total_cost_today += cost;
            }
            notify_waiters(&mut guard, &id, &snap);
        }

        intent::write_snapshot(&self.data_dir, &snap).ok();

        info!(
            layer = "workers",
            component = "pool",
            worker_id = %id,
            status = snap.status.as_str(),
            cost_usd = outcome.cost_usd,
            duration_ms = outcome.duration_ms,
            "Worker finished"
        );

        if let Some(sink) = self.finish_sink.as_ref() {
            sink(&snap);
        }

        self.tick.notify_waiters();
    }

    async fn finalise_snapshot(&self, id: WorkerId, snap: WorkerSnapshot) {
        {
            let mut guard = self.inner.lock().await;
            guard.snapshots.insert(id.clone(), snap.clone());
            match snap.status {
                WorkerStatus::Failed | WorkerStatus::TimedOut => {
                    guard.failed_today = guard.failed_today.saturating_add(1);
                    guard
                        .recent_failures
                        .push_back((Instant::now(), snap.task.clone()));
                }
                WorkerStatus::Completed => {
                    guard.completed_today = guard.completed_today.saturating_add(1);
                }
                _ => {}
            }
            notify_waiters(&mut guard, &id, &snap);
        }
        intent::write_snapshot(&self.data_dir, &snap).ok();
        if let Some(sink) = self.finish_sink.as_ref() {
            sink(&snap);
        }
    }

    /// Returns true if the task pattern has failed `failure_streak_limit`
    /// times within `failure_window_secs`. Cheap prefix match on the first
    /// 80 chars so it catches obvious retry loops without hashing.
    async fn should_refuse_pattern(&self, task: &str) -> bool {
        let cfg = self.config.lock().await.clone();
        if cfg.failure_streak_limit == 0 {
            return false;
        }
        let window = Duration::from_secs(cfg.failure_window_secs.max(60));
        let prefix: String = task.chars().take(80).collect();
        let now = Instant::now();
        let mut guard = self.inner.lock().await;
        guard
            .recent_failures
            .retain(|(ts, _)| now.duration_since(*ts) <= window);
        let hits = guard
            .recent_failures
            .iter()
            .filter(|(_, t)| t.starts_with(&prefix))
            .count();
        hits as u32 >= cfg.failure_streak_limit
    }

    async fn store_snapshot(&self, id: &str, snap: WorkerSnapshot) {
        {
            let mut guard = self.inner.lock().await;
            guard.snapshots.insert(id.to_string(), snap.clone());
        }
        intent::write_snapshot(&self.data_dir, &snap).ok();
    }

    /// Run a one-off health check: accept a dry-run worker and confirm it
    /// completes. Returns `Ok(())` on success, `Err` on any failure.
    pub async fn run_health_probe(&self) -> Result<()> {
        std::env::set_var("CONTINUUM_WORKER_DRY_RUN", "1");
        let id = self
            .submit(WorkerSpec::new("health probe", std::env::temp_dir()))
            .await?;
        let snap = self
            .wait(&id, Some(Duration::from_secs(5)))
            .await
            .ok_or_else(|| anyhow::anyhow!("health probe worker never reached terminal state"))?;
        if snap.status == WorkerStatus::Completed {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "health probe status: {}",
                snap.status.as_str()
            ))
        }
    }

    pub fn data_dir(&self) -> &PathBuf {
        &self.data_dir
    }

    pub fn claude_bin(&self) -> &str {
        &self.claude_bin
    }

    /// Uptime of the running worker (if active) in seconds.
    pub async fn elapsed_secs(&self, id: &str) -> Option<u64> {
        let guard = self.inner.lock().await;
        guard
            .active
            .get(id)
            .map(|a| a.started_at.elapsed().as_secs())
    }

    /// Reload skills and return the active list. Used by dashboard +
    /// health probes; callers that don't have skills disabled get `[]`.
    pub fn active_skills(&self) -> Vec<Skill> {
        match &self.skill_loader {
            Some(l) => l.enabled(),
            None => Vec::new(),
        }
    }

    /// Materialise the worker's system prompt on disk. Returns
    /// (matched-skill-names, path to the prompt file — `None` if neither
    /// base prompt nor skills are configured).
    async fn assemble_worker_prompt(
        &self,
        worker_id: &str,
        spec: &WorkerSpec,
    ) -> (Vec<String>, Option<PathBuf>) {
        let mut sections: Vec<String> = Vec::new();
        if let Some(base) = &self.base_system_prompt {
            sections.push(base.clone());
        }
        let mut matched_names: Vec<String> = Vec::new();
        if let Some(loader) = &self.skill_loader {
            let skills = loader.enabled();
            if !skills.is_empty() {
                let ctx = MatchContext {
                    wake_reason: None,
                    task: Some(spec.task.clone()),
                    project: None,
                    audio_transcript: None,
                    foreground_app: None,
                    tags: spec.tags.clone(),
                    forced: spec.skills.clone(),
                };
                let (prompt, names) =
                    SkillMatcher::render_prompt(&skills, &ctx, self.skill_token_budget);
                if !prompt.is_empty() {
                    sections.push(prompt);
                }
                matched_names = names;
            }
        }
        if sections.is_empty() {
            return (matched_names, None);
        }
        let combined = sections.join("\n\n");
        let prompts_dir = self.data_dir.join("worker-prompts");
        if std::fs::create_dir_all(&prompts_dir).is_err() {
            return (matched_names, None);
        }
        let path = prompts_dir.join(format!("{}.md", worker_id));
        let file_path = if std::fs::write(&path, combined).is_ok() {
            Some(path)
        } else {
            None
        };
        (matched_names, file_path)
    }
}

fn pop_highest_priority(q: &mut VecDeque<QueuedWorker>) -> Option<QueuedWorker> {
    if q.is_empty() {
        return None;
    }
    let mut best = 0usize;
    for i in 1..q.len() {
        let a = &q[best];
        let b = &q[i];
        if (b.spec.priority, std::cmp::Reverse(b.seq)) > (a.spec.priority, std::cmp::Reverse(a.seq))
        {
            best = i;
        }
    }
    q.remove(best)
}

fn notify_waiters(inner: &mut PoolInner, id: &str, snap: &WorkerSnapshot) {
    if let Some(list) = inner.waiters.remove(id) {
        for tx in list {
            let _ = tx.send(snap.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn dry_options(dir: &std::path::Path) -> WorkerPoolOptions {
        // Force dry-run via env flag so spawn doesn't shell out to claude.
        std::env::set_var("CONTINUUM_WORKER_DRY_RUN", "1");
        WorkerPoolOptions::new(dir.to_path_buf())
    }

    #[tokio::test]
    async fn submit_then_wait_completes_dry_worker() {
        let tmp = TempDir::new().unwrap();
        let pool = WorkerPool::new(dry_options(tmp.path()));
        let (_tx, rx) = watch::channel(false);
        pool.spawn_background(rx);

        let id = pool
            .submit(WorkerSpec::new("rename files in src", tmp.path()))
            .await
            .unwrap();
        let snap = pool.wait(&id, Some(Duration::from_secs(5))).await.unwrap();
        assert_eq!(snap.status, WorkerStatus::Completed);
        assert!(snap.result.unwrap().contains("dry-run"));
        let stats = pool.stats().await;
        assert_eq!(stats.completed_today, 1);
    }

    #[tokio::test]
    async fn max_concurrent_caps_active_count() {
        let tmp = TempDir::new().unwrap();
        let mut opts = dry_options(tmp.path());
        opts.config.max_concurrent = 1;
        opts.config.status_refresh_ms = 50;
        let pool = WorkerPool::new(opts);
        let (_tx, rx) = watch::channel(false);
        pool.spawn_background(rx);

        let mut ids = Vec::new();
        for i in 0..4 {
            ids.push(
                pool.submit(WorkerSpec::new(format!("dry task {i}"), tmp.path()))
                    .await
                    .unwrap(),
            );
        }

        for id in ids {
            let snap = pool.wait(&id, Some(Duration::from_secs(10))).await.unwrap();
            assert_eq!(snap.status, WorkerStatus::Completed);
        }
        let stats = pool.stats().await;
        assert_eq!(stats.completed_today, 4);
    }

    #[tokio::test]
    async fn cancel_queued_worker() {
        let tmp = TempDir::new().unwrap();
        let mut opts = dry_options(tmp.path());
        opts.config.max_concurrent = 1;
        let pool = WorkerPool::new(opts);
        // Do NOT spawn the background loop — we want the queued worker to
        // stay queued so the cancel path is deterministic.

        let id_a = pool
            .submit(WorkerSpec::new("first", tmp.path()))
            .await
            .unwrap();
        let id_b = pool
            .submit(WorkerSpec::new("second", tmp.path()))
            .await
            .unwrap();
        assert!(pool.cancel(&id_b).await);
        let snap_b = pool.status(&id_b).await.unwrap();
        assert_eq!(snap_b.status, WorkerStatus::Cancelled);

        // A is still queued because no background loop ran.
        let snap_a = pool.status(&id_a).await.unwrap();
        assert_eq!(snap_a.status, WorkerStatus::Queued);
    }

    #[tokio::test]
    async fn priority_wins_over_fifo() {
        let tmp = TempDir::new().unwrap();
        let mut opts = dry_options(tmp.path());
        opts.config.max_concurrent = 1;
        opts.config.status_refresh_ms = 50;
        let pool = WorkerPool::new(opts);

        // Queue a scheduled worker first, then a user-requested one second.
        let mut low = WorkerSpec::new("low priority", tmp.path());
        low.priority = super::super::types::WorkerPriority::Scheduled;
        let mut high = WorkerSpec::new("high priority", tmp.path());
        high.priority = super::super::types::WorkerPriority::UserRequested;

        let id_low = pool.submit(low).await.unwrap();
        let id_high = pool.submit(high).await.unwrap();

        let (_tx, rx) = watch::channel(false);
        pool.spawn_background(rx);

        let snap_high = pool
            .wait(&id_high, Some(Duration::from_secs(5)))
            .await
            .unwrap();
        let snap_low = pool
            .wait(&id_low, Some(Duration::from_secs(5)))
            .await
            .unwrap();
        assert!(
            snap_high.started_at <= snap_low.started_at,
            "high-priority worker should start first"
        );
    }

    #[tokio::test]
    async fn intent_file_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let pool = WorkerPool::new(dry_options(tmp.path()));
        let id = new_worker_id();
        intent::write_intent(
            tmp.path(),
            &WorkerIntent::spawn(id.clone(), WorkerSpec::new("via intent", tmp.path())),
        )
        .unwrap();
        pool.process_intents().await.unwrap();
        let snap = pool.status(&id).await.unwrap();
        assert_eq!(snap.task, "via intent");
    }
}
