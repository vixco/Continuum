//! # kairo-perception
//!
//! Standalone binary that runs the Kairo senses layer (Layer 1).
//!
//! Captures screenshots, microphone audio, and Windows context, assembles
//! perception frames, and writes them to the SQLite raw log.
//!
//! This binary is for Phase 1 development and testing. In the full Kairo
//! runtime, the senses layer runs inside `kairo-core` as part of the
//! four-layer cognitive engine.
//!
//! # Usage
//!
//! ```bash
//! cargo run --bin kairo-perception
//! ```
//!
//! Configuration is loaded from `~/.kairo-dev/config.toml`. If no config
//! file exists, sensible defaults are used.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;
use kairo_vision::VisionModel;

use kairo_core::config::{kairo_dev_dir, load_config, KairoConfig};
use kairo_core::memory::raw_log::RawLog;
use kairo_core::senses::audio::AudioWatcher;
use kairo_core::senses::context::ContextWatcher;
use kairo_core::senses::frame::PerceptionFrameBuilder;
use kairo_core::senses::types::{AudioObservation, ContextObservation, ScreenObservation};
use kairo_core::senses::vision::VisionWatcher;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize structured logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,kairo_core=debug,kairo_vision=debug")),
        )
        .with_target(false)
        .compact()
        .init();

    tracing::info!(
        layer = "senses",
        component = "main",
        "Starting kairo-perception"
    );

    // Load configuration.
    let config_path = kairo_dev_dir().join("config.toml");
    let config = load_config(&config_path)
        .context("Failed to load configuration")?;

    tracing::info!(
        layer = "senses",
        component = "main",
        config_path = %config_path.display(),
        "Configuration loaded"
    );

    // Ensure data directories exist.
    let dev_dir = kairo_dev_dir();
    std::fs::create_dir_all(&dev_dir)
        .context("Failed to create ~/.kairo-dev/")?;
    std::fs::create_dir_all(&config.storage.screenshots_dir)
        .context("Failed to create screenshots directory")?;

    // Open the raw log database.
    let raw_log = RawLog::open(&config.storage.db_path)
        .await
        .context("Failed to open raw log database")?;

    // Create the shutdown signal.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Set up Ctrl+C handler.
    let shutdown_tx_ctrlc = shutdown_tx.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
        tracing::info!(
            layer = "senses",
            component = "main",
            "Ctrl+C received, shutting down..."
        );
        let _ = shutdown_tx_ctrlc.send(true);
    });

    // Create observation channels.
    let (screen_tx, screen_rx) = mpsc::channel::<ScreenObservation>(16);
    let (audio_tx, audio_rx) = mpsc::channel::<AudioObservation>(16);
    let (ctx_tx, ctx_rx) = mpsc::channel::<ContextObservation>(64);
    let (frame_tx, mut frame_rx) = mpsc::channel(32);

    // Initialize the vision model.
    let vision_model = init_vision_model(&config).await;

    // Spawn the three senses watchers.
    let vision_watcher = VisionWatcher::new(
        config.screen.clone(),
        vision_model,
        PathBuf::from(&config.storage.screenshots_dir),
    );
    let vision_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        vision_watcher.run(screen_tx, vision_shutdown).await;
    });

    let audio_watcher = AudioWatcher::new(config.audio.clone());
    let audio_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        audio_watcher.run(audio_tx, audio_shutdown).await;
    });

    let context_watcher = ContextWatcher::new(config.context.clone());
    let context_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        let _ = context_watcher.run(ctx_tx, context_shutdown).await;
    });

    // Spawn the frame builder.
    let frame_builder = PerceptionFrameBuilder::new(config.frame.clone());
    let builder_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        frame_builder
            .run(screen_rx, audio_rx, ctx_rx, frame_tx, builder_shutdown)
            .await;
    });

    tracing::info!(
        layer = "senses",
        component = "main",
        "All senses watchers running. Press Ctrl+C to stop."
    );

    // Main loop: receive frames, log to DB, print summary.
    let mut frame_count: u64 = 0;
    let mut main_shutdown = shutdown_rx.clone();
    loop {
        tokio::select! {
            Some(frame) = frame_rx.recv() => {
                frame_count += 1;

                // Print one-line summary.
                let audio_text = frame.audio
                    .as_ref()
                    .map(|a| a.transcript.as_str())
                    .unwrap_or("");
                let ts = frame.ts.format("%H:%M:%S");

                println!(
                    "[{ts}] app={app} | screen=\"{desc}\" | audio=\"{audio}\" | salience={sal:.2}",
                    app = frame.context.foreground_process_name,
                    desc = truncate(&frame.screen.description, 60),
                    audio = truncate(audio_text, 40),
                    sal = frame.salience_hint,
                );

                // Write to raw log.
                if let Err(e) = raw_log.write_frame(&frame).await {
                    tracing::error!(
                        layer = "senses",
                        component = "main",
                        error = %e,
                        "Failed to write frame to raw log"
                    );
                }
            }
            _ = main_shutdown.changed() => {
                if *main_shutdown.borrow() {
                    break;
                }
            }
        }
    }

    // Graceful shutdown.
    tracing::info!(
        layer = "senses",
        component = "main",
        frames = frame_count,
        "Shutting down, flushing database..."
    );

    raw_log.close().await;

    tracing::info!(
        layer = "senses",
        component = "main",
        "kairo-perception stopped cleanly"
    );

    Ok(())
}

/// Initialize the vision model, falling back to a stub if loading fails.
async fn init_vision_model(
    config: &KairoConfig,
) -> Arc<dyn kairo_vision::VisionModel> {
    let model_path = &config.vision.model_path;

    match kairo_vision::onnx::OnnxVisionModel::new(model_path).await {
        Ok(model) => {
            // Warm up the model.
            if let Err(e) = model.warmup().await {
                tracing::warn!(
                    layer = "senses",
                    component = "main",
                    error = %e,
                    "Vision model warmup failed, using stub descriptions"
                );
            }
            Arc::new(model)
        }
        Err(e) => {
            tracing::warn!(
                layer = "senses",
                component = "main",
                model_path = model_path,
                error = %e,
                "Failed to load vision model, using stub. Download models with \
                 scripts/download-models.ps1"
            );
            Arc::new(StubVisionModel)
        }
    }
}

/// Fallback vision model that returns placeholder descriptions.
struct StubVisionModel;

#[async_trait::async_trait]
impl kairo_vision::VisionModel for StubVisionModel {
    async fn describe(
        &self,
        _image: &image::DynamicImage,
    ) -> Result<kairo_vision::VisionOutput> {
        Ok(kairo_vision::VisionOutput {
            description: "(no vision model loaded)".to_string(),
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

/// Truncates a string to `max_len` characters, adding "..." if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}
