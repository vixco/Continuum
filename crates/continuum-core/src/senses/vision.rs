//! # Continuous multi-monitor vision watcher
//!
//! Every connected monitor is captured by its own local worker at the target
//! cadence. A bounded FIFO decouples capture from slower local VLM inference;
//! overflow drops the oldest pending capture and is reported in live context.
//! Cheap 64x36 luma differencing selects meaningful changes for description.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use image::imageops::FilterType;
use image::DynamicImage;
use parking_lot::Mutex;
use tokio::sync::{mpsc, Notify};
use xcap::Monitor;

use crate::config::{ContextConfig, ObservationToggles, PrivacyConfig, ScreenConfig};
use crate::senses::cadence::CadenceControl;
use crate::senses::live_context::{
    strictest_disposition, LiveContextHub, MonitorWorldState, PrivacyDisposition,
};
use crate::senses::privacy::{emit_system_event, source_enabled, ObservedSource, PrivacyFilter};
use crate::senses::toggles::ToggleControl;
use crate::senses::types::ScreenObservation;

/// Capture the primary monitor once. Retained for diagnostics and compatibility.
pub fn capture_primary_monitor() -> Result<image::RgbaImage> {
    let monitors = Monitor::all().context("Failed to enumerate monitors")?;
    let primary = monitors
        .into_iter()
        .find(|monitor| monitor.is_primary().unwrap_or(false))
        .ok_or_else(|| anyhow::anyhow!("No primary monitor found"))?;
    primary
        .capture_image()
        .context("Failed to capture primary monitor screenshot")
}

/// Downscale a screenshot for the local vision model.
pub fn downscale_screenshot(screenshot: image::RgbaImage, width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgba8(screenshot).resize_exact(width, height, FilterType::Triangle)
}

/// Save a selected changed frame as a local JPEG.
pub fn save_screenshot(image: &DynamicImage, screenshots_dir: &Path) -> Result<PathBuf> {
    let now = Utc::now();
    let date_dir = screenshots_dir.join(now.format("%Y-%m-%d").to_string());
    std::fs::create_dir_all(&date_dir).with_context(|| {
        format!(
            "Failed to create screenshot directory: {}",
            date_dir.display()
        )
    })?;
    let path = date_dir.join(format!("{}.jpg", now.format("%H-%M-%S-%3f")));
    image
        .to_rgb8()
        .save_with_format(&path, image::ImageFormat::Jpeg)
        .with_context(|| format!("Failed to save screenshot to {}", path.display()))?;
    Ok(path)
}

fn description_indicates_error(description: &str) -> bool {
    let lower = description.to_lowercase();
    [
        "error",
        "exception",
        "stack trace",
        "stacktrace",
        "crash",
        "fatal",
        "failed",
        "traceback",
        "panic",
        "blue screen",
        "bsod",
        "not responding",
        "error dialog",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
}

#[derive(Debug, Clone)]
struct MonitorDescriptor {
    native_id: u32,
    id: String,
    name: String,
    is_primary: bool,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl MonitorDescriptor {
    fn signature(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}",
            self.name, self.x, self.y, self.width, self.height, self.is_primary
        )
    }
}

#[derive(Debug)]
struct CapturePacket {
    capture_sequence: u64,
    captured_at: chrono::DateTime<Utc>,
    target: MonitorDescriptor,
    image: Option<image::RgbaImage>,
    change_score: f32,
    meaningful_change: bool,
    privacy: PrivacyDisposition,
    capture_latency_ms: u64,
}

#[derive(Debug)]
struct Buffered<T> {
    sequence: u64,
    dropped_before: u64,
    value: T,
}

#[derive(Debug)]
struct OrderedBufferInner<T> {
    items: VecDeque<Buffered<T>>,
    next_sequence: u64,
}

/// Bounded FIFO with explicit oldest-drop accounting.
#[derive(Debug)]
struct OrderedBuffer<T> {
    capacity: usize,
    inner: Arc<Mutex<OrderedBufferInner<T>>>,
    notify: Arc<Notify>,
}

impl<T> Clone for OrderedBuffer<T> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            inner: self.inner.clone(),
            notify: self.notify.clone(),
        }
    }
}

impl<T> OrderedBuffer<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Arc::new(Mutex::new(OrderedBufferInner {
                items: VecDeque::with_capacity(capacity.max(1)),
                next_sequence: 0,
            })),
            notify: Arc::new(Notify::new()),
        }
    }

    #[cfg(test)]
    fn push(&self, value: T) -> (u64, u64) {
        self.push_with(value, |_| {})
    }

    fn push_with(&self, value: T, publish: impl FnOnce(&Buffered<T>)) -> (u64, u64) {
        let mut inner = self.inner.lock();
        let dropped_before = if inner.items.len() == self.capacity {
            1
        } else {
            0
        };
        if dropped_before > 0 {
            inner.items.pop_front();
        }
        inner.next_sequence = inner.next_sequence.saturating_add(1);
        let sequence = inner.next_sequence;
        inner.items.push_back(Buffered {
            sequence,
            dropped_before,
            value,
        });
        if let Some(buffered) = inner.items.back() {
            publish(buffered);
        }
        drop(inner);
        self.notify.notify_one();
        (sequence, dropped_before)
    }

    async fn pop(&self) -> Buffered<T> {
        loop {
            let notified = self.notify.notified();
            if let Some(value) = self.inner.lock().items.pop_front() {
                return value;
            }
            notified.await;
        }
    }
}

#[derive(Debug, Clone)]
struct VisionCache {
    description: String,
    has_error_visible: bool,
    confidence: f32,
    updated_at: Option<chrono::DateTime<Utc>>,
    last_inference: Option<Instant>,
    /// Last wake-nudge sequence this monitor served (spec §4.11): a
    /// changed sequence forces one inference regardless of the minimum
    /// interval, consumed only by a describable packet so an old
    /// buffered capture can't waste the forced pass.
    last_nudge_seen: u64,
}

impl Default for VisionCache {
    fn default() -> Self {
        Self {
            description: "awaiting local vision".into(),
            has_error_visible: false,
            confidence: 0.0,
            updated_at: None,
            last_inference: None,
            last_nudge_seen: 0,
        }
    }
}

/// Continuous all-monitor capture plus selective local vision description.
pub struct VisionWatcher {
    config: ScreenConfig,
    vision_model: Arc<dyn continuum_vision::VisionModel>,
    screenshots_dir: PathBuf,
    live_context: LiveContextHub,
    /// Privacy choke point (spec §4.1): captions are scrubbed through this
    /// filter at collector emit, before the hub and the frame channel.
    privacy: Arc<PrivacyFilter>,
    /// Honest per-source toggles; `screen` (and `pause_all`) gate this
    /// watcher entirely.
    toggles: ObservationToggles,
    /// Live toggle control (Task C5): re-read by the capture scheduler and
    /// the vision consumer so a Context-page switch stops screen capture
    /// without a restart.
    toggle_control: Option<ToggleControl>,
    /// Runtime-adjustable cadences (spec §3 sanctioned pattern, Task A8):
    /// the capture workers and the vision gate read these every
    /// iteration; the runtime's idle controller adjusts them without a
    /// restart. Standalone construction seeds them from `[screen]`.
    cadence: CadenceControl,
}

impl VisionWatcher {
    /// Construct a watcher with an internal world-state handle (mainly tests).
    pub fn new(
        config: ScreenConfig,
        vision_model: Arc<dyn continuum_vision::VisionModel>,
        screenshots_dir: impl Into<PathBuf>,
    ) -> Self {
        Self::new_with_live_context(
            config,
            vision_model,
            screenshots_dir,
            LiveContextHub::default(),
        )
    }

    /// Construct a watcher attached to the runtime's shared world-state.
    ///
    /// Standalone construction synthesizes a privacy filter from default
    /// config; the runtime shares its boot-time filter via
    /// [`VisionWatcher::with_privacy`].
    pub fn new_with_live_context(
        config: ScreenConfig,
        vision_model: Arc<dyn continuum_vision::VisionModel>,
        screenshots_dir: impl Into<PathBuf>,
        live_context: LiveContextHub,
    ) -> Self {
        let cadence =
            CadenceControl::new(config.capture_interval_ms, config.vision_min_interval_ms);
        Self {
            config,
            vision_model,
            screenshots_dir: screenshots_dir.into(),
            live_context,
            privacy: Arc::new(PrivacyFilter::from_config(
                &ContextConfig::default(),
                &PrivacyConfig::default(),
            )),
            toggles: ObservationToggles::default(),
            toggle_control: None,
            cadence,
        }
    }

    /// Attaches the shared boot-time privacy filter and observation
    /// toggles (spec §4.1). Called once at senses spawn.
    pub fn with_privacy(mut self, filter: Arc<PrivacyFilter>, toggles: ObservationToggles) -> Self {
        self.privacy = filter;
        self.toggles = toggles;
        self
    }

    /// Attaches the shared **live** toggle control (Task C5, spec §4.13).
    ///
    /// Without it the watcher honours the boot-time [`ObservationToggles`]
    /// copy it was given; with it, every loop iteration re-reads the
    /// current value, so a Context-page switch takes effect without a
    /// restart.
    pub fn with_toggle_control(mut self, control: ToggleControl) -> Self {
        self.toggle_control = Some(control);
        self
    }

    /// The toggle values to honour right now: the live control when one is
    /// attached, else the boot-time copy.
    fn live_toggles(&self) -> ObservationToggles {
        match &self.toggle_control {
            Some(control) => control.snapshot(),
            None => self.toggles.clone(),
        }
    }

    /// Attaches the runtime's shared cadence control (Task A8, spec
    /// §4.11) so the idle controller can adjust capture/vision cadences
    /// while this watcher runs. Called once at senses spawn.
    pub fn with_cadence(mut self, cadence: CadenceControl) -> Self {
        self.cadence = cadence;
        self
    }

    /// Run until shutdown. Capture workers never await inference or triage.
    pub async fn run(
        &self,
        tx: mpsc::Sender<ScreenObservation>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        if !self.config.enabled {
            tracing::info!(
                layer = "senses",
                component = "vision",
                "Screen capture disabled by user configuration"
            );
            return;
        }

        // Honest toggles (spec §4.1): a disabled screen source emits
        // nothing. Cheapest honest implementation: the capture scheduler
        // never starts, so no worker threads run, no bitmaps are captured,
        // and no packets exist to leak. Toggles are read at spawn time
        // (config has no hot-reload yet — NEXT-tier work); changing the
        // toggle requires a runtime restart until then.
        if !source_enabled(&self.live_toggles(), ObservedSource::Screen) {
            emit_system_event(
                "toggle_change",
                "screen observation disabled by [privacy.toggles]; vision watcher emits nothing",
            );
            let _ = shutdown.changed().await;
            return;
        }

        tracing::info!(
            layer = "senses",
            component = "vision",
            capture_interval_ms = self.config.capture_interval_ms,
            all_monitors = self.config.all_monitors,
            buffer_capacity = self.config.buffer_capacity,
            change_threshold = self.config.meaningful_change_threshold,
            save_screenshots = self.config.save_screenshots,
            "Continuous multi-monitor watcher starting"
        );

        let buffer = OrderedBuffer::new(self.config.buffer_capacity);
        let scheduler = tokio::spawn(run_capture_scheduler(
            self.config.clone(),
            self.cadence.clone(),
            self.toggle_control
                .clone()
                .unwrap_or_else(|| ToggleControl::new(&self.toggles)),
            buffer.clone(),
            self.live_context.clone(),
            shutdown.clone(),
        ));
        let mut cache: HashMap<String, VisionCache> = HashMap::new();

        loop {
            tokio::select! {
                packet = buffer.pop() => {
                    if tx.is_closed() {
                        break;
                    }
                    self.process_capture(packet, &tx, &mut cache).await;
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }

        scheduler.abort();
        let _ = scheduler.await;
        tracing::info!(
            layer = "senses",
            component = "vision",
            "Vision watcher stopped"
        );
    }

    async fn process_capture(
        &self,
        buffered: Buffered<CapturePacket>,
        tx: &mpsc::Sender<ScreenObservation>,
        cache: &mut HashMap<String, VisionCache>,
    ) {
        tracing::trace!(
            layer = "senses",
            component = "vision",
            capture_event_sequence = buffered.sequence,
            "Processing ordered capture event"
        );
        // Defense in depth for the live toggle (Task C5): a bitmap that
        // was already in the ordered buffer when the switch flipped is
        // dropped rather than captioned, stored or published.
        if !source_enabled(&self.live_toggles(), ObservedSource::Screen) {
            return;
        }
        let mut packet = buffered.value;
        // Privacy is attached at capture time, per monitor (foreground
        // disposition + the §4.1 visible-window sweep). A later change may
        // make processing stricter, but never less strict for this bitmap.
        let current_privacy = self.live_context.monitor_privacy(&packet.target.id);
        let privacy = strictest_disposition(packet.privacy, current_privacy);
        let monitor_cache = cache.entry(packet.target.id.clone()).or_default();
        // Vision minimum interval is read per packet from the shared
        // cadence (spec §3 sanctioned pattern): the idle controller may
        // have relaxed it (or set 0 = fully paused) since this watcher
        // spawned. A pending wake nudge (spec §4.11) forces one
        // inference; it is consumed only by a describable packet so an
        // old buffered capture can't waste it.
        let vision_min_interval_ms = self.cadence.vision_min_interval_ms();
        let nudge = self.cadence.nudge_seq();
        let nudged = monitor_cache.last_nudge_seen != nudge
            && privacy == PrivacyDisposition::Visible
            && packet.meaningful_change
            && packet.image.is_some();
        if nudged {
            monitor_cache.last_nudge_seen = nudge;
        }
        let inference_due = if nudged {
            true
        } else if vision_min_interval_ms == 0 {
            // 0 = vision fully paused during idle (spec §4.11).
            false
        } else {
            monitor_cache
                .last_inference
                .map(|last| {
                    last.elapsed() >= Duration::from_millis(vision_min_interval_ms.max(100))
                })
                .unwrap_or(true)
        };
        let should_describe = privacy == PrivacyDisposition::Visible
            && packet.meaningful_change
            && packet.image.is_some()
            && inference_due;
        let mut screenshot_path = None;
        let mut vision_updated = false;

        if privacy == PrivacyDisposition::Excluded {
            // Sentinel semantics (spec §4.1): a never_observe monitor gets
            // no caption and no screenshot file — the bitmap is dropped
            // here and never reaches the vision model or the disk.
            monitor_cache.description = String::new();
            monitor_cache.has_error_visible = false;
            monitor_cache.confidence = 1.0;
            monitor_cache.updated_at = None;
            packet.image = None;
        } else if privacy != PrivacyDisposition::Visible {
            monitor_cache.description = "[redacted by local privacy policy]".into();
            monitor_cache.has_error_visible = false;
            monitor_cache.confidence = 1.0;
            packet.image = None;
        } else if should_describe {
            let raw = packet.image.take().expect("image checked above");
            let image =
                downscale_screenshot(raw, self.config.capture_width, self.config.capture_height);
            if self.config.save_screenshots {
                let monitor_dir = self.screenshots_dir.join(&packet.target.id);
                match save_screenshot(&image, &monitor_dir) {
                    Ok(path) => screenshot_path = Some(path.to_string_lossy().into_owned()),
                    Err(error) => tracing::warn!(
                        layer = "senses",
                        component = "vision",
                        monitor_id = %packet.target.id,
                        error = %error,
                        "Failed to save selected screenshot"
                    ),
                }
            }
            monitor_cache.last_inference = Some(Instant::now());
            match self.vision_model.describe(&image).await {
                Ok(output) => {
                    // Caption is free text: scrub at collector emit
                    // (spec §4.1), before the hub fork and the frame
                    // channel.
                    let description = self.privacy.scrub_text(&output.description);
                    monitor_cache.has_error_visible =
                        output.has_error_visible || description_indicates_error(&description);
                    monitor_cache.description = description;
                    monitor_cache.confidence = output.confidence;
                    monitor_cache.updated_at = Some(Utc::now());
                    vision_updated = true;
                }
                Err(error) => tracing::warn!(
                    layer = "senses",
                    component = "vision",
                    monitor_id = %packet.target.id,
                    error = %error,
                    "Local vision inference failed; capture continues"
                ),
            }
        }

        // Re-check at publication time (monotonic tightening — the zone may
        // have become stricter while the model ran, never less strict).
        let publication_privacy = strictest_disposition(
            privacy,
            self.live_context.monitor_privacy(&packet.target.id),
        );
        if publication_privacy == PrivacyDisposition::Excluded {
            monitor_cache.description = String::new();
            monitor_cache.has_error_visible = false;
            monitor_cache.confidence = 1.0;
            monitor_cache.updated_at = None;
            vision_updated = false;
        } else if publication_privacy != PrivacyDisposition::Visible {
            monitor_cache.description = "[redacted by local privacy policy]".into();
            monitor_cache.has_error_visible = false;
            monitor_cache.confidence = 1.0;
            monitor_cache.updated_at = None;
            vision_updated = false;
        }

        // The caption for THIS monitor, after the publication-time privacy
        // re-check: the raw one-sentence vision-model output (scrubbed), the
        // redaction marker, or "" for an excluded monitor. Bound here so the
        // mutable borrow of `cache` ends before the observation below reads
        // the whole cache for the error rollup.
        let caption = monitor_cache.description.clone();

        self.live_context.record_monitor_vision(
            &packet.target.id,
            caption.clone(),
            monitor_cache.confidence,
            monitor_cache.updated_at,
            publication_privacy,
            vision_updated,
        );

        let snapshot = self.live_context.snapshot();
        let observation = ScreenObservation {
            // Caption only (spec §4.10). The compact world-state blob used
            // to live here, which put ~1.4 kB of monitor/window/project
            // text into every triage prompt; it now rides `world_compact`
            // and is consumed by the context packager instead.
            description: caption,
            world_compact: Some(snapshot.compact_for_agents(1_400)),
            foreground_app: String::new(),
            has_error_visible: snapshot.monitors.iter().any(|monitor| {
                cache
                    .get(&monitor.monitor_id)
                    .is_some_and(|entry| entry.has_error_visible)
            }),
            confidence: snapshot
                .monitors
                .iter()
                .map(|monitor| monitor.confidence)
                .fold(0.0, f32::max),
            screenshot_path,
            ts: packet.captured_at,
        };
        match tx.try_send(observation) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => self.live_context.record_output_drop(1),
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

async fn run_capture_scheduler(
    config: ScreenConfig,
    cadence: CadenceControl,
    toggles: ToggleControl,
    buffer: OrderedBuffer<CapturePacket>,
    live_context: LiveContextHub,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    struct CaptureWorker {
        signature: String,
        cancel: Arc<AtomicBool>,
        handle: std::thread::JoinHandle<()>,
    }

    async fn stop_worker(worker: CaptureWorker) {
        worker.cancel.store(true, Ordering::Release);
        let _ = tokio::task::spawn_blocking(move || worker.handle.join()).await;
    }

    let global_shutdown = Arc::new(AtomicBool::new(false));
    let mut workers: HashMap<String, CaptureWorker> = HashMap::new();
    let mut discovery = tokio::time::interval(Duration::from_secs(2));
    discovery.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = discovery.tick() => {
                // Honest toggle, live (spec §4.1, Task C5): switching the
                // screen source off stops every capture worker within one
                // discovery tick — no bitmap is taken, so no packet can
                // leak. Switching it back on respawns them here.
                if !toggles.enabled(ObservedSource::Screen) {
                    let stopped: Vec<CaptureWorker> =
                        workers.drain().map(|(_, worker)| worker).collect();
                    if !stopped.is_empty() {
                        tracing::info!(
                            layer = "senses",
                            component = "vision",
                            workers = stopped.len(),
                            "screen observation switched off; stopping capture workers"
                        );
                    }
                    for worker in stopped {
                        stop_worker(worker).await;
                    }
                    continue;
                }
                let discovery_config = config.clone();
                match tokio::task::spawn_blocking(move || enumerate_monitors(&discovery_config)).await {
                    Ok(Ok(targets)) => {
                        let connected: HashSet<String> =
                            targets.iter().map(|target| target.id.clone()).collect();
                        live_context.set_connected_monitors(connected.iter().cloned());
                        let removed: Vec<String> = workers
                            .keys()
                            .filter(|id| !connected.contains(*id))
                            .cloned()
                            .collect();
                        for id in removed {
                            if let Some(worker) = workers.remove(&id) {
                                stop_worker(worker).await;
                            }
                        }
                        for target in targets {
                            let signature = target.signature();
                            if workers.get(&target.id).is_some_and(|worker| worker.signature != signature) {
                                if let Some(worker) = workers.remove(&target.id) {
                                    stop_worker(worker).await;
                                }
                            }
                            if !workers.contains_key(&target.id) {
                                let id = target.id.clone();
                                let worker_config = config.clone();
                                let worker_cadence = cadence.clone();
                                let worker_buffer = buffer.clone();
                                let worker_context = live_context.clone();
                                let worker_shutdown = global_shutdown.clone();
                                let cancel = Arc::new(AtomicBool::new(false));
                                let worker_cancel = cancel.clone();
                                let handle = std::thread::Builder::new()
                                    .name(format!("continuum-capture-{id}"))
                                    .spawn(move || run_monitor_capture_loop(
                                        target,
                                        worker_config,
                                        worker_cadence,
                                        worker_buffer,
                                        worker_context,
                                        worker_shutdown,
                                        worker_cancel,
                                    ));
                                match handle {
                                    Ok(handle) => {
                                        workers.insert(id, CaptureWorker { signature, cancel, handle });
                                    }
                                    Err(error) => live_context.record_capture_failure(format!(
                                        "monitor {id} worker thread failed to start: {error}"
                                    )),
                                }
                            }
                        }
                    }
                    Ok(Err(error)) => live_context.record_capture_failure(error.to_string()),
                    Err(error) => live_context.record_capture_failure(format!(
                        "monitor discovery task failed: {error}"
                    )),
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    global_shutdown.store(true, Ordering::Release);
    for (_, worker) in workers {
        stop_worker(worker).await;
    }
}

fn run_monitor_capture_loop(
    target: MonitorDescriptor,
    config: ScreenConfig,
    cadence: CadenceControl,
    buffer: OrderedBuffer<CapturePacket>,
    live_context: LiveContextHub,
    shutdown: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
) {
    let monitor = match monitor_by_native_id(target.native_id) {
        Ok(monitor) => monitor,
        Err(error) => {
            live_context.record_capture_failure(format!(
                "monitor {} could not initialize on its capture thread: {error}",
                target.id
            ));
            return;
        }
    };
    let mut previous_signature: Option<Vec<u8>> = None;
    let mut previous_privacy = PrivacyDisposition::Redacted;
    let mut capture_sequence = 0u64;
    // Wake-nudge tracking (spec §4.11): initialized to the current
    // sequence so only nudges issued *after* this worker started force a
    // meaningful capture.
    let mut last_nudge_seen = cadence.nudge_seq();
    while !shutdown.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
        capture_sequence = capture_sequence.saturating_add(1);
        let captured_at = Utc::now();
        let started = Instant::now();
        let nudge = cadence.nudge_seq();
        let nudged = nudge != last_nudge_seen;
        last_nudge_seen = nudge;
        // Interval is re-read every iteration from the shared cadence
        // (spec §3 sanctioned pattern) — the idle controller adjusts it
        // without restarting this thread.
        let target_interval_ms = cadence.capture_interval_ms().max(50);
        match monitor.capture_image() {
            Ok(image) => {
                let signature = change_signature(&image);
                let change_score = previous_signature
                    .as_deref()
                    .map(|previous| mean_luma_difference(previous, &signature))
                    .unwrap_or(1.0);
                // Per-monitor disposition: strictest of the foreground
                // window and this monitor's sweep-derived zone (spec §4.1).
                let privacy = live_context.monitor_privacy(&target.id);
                let privacy_became_visible = previous_privacy != PrivacyDisposition::Visible
                    && privacy == PrivacyDisposition::Visible;
                // A wake nudge marks this capture meaningful (image
                // attached) so the wake gets a fresh caption even when
                // the screen didn't change while idle (spec §4.11).
                let meaningful_change = previous_signature.is_none()
                    || change_score >= config.meaningful_change_threshold
                    || privacy_became_visible
                    || nudged;
                previous_signature = Some(signature);
                previous_privacy = privacy;
                let capture_latency_ms = started.elapsed().as_millis() as u64;
                let packet = CapturePacket {
                    capture_sequence,
                    captured_at,
                    target: target.clone(),
                    image: meaningful_change.then_some(image),
                    change_score,
                    meaningful_change,
                    privacy,
                    capture_latency_ms,
                };
                buffer.push_with(packet, |buffered| {
                    if buffered.dropped_before > 0 {
                        live_context.record_capture_drop(buffered.dropped_before);
                    }
                    let packet = &buffered.value;
                    live_context.record_monitor_capture(MonitorWorldState {
                        monitor_id: packet.target.id.clone(),
                        name: packet.target.name.clone(),
                        is_primary: packet.target.is_primary,
                        x: packet.target.x,
                        y: packet.target.y,
                        width: packet.target.width,
                        height: packet.target.height,
                        capture_event_sequence: buffered.sequence,
                        capture_sequence: packet.capture_sequence,
                        captured_at: packet.captured_at,
                        change_score: packet.change_score,
                        meaningful_change: packet.meaningful_change,
                        description: String::new(),
                        confidence: 0.0,
                        vision_updated_at: None,
                        privacy,
                        target_interval_ms,
                        capture_latency_ms: packet.capture_latency_ms,
                        dropped_before: buffered.dropped_before,
                    });
                });
            }
            Err(error) => live_context
                .record_capture_failure(format!("monitor {} capture failed: {error}", target.id)),
        }

        // The deadline is recomputed every ≤50 ms slice from the *current*
        // cadence (spec §3): when the idle controller shortens the
        // interval mid-sleep the shorter deadline applies immediately,
        // and a pending wake nudge breaks the wait for an instant
        // capture (spec §4.11 wake-during-pause).
        loop {
            if shutdown.load(Ordering::Acquire) || cancel.load(Ordering::Acquire) {
                break;
            }
            let deadline = started + Duration::from_millis(cadence.capture_interval_ms().max(50));
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if cadence.nudge_seq() != last_nudge_seen {
                break;
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(50)),
            );
        }
    }
}

fn monitor_by_native_id(native_id: u32) -> Result<Monitor> {
    Monitor::all()
        .context("Failed to enumerate monitors on capture thread")?
        .into_iter()
        .find(|monitor| monitor.id().is_ok_and(|id| id == native_id))
        .ok_or_else(|| anyhow::anyhow!("monitor id {native_id} is no longer connected"))
}

fn enumerate_monitors(config: &ScreenConfig) -> Result<Vec<MonitorDescriptor>> {
    let mut targets = Vec::new();
    for monitor in Monitor::all().context("Failed to enumerate monitors")? {
        let native_id = monitor.id().context("Failed to read monitor id")?;
        let id = format!("display-{native_id}");
        if config
            .excluded_monitor_ids
            .iter()
            .any(|excluded| excluded == &id)
        {
            continue;
        }
        let is_primary = monitor.is_primary().unwrap_or(false);
        if !config.all_monitors && !is_primary {
            continue;
        }
        targets.push(MonitorDescriptor {
            native_id,
            name: monitor.name().unwrap_or_else(|_| id.clone()),
            x: monitor.x().unwrap_or(0),
            y: monitor.y().unwrap_or(0),
            width: monitor.width().unwrap_or(0),
            height: monitor.height().unwrap_or(0),
            is_primary,
            id,
        });
    }
    if targets.is_empty() {
        anyhow::bail!("No non-excluded monitors found");
    }
    targets.sort_by_key(|target| (target.x, target.y, target.id.clone()));
    Ok(targets)
}

fn change_signature(image: &image::RgbaImage) -> Vec<u8> {
    let reduced = image::imageops::resize(image, 64, 36, FilterType::Triangle);
    DynamicImage::ImageRgba8(reduced).to_luma8().into_raw()
}

fn mean_luma_difference(previous: &[u8], current: &[u8]) -> f32 {
    if previous.len() != current.len() || current.is_empty() {
        return 1.0;
    }
    let total: u64 = previous
        .iter()
        .zip(current)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    total as f32 / (current.len() as f32 * 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luma_difference_ignores_identical_frames() {
        assert_eq!(mean_luma_difference(&[10, 20, 30], &[10, 20, 30]), 0.0);
    }

    #[test]
    fn luma_difference_is_normalized() {
        let score = mean_luma_difference(&[0, 0], &[255, 127]);
        assert!((score - 0.749).abs() < 0.01);
    }

    #[tokio::test]
    async fn bounded_buffer_keeps_order_and_reports_gap() {
        let buffer = OrderedBuffer::new(2);
        assert_eq!(buffer.push("one").1, 0);
        assert_eq!(buffer.push("two").1, 0);
        assert_eq!(buffer.push("three").1, 1);
        let first = buffer.pop().await;
        let second = buffer.pop().await;
        assert_eq!(first.value, "two");
        assert_eq!(second.value, "three");
        assert!(first.sequence < second.sequence);
        assert_eq!(second.dropped_before, 1);
    }

    #[test]
    fn screenshot_save_uses_unique_millisecond_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let image = DynamicImage::new_rgb8(10, 10);
        let path = save_screenshot(&image, dir.path()).expect("save screenshot");
        assert!(path.exists());
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("jpg")
        );
    }

    struct StubModel;

    #[async_trait::async_trait]
    impl continuum_vision::VisionModel for StubModel {
        async fn describe(
            &self,
            _image: &image::DynamicImage,
        ) -> Result<continuum_vision::VisionOutput> {
            Ok(continuum_vision::VisionOutput {
                description: "stub".into(),
                has_error_visible: false,
                confidence: 0.0,
            })
        }

        fn model_name(&self) -> &str {
            "stub"
        }

        async fn warmup(&self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn screen_toggle_off_stops_all_emission() {
        // Honest toggles (spec §4.1): with `[privacy.toggles].screen =
        // false` the watcher emits nothing even though capture is enabled
        // in config — the capture scheduler never starts.
        use crate::config::{ContextConfig, ObservationToggles, PrivacyConfig};
        use crate::senses::privacy::PrivacyFilter;

        let config = ScreenConfig {
            enabled: true,
            ..ScreenConfig::default()
        };
        let dir = tempfile::tempdir().expect("temp dir");
        let filter = Arc::new(PrivacyFilter::from_config(
            &ContextConfig::default(),
            &PrivacyConfig::default(),
        ));
        let toggles = ObservationToggles {
            screen: false,
            ..ObservationToggles::default()
        };
        let watcher = VisionWatcher::new(config, Arc::new(StubModel), dir.path())
            .with_privacy(filter, toggles);
        let (tx, mut rx) = mpsc::channel::<ScreenObservation>(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            watcher.run(tx, shutdown_rx).await;
        });

        let _ = shutdown_tx.send(true);
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(
            result.is_ok(),
            "Screen-toggled-off watcher should exit promptly on shutdown"
        );
        assert!(
            rx.try_recv().is_err(),
            "Screen-toggled-off watcher must emit no observations"
        );
    }
}
