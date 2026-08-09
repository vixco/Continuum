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
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProcessWatchHealth {
    /// Whether sampling is enabled.
    pub enabled: bool,
    /// Deliberate disabled reason.
    pub disabled_reason: Option<String>,
    /// Last successful process-table sample.
    pub last_poll_at: Option<DateTime<Utc>>,
    /// Successful sample count.
    pub polls: u64,
    /// Lifecycle/pressure events handed to the events channel.
    pub events_emitted: u64,
    /// Relevant processes in the most recent snapshot.
    pub active_processes: usize,
    /// Most recent snapshot-publication error; cleared by the next success.
    pub last_error: Option<String>,
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

/// Event-driven process watcher. Poll failures are transient and a restart
/// cannot improve them, so `should_restart()` is always false.
pub struct ProcessWatcher {
    config: ProcessWatcherConfig,
    privacy: Arc<PrivacyFilter>,
    events: Option<Arc<dyn ProcessEventSink>>,
    snapshot_path: PathBuf,
    health: SharedProcessWatchHealth,
    toggles: ToggleControl,
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

    /// Poll failures self-heal on the next interval; restarting is not useful.
    pub fn should_restart(&self) -> bool {
        false
    }

    fn emit(&self, kind: ProcessEventKind, process: &TrackedProcess, pid: u32, summary: String) {
        if let Some(events) = &self.events {
            events.emit(kind, process, pid, self.privacy.scrub_text(&summary));
            self.health.write().events_emitted += 1;
        }
    }

    /// Runs until shutdown. Disabled mode performs no process-table refresh.
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        if !self.config.enabled {
            {
                let mut health = self.health.write();
                health.enabled = false;
                health.disabled_reason = Some("disabled by [process_watcher].enabled".to_string());
            }
            while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
            return;
        }

        self.health.write().enabled = true;
        info!(
            layer = "senses",
            component = "process_watch",
            poll_secs = self.config.poll_secs.max(1),
            "Background-process collector started"
        );

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
        let mut system = System::new();
        let mut tracked: HashMap<u32, TrackedProcess> = HashMap::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(self.config.poll_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if self.toggles.paused() {
                        let snapshot = ProcessActivitySnapshot {
                            version: 1,
                            observed_at: Utc::now(),
                            active: Vec::new(),
                        };
                        if let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) {
                            let _ = std::fs::write(&self.snapshot_path, bytes);
                        }
                        let mut health = self.health.write();
                        health.enabled = false;
                        health.disabled_reason = Some("all observation paused by user".to_string());
                        drop(health);
                        tracked.clear();
                        continue;
                    }
                    {
                        let mut health = self.health.write();
                        health.enabled = true;
                        health.disabled_reason = None;
                    }
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
                        // Reuse title-keyword privacy rules for executable
                        // paths, so a private project/directory rule also
                        // protects a generically named runtime such as node.
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

                    let mut active: Vec<ProcessActivityEntry> = current
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
                    active.sort_by(|a, b| {
                        b.cpu_percent
                            .total_cmp(&a.cpu_percent)
                            .then_with(|| b.memory_mb.cmp(&a.memory_mb))
                    });
                    active.truncate(self.config.snapshot_limit.max(1));
                    let snapshot = ProcessActivitySnapshot { version: 1, observed_at, active };
                    match serde_json::to_vec_pretty(&snapshot)
                        .map_err(anyhow::Error::from)
                        .and_then(|bytes| std::fs::write(&self.snapshot_path, bytes).map_err(anyhow::Error::from))
                    {
                        Ok(()) => {
                            let mut health = self.health.write();
                            health.polls += 1;
                            health.last_poll_at = Some(observed_at);
                            health.active_processes = snapshot.active.len();
                            health.last_error = None;
                        }
                        Err(error) => {
                            self.health.write().last_error = Some(error.to_string());
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
                        break;
                    }
                }
            }
        }
    }
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
