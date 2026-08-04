//! # Shared live world-state
//!
//! Projects ordered local observations from the senses layer into one compact
//! snapshot. Raw screenshots and raw keyboard/mouse input are deliberately not
//! part of this agent-facing contract.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// Schema version for the agent-facing live-context contract.
pub const LIVE_CONTEXT_SCHEMA_VERSION: u32 = 1;

/// Default number of source events retained in the in-memory projection.
pub const DEFAULT_EVENT_CAPACITY: usize = 256;

/// Origin of a live-context event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveContextSource {
    Monitor,
    Window,
    InputActivity,
    Terminal,
    Project,
    System,
}

/// Privacy treatment applied before an observation enters shared context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyDisposition {
    Visible,
    #[default]
    Redacted,
    Excluded,
}

/// Stable monitor geometry and the latest locally-derived visual context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorWorldState {
    pub monitor_id: String,
    pub name: String,
    pub is_primary: bool,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Global ordered-buffer sequence across all monitor capture workers.
    pub capture_event_sequence: u64,
    /// Per-monitor capture sequence.
    pub capture_sequence: u64,
    pub captured_at: DateTime<Utc>,
    pub change_score: f32,
    pub meaningful_change: bool,
    pub description: String,
    pub confidence: f32,
    pub vision_updated_at: Option<DateTime<Utc>>,
    pub privacy: PrivacyDisposition,
    /// Configured per-monitor capture target used to report missed cadence.
    pub target_interval_ms: u64,
    pub capture_latency_ms: u64,
    pub dropped_before: u64,
}

/// Latest safe foreground-window projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowWorldState {
    pub process_name: String,
    pub title: String,
    pub observed_at: DateTime<Utc>,
    pub in_call: bool,
    pub privacy: PrivacyDisposition,
}

/// Coarse activity only; no key values, pointer coordinates, or click targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputActivityWorldState {
    pub observed_at: DateTime<Utc>,
    pub idle_seconds: u64,
    pub active: bool,
}

/// Lightweight local terminal/project projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectWorldState {
    pub observed_at: DateTime<Utc>,
    pub terminal_active: bool,
    pub terminal_process: Option<String>,
    pub project_root: Option<String>,
    pub project_name: Option<String>,
    pub git_head: Option<String>,
}

/// One ordered, source-attributed change in the live world-state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveContextEvent {
    pub sequence: u64,
    pub observed_at: DateTime<Utc>,
    pub source: LiveContextSource,
    pub source_id: String,
    pub summary: String,
    pub degraded: bool,
    pub dropped_before: u64,
}

/// Health and backpressure counters for the live-context component.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveContextHealth {
    pub capture_events: u64,
    pub vision_updates: u64,
    pub dropped_capture_events: u64,
    pub capture_deadline_misses: u64,
    pub output_events_dropped: u64,
    pub capture_failures: u64,
    pub last_capture_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl LiveContextHealth {
    /// Whether a configured capture component has stalled long enough to restart.
    pub fn should_restart(
        &self,
        now: DateTime<Utc>,
        capture_enabled: bool,
        capture_interval: Duration,
    ) -> bool {
        if !capture_enabled {
            return false;
        }
        let Some(last) = self.last_capture_at else {
            return self.capture_failures >= 3;
        };
        let threshold_ms = capture_interval.as_millis().saturating_mul(10).max(5_000);
        now.signed_duration_since(last).num_milliseconds() > threshold_ms as i64
    }
}

/// Versioned agent-facing projection of the current local world.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveWorldState {
    pub schema_version: u32,
    pub sequence: u64,
    pub generated_at: DateTime<Utc>,
    pub monitors: Vec<MonitorWorldState>,
    pub window: Option<WindowWorldState>,
    pub input_activity: Option<InputActivityWorldState>,
    pub project: Option<ProjectWorldState>,
    pub recent_events: Vec<LiveContextEvent>,
    pub health: LiveContextHealth,
}

impl LiveWorldState {
    /// Compile bounded plain text for models that cannot accept images.
    pub fn compact_for_agents(&self, max_chars: usize) -> String {
        let mut lines = vec![format!(
            "live-context/v{} seq={} generated={}",
            self.schema_version,
            self.sequence,
            self.generated_at.to_rfc3339()
        )];
        for monitor in &self.monitors {
            let description = if monitor.privacy == PrivacyDisposition::Visible {
                monitor.description.replace(['\r', '\n'], " ")
            } else {
                "[redacted by local privacy policy]".into()
            };
            lines.push(format!(
                "[monitor:{}] {}{} event={} capture={} change={:.3} privacy={:?} vision=\"{}\"",
                monitor.monitor_id,
                monitor.name,
                if monitor.is_primary { " primary" } else { "" },
                monitor.capture_event_sequence,
                monitor.capture_sequence,
                monitor.change_score,
                monitor.privacy,
                description
            ));
        }
        if let Some(window) = &self.window {
            lines.push(format!(
                "[window:foreground] process={} title=\"{}\" privacy={:?}",
                window.process_name,
                window.title.replace(['\r', '\n'], " "),
                window.privacy
            ));
        }
        if let Some(activity) = &self.input_activity {
            lines.push(format!(
                "[input:activity] active={} idle_seconds={} (no raw input captured)",
                activity.active, activity.idle_seconds
            ));
        }
        if let Some(project) = &self.project {
            lines.push(format!(
                "[project:current] root={} git_head={} terminal={}",
                project.project_root.as_deref().unwrap_or("unknown"),
                project.git_head.as_deref().unwrap_or("unknown"),
                project.terminal_process.as_deref().unwrap_or("inactive")
            ));
        }
        if self.health.dropped_capture_events > 0
            || self.health.capture_deadline_misses > 0
            || self.health.output_events_dropped > 0
        {
            lines.push(format!(
                "[system:degradation] capture_dropped={} cadence_missed={} output_dropped={}",
                self.health.dropped_capture_events,
                self.health.capture_deadline_misses,
                self.health.output_events_dropped
            ));
        }
        truncate_utf8(&lines.join("\n"), max_chars)
    }
}

#[derive(Debug)]
struct Projection {
    connected_monitors: BTreeSet<String>,
    monitors: BTreeMap<String, MonitorWorldState>,
    window: Option<WindowWorldState>,
    input_activity: Option<InputActivityWorldState>,
    project: Option<ProjectWorldState>,
    events: VecDeque<LiveContextEvent>,
    health: LiveContextHealth,
}

/// Shared, cheap-to-clone handle used by every live-context producer.
#[derive(Debug, Clone)]
pub struct LiveContextHub {
    inner: Arc<RwLock<Projection>>,
    sequence: Arc<AtomicU64>,
    event_capacity: usize,
}

impl LiveContextHub {
    /// Create an empty projection with a bounded event history.
    pub fn new(event_capacity: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Projection {
                connected_monitors: BTreeSet::new(),
                monitors: BTreeMap::new(),
                window: None,
                input_activity: None,
                project: None,
                events: VecDeque::with_capacity(event_capacity.max(1)),
                health: LiveContextHealth::default(),
            })),
            sequence: Arc::new(AtomicU64::new(0)),
            event_capacity: event_capacity.max(1),
        }
    }

    /// Publish lightweight capture metadata without waiting for local vision.
    pub fn record_monitor_capture(&self, mut monitor: MonitorWorldState) -> u64 {
        let observed_at = monitor.captured_at;
        let source_id = monitor.monitor_id.clone();
        let dropped_before = monitor.dropped_before;
        let deadline_missed = monitor.capture_latency_ms > monitor.target_interval_ms;
        let mut inner = self.inner.write();
        if !inner.connected_monitors.contains(&source_id) {
            return self.sequence.load(Ordering::Acquire);
        }
        if monitor.privacy != PrivacyDisposition::Visible {
            monitor.description = "[redacted by local privacy policy]".into();
            monitor.confidence = 1.0;
            monitor.vision_updated_at = None;
        } else if let Some(previous) = inner.monitors.get(&source_id) {
            if monitor.description.is_empty() {
                monitor.description = previous.description.clone();
                monitor.confidence = previous.confidence;
                monitor.vision_updated_at = previous.vision_updated_at;
            }
        }
        let summary = if monitor.privacy != PrivacyDisposition::Visible {
            format!("{} {:?}", monitor.name, monitor.privacy)
        } else if monitor.meaningful_change {
            format!("{} changed: {}", monitor.name, monitor.description)
        } else {
            format!("{} unchanged", monitor.name)
        };
        inner.health.capture_events = inner.health.capture_events.saturating_add(1);
        if deadline_missed {
            inner.health.capture_deadline_misses =
                inner.health.capture_deadline_misses.saturating_add(1);
        }
        inner.health.last_capture_at = Some(observed_at);
        inner.monitors.insert(source_id.clone(), monitor);
        self.push_event_locked(
            &mut inner,
            observed_at,
            LiveContextSource::Monitor,
            source_id,
            summary,
            dropped_before > 0 || deadline_missed,
            dropped_before,
        )
    }

    /// Apply a selective local-vision result to the latest monitor capture.
    pub fn record_monitor_vision(
        &self,
        monitor_id: &str,
        description: String,
        confidence: f32,
        vision_updated_at: Option<DateTime<Utc>>,
        privacy: PrivacyDisposition,
        vision_updated: bool,
    ) {
        let mut inner = self.inner.write();
        let Some(monitor) = inner.monitors.get_mut(monitor_id) else {
            return;
        };
        let privacy_changed = monitor.privacy != privacy;
        monitor.description = description;
        monitor.confidence = confidence;
        monitor.vision_updated_at = vision_updated_at;
        monitor.privacy = privacy;
        if vision_updated {
            inner.health.vision_updates = inner.health.vision_updates.saturating_add(1);
        }
        if vision_updated || privacy_changed {
            self.push_event_locked(
                &mut inner,
                Utc::now(),
                LiveContextSource::Monitor,
                monitor_id.to_string(),
                if privacy == PrivacyDisposition::Visible {
                    "local vision summary updated".into()
                } else {
                    "visual context redacted by local privacy policy".into()
                },
                false,
                0,
            );
        }
    }

    /// Read only the current foreground privacy classification.
    pub fn current_privacy(&self) -> PrivacyDisposition {
        self.inner
            .read()
            .window
            .as_ref()
            .map(|window| window.privacy)
            .unwrap_or_default()
    }

    /// Reconcile hot-plug topology and evict disconnected monitor state.
    pub fn set_connected_monitors(&self, monitor_ids: impl IntoIterator<Item = String>) {
        let connected: BTreeSet<String> = monitor_ids.into_iter().collect();
        let mut inner = self.inner.write();
        let removed: Vec<String> = inner
            .connected_monitors
            .difference(&connected)
            .cloned()
            .collect();
        inner.connected_monitors = connected;
        for monitor_id in removed {
            if inner.monitors.remove(&monitor_id).is_some() {
                self.push_event_locked(
                    &mut inner,
                    Utc::now(),
                    LiveContextSource::System,
                    monitor_id,
                    "monitor disconnected".into(),
                    false,
                    0,
                );
            }
        }
    }

    /// Replace the foreground-window and coarse input-activity projections.
    pub fn record_context(
        &self,
        window: WindowWorldState,
        input_activity: InputActivityWorldState,
    ) -> u64 {
        let summary = format!(
            "foreground={} active={} title={}",
            window.process_name, input_activity.active, window.title
        );
        let observed_at = window.observed_at;
        let mut inner = self.inner.write();
        inner.window = Some(window);
        inner.input_activity = Some(input_activity);
        self.push_event_locked(
            &mut inner,
            observed_at,
            LiveContextSource::Window,
            "foreground".into(),
            summary,
            false,
            0,
        )
    }

    /// Replace the lightweight terminal/project projection.
    pub fn record_project(&self, project: ProjectWorldState) -> u64 {
        let source = if project.terminal_active {
            LiveContextSource::Terminal
        } else {
            LiveContextSource::Project
        };
        let summary = format!(
            "project={} head={} terminal={}",
            project.project_name.as_deref().unwrap_or("unknown"),
            project.git_head.as_deref().unwrap_or("unknown"),
            project.terminal_process.as_deref().unwrap_or("inactive")
        );
        let observed_at = project.observed_at;
        let mut inner = self.inner.write();
        inner.project = Some(project);
        self.push_event_locked(
            &mut inner,
            observed_at,
            source,
            "current-project".into(),
            summary,
            false,
            0,
        )
    }

    /// Record explicit bounded-buffer degradation without stopping capture.
    pub fn record_capture_drop(&self, count: u64) {
        if count == 0 {
            return;
        }
        let mut inner = self.inner.write();
        inner.health.dropped_capture_events =
            inner.health.dropped_capture_events.saturating_add(count);
        self.push_event_locked(
            &mut inner,
            Utc::now(),
            LiveContextSource::System,
            "capture-buffer".into(),
            format!("bounded capture buffer dropped {count} oldest event(s)"),
            true,
            count,
        );
    }

    /// Record a downstream summary-channel drop.
    pub fn record_output_drop(&self, count: u64) {
        let mut inner = self.inner.write();
        inner.health.output_events_dropped =
            inner.health.output_events_dropped.saturating_add(count);
    }

    /// Record a capture failure while leaving the watcher alive.
    pub fn record_capture_failure(&self, error: impl Into<String>) {
        let error = error.into();
        let mut inner = self.inner.write();
        inner.health.capture_failures = inner.health.capture_failures.saturating_add(1);
        inner.health.last_error = Some(error.clone());
        self.push_event_locked(
            &mut inner,
            Utc::now(),
            LiveContextSource::System,
            "capture".into(),
            error,
            true,
            0,
        );
    }

    /// Clone the complete bounded projection for serialization or agents.
    pub fn snapshot(&self) -> LiveWorldState {
        let inner = self.inner.read();
        LiveWorldState {
            schema_version: LIVE_CONTEXT_SCHEMA_VERSION,
            sequence: self.sequence.load(Ordering::Acquire),
            generated_at: Utc::now(),
            monitors: inner.monitors.values().cloned().collect(),
            window: inner.window.clone(),
            input_activity: inner.input_activity.clone(),
            project: inner.project.clone(),
            recent_events: inner.events.iter().cloned().collect(),
            health: inner.health.clone(),
        }
    }

    // Keeping the serialized event fields explicit here makes every producer
    // pass source, ordering, and degradation metadata through one choke point.
    #[allow(clippy::too_many_arguments)]
    fn push_event_locked(
        &self,
        inner: &mut Projection,
        observed_at: DateTime<Utc>,
        source: LiveContextSource,
        source_id: String,
        summary: String,
        degraded: bool,
        dropped_before: u64,
    ) -> u64 {
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel) + 1;
        if inner.events.len() == self.event_capacity {
            inner.events.pop_front();
        }
        inner.events.push_back(LiveContextEvent {
            sequence,
            observed_at,
            source,
            source_id,
            summary: truncate_utf8(&summary, 240),
            degraded,
            dropped_before,
        });
        sequence
    }
}

impl Default for LiveContextHub {
    fn default() -> Self {
        Self::new(DEFAULT_EVENT_CAPACITY)
    }
}

/// Atomically persist an agent-facing world-state snapshot.
pub fn write_snapshot(path: &Path, snapshot: &LiveWorldState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(snapshot)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Publish without blocking producers, so slow disk I/O cannot pause capture.
pub fn spawn_publisher(
    hub: LiveContextHub,
    path: std::path::PathBuf,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(100)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let snapshot = hub.snapshot();
                    let path = path.clone();
                    let result = tokio::task::spawn_blocking(move || write_snapshot(&path, &snapshot)).await;
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!(layer = "senses", component = "live_context", error = %error, "Failed to publish live world-state"),
                        Err(error) => tracing::warn!(layer = "senses", component = "live_context", error = %error, "Live world-state publisher task failed"),
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });
}

fn truncate_utf8(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 1 {
        return "…".chars().take(max_chars).collect();
    }
    let mut out: String = value.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: &str, capture_sequence: u64) -> MonitorWorldState {
        MonitorWorldState {
            monitor_id: id.into(),
            name: id.into(),
            is_primary: id == "display-1",
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            capture_event_sequence: capture_sequence,
            capture_sequence,
            captured_at: Utc::now(),
            change_score: 0.2,
            meaningful_change: true,
            description: "code editor".into(),
            confidence: 0.8,
            vision_updated_at: Some(Utc::now()),
            privacy: PrivacyDisposition::Visible,
            target_interval_ms: 200,
            capture_latency_ms: 4,
            dropped_before: 0,
        }
    }

    #[test]
    fn events_are_globally_ordered_and_bounded() {
        let hub = LiveContextHub::new(2);
        hub.set_connected_monitors(["display-1".into(), "display-2".into()]);
        hub.record_monitor_capture(monitor("display-1", 1));
        hub.record_monitor_capture(monitor("display-2", 1));
        hub.record_capture_drop(1);
        let snapshot = hub.snapshot();
        let sequences: Vec<u64> = snapshot.recent_events.iter().map(|e| e.sequence).collect();
        assert_eq!(sequences.len(), 2);
        assert!(sequences[0] < sequences[1]);
        assert_eq!(snapshot.monitors.len(), 2);
        assert_eq!(snapshot.health.dropped_capture_events, 1);
    }

    #[test]
    fn compact_context_is_source_attributed_and_bounded() {
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into()]);
        hub.record_monitor_capture(monitor("display-1", 1));
        let text = hub.snapshot().compact_for_agents(180);
        assert!(text.contains("[monitor:display-1]"));
        assert!(text.chars().count() <= 180);
    }

    #[test]
    fn disconnected_monitor_leaves_current_projection() {
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into()]);
        hub.record_monitor_capture(monitor("display-1", 1));
        hub.set_connected_monitors(Vec::new());
        let snapshot = hub.snapshot();
        assert!(snapshot.monitors.is_empty());
        assert!(snapshot
            .recent_events
            .last()
            .is_some_and(|event| event.summary == "monitor disconnected"));
    }

    #[test]
    fn slow_capture_reports_a_cadence_miss() {
        let hub = LiveContextHub::default();
        hub.set_connected_monitors(["display-1".into()]);
        let mut slow = monitor("display-1", 1);
        slow.capture_latency_ms = 250;
        hub.record_monitor_capture(slow);
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.health.capture_deadline_misses, 1);
        assert!(snapshot
            .recent_events
            .last()
            .is_some_and(|event| event.degraded));
    }

    #[test]
    fn health_restart_only_after_stall_or_repeated_boot_failures() {
        let mut health = LiveContextHealth::default();
        assert!(!health.should_restart(Utc::now(), false, Duration::from_millis(200)));
        health.capture_failures = 3;
        assert!(health.should_restart(Utc::now(), true, Duration::from_millis(200)));
        health.last_capture_at = Some(Utc::now());
        assert!(!health.should_restart(Utc::now(), true, Duration::from_millis(200)));
    }

    #[test]
    fn snapshot_publication_replaces_previous_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("live-context.json");
        let hub = LiveContextHub::default();
        write_snapshot(&path, &hub.snapshot()).expect("first snapshot");
        hub.set_connected_monitors(["display-2".into()]);
        hub.record_monitor_capture(monitor("display-2", 1));
        write_snapshot(&path, &hub.snapshot()).expect("replacement snapshot");
        let published: LiveWorldState =
            serde_json::from_slice(&std::fs::read(&path).expect("read replacement snapshot"))
                .expect("decode replacement snapshot");
        assert_eq!(published.monitors.len(), 1);
        assert!(!path.with_extension("json.tmp").exists());
    }
}
