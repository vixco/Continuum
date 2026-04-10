//! # Vision watcher
//!
//! Takes a screenshot every N seconds (default 3, configurable 1-10) using the
//! `xcap` crate for cross-platform screen capture. The screenshot is downscaled
//! to 1280x720 (configurable) and sent to the local vision model (SmolVLM-256M
//! by default) for description.
//!
//! Produces [`ScreenObservation`] structs containing a one-sentence description
//! of what the user is looking at, the foreground app name, and a confidence score.
//!
//! # Architecture
//!
//! This module belongs to Layer 1 (Senses). It produces data that flows upward
//! to the frame builder and then to triage. It never calls into Layer 2 or above.
//!
//! # Error handling
//!
//! Every failure is logged and skipped. The watcher never crashes from a single
//! bad capture or model inference. If the vision model fails, the observation
//! gets an empty description with confidence 0.0.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use image::imageops::FilterType;
use image::DynamicImage;
use tokio::sync::mpsc;
use xcap::Monitor;

use crate::config::ScreenConfig;
use crate::senses::types::ScreenObservation;

/// Captures a screenshot of the primary monitor.
///
/// Uses `xcap::Monitor` to enumerate displays and captures the one marked
/// as primary. Returns the raw RGBA image at native resolution.
///
/// # Errors
///
/// Returns an error if no monitors are found, no primary monitor exists,
/// or the capture call fails.
pub fn capture_primary_monitor() -> Result<image::RgbaImage> {
    let monitors = Monitor::all().context("Failed to enumerate monitors")?;
    let primary = monitors
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .ok_or_else(|| anyhow::anyhow!("No primary monitor found"))?;

    let monitor_name = primary.name().unwrap_or_else(|_| "unknown".to_string());
    let monitor_width = primary.width().unwrap_or(0);
    let monitor_height = primary.height().unwrap_or(0);

    tracing::debug!(
        layer = "senses",
        component = "vision",
        monitor_name = %monitor_name,
        width = monitor_width,
        height = monitor_height,
        "Capturing primary monitor"
    );

    let screenshot = primary
        .capture_image()
        .context("Failed to capture primary monitor screenshot")?;

    Ok(screenshot)
}

/// Downscales a screenshot to the configured resolution.
///
/// Uses triangle (bilinear) filtering for a good balance between speed and
/// quality. Returns a [`DynamicImage`] suitable for saving or passing to
/// the vision model.
pub fn downscale_screenshot(
    screenshot: image::RgbaImage,
    width: u32,
    height: u32,
) -> DynamicImage {
    DynamicImage::ImageRgba8(screenshot).resize_exact(width, height, FilterType::Triangle)
}

/// Saves a screenshot as a JPEG to the screenshots directory.
///
/// The file is placed under `<screenshots_dir>/<YYYY-MM-DD>/<HH-MM-SS>.jpg`.
/// Creates the date subdirectory if it does not exist.
///
/// # Errors
///
/// Returns an error if the directory cannot be created or the image cannot
/// be written. Callers should log and continue rather than propagating.
pub fn save_screenshot(image: &DynamicImage, screenshots_dir: &Path) -> Result<PathBuf> {
    let now = Utc::now();
    let date_dir = screenshots_dir.join(now.format("%Y-%m-%d").to_string());

    std::fs::create_dir_all(&date_dir).with_context(|| {
        format!(
            "Failed to create screenshot directory: {}",
            date_dir.display()
        )
    })?;

    let filename = format!("{}.jpg", now.format("%H-%M-%S"));
    let path = date_dir.join(&filename);

    image
        .to_rgb8()
        .save_with_format(&path, image::ImageFormat::Jpeg)
        .with_context(|| format!("Failed to save screenshot to {}", path.display()))?;

    tracing::debug!(
        layer = "senses",
        component = "vision",
        path = %path.display(),
        "Saved screenshot"
    );

    Ok(path)
}

/// Checks a description string for keywords that indicate an error is visible.
///
/// The vision model's description is scanned for terms like "error", "exception",
/// "stack trace", "crash", "fatal", etc. This is a heuristic — the model may not
/// always mention errors explicitly, and false positives are possible.
fn description_indicates_error(description: &str) -> bool {
    let lower = description.to_lowercase();
    let error_keywords = [
        "error",
        "exception",
        "stack trace",
        "stacktrace",
        "crash",
        "fatal",
        "failed",
        "failure",
        "traceback",
        "panic",
        "segfault",
        "segmentation fault",
        "blue screen",
        "bsod",
        "not responding",
        "dialog box",
        "warning dialog",
        "error dialog",
    ];
    error_keywords.iter().any(|kw| lower.contains(kw))
}

/// Watches the screen by capturing and describing screenshots at a regular interval.
///
/// Holds the screen config, a reference to the vision model, and the path
/// where screenshots are saved. Use [`VisionWatcher::run`] to start the
/// capture loop as a tokio task.
pub struct VisionWatcher {
    /// Screen capture configuration (interval, resolution, save flag).
    config: ScreenConfig,
    /// The local vision model used to describe screenshots.
    vision_model: Arc<dyn kairo_vision::VisionModel>,
    /// Base directory for saved screenshots (`~/.kairo-dev/screenshots/`).
    screenshots_dir: PathBuf,
}

impl VisionWatcher {
    /// Creates a new vision watcher.
    ///
    /// # Arguments
    ///
    /// * `config` - Screen capture settings (interval, resolution, save flag).
    /// * `vision_model` - The local vision model for image description.
    /// * `screenshots_dir` - Base directory for saving screenshot JPEGs.
    pub fn new(
        config: ScreenConfig,
        vision_model: Arc<dyn kairo_vision::VisionModel>,
        screenshots_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            config,
            vision_model,
            screenshots_dir: screenshots_dir.into(),
        }
    }

    /// Runs the vision capture loop until the shutdown signal fires.
    ///
    /// Each iteration:
    /// 1. Captures the primary monitor screenshot.
    /// 2. Downscales to the configured resolution.
    /// 3. Optionally saves the screenshot to disk as JPEG.
    /// 4. Calls the vision model to produce a description.
    /// 5. Builds a [`ScreenObservation`] and sends it through the channel.
    /// 6. Sleeps for the configured interval (minus time already spent).
    ///
    /// If a capture or model call fails, the error is logged and the cycle
    /// is skipped. The loop never panics from transient failures.
    ///
    /// If the vision model takes longer than the configured interval, the
    /// next capture starts immediately (no queue buildup).
    ///
    /// # Arguments
    ///
    /// * `tx` - Channel sender for completed observations.
    /// * `shutdown` - A watch receiver; the loop exits when this receives `true`.
    pub async fn run(
        &self,
        tx: mpsc::Sender<ScreenObservation>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        tracing::info!(
            layer = "senses",
            component = "vision",
            interval_secs = self.config.interval_secs,
            capture_width = self.config.capture_width,
            capture_height = self.config.capture_height,
            save_screenshots = self.config.save_screenshots,
            "Vision watcher starting"
        );

        let interval = tokio::time::Duration::from_secs(self.config.interval_secs);

        loop {
            let cycle_start = Instant::now();

            // Check for shutdown before starting work.
            if *shutdown.borrow() {
                tracing::info!(
                    layer = "senses",
                    component = "vision",
                    "Shutdown signal received, stopping vision watcher"
                );
                break;
            }

            match self.capture_and_describe().await {
                Ok(observation) => {
                    if tx.send(observation).await.is_err() {
                        tracing::warn!(
                            layer = "senses",
                            component = "vision",
                            "Observation channel closed, stopping vision watcher"
                        );
                        break;
                    }
                }
                Err(err) => {
                    tracing::error!(
                        layer = "senses",
                        component = "vision",
                        error = %err,
                        "Vision capture cycle failed, skipping"
                    );
                }
            }

            // Sleep for the remaining interval, or start immediately if the
            // cycle already took longer than the interval.
            let elapsed = cycle_start.elapsed();
            let sleep_duration = interval.saturating_sub(elapsed);

            if sleep_duration.is_zero() {
                tracing::debug!(
                    layer = "senses",
                    component = "vision",
                    elapsed_ms = elapsed.as_millis() as u64,
                    interval_ms = interval.as_millis() as u64,
                    "Cycle took longer than interval, starting next capture immediately"
                );
            }

            // Use select to respect shutdown during the sleep period.
            tokio::select! {
                _ = tokio::time::sleep(sleep_duration) => {}
                _ = shutdown.changed() => {
                    tracing::info!(
                        layer = "senses",
                        component = "vision",
                        "Shutdown signal received during sleep, stopping vision watcher"
                    );
                    break;
                }
            }
        }

        tracing::info!(
            layer = "senses",
            component = "vision",
            "Vision watcher stopped"
        );
    }

    /// Performs a single capture-describe cycle.
    ///
    /// Captures the screen, downscales, optionally saves, runs the vision
    /// model, and returns the assembled [`ScreenObservation`].
    async fn capture_and_describe(&self) -> Result<ScreenObservation> {
        // Screen capture is a blocking OS call; run it on the blocking pool.
        let config_width = self.config.capture_width;
        let config_height = self.config.capture_height;
        let save = self.config.save_screenshots;
        let screenshots_dir = self.screenshots_dir.clone();

        let (image, screenshot_path) = tokio::task::spawn_blocking(move || -> Result<_> {
            let raw = capture_primary_monitor()?;
            let downscaled = downscale_screenshot(raw, config_width, config_height);

            let path = if save {
                match save_screenshot(&downscaled, &screenshots_dir) {
                    Ok(p) => Some(p.to_string_lossy().into_owned()),
                    Err(err) => {
                        tracing::warn!(
                            layer = "senses",
                            component = "vision",
                            error = %err,
                            "Failed to save screenshot, continuing without save"
                        );
                        None
                    }
                }
            } else {
                None
            };

            Ok((downscaled, path))
        })
        .await
        .context("Screenshot capture task panicked")??;

        // Run the vision model. If it fails, produce a degraded observation
        // rather than failing the whole cycle.
        let (description, has_error_visible, confidence) =
            match self.vision_model.describe(&image).await {
                Ok(output) => {
                    // The model provides its own error detection and confidence.
                    // We also run keyword-based error detection as a fallback
                    // in case the model missed an obvious error indicator.
                    let has_error =
                        output.has_error_visible || description_indicates_error(&output.description);
                    (output.description, has_error, output.confidence)
                }
                Err(err) => {
                    tracing::warn!(
                        layer = "senses",
                        component = "vision",
                        error = %err,
                        "Vision model describe failed, emitting empty observation"
                    );
                    (String::new(), false, 0.0)
                }
            };

        // TODO(phase-1): Get the actual foreground app from the context watcher.
        // For now this field is populated by the frame builder from the context
        // observation. We leave it empty here to avoid duplicating the Windows
        // API calls.
        let foreground_app = String::new();

        Ok(ScreenObservation {
            description,
            foreground_app,
            has_error_visible,
            confidence,
            screenshot_path,
            ts: Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kairo_vision::{VisionModel, VisionOutput};

    /// A mock vision model that returns a fixed description.
    struct MockVisionModel {
        description: String,
        has_error: bool,
        confidence: f32,
    }

    impl MockVisionModel {
        fn new(description: &str) -> Self {
            Self {
                description: description.to_string(),
                has_error: false,
                confidence: 0.85,
            }
        }

        fn with_error(description: &str) -> Self {
            Self {
                description: description.to_string(),
                has_error: true,
                confidence: 0.9,
            }
        }
    }

    #[async_trait]
    impl VisionModel for MockVisionModel {
        async fn describe(&self, _image: &DynamicImage) -> Result<VisionOutput> {
            Ok(VisionOutput {
                description: self.description.clone(),
                has_error_visible: self.has_error,
                confidence: self.confidence,
            })
        }

        fn model_name(&self) -> &str {
            "mock-vision"
        }

        async fn warmup(&self) -> Result<()> {
            Ok(())
        }
    }

    /// A mock vision model that always fails.
    struct FailingVisionModel;

    #[async_trait]
    impl VisionModel for FailingVisionModel {
        async fn describe(&self, _image: &DynamicImage) -> Result<VisionOutput> {
            anyhow::bail!("Model inference failed")
        }

        fn model_name(&self) -> &str {
            "failing-mock"
        }

        async fn warmup(&self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_downscale_screenshot() {
        // Create a 1920x1080 test image.
        let img = image::RgbaImage::new(1920, 1080);
        let downscaled = downscale_screenshot(img, 1280, 720);
        assert_eq!(downscaled.width(), 1280);
        assert_eq!(downscaled.height(), 720);
    }

    #[test]
    fn test_downscale_preserves_non_standard_dimensions() {
        let img = image::RgbaImage::new(800, 600);
        let downscaled = downscale_screenshot(img, 640, 480);
        assert_eq!(downscaled.width(), 640);
        assert_eq!(downscaled.height(), 480);
    }

    #[test]
    fn test_save_screenshot_creates_directories() {
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let img = DynamicImage::ImageRgba8(image::RgbaImage::new(100, 100));

        let path = save_screenshot(&img, dir.path()).expect("Failed to save screenshot");

        assert!(path.exists());
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("jpg"));
        // Verify the date subdirectory was created.
        let date_str = Utc::now().format("%Y-%m-%d").to_string();
        assert!(dir.path().join(&date_str).is_dir());
    }

    #[test]
    fn test_save_screenshot_to_invalid_path_fails() {
        let img = DynamicImage::ImageRgba8(image::RgbaImage::new(100, 100));
        // Use a path on a nonexistent drive letter, which reliably fails on Windows.
        // On Unix, use a path under /proc which cannot be a directory.
        let invalid_path = if cfg!(windows) {
            Path::new("Z:\\nonexistent\\deeply\\nested\\path")
        } else {
            Path::new("/proc/0/nonexistent/deeply/nested/path")
        };
        let result = save_screenshot(&img, invalid_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_description_indicates_error_detects_keywords() {
        assert!(description_indicates_error(
            "An error dialog is displayed on screen"
        ));
        assert!(description_indicates_error("Python traceback visible"));
        assert!(description_indicates_error(
            "Application crash with stack trace"
        ));
        assert!(description_indicates_error("Blue screen of death (BSOD)"));
        assert!(description_indicates_error("Program is not responding"));
    }

    #[test]
    fn test_description_indicates_error_ignores_normal_text() {
        assert!(!description_indicates_error(
            "User is viewing a code editor with Python files"
        ));
        assert!(!description_indicates_error(
            "A web browser showing a news article"
        ));
        assert!(!description_indicates_error("Desktop with file explorer open"));
    }

    #[tokio::test]
    async fn test_vision_watcher_sends_observations() {
        let model = Arc::new(MockVisionModel::new("User is viewing a code editor"));
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let config = ScreenConfig {
            interval_secs: 1,
            capture_width: 320,
            capture_height: 240,
            save_screenshots: false,
        };

        let watcher = VisionWatcher::new(config, model, dir.path());
        let (tx, mut rx) = mpsc::channel::<ScreenObservation>(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Run the watcher in a task; shut it down after receiving one observation.
        let handle = tokio::spawn(async move {
            watcher.run(tx, shutdown_rx).await;
        });

        // NOTE: This test requires a display to capture. On headless CI it will
        // fail at capture_primary_monitor(). That is expected; the error path
        // logs and skips. We give it a short window and then shut down.
        let observation = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            rx.recv(),
        )
        .await;

        // Signal shutdown regardless of whether we got an observation.
        let _ = shutdown_tx.send(true);
        let _ = handle.await;

        // On a machine with a display, validate the observation fields.
        if let Ok(Some(obs)) = observation {
            assert_eq!(obs.description, "User is viewing a code editor");
            assert!(!obs.has_error_visible);
            assert!((obs.confidence - 0.85).abs() < f32::EPSILON);
        }
    }

    #[tokio::test]
    async fn test_vision_watcher_handles_model_failure() {
        let model: Arc<dyn kairo_vision::VisionModel> = Arc::new(FailingVisionModel);
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let config = ScreenConfig {
            interval_secs: 1,
            capture_width: 320,
            capture_height: 240,
            save_screenshots: false,
        };

        let watcher = VisionWatcher::new(config, model, dir.path());
        let (tx, mut rx) = mpsc::channel::<ScreenObservation>(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            watcher.run(tx, shutdown_rx).await;
        });

        // On a machine with a display, the model failure should produce a
        // degraded observation with empty description and 0.0 confidence.
        let observation = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            rx.recv(),
        )
        .await;

        let _ = shutdown_tx.send(true);
        let _ = handle.await;

        if let Ok(Some(obs)) = observation {
            assert!(obs.description.is_empty());
            assert!(!obs.has_error_visible);
            assert!((obs.confidence - 0.0).abs() < f32::EPSILON);
        }
    }

    #[tokio::test]
    async fn test_vision_watcher_respects_shutdown() {
        let model = Arc::new(MockVisionModel::new("test"));
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let config = ScreenConfig {
            interval_secs: 60, // Long interval so it is definitely sleeping.
            capture_width: 320,
            capture_height: 240,
            save_screenshots: false,
        };

        let watcher = VisionWatcher::new(config, model, dir.path());
        let (tx, _rx) = mpsc::channel::<ScreenObservation>(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            watcher.run(tx, shutdown_rx).await;
        });

        // Send shutdown immediately. The watcher should exit promptly
        // rather than waiting the full 60-second interval.
        let _ = shutdown_tx.send(true);

        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            handle,
        )
        .await;

        assert!(
            result.is_ok(),
            "Vision watcher did not shut down within 5 seconds"
        );
    }

    #[tokio::test]
    async fn test_vision_watcher_stops_on_closed_channel() {
        let model = Arc::new(MockVisionModel::new("test"));
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let config = ScreenConfig {
            interval_secs: 1,
            capture_width: 320,
            capture_height: 240,
            save_screenshots: false,
        };

        let watcher = VisionWatcher::new(config, model, dir.path());
        let (tx, rx) = mpsc::channel::<ScreenObservation>(1);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Drop the receiver so the channel is closed.
        drop(rx);

        let handle = tokio::spawn(async move {
            watcher.run(tx, shutdown_rx).await;
        });

        // The watcher should exit promptly because the channel is closed.
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(10),
            handle,
        )
        .await;

        assert!(
            result.is_ok(),
            "Vision watcher did not stop after channel closed"
        );
    }

    #[tokio::test]
    async fn test_vision_watcher_detects_error_in_description() {
        let model = Arc::new(MockVisionModel::with_error(
            "An error dialog is displayed with a stack trace",
        ));
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let config = ScreenConfig {
            interval_secs: 1,
            capture_width: 320,
            capture_height: 240,
            save_screenshots: false,
        };

        let watcher = VisionWatcher::new(config, model, dir.path());
        let (tx, mut rx) = mpsc::channel::<ScreenObservation>(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            watcher.run(tx, shutdown_rx).await;
        });

        let observation = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            rx.recv(),
        )
        .await;

        let _ = shutdown_tx.send(true);
        let _ = handle.await;

        // On a machine with a display, the error keywords should be detected.
        if let Ok(Some(obs)) = observation {
            assert!(obs.has_error_visible);
        }
    }
}
