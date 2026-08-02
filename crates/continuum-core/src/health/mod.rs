//! # Health and self-healing
//!
//! Each subsystem registers a [`HealthCheck`] with a [`HealthRegistry`].
//! The registry is polled on a schedule (every 30 s by default) and
//! summarises statuses into [`ComponentHealth`] records that the state
//! store surfaces to the dashboard Health tab.
//!
//! Nightly at 04:00 we snapshot `~/.continuum-dev/config.toml` and the
//! per-component state needed for recovery into
//! `~/.continuum-backups/<date>/` (see [`backup`]).
//!
//! The [`repair`] submodule implements the Claude Code-driven repair agent.

pub mod backup;
pub mod repair;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use tokio::sync::watch;

use crate::state::{ComponentHealth, ComponentStatus, StateHandle};

/// One health probe.
#[async_trait]
pub trait HealthCheck: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn log_path(&self) -> Option<String> {
        None
    }
    fn recovery_note(&self) -> Option<String> {
        None
    }
    async fn probe(&self) -> HealthResult;
}

/// Outcome of one probe.
#[derive(Debug, Clone)]
pub struct HealthResult {
    pub status: ComponentStatus,
    pub message: Option<String>,
    pub latency_ms: u64,
}

impl HealthResult {
    pub fn healthy(latency_ms: u64) -> Self {
        Self {
            status: ComponentStatus::Healthy,
            message: None,
            latency_ms,
        }
    }
    /// Healthy with an informational message (e.g. a resource-usage summary
    /// the repair agent can read alongside the status).
    pub fn healthy_with(msg: impl Into<String>, latency_ms: u64) -> Self {
        Self {
            status: ComponentStatus::Healthy,
            message: Some(msg.into()),
            latency_ms,
        }
    }
    pub fn degrading(msg: impl Into<String>, latency_ms: u64) -> Self {
        Self {
            status: ComponentStatus::Degrading,
            message: Some(msg.into()),
            latency_ms,
        }
    }
    pub fn error(msg: impl Into<String>, latency_ms: u64) -> Self {
        Self {
            status: ComponentStatus::Error,
            message: Some(msg.into()),
            latency_ms,
        }
    }
    pub fn unknown(msg: impl Into<String>, latency_ms: u64) -> Self {
        Self {
            status: ComponentStatus::Unknown,
            message: Some(msg.into()),
            latency_ms,
        }
    }
}

/// Registered check with its rolling stats.
struct RegisteredCheck {
    check: Arc<dyn HealthCheck>,
    stats: Mutex<CheckStats>,
}

#[derive(Default)]
struct CheckStats {
    last_error: Option<String>,
    error_count_24h: u64,
    recent_latencies_ms: Vec<u64>,
    last_probe_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl CheckStats {
    fn push_probe(&mut self, result: &HealthResult) {
        self.last_probe_at = Some(Utc::now());
        if let ComponentStatus::Error = result.status {
            self.error_count_24h = self.error_count_24h.saturating_add(1);
            self.last_error = result.message.clone();
        }
        self.recent_latencies_ms.push(result.latency_ms);
        if self.recent_latencies_ms.len() > 20 {
            self.recent_latencies_ms.remove(0);
        }
    }

    fn avg_latency(&self) -> Option<u64> {
        if self.recent_latencies_ms.is_empty() {
            None
        } else {
            let sum: u64 = self.recent_latencies_ms.iter().sum();
            Some(sum / self.recent_latencies_ms.len() as u64)
        }
    }
}

/// Registry of all health probes.
#[derive(Clone, Default)]
pub struct HealthRegistry {
    checks: Arc<Mutex<HashMap<String, Arc<RegisteredCheck>>>>,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<C: HealthCheck>(&self, check: C) {
        let name = check.name().to_string();
        let reg = Arc::new(RegisteredCheck {
            check: Arc::new(check),
            stats: Mutex::new(CheckStats::default()),
        });
        self.checks.lock().insert(name, reg);
    }

    pub fn register_fn(&self, name: impl Into<String>, probe: ProbeFn) {
        let name: String = name.into();
        self.register(FnCheck {
            name,
            probe,
            log_path: None,
            recovery_note: None,
        });
    }

    pub async fn run_all(&self) -> Vec<ComponentHealth> {
        let items: Vec<Arc<RegisteredCheck>> = self.checks.lock().values().cloned().collect();
        let mut out = Vec::with_capacity(items.len());
        for reg in items {
            let start = Instant::now();
            let result = reg.check.probe().await;
            let elapsed_ms = start.elapsed().as_millis() as u64;
            let mut res = result;
            if res.latency_ms == 0 {
                res.latency_ms = elapsed_ms;
            }
            let mut stats = reg.stats.lock();
            stats.push_probe(&res);
            let effective_status = derive_status(&res, &stats);
            out.push(ComponentHealth {
                name: reg.check.name().to_string(),
                status: effective_status,
                last_check_ts: stats.last_probe_at,
                last_error: stats.last_error.clone(),
                error_count_24h: stats.error_count_24h,
                avg_response_ms: stats.avg_latency(),
                log_path: reg.check.log_path(),
                recovery_note: reg.check.recovery_note(),
            });
        }
        out
    }

    /// Run a single probe by name. Used by repair tool `repair_test_component`.
    pub async fn run_single(&self, name: &str) -> Option<ComponentHealth> {
        let reg = self.checks.lock().get(name).cloned()?;
        let start = Instant::now();
        let result = reg.check.probe().await;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let mut res = result;
        if res.latency_ms == 0 {
            res.latency_ms = elapsed_ms;
        }
        let mut stats = reg.stats.lock();
        stats.push_probe(&res);
        let effective_status = derive_status(&res, &stats);
        Some(ComponentHealth {
            name: reg.check.name().to_string(),
            status: effective_status,
            last_check_ts: stats.last_probe_at,
            last_error: stats.last_error.clone(),
            error_count_24h: stats.error_count_24h,
            avg_response_ms: stats.avg_latency(),
            log_path: reg.check.log_path(),
            recovery_note: reg.check.recovery_note(),
        })
    }

    pub fn names(&self) -> Vec<String> {
        self.checks.lock().keys().cloned().collect()
    }
}

/// Predictive heuristics: a single healthy probe doesn't prove long-term
/// wellbeing. Downgrade to `degrading` when the rolling error rate is
/// above 5 % across the last 20 probes.
fn derive_status(latest: &HealthResult, stats: &CheckStats) -> ComponentStatus {
    if matches!(latest.status, ComponentStatus::Error) {
        return ComponentStatus::Error;
    }
    let recent = &stats.recent_latencies_ms;
    if stats.error_count_24h > 0 && recent.len() >= 10 {
        let errors_per_20 = stats.error_count_24h.min(recent.len() as u64);
        if errors_per_20 as f32 / recent.len() as f32 > 0.05 {
            return ComponentStatus::Degrading;
        }
    }
    latest.status
}

pub type ProbeFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = HealthResult> + Send>> + Send + Sync + 'static>;

struct FnCheck {
    name: String,
    probe: ProbeFn,
    log_path: Option<String>,
    recovery_note: Option<String>,
}

#[async_trait]
impl HealthCheck for FnCheck {
    fn name(&self) -> &str {
        &self.name
    }
    fn log_path(&self) -> Option<String> {
        self.log_path.clone()
    }
    fn recovery_note(&self) -> Option<String> {
        self.recovery_note.clone()
    }
    async fn probe(&self) -> HealthResult {
        (self.probe)().await
    }
}

/// Spawn the background poller that runs every `interval_secs` and pushes
/// results into the shared state. Exits when `shutdown` flips to `true`.
pub fn spawn_poller(
    registry: HealthRegistry,
    state: StateHandle,
    interval_secs: u64,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let components = registry.run_all().await;
                    state.set_components(components).await;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysHealthy;
    #[async_trait]
    impl HealthCheck for AlwaysHealthy {
        fn name(&self) -> &str {
            "always-healthy"
        }
        async fn probe(&self) -> HealthResult {
            HealthResult::healthy(1)
        }
    }

    struct AlwaysError;
    #[async_trait]
    impl HealthCheck for AlwaysError {
        fn name(&self) -> &str {
            "always-error"
        }
        async fn probe(&self) -> HealthResult {
            HealthResult::error("boom", 1)
        }
    }

    #[tokio::test]
    async fn run_all_reports_status_per_component() {
        let reg = HealthRegistry::new();
        reg.register(AlwaysHealthy);
        reg.register(AlwaysError);
        let results = reg.run_all().await;
        assert_eq!(results.len(), 2);
        let healthy = results.iter().find(|r| r.name == "always-healthy").unwrap();
        let errored = results.iter().find(|r| r.name == "always-error").unwrap();
        assert_eq!(healthy.status, ComponentStatus::Healthy);
        assert_eq!(errored.status, ComponentStatus::Error);
        assert_eq!(errored.error_count_24h, 1);
    }

    #[tokio::test]
    async fn run_single_targets_one_component() {
        let reg = HealthRegistry::new();
        reg.register(AlwaysHealthy);
        reg.register(AlwaysError);
        let out = reg.run_single("always-error").await.unwrap();
        assert_eq!(out.name, "always-error");
        assert_eq!(out.status, ComponentStatus::Error);
    }

    #[tokio::test]
    async fn run_single_returns_none_for_unknown() {
        let reg = HealthRegistry::new();
        reg.register(AlwaysHealthy);
        assert!(reg.run_single("nope").await.is_none());
    }
}
