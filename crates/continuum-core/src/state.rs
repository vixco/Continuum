//! # Runtime state store
//!
//! A centralized snapshot of everything the dashboard needs to render live:
//! perception, triage, orchestrator, workers, voice, memory, health, system.
//!
//! The store is held as `Arc<RwLock<ContinuumState>>` and wrapped by
//! [`StateHandle`] for ergonomic updates. Subsystems call the typed update
//! helpers (e.g. [`StateHandle::set_voice_mode`]) rather than reaching into
//! the inner struct, so the mutation surface stays small.
//!
//! Every write publishes a [`StateEvent`] over a tokio broadcast channel.
//! The dashboard event bridge subscribes, serialises the current snapshot,
//! and re-emits to the frontend via Tauri's `emit`. The channel is lossy by
//! design — consumers that lag simply miss intermediate snapshots and pick
//! up the next one, which is correct for a coalesce-friendly UI update.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};

use crate::runtime_publish::CuratorSnapshot;
use crate::senses::types::PerceptionFrame;
use crate::triage::TriageDecision;

/// Size of the broadcast channel; consumers that lag beyond this lose events.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Size of the recent-actions ring buffer shown on the Home tab.
const RECENT_ACTIONS_CAPACITY: usize = 50;

/// Top-level runtime snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContinuumState {
    pub perception: PerceptionState,
    pub triage: TriageState,
    pub orchestrator: OrchestratorState,
    pub workers: WorkersState,
    pub voice: VoiceState,
    pub memory: MemoryState,
    pub health: HealthState,
    pub system: SystemState,
    /// Recent actions timeline (triage decisions + orchestrator wakes).
    pub recent_actions: VecDeque<RecentAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PerceptionState {
    pub last_frame_id: Option<String>,
    pub last_frame_ts: Option<DateTime<Utc>>,
    pub last_description: String,
    pub last_foreground_app: String,
    pub last_screenshot_path: Option<String>,
    pub last_salience: f32,
    pub has_error_visible: bool,
    pub frames_today: u64,
    pub monitor_count: usize,
    pub capture_events: u64,
    pub dropped_capture_events: u64,
    pub last_capture_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TriageState {
    pub last_decision: Option<String>,
    pub last_decision_ts: Option<DateTime<Utc>>,
    pub last_latency_ms: Option<u64>,
    /// Counts indexed by variant name (ignore/remember/whisper/execute_simple/wake_orchestrator).
    pub decision_counts_today: std::collections::HashMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OrchestratorState {
    pub active: bool,
    pub current_session_id: Option<String>,
    pub last_wake_reason: Option<String>,
    pub last_wake_ts: Option<DateTime<Utc>>,
    pub last_duration_ms: Option<u64>,
    pub cost_usd_today: f64,
    pub wakes_today: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkersState {
    pub active: Vec<WorkerInfo>,
    pub queue_depth: usize,
    pub completed_today: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub id: String,
    pub task: String,
    pub model: String,
    pub started_at: DateTime<Utc>,
    pub progress: f32,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoiceState {
    pub mode: VoiceMode,
    pub partial_transcript: String,
    pub tts_queue_len: usize,
    pub volume: f32,
    pub muted: bool,
    pub ambient_mute_active: bool,
    pub detected_call_app: Option<String>,
    pub wake_word_enabled: bool,
    pub last_heard_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VoiceMode {
    #[default]
    Idle,
    Listening,
    Thinking,
    Speaking,
    Muted,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryState {
    pub raw_log_rows: u64,
    pub raw_log_bytes: u64,
    pub episodic_count: u64,
    pub semantic_count: u64,
    pub last_distill_ts: Option<DateTime<Utc>>,
    /// Memory-vault curator (Plan B) health, mirrored from the `continuum`
    /// runtime's `state.json` by `runtime_bridge::tick_once` (Task 11).
    /// `None` until the dashboard's IPC bridge has read at least one
    /// snapshot from a running runtime — distinct from a `Some` value with
    /// `enabled: false`, which means the runtime is reachable but the
    /// curator itself isn't running (see [`CuratorSnapshot::enabled`]).
    pub curator: Option<CuratorSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealthState {
    pub components: Vec<ComponentHealth>,
    pub last_check_ts: Option<DateTime<Utc>>,
    pub error_count_24h: u64,
    pub repair_running: bool,
    pub last_repair_ts: Option<DateTime<Utc>>,
    pub last_backup_ts: Option<DateTime<Utc>>,
    pub backups_retained: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealth {
    pub name: String,
    pub status: ComponentStatus,
    pub last_check_ts: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub error_count_24h: u64,
    pub avg_response_ms: Option<u64>,
    pub log_path: Option<String>,
    pub recovery_note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ComponentStatus {
    Healthy,
    Degrading,
    Error,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemState {
    pub started_at: Option<DateTime<Utc>>,
    pub uptime_secs: u64,
    pub cpu_percent: f32,
    pub ram_used_mb: u64,
    pub ram_total_mb: u64,
    pub gpu_percent: Option<f32>,
    pub triage_model_loaded: bool,
    pub vision_model_loaded: bool,
    pub tts_loaded: bool,
    pub stt_loaded: bool,
    pub orchestrator_ready: bool,
    pub paused: bool,
    pub version: String,
}

/// A single entry in the recent-actions timeline (shown on the Home tab).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentAction {
    pub ts: DateTime<Utc>,
    pub kind: RecentActionKind,
    pub summary: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecentActionKind {
    Triage,
    Wake,
    Worker,
    Voice,
    Repair,
}

/// Event emitted when state changes. Consumers debounce as needed.
///
/// We use coarse-grained topics instead of per-field events so the dashboard
/// can subscribe only to the panels it displays.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateEvent {
    Perception,
    Triage,
    Orchestrator,
    Workers,
    Voice,
    Memory,
    Health,
    System,
    RecentActions,
}

/// Shared handle with typed update helpers.
///
/// Cloning is cheap — the inner state and broadcast are both `Arc`-backed.
#[derive(Clone)]
pub struct StateHandle {
    inner: Arc<RwLock<ContinuumState>>,
    events: broadcast::Sender<StateEvent>,
    started_at: DateTime<Utc>,
    monotonic_start: Arc<Instant>,
}

impl StateHandle {
    pub fn new() -> Self {
        Self::new_with_voice_config(0.0, false)
    }

    /// Create state with the persisted voice settings already represented.
    /// Live runtime telemetry can overwrite these values once the separate
    /// runtime process publishes its first snapshot.
    pub fn new_with_voice_config(volume: f32, wake_word_enabled: bool) -> Self {
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let started_at = Utc::now();
        let mut initial = ContinuumState::default();
        initial.system.started_at = Some(started_at);
        initial.system.version = env!("CARGO_PKG_VERSION").to_string();
        initial.voice.volume = volume.clamp(0.0, 1.0);
        initial.voice.wake_word_enabled = wake_word_enabled;
        Self {
            inner: Arc::new(RwLock::new(initial)),
            events: tx,
            started_at,
            monotonic_start: Arc::new(Instant::now()),
        }
    }

    /// Subscribe to state-change events. The returned receiver is
    /// independent per subscriber; backlogged receivers lose events past
    /// the channel capacity.
    pub fn subscribe(&self) -> broadcast::Receiver<StateEvent> {
        self.events.subscribe()
    }

    /// Clone the full state for serialisation.
    pub async fn snapshot(&self) -> ContinuumState {
        let mut state = self.inner.read().await.clone();
        state.system.uptime_secs = self.monotonic_start.elapsed().as_secs();
        state
    }

    fn notify(&self, topic: StateEvent) {
        // `send` only errors if there are zero subscribers, which is a
        // normal boot condition. Swallow it.
        let _ = self.events.send(topic);
    }

    // --- Perception ---

    pub async fn apply_frame(&self, frame: &PerceptionFrame) {
        {
            let mut s = self.inner.write().await;
            s.perception.last_frame_id = Some(frame.id.to_string());
            s.perception.last_frame_ts = Some(frame.ts);
            s.perception.last_description = frame.screen.description.clone();
            s.perception.last_foreground_app = frame.context.foreground_process_name.clone();
            s.perception.last_screenshot_path = frame.screen.screenshot_path.clone();
            s.perception.last_salience = frame.salience_hint;
            s.perception.has_error_visible = frame.screen.has_error_visible;
            s.perception.frames_today = s.perception.frames_today.saturating_add(1);
        }
        self.notify(StateEvent::Perception);
    }

    pub async fn set_live_context_status(
        &self,
        monitor_count: usize,
        capture_events: u64,
        dropped_capture_events: u64,
        last_capture_at: Option<DateTime<Utc>>,
    ) {
        {
            let mut state = self.inner.write().await;
            state.perception.monitor_count = monitor_count;
            state.perception.capture_events = capture_events;
            state.perception.dropped_capture_events = dropped_capture_events;
            state.perception.last_capture_at = last_capture_at;
        }
        self.notify(StateEvent::Perception);
    }

    // --- Triage ---

    pub async fn apply_triage(
        &self,
        decision: &TriageDecision,
        latency_ms: u64,
        frame_description: &str,
    ) {
        {
            let mut s = self.inner.write().await;
            s.triage.last_decision = Some(decision.variant_name().to_string());
            s.triage.last_decision_ts = Some(Utc::now());
            s.triage.last_latency_ms = Some(latency_ms);
            let counter = s
                .triage
                .decision_counts_today
                .entry(decision.variant_name().to_string())
                .or_insert(0);
            *counter = counter.saturating_add(1);

            push_recent(
                &mut s.recent_actions,
                RecentAction {
                    ts: Utc::now(),
                    kind: RecentActionKind::Triage,
                    summary: format!(
                        "{}: {}",
                        decision.variant_name(),
                        truncate(frame_description, 80)
                    ),
                    detail: None,
                },
            );
        }
        self.notify(StateEvent::Triage);
        self.notify(StateEvent::RecentActions);
    }

    // --- Orchestrator ---

    pub async fn mark_wake_start(&self, reason: &str, session_id: Option<String>) {
        {
            let mut s = self.inner.write().await;
            s.orchestrator.active = true;
            s.orchestrator.current_session_id = session_id;
            s.orchestrator.last_wake_reason = Some(reason.to_string());
            s.orchestrator.last_wake_ts = Some(Utc::now());
            s.orchestrator.wakes_today = s.orchestrator.wakes_today.saturating_add(1);

            push_recent(
                &mut s.recent_actions,
                RecentAction {
                    ts: Utc::now(),
                    kind: RecentActionKind::Wake,
                    summary: format!("wake: {}", truncate(reason, 80)),
                    detail: None,
                },
            );
        }
        self.notify(StateEvent::Orchestrator);
        self.notify(StateEvent::RecentActions);
    }

    pub async fn mark_wake_end(&self, duration_ms: Option<u64>, cost_usd: Option<f64>) {
        {
            let mut s = self.inner.write().await;
            s.orchestrator.active = false;
            s.orchestrator.last_duration_ms = duration_ms;
            if let Some(c) = cost_usd {
                s.orchestrator.cost_usd_today += c;
            }
            s.orchestrator.current_session_id = None;
        }
        self.notify(StateEvent::Orchestrator);
    }

    // --- Workers ---

    pub async fn worker_started(&self, worker: WorkerInfo) {
        {
            let mut s = self.inner.write().await;
            s.workers.active.push(worker);
        }
        self.notify(StateEvent::Workers);
    }

    pub async fn worker_finished(&self, id: &str) {
        {
            let mut s = self.inner.write().await;
            s.workers.active.retain(|w| w.id != id);
            s.workers.completed_today = s.workers.completed_today.saturating_add(1);
        }
        self.notify(StateEvent::Workers);
    }

    pub async fn worker_progress(&self, id: &str, progress: f32, status: &str) {
        {
            let mut s = self.inner.write().await;
            if let Some(w) = s.workers.active.iter_mut().find(|w| w.id == id) {
                w.progress = progress.clamp(0.0, 1.0);
                w.status = status.to_string();
            }
        }
        self.notify(StateEvent::Workers);
    }

    // --- Voice ---

    pub async fn set_voice_mode(&self, mode: VoiceMode) {
        {
            let mut s = self.inner.write().await;
            s.voice.mode = mode;
            s.voice.muted = matches!(mode, VoiceMode::Muted);
            if matches!(mode, VoiceMode::Listening) {
                s.voice.last_heard_at = Some(Utc::now());
            }
        }
        self.notify(StateEvent::Voice);
    }

    pub async fn set_partial_transcript(&self, text: &str) {
        {
            let mut s = self.inner.write().await;
            s.voice.partial_transcript = text.to_string();
        }
        self.notify(StateEvent::Voice);
    }

    pub async fn set_voice_config_snapshot(
        &self,
        volume: f32,
        wake_word_enabled: bool,
        ambient_mute_active: bool,
        detected_call_app: Option<String>,
    ) {
        {
            let mut s = self.inner.write().await;
            s.voice.volume = volume;
            s.voice.wake_word_enabled = wake_word_enabled;
            s.voice.ambient_mute_active = ambient_mute_active;
            s.voice.detected_call_app = detected_call_app;
        }
        self.notify(StateEvent::Voice);
    }

    /// Apply the optional voice telemetry fields published by the separate
    /// runtime process without resetting fields omitted by older snapshots.
    pub async fn apply_voice_runtime_snapshot(
        &self,
        volume: Option<f32>,
        wake_word_enabled: Option<bool>,
        tts_queue_len: Option<usize>,
        ambient_mute_active: Option<bool>,
        detected_call_app: Option<String>,
    ) {
        {
            let mut s = self.inner.write().await;
            if let Some(volume) = volume {
                s.voice.volume = volume.clamp(0.0, 1.0);
            }
            if let Some(enabled) = wake_word_enabled {
                s.voice.wake_word_enabled = enabled;
            }
            if let Some(len) = tts_queue_len {
                s.voice.tts_queue_len = len;
            }
            if let Some(active) = ambient_mute_active {
                s.voice.ambient_mute_active = active;
                s.voice.detected_call_app = if active { detected_call_app } else { None };
            }
        }
        self.notify(StateEvent::Voice);
    }

    pub async fn set_tts_queue_len(&self, len: usize) {
        {
            let mut s = self.inner.write().await;
            s.voice.tts_queue_len = len;
        }
        self.notify(StateEvent::Voice);
    }

    // --- Memory ---

    pub async fn set_memory_counts(&self, raw_rows: u64, episodic: u64, semantic: u64) {
        {
            let mut s = self.inner.write().await;
            s.memory.raw_log_rows = raw_rows;
            s.memory.episodic_count = episodic;
            s.memory.semantic_count = semantic;
        }
        self.notify(StateEvent::Memory);
    }

    pub async fn mark_distill(&self) {
        {
            let mut s = self.inner.write().await;
            s.memory.last_distill_ts = Some(Utc::now());
        }
        self.notify(StateEvent::Memory);
    }

    /// Mirrors the `continuum` runtime's curator status (Task 11), as read
    /// from `state.json` by `runtime_bridge::tick_once`. Pass `None` only
    /// when the runtime bridge itself has nothing to report (e.g. an
    /// `state.json` predating this field); once a snapshot has been read,
    /// callers should pass `Some` even when the curator is disabled — see
    /// [`MemoryState::curator`].
    pub async fn set_curator_snapshot(&self, curator: Option<CuratorSnapshot>) {
        {
            let mut s = self.inner.write().await;
            s.memory.curator = curator;
        }
        self.notify(StateEvent::Memory);
    }

    // --- Health ---

    pub async fn set_components(&self, components: Vec<ComponentHealth>) {
        {
            let mut s = self.inner.write().await;
            s.health.components = components;
            s.health.last_check_ts = Some(Utc::now());
            s.health.error_count_24h = s.health.components.iter().map(|c| c.error_count_24h).sum();
        }
        self.notify(StateEvent::Health);
    }

    pub async fn set_repair_running(&self, running: bool) {
        {
            let mut s = self.inner.write().await;
            s.health.repair_running = running;
            if !running {
                s.health.last_repair_ts = Some(Utc::now());
            }

            push_recent(
                &mut s.recent_actions,
                RecentAction {
                    ts: Utc::now(),
                    kind: RecentActionKind::Repair,
                    summary: if running {
                        "repair started".into()
                    } else {
                        "repair finished".into()
                    },
                    detail: None,
                },
            );
        }
        self.notify(StateEvent::Health);
        self.notify(StateEvent::RecentActions);
    }

    pub async fn set_backup_status(&self, last_backup_ts: Option<DateTime<Utc>>, retained: u32) {
        {
            let mut s = self.inner.write().await;
            s.health.last_backup_ts = last_backup_ts;
            s.health.backups_retained = retained;
        }
        self.notify(StateEvent::Health);
    }

    // --- System ---

    pub async fn set_system_flag(&self, setter: impl FnOnce(&mut SystemState)) {
        {
            let mut s = self.inner.write().await;
            setter(&mut s.system);
        }
        self.notify(StateEvent::System);
    }

    pub async fn set_paused(&self, paused: bool) {
        self.set_system_flag(|sys| sys.paused = paused).await;
    }

    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }
}

impl Default for StateHandle {
    fn default() -> Self {
        Self::new()
    }
}

fn push_recent(queue: &mut VecDeque<RecentAction>, action: RecentAction) {
    queue.push_front(action);
    while queue.len() > RECENT_ACTIONS_CAPACITY {
        queue.pop_back();
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_len.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn dummy_frame() -> PerceptionFrame {
        use crate::senses::types::{ContextObservation, ScreenObservation};
        PerceptionFrame {
            id: Uuid::new_v4(),
            ts: Utc::now(),
            screen: ScreenObservation {
                description: "VS Code open on state.rs".into(),
                foreground_app: "Code.exe".into(),
                has_error_visible: false,
                confidence: 0.8,
                screenshot_path: Some("/tmp/a.jpg".into()),
                ts: Utc::now(),
            },
            audio: None,
            context: ContextObservation {
                foreground_window_title: "state.rs - continuum-ai".into(),
                foreground_process_name: "Code.exe".into(),
                idle_seconds: 0,
                in_call: false,
                ts: Utc::now(),
            },
            salience_hint: 0.25,
        }
    }

    #[tokio::test]
    async fn apply_frame_updates_perception_state() {
        let handle = StateHandle::new();
        let frame = dummy_frame();
        handle.apply_frame(&frame).await;

        let snap = handle.snapshot().await;
        assert_eq!(snap.perception.frames_today, 1);
        assert_eq!(snap.perception.last_foreground_app, "Code.exe");
        assert!(snap.perception.last_screenshot_path.is_some());
    }

    #[tokio::test]
    async fn apply_triage_bumps_counts_and_recent_actions() {
        let handle = StateHandle::new();
        handle
            .apply_triage(
                &TriageDecision::Remember {
                    summary: "opened file".into(),
                },
                420,
                "VS Code is open",
            )
            .await;

        let snap = handle.snapshot().await;
        assert_eq!(
            snap.triage.decision_counts_today.get("remember").copied(),
            Some(1)
        );
        assert_eq!(snap.triage.last_latency_ms, Some(420));
        assert_eq!(snap.recent_actions.len(), 1);
        assert_eq!(snap.recent_actions[0].kind, RecentActionKind::Triage);
    }

    #[tokio::test]
    async fn wake_start_and_end_toggle_active_and_cost() {
        let handle = StateHandle::new();
        handle
            .mark_wake_start("test reason", Some("sess-1".into()))
            .await;
        let snap = handle.snapshot().await;
        assert!(snap.orchestrator.active);
        assert_eq!(snap.orchestrator.wakes_today, 1);

        handle.mark_wake_end(Some(1234), Some(0.0123)).await;
        let snap = handle.snapshot().await;
        assert!(!snap.orchestrator.active);
        assert_eq!(snap.orchestrator.last_duration_ms, Some(1234));
        assert!((snap.orchestrator.cost_usd_today - 0.0123).abs() < 1e-9);
    }

    #[tokio::test]
    async fn subscribe_receives_events() {
        let handle = StateHandle::new();
        let mut rx = handle.subscribe();
        handle.set_voice_mode(VoiceMode::Listening).await;

        let got = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .expect("event arrived in time")
            .expect("event not lost");
        assert!(matches!(got, StateEvent::Voice));
    }

    #[tokio::test]
    async fn set_curator_snapshot_updates_memory_and_notifies() {
        let handle = StateHandle::new();
        let mut rx = handle.subscribe();

        // Starts unknown.
        assert!(handle.snapshot().await.memory.curator.is_none());

        handle
            .set_curator_snapshot(Some(CuratorSnapshot {
                last_pass_at: Some("2026-04-14T10:05:00Z".to_string()),
                consecutive_failures: 3,
                candidates_written_total: 9,
                pending_count: 2,
                enabled: true,
            }))
            .await;

        let snap = handle.snapshot().await;
        let curator = snap
            .memory
            .curator
            .expect("curator should be Some after set");
        assert_eq!(curator.consecutive_failures, 3);
        assert_eq!(curator.pending_count, 2);
        assert!(curator.enabled);

        let got = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .expect("event arrived in time")
            .expect("event not lost");
        assert!(matches!(got, StateEvent::Memory));
    }

    #[tokio::test]
    async fn voice_runtime_snapshot_updates_live_fields_without_resetting_mode() {
        let handle = StateHandle::new_with_voice_config(0.8, true);
        handle.set_voice_mode(VoiceMode::Listening).await;
        handle
            .apply_voice_runtime_snapshot(
                Some(0.65),
                Some(false),
                Some(3),
                Some(true),
                Some("Discord.exe".into()),
            )
            .await;

        let snap = handle.snapshot().await;
        assert_eq!(snap.voice.mode, VoiceMode::Listening);
        assert_eq!(snap.voice.volume, 0.65);
        assert_eq!(snap.voice.tts_queue_len, 3);
        assert!(snap.voice.ambient_mute_active);
        assert_eq!(snap.voice.detected_call_app.as_deref(), Some("Discord.exe"));
        assert!(!snap.voice.wake_word_enabled);
    }

    #[tokio::test]
    async fn inactive_ambient_mute_clears_stale_call_app() {
        let handle = StateHandle::new();
        handle
            .apply_voice_runtime_snapshot(None, None, None, Some(true), Some("Zoom.exe".into()))
            .await;
        handle
            .apply_voice_runtime_snapshot(None, None, None, Some(false), None)
            .await;

        let snap = handle.snapshot().await;
        assert!(!snap.voice.ambient_mute_active);
        assert_eq!(snap.voice.detected_call_app, None);
    }

    #[tokio::test]
    async fn recent_actions_cap_at_capacity() {
        let handle = StateHandle::new();
        for i in 0..(RECENT_ACTIONS_CAPACITY + 10) {
            handle
                .apply_triage(&TriageDecision::Ignore, 10, &format!("f{i}"))
                .await;
        }
        let snap = handle.snapshot().await;
        assert_eq!(snap.recent_actions.len(), RECENT_ACTIONS_CAPACITY);
    }
}
