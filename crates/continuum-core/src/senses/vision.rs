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

use crate::config::ScreenConfig;
use crate::senses::live_context::{LiveContextHub, MonitorWorldState, PrivacyDisposition};
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
}

impl Default for VisionCache {
    fn default() -> Self {
        Self {
            description: "awaiting local vision".into(),
            has_error_visible: false,
            confidence: 0.0,
            updated_at: None,
            last_inference: None,
        }
    }
}

/// Continuous all-monitor capture plus selective local vision description.
pub struct VisionWatcher {
    config: ScreenConfig,
    vision_model: Arc<dyn continuum_vision::VisionModel>,
    screenshots_dir: PathBuf,
    live_context: LiveContextHub,
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
    pub fn new_with_live_context(
        config: ScreenConfig,
        vision_model: Arc<dyn continuum_vision::VisionModel>,
        screenshots_dir: impl Into<PathBuf>,
        live_context: LiveContextHub,
    ) -> Self {
        Self {
            config,
            vision_model,
            screenshots_dir: screenshots_dir.into(),
            live_context,
        }
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
        let mut packet = buffered.value;
        // Privacy is attached at capture time. A later foreground change may
        // make processing stricter, but never less strict for this bitmap.
        let current_privacy = self.live_context.current_privacy();
        let privacy = if packet.privacy == PrivacyDisposition::Visible
            && current_privacy == PrivacyDisposition::Visible
        {
            PrivacyDisposition::Visible
        } else {
            PrivacyDisposition::Redacted
        };
        let monitor_cache = cache.entry(packet.target.id.clone()).or_default();
        let inference_due = monitor_cache
            .last_inference
            .map(|last| {
                last.elapsed() >= Duration::from_millis(self.config.vision_min_interval_ms.max(100))
            })
            .unwrap_or(true);
        let should_describe = privacy == PrivacyDisposition::Visible
            && packet.meaningful_change
            && packet.image.is_some()
            && inference_due;
        let mut screenshot_path = None;
        let mut vision_updated = false;

        if privacy != PrivacyDisposition::Visible {
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
                    monitor_cache.has_error_visible = output.has_error_visible
                        || description_indicates_error(&output.description);
                    monitor_cache.description = output.description;
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

        let publication_privacy = self.live_context.current_privacy();
        if publication_privacy != PrivacyDisposition::Visible {
            monitor_cache.description = "[redacted by local privacy policy]".into();
            monitor_cache.has_error_visible = false;
            monitor_cache.confidence = 1.0;
            monitor_cache.updated_at = None;
            vision_updated = false;
        }

        self.live_context.record_monitor_vision(
            &packet.target.id,
            monitor_cache.description.clone(),
            monitor_cache.confidence,
            monitor_cache.updated_at,
            publication_privacy,
            vision_updated,
        );

        let snapshot = self.live_context.snapshot();
        let observation = ScreenObservation {
            description: snapshot.compact_for_agents(1_400),
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
    buffer: OrderedBuffer<CapturePacket>,
    live_context: LiveContextHub,
    shutdown: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
) {
    let cadence = Duration::from_millis(config.capture_interval_ms.max(50));
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
    while !shutdown.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
        capture_sequence = capture_sequence.saturating_add(1);
        let captured_at = Utc::now();
        let started = Instant::now();
        match monitor.capture_image() {
            Ok(image) => {
                let signature = change_signature(&image);
                let change_score = previous_signature
                    .as_deref()
                    .map(|previous| mean_luma_difference(previous, &signature))
                    .unwrap_or(1.0);
                let privacy = live_context.current_privacy();
                let privacy_became_visible = previous_privacy != PrivacyDisposition::Visible
                    && privacy == PrivacyDisposition::Visible;
                let meaningful_change = previous_signature.is_none()
                    || change_score >= config.meaningful_change_threshold
                    || privacy_became_visible;
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
                        target_interval_ms: config.capture_interval_ms.max(50),
                        capture_latency_ms: packet.capture_latency_ms,
                        dropped_before: buffered.dropped_before,
                    });
                });
            }
            Err(error) => live_context
                .record_capture_failure(format!("monitor {} capture failed: {error}", target.id)),
        }

        let deadline = started + cadence;
        while !shutdown.load(Ordering::Acquire)
            && !cancel.load(Ordering::Acquire)
            && Instant::now() < deadline
        {
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
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
}
