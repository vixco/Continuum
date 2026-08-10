//! # Background-process activity collector
//!
//! Samples the operating-system process table and emits only meaningful
//! changes: configured developer/model processes starting or stopping, and
//! sustained CPU or resident-memory pressure. It deliberately never reads
//! command lines, environment variables, window contents, or process memory.
//! A compact current snapshot is written to `processes.json` for read-only MCP
//! access; lifecycle history uses the normal deduped context-events pipeline.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sysinfo::{ProcessesToUpdate, System};
use tracing::{info, warn};

use crate::config::{ContextConfig, PrivacyConfig, ProcessWatcherConfig};
use crate::operational_state::{
    ComponentDiagnostic, OperationalState, RepairPolicyClass, RootCauseCategory,
};
use crate::runtime_control::{RuntimeServiceControl, RuntimeServiceName};
use crate::senses::privacy::{strictest, PrivacyFilter, Zone};
use crate::senses::toggles::ToggleControl;

#[cfg(feature = "runtime")]
use crate::memory::events::{
    ContextEvent, EventSender, EventSensitivity, EventSource, EventType, COLLECTOR_EVENT_IMPORTANCE,
};

/// Privacy classification persisted in `processes.json`. Kept outside the
/// heavy runtime feature so the desktop and MCP readers can deserialize the
/// snapshot without linking local-model dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSensitivity {
    /// Visible to local consumers only.
    LocalOnly,
    /// Eligible for cloud-bound context after re-checking live privacy rules.
    CloudAllowed,
}

/// Versioned file published by the collector for `context_processes`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessActivitySnapshot {
    /// Schema version for additive evolution.
    pub version: u32,
    /// Time of the process-table sample.
    pub observed_at: DateTime<Utc>,
    /// Relevant processes that were active in that sample.
    pub active: Vec<ProcessActivityEntry>,
}

/// One privacy-classified active process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessActivityEntry {
    /// Operating-system process id.
    pub pid: u32,
    /// Executable basename, without `.exe` for matching stability.
    pub name: String,
    /// Coarse activity category inferred only from the executable basename.
    pub category: String,
    /// Scrubbed executable path when the OS exposed it.
    pub exe_path: Option<String>,
    /// CPU percentage reported by `sysinfo` for the latest interval.
    pub cpu_percent: f32,
    /// Resident memory in MiB.
    pub memory_mb: u64,
    /// Process start time, when reported by the OS.
    pub started_at: Option<DateTime<Utc>>,
    /// Whether cloud-bound consumers may see this entry.
    pub sensitivity: ProcessSensitivity,
}

/// Health snapshot consumed by `RuntimeSnapshot.context_engine`.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessWatchHealth {
    /// Legacy compatibility bit. Prefer [`ProcessWatchHealth::state`].
    pub enabled: bool,
    /// Deliberate public-safe disabled reason.
    pub disabled_reason: Option<String>,
    /// Typed lifecycle state.
    pub state: OperationalState,
    /// Stable public reason code.
    pub reason_code: String,
    /// Public-safe explanation; never a raw process or filesystem error.
    pub explanation: String,
    /// Last successful process-table sample.
    pub last_poll_at: Option<DateTime<Utc>>,
    /// When the current active generation started.
    pub activated_at: Option<DateTime<Utc>>,
    /// Successful sample count.
    pub polls: u64,
    /// Lifecycle/pressure events handed to the events channel.
    pub events_emitted: u64,
    /// Relevant processes in the most recent bounded snapshot.
    pub active_processes: usize,
    /// Public-safe snapshot-publication error; cleared by the next success.
    pub last_error: Option<String>,
    /// Number of real inactive→active or restart transitions.
    pub activation_count: u64,
    /// Number of currently active collector generations (zero or one).
    pub current_instances: usize,
    /// Highest number of collector loops active concurrently. The single
    /// supervisor design guarantees this is at most one.
    pub max_concurrent_instances: usize,
}

impl Default for ProcessWatchHealth {
    fn default() -> Self {
        Self {
            enabled: false,
            disabled_reason: Some("disabled by [process_watcher].enabled".to_string()),
            state: OperationalState::DisabledByUser,
            reason_code: "disabled_by_user".to_string(),
            explanation: "Background activity observation is turned off.".to_string(),
            last_poll_at: None,
            activated_at: None,
            polls: 0,
            events_emitted: 0,
            active_processes: 0,
            last_error: None,
            activation_count: 0,
            current_instances: 0,
            max_concurrent_instances: 0,
        }
    }
}

impl ProcessWatchHealth {
    pub fn should_restart(&self, now: DateTime<Utc>, poll_interval: Duration) -> bool {
        if !self.enabled
            || !matches!(
                self.state,
                OperationalState::Starting | OperationalState::Running | OperationalState::Idle
            )
        {
            return false;
        }
        let grace = chrono::Duration::from_std(
            poll_interval.saturating_mul(3).max(Duration::from_secs(15)),
        )
        .unwrap_or_else(|_| chrono::Duration::seconds(30));
        let anchor = self.last_poll_at.or(self.activated_at);
        anchor.is_some_and(|ts| now.signed_duration_since(ts) > grace)
    }

    pub fn diagnostic(&self, now: DateTime<Utc>, poll_interval: Duration) -> ComponentDiagnostic {
        let stalled = self.should_restart(now, poll_interval);
        let (state, reason, explanation, root_cause, retryable) = if stalled {
            (
                OperationalState::Failed,
                "collector_stalled".to_string(),
                "Background activity observation stopped producing bounded samples.".to_string(),
                RootCauseCategory::Internal,
                true,
            )
        } else {
            (
                self.state,
                self.reason_code.clone(),
                self.explanation.clone(),
                match self.state {
                    OperationalState::DisabledByUser => RootCauseCategory::UserChoice,
                    OperationalState::DisabledByPolicy => RootCauseCategory::Policy,
                    OperationalState::PermissionRequired => RootCauseCategory::Permission,
                    OperationalState::Degraded | OperationalState::Failed => {
                        RootCauseCategory::Internal
                    }
                    _ => RootCauseCategory::Unknown,
                },
                self.state == OperationalState::Degraded,
            )
        };
        let mut diagnostic = ComponentDiagnostic::new(
            "process_watcher",
            "bounded_background_activity",
            state,
            reason,
            explanation,
            root_cause,
            retryable,
        )
        .with_evidence("runtime_state", "context_engine.process_watcher")
        .with_evidence("metric", "process_watcher.last_poll_at");
        if stalled {
            diagnostic = diagnostic
                .with_action("Restart the process watcher and verify a fresh bounded sample.")
                .with_repair(
                    RepairPolicyClass::AutomaticallySafe,
                    true,
                    Some("restart_process_watcher"),
                );
        } else if self.state == OperationalState::PermissionRequired {
            diagnostic = diagnostic
                .with_action("Restore write permission for the local runtime state directory.")
                .with_repair(RepairPolicyClass::ManualOnly, false, None);
        }
        diagnostic
    }
}

/// Shared process-watcher health handle.
pub type SharedProcessWatchHealth = Arc<RwLock<ProcessWatchHealth>>;

#[derive(Debug, Clone)]
struct TrackedProcess {
    name: String,
    category: String,
    exe_path: Option<String>,
    started_at: Option<DateTime<Utc>>,
    sensitivity: ProcessSensitivity,
    significant: bool,
    pressure_samples: u32,
    pressure_reported: bool,
    cpu_percent: f32,
    memory_mb: u64,
}

/// Normalizes an executable basename for config matching.
pub fn normalize_process_name(name: &str) -> String {
    name.trim()
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .to_lowercase()
}

/// Coarse, deterministic category used in summaries and snapshots.
pub fn process_category(name: &str) -> &'static str {
    match normalize_process_name(name).as_str() {
        "cargo" | "rustc" | "cmake" | "ninja" | "msbuild" | "dotnet" => "build",
        "node" | "deno" | "bun" | "python" | "python3" | "java" => "runtime",
        "ollama" | "lmstudio" | "lm studio" | "continuum" | "continuum-mcp" => "ai",
        "docker" | "dockerd" | "com.docker.backend" => "service",
        _ => "application",
    }
}

/// Resolves privacy for a normalized process basename and optional executable
/// path. Existing Windows privacy rules often include `.exe`, while snapshots
/// intentionally store a stable extensionless name, so both representations
/// must participate and the strictest result wins.
pub fn resolve_process_zone(
    filter: &PrivacyFilter,
    normalized_name: &str,
    exe_path: Option<&str>,
) -> Zone {
    let windows_name = format!("{normalized_name}.exe");
    let path = exe_path.unwrap_or("");
    strictest([
        filter.resolve_zone(normalized_name, path),
        filter.resolve_zone(&windows_name, path),
    ])
}

/// Bounded process watcher with one process-local supervisor. Transient
/// sample/publication failures self-heal on the next poll; a genuinely stale
/// enabled collector may request one verified in-process reinitialization.
pub struct ProcessWatcher {
    config: ProcessWatcherConfig,
    privacy: Arc<PrivacyFilter>,
    events: Option<Arc<dyn ProcessEventSink>>,
    snapshot_path: PathBuf,
    health: SharedProcessWatchHealth,
    toggles: ToggleControl,
    /// Live optional-service request, separate from the privacy pause.
    service_control: Option<RuntimeServiceControl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessEventKind {
    Started,
    Stopped,
    Pressure,
}

trait ProcessEventSink: Send + Sync {
    fn emit(&self, kind: ProcessEventKind, process: &TrackedProcess, pid: u32, summary: String);
}

#[cfg(feature = "runtime")]
impl ProcessEventSink for EventSender {
    fn emit(&self, kind: ProcessEventKind, process: &TrackedProcess, pid: u32, summary: String) {
        let event_type = match kind {
            ProcessEventKind::Started => EventType::ProcessStarted,
            ProcessEventKind::Stopped => EventType::ProcessStopped,
            ProcessEventKind::Pressure => EventType::ResourcePressure,
        };
        self.send(ContextEvent {
            ts: Utc::now(),
            source: EventSource::Process,
            application: process.name.clone(),
            window_title: String::new(),
            project_id: None,
            event_type,
            summary,
            importance: if kind == ProcessEventKind::Pressure {
                0.55
            } else {
                COLLECTOR_EVENT_IMPORTANCE
            },
            confidence: 1.0,
            sensitivity: match process.sensitivity {
                ProcessSensitivity::LocalOnly => EventSensitivity::LocalOnly,
                ProcessSensitivity::CloudAllowed => EventSensitivity::CloudAllowed,
            },
            raw_reference: Some(format!(
                "pid:{pid}:started:{}",
                process
                    .started_at
                    .map(|started_at| started_at.timestamp())
                    .unwrap_or_default()
            )),
        });
    }
}

impl ProcessWatcher {
    /// Creates a watcher publishing into `dev_dir/processes.json`.
    pub fn new(config: ProcessWatcherConfig, dev_dir: PathBuf) -> Self {
        Self {
            config,
            privacy: Arc::new(PrivacyFilter::from_config(
                &ContextConfig::default(),
                &PrivacyConfig::default(),
            )),
            events: None,
            snapshot_path: dev_dir.join("processes.json"),
            health: SharedProcessWatchHealth::default(),
            toggles: ToggleControl::default(),
            service_control: None,
        }
    }

    /// Attaches the shared privacy choke point.
    pub fn with_privacy(mut self, privacy: Arc<PrivacyFilter>) -> Self {
        self.privacy = privacy;
        self
    }

    /// Attaches the live master privacy control shared by all collectors.
    pub fn with_toggle_control(mut self, toggles: ToggleControl) -> Self {
        self.toggles = toggles;
        self
    }

    /// Attaches the live service control. The same supervisor task performs
    /// every start/stop/restart, preventing duplicate collectors.
    pub fn with_service_control(mut self, control: RuntimeServiceControl) -> Self {
        self.service_control = Some(control);
        self
    }

    fn requested(&self) -> bool {
        self.service_control
            .as_ref()
            .map(|control| control.enabled(RuntimeServiceName::BackgroundActivity))
            .unwrap_or(self.config.enabled)
    }

    fn restart_generation(&self) -> u64 {
        self.service_control
            .as_ref()
            .map(|control| control.restart_generation(RuntimeServiceName::BackgroundActivity))
            .unwrap_or(0)
    }

    /// Attaches the deduped events transport.
    #[cfg(feature = "runtime")]
    pub fn with_event_sender(mut self, events: EventSender) -> Self {
        self.events = Some(Arc::new(events));
        self
    }

    /// Shared health handle for the runtime publisher.
    pub fn health_handle(&self) -> SharedProcessWatchHealth {
        self.health.clone()
    }

    /// A stale enabled collector can be safely reinitialized in process.
    pub fn should_restart(&self) -> bool {
        self.health.read().should_restart(
            Utc::now(),
            Duration::from_secs(self.config.poll_secs.max(1)),
        )
    }

    fn emit(&self, kind: ProcessEventKind, process: &TrackedProcess, pid: u32, summary: String) {
        if let Some(events) = &self.events {
            events.emit(kind, process, pid, self.privacy.scrub_text(&summary));
            self.health.write().events_emitted += 1;
        }
    }

    fn set_state(
        &self,
        state: OperationalState,
        reason_code: &str,
        explanation: &str,
        disabled_reason: Option<&str>,
    ) {
        let mut health = self.health.write();
        health.enabled = state.enabled();
        health.state = state;
        health.reason_code = reason_code.to_string();
        health.explanation = explanation.to_string();
        health.disabled_reason = disabled_reason.map(ToOwned::to_owned);
        if !state.enabled() {
            health.active_processes = 0;
            health.activated_at = None;
            health.current_instances = 0;
        }
    }

    fn clear_snapshot(&self) {
        let snapshot = ProcessActivitySnapshot {
            version: 1,
            observed_at: Utc::now(),
            active: Vec::new(),
        };
        if let Err(error) = write_process_snapshot(&self.snapshot_path, &snapshot) {
            warn!(
                layer = "senses",
                component = "process_watch",
                error = %error,
                "Failed to clear process snapshot"
            );
        }
    }

    /// Runs one supervisor until shutdown. Disabled states perform no process
    /// table refresh. Repeated enable requests cannot create another loop.
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let includes: HashSet<String> = self
            .config
            .include_names
            .iter()
            .map(|name| normalize_process_name(name))
            .collect();
        let excludes: HashSet<String> = self
            .config
            .exclude_names
            .iter()
            .map(|name| normalize_process_name(name))
            .collect();
        let poll_interval = Duration::from_secs(self.config.poll_secs.max(1));
        let mut control_tick = tokio::time::interval(Duration::from_millis(100));
        control_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut system = System::new();
        let mut tracked: HashMap<u32, TrackedProcess> = HashMap::new();
        let mut active = false;
        let mut snapshot_cleared = false;
        let mut generation = self.restart_generation();
        let mut next_poll = tokio::time::Instant::now();

        loop {
            tokio::select! {
                _ = control_tick.tick() => {
                    let requested = self.requested();
                    let paused = self.toggles.paused();
                    let next_generation = self.restart_generation();
                    if !requested || paused {
                        if active || !snapshot_cleared {
                            self.clear_snapshot();
                            snapshot_cleared = true;
                            tracked.clear();
                            system = System::new();
                            active = false;
                        }
                        if paused {
                            self.set_state(
                                OperationalState::DisabledByPolicy,
                                "disabled_by_policy",
                                "Background activity observation is blocked while all observation is paused.",
                                Some("all observation paused by user"),
                            );
                        } else {
                            let legacy = if self.service_control.is_some() {
                                "disabled by user"
                            } else {
                                "disabled by [process_watcher].enabled"
                            };
                            self.set_state(
                                OperationalState::DisabledByUser,
                                "disabled_by_user",
                                "Background activity observation is turned off.",
                                Some(legacy),
                            );
                        }
                        generation = next_generation;
                        continue;
                    }

                    if !active || next_generation != generation {
                        tracked.clear();
                        system = System::new();
                        generation = next_generation;
                        active = true;
                        snapshot_cleared = false;
                        next_poll = tokio::time::Instant::now();
                        let mut health = self.health.write();
                        health.enabled = true;
                        health.disabled_reason = None;
                        health.state = OperationalState::Starting;
                        health.reason_code = if health.activation_count == 0 {
                            "starting".to_string()
                        } else {
                            "restart_requested".to_string()
                        };
                        health.explanation = "Background activity observation is starting.".to_string();
                        health.activated_at = Some(Utc::now());
                        health.activation_count = health.activation_count.saturating_add(1);
                        health.current_instances = 1;
                        health.max_concurrent_instances = health.max_concurrent_instances.max(health.current_instances);
                        info!(
                            layer = "senses",
                            component = "process_watch",
                            generation,
                            poll_secs = self.config.poll_secs.max(1),
                            "Background-process collector activated"
                        );
                    }

                    if tokio::time::Instant::now() < next_poll {
                        continue;
                    }
                    next_poll = tokio::time::Instant::now() + poll_interval;
                    system.refresh_processes(ProcessesToUpdate::All, true);
                    let observed_at = Utc::now();
                    let mut current = HashMap::new();

                    for (pid, process) in system.processes() {
                        let pid = pid.as_u32();
                        let name = normalize_process_name(&process.name().to_string_lossy());
                        if name.is_empty() || excludes.contains(&name) {
                            continue;
                        }
                        let exe_path = process
                            .exe()
                            .map(|path| self.privacy.scrub_path(path.to_string_lossy().as_ref()));
                        let zone = resolve_process_zone(&self.privacy, &name, exe_path.as_deref());
                        if zone == Zone::NeverObserve {
                            continue;
                        }
                        let sensitivity = if zone == Zone::LocalOnly {
                            ProcessSensitivity::LocalOnly
                        } else {
                            ProcessSensitivity::CloudAllowed
                        };
                        let cpu_percent = process.cpu_usage();
                        let memory_mb = process.memory() / 1024 / 1024;
                        let pressure = cpu_percent >= self.config.cpu_threshold_percent
                            || memory_mb >= self.config.memory_threshold_mb;
                        let previous = tracked.get(&pid);
                        let pressure_samples = if pressure {
                            previous.map_or(1, |state| state.pressure_samples.saturating_add(1))
                        } else {
                            0
                        };
                        let configured = includes.contains(&name);
                        let significant = configured
                            || previous.is_some_and(|state| state.significant)
                            || pressure_samples >= self.config.sustained_samples.max(1);
                        let started_at = i64::try_from(process.start_time())
                            .ok()
                            .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single());
                        let state = TrackedProcess {
                            name: name.clone(),
                            category: process_category(&name).to_string(),
                            exe_path,
                            started_at,
                            sensitivity,
                            significant,
                            pressure_samples,
                            pressure_reported: previous.is_some_and(|state| state.pressure_reported),
                            cpu_percent,
                            memory_mb,
                        };

                        if significant && !previous.is_some_and(|state| state.significant) {
                            self.emit(
                                ProcessEventKind::Started,
                                &state,
                                pid,
                                format!("{} process {} started (pid {})", state.category, state.name, pid),
                            );
                        }
                        let mut state = state;
                        if pressure_samples >= self.config.sustained_samples.max(1)
                            && !state.pressure_reported
                        {
                            self.emit(
                                ProcessEventKind::Pressure,
                                &state,
                                pid,
                                format!(
                                    "{} sustained resource pressure: {:.0}% CPU, {} MB memory",
                                    state.name, state.cpu_percent, state.memory_mb
                                ),
                            );
                            state.pressure_reported = true;
                        } else if !pressure {
                            state.pressure_reported = false;
                        }
                        current.insert(pid, state);
                    }

                    for (pid, process) in &tracked {
                        if process.significant && !current.contains_key(pid) {
                            self.emit(
                                ProcessEventKind::Stopped,
                                process,
                                *pid,
                                format!(
                                    "{} process {} stopped (pid {}; exit reason unavailable)",
                                    process.category, process.name, pid
                                ),
                            );
                        }
                    }

                    let mut active_entries: Vec<ProcessActivityEntry> = current
                        .iter()
                        .filter(|(_, process)| process.significant)
                        .map(|(pid, process)| ProcessActivityEntry {
                            pid: *pid,
                            name: process.name.clone(),
                            category: process.category.clone(),
                            exe_path: process.exe_path.clone(),
                            cpu_percent: process.cpu_percent,
                            memory_mb: process.memory_mb,
                            started_at: process.started_at,
                            sensitivity: process.sensitivity,
                        })
                        .collect();
                    active_entries.sort_by(|a, b| {
                        b.cpu_percent
                            .total_cmp(&a.cpu_percent)
                            .then_with(|| b.memory_mb.cmp(&a.memory_mb))
                    });
                    active_entries.truncate(self.config.snapshot_limit.max(1));
                    let snapshot = ProcessActivitySnapshot {
                        version: 1,
                        observed_at,
                        active: active_entries,
                    };
                    match write_process_snapshot(&self.snapshot_path, &snapshot) {
                        Ok(()) => {
                            let mut health = self.health.write();
                            health.polls = health.polls.saturating_add(1);
                            health.last_poll_at = Some(observed_at);
                            health.active_processes = snapshot.active.len();
                            health.last_error = None;
                            health.enabled = true;
                            health.disabled_reason = None;
                            if snapshot.active.is_empty() {
                                health.state = OperationalState::Idle;
                                health.reason_code = "no_relevant_activity".to_string();
                                health.explanation = "Background activity is enabled and currently idle.".to_string();
                            } else {
                                health.state = OperationalState::Running;
                                health.reason_code = "sampling".to_string();
                                health.explanation = format!(
                                    "Background activity is sampling {} relevant process(es).",
                                    snapshot.active.len()
                                );
                            }
                        }
                        Err(error) => {
                            let permission = error
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied);
                            let mut health = self.health.write();
                            health.last_error = Some("process snapshot publication failed".to_string());
                            health.state = if permission {
                                OperationalState::PermissionRequired
                            } else {
                                OperationalState::Degraded
                            };
                            health.reason_code = if permission {
                                "state_directory_permission_required".to_string()
                            } else {
                                "snapshot_publish_failed".to_string()
                            };
                            health.explanation = if permission {
                                "Background activity was sampled, but the local state directory is not writable.".to_string()
                            } else {
                                "Background activity was sampled, but its bounded snapshot could not be published.".to_string()
                            };
                            warn!(
                                layer = "senses",
                                component = "process_watch",
                                error = %error,
                                "Failed to publish process snapshot"
                            );
                        }
                    }
                    tracked = current;
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        self.set_state(
                            OperationalState::Stopping,
                            "shutdown",
                            "Background activity observation is stopping.",
                            None,
                        );
                        break;
                    }
                }
            }
        }
        let mut health = self.health.write();
        health.current_instances = 0;
        health.activated_at = None;
    }
}

fn write_process_snapshot(
    path: &std::path::Path,
    snapshot: &ProcessActivitySnapshot,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(snapshot)?;
    if let Err(error) = std::fs::write(&temp, bytes) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    std::fs::rename(&temp, path).or_else(|error| {
        // Windows does not replace an existing destination with rename. The
        // bounded snapshot is derived state, so remove-and-replace is safe;
        // failure still remains visible as degraded/permission-required.
        if path.exists() {
            std::fs::remove_file(path)?;
            std::fs::rename(&temp, path)
        } else {
            Err(error)
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::intents::ToggleName;

    #[test]
    fn process_names_are_normalized_for_config_matching() {
        assert_eq!(normalize_process_name("Cargo.EXE"), "cargo");
        assert_eq!(normalize_process_name(" node.exe "), "node");
    }

    #[test]
    fn categories_are_coarse_and_deterministic() {
        assert_eq!(process_category("rustc.exe"), "build");
        assert_eq!(process_category("ollama.exe"), "ai");
        assert_eq!(process_category("unknown.exe"), "application");
    }

    #[test]
    fn privacy_resolution_honors_windows_executable_rules_after_normalization() {
        let filter =
            PrivacyFilter::from_config(&ContextConfig::default(), &PrivacyConfig::default());
        assert_eq!(
            resolve_process_zone(&filter, "1password", None),
            Zone::NeverObserve
        );
    }

    async fn wait_until(mut check: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if check() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        check()
    }

    #[tokio::test]
    async fn live_control_starts_stops_and_restarts_one_collector() {
        let dir = tempfile::tempdir().unwrap();
        let config = ProcessWatcherConfig {
            enabled: false,
            poll_secs: 1,
            include_names: Vec::new(),
            ..ProcessWatcherConfig::default()
        };
        let control = RuntimeServiceControl::default();
        let watcher = ProcessWatcher::new(config, dir.path().to_path_buf())
            .with_service_control(control.clone());
        let health = watcher.health_handle();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { watcher.run(shutdown_rx).await });

        assert!(
            wait_until(
                || health.read().state == OperationalState::DisabledByUser,
                Duration::from_secs(2),
            )
            .await
        );
        assert_eq!(health.read().polls, 0);

        assert!(control.set(RuntimeServiceName::BackgroundActivity, true));
        assert!(
            wait_until(
                || health.read().activation_count == 1 && health.read().polls > 0,
                Duration::from_secs(4),
            )
            .await
        );
        assert_eq!(health.read().current_instances, 1);

        control.request_restart(RuntimeServiceName::BackgroundActivity);
        assert!(
            wait_until(
                || health.read().activation_count >= 2,
                Duration::from_secs(2),
            )
            .await
        );
        assert_eq!(health.read().max_concurrent_instances, 1);

        assert!(control.set(RuntimeServiceName::BackgroundActivity, false));
        assert!(
            wait_until(
                || {
                    let snapshot = health.read();
                    snapshot.state == OperationalState::DisabledByUser
                        && snapshot.current_instances == 0
                },
                Duration::from_secs(2),
            )
            .await
        );

        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[test]
    fn stale_enabled_collector_requests_verified_restart() {
        let mut health = ProcessWatchHealth::default();
        health.enabled = true;
        health.state = OperationalState::Running;
        health.activated_at = Some(Utc::now() - chrono::Duration::minutes(2));
        assert!(health.should_restart(Utc::now(), Duration::from_secs(1)));
        let diagnostic = health.diagnostic(Utc::now(), Duration::from_secs(1));
        assert_eq!(diagnostic.reason_code, "collector_stalled");
        assert!(diagnostic.repair.available);
    }

    #[tokio::test]
    async fn master_pause_prevents_process_polling_and_clears_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let config = ProcessWatcherConfig {
            enabled: true,
            ..ProcessWatcherConfig::default()
        };
        let toggles = ToggleControl::default();
        toggles.set(ToggleName::PauseAll, true);
        let watcher =
            ProcessWatcher::new(config, dir.path().to_path_buf()).with_toggle_control(toggles);
        let health = watcher.health_handle();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { watcher.run(shutdown_rx).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown_tx.send(true).unwrap();
        task.await.unwrap();

        let snapshot: ProcessActivitySnapshot =
            serde_json::from_slice(&std::fs::read(dir.path().join("processes.json")).unwrap())
                .unwrap();
        assert!(snapshot.active.is_empty());
        assert!(!health.read().enabled);
        assert_eq!(health.read().polls, 0);
    }
}
