//! # continuum-perception
//!
//! Standalone binary that runs the Continuum senses layer (Layer 1).
//!
//! Captures screenshots, microphone audio, and Windows context, assembles
//! perception frames, and writes them to the SQLite raw log.
//!
//! This binary is for Phase 1 development and testing. In the full Continuum
//! runtime, the senses layer runs inside `continuum-core` as part of the
//! four-layer cognitive engine.
//!
//! # Usage
//!
//! ```bash
//! cargo run --bin continuum-perception
//! cargo run --bin continuum-perception -- --triage   # with triage decisions
//! ```
//!
//! Configuration is loaded from `~/.continuum-dev/config.toml`. If no config
//! file exists, sensible defaults are used.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use continuum_vision::VisionModel;
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

use continuum_core::config::{continuum_dev_dir, load_config, ContinuumConfig};
use continuum_core::memory::raw_log::RawLog;
use continuum_core::senses::audio::AudioWatcher;
use continuum_core::senses::context::ContextWatcher;
use continuum_core::senses::frame::PerceptionFrameBuilder;
use continuum_core::senses::live_context::{self, LiveContextHub};
use continuum_core::senses::types::{AudioObservation, ContextObservation, ScreenObservation};
use continuum_core::senses::vision::VisionWatcher;
use continuum_core::triage::handlers::handle_decision;
use continuum_core::triage::llm::{TriageConfig, TriageLayer};

#[tokio::main]
async fn main() -> Result<()> {
    let triage_enabled = std::env::args().any(|a| a == "--triage");

    // Initialize structured logging.
    let default_filter = if triage_enabled {
        "info,continuum_core=debug,continuum_vision=debug,continuum_llm=debug"
    } else {
        "info,continuum_core=debug,continuum_vision=debug"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_target(false)
        .compact()
        .init();

    tracing::info!(
        layer = "senses",
        component = "main",
        triage = triage_enabled,
        "Starting continuum-perception"
    );

    // Load configuration.
    let config_path = continuum_dev_dir().join("config.toml");
    let mut config = load_config(&config_path).context("Failed to load configuration")?;

    // Adaptive resource policy: probe once and apply the resolved knobs to the
    // loaded config so triage/screen/vision pick up the adapted values. See
    // `continuum_core::hardware`.
    let hardware_specs = continuum_core::hardware::probe_hardware();
    let resource_plan =
        continuum_core::hardware::resolve_resource_policy(&hardware_specs, &config.resources);
    tracing::info!(
        layer = "hardware",
        component = "main",
        plan = ?resource_plan,
        "Resolved adaptive resource plan"
    );
    config.triage.gpu_layers = resource_plan.triage_gpu_layers;
    config.screen.interval_secs = resource_plan.screen_interval_secs;
    config.context.poll_interval_secs = resource_plan.context_interval_secs;
    config.audio.whisper_threads = resource_plan.whisper_threads as i32;
    config.audio.whisper_use_gpu = resource_plan.vision_gpu;

    tracing::info!(
        layer = "senses",
        component = "main",
        config_path = %config_path.display(),
        "Configuration loaded"
    );

    // Ensure data directories exist.
    let dev_dir = continuum_dev_dir();
    std::fs::create_dir_all(&dev_dir).context("Failed to create ~/.continuum-dev/")?;
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

    // Create the shared agent-facing projection and observation channels.
    let live_context = LiveContextHub::new(config.screen.buffer_capacity.saturating_mul(4));
    live_context::spawn_publisher(
        live_context.clone(),
        dev_dir.join("live-context.json"),
        std::time::Duration::from_millis(200),
        shutdown_rx.clone(),
    );
    let (screen_tx, screen_rx) = mpsc::channel::<ScreenObservation>(64);
    let (audio_tx, audio_rx) = mpsc::channel::<AudioObservation>(16);
    let (ctx_tx, ctx_rx) = mpsc::channel::<ContextObservation>(64);
    let (frame_tx, mut frame_rx) = mpsc::channel(32);

    // Initialize the vision model.
    let vision_model = init_vision_model(&config, &resource_plan).await;

    // Spawn the three senses watchers.
    let vision_watcher = VisionWatcher::new_with_live_context(
        config.screen.clone(),
        vision_model,
        PathBuf::from(&config.storage.screenshots_dir),
        live_context.clone(),
    );
    let vision_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        vision_watcher.run(screen_tx, vision_shutdown).await;
    });

    let audio_watcher = AudioWatcher::new(config.audio.clone());
    let audio_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        audio_watcher.run(audio_tx, audio_shutdown, None).await;
    });

    let context_watcher = ContextWatcher::new(config.context.clone());
    let context_shutdown = shutdown_rx.clone();
    let context_live_context = live_context.clone();
    tokio::spawn(async move {
        let _ = context_watcher
            .run_with_live_context(ctx_tx, context_shutdown, context_live_context)
            .await;
    });

    // Spawn the frame builder.
    let frame_builder = PerceptionFrameBuilder::new(config.frame.clone());
    let builder_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        frame_builder
            .run(screen_rx, audio_rx, ctx_rx, frame_tx, builder_shutdown)
            .await;
    });

    // Optionally initialize the triage layer.
    let triage: Option<TriageLayer> = if triage_enabled {
        let model_path = config.triage.resolve_model_path(&dev_dir);

        if !model_path.exists() {
            tracing::error!(
                layer = "triage",
                component = "main",
                path = %model_path.display(),
                "Triage model not found. Run: powershell scripts/download-models.ps1"
            );
            None
        } else {
            tracing::info!(
                layer = "triage",
                component = "main",
                model = %model_path.display(),
                "Initializing triage layer..."
            );

            // Threads come from the resolved adaptive resource plan — a
            // fraction of logical cores with headroom (see
            // `hardware::resolve_resource_policy`).
            let n_threads = resource_plan.triage_threads;

            let triage_config = TriageConfig {
                model_path: model_path.to_string_lossy().into_owned(),
                context_size: config.triage.context_size,
                n_threads,
                gpu_layers: config.triage.gpu_layers,
                max_tokens: config.triage.max_tokens,
                temperature: config.triage.temperature,
                latency_warn_ms: config.triage.latency_warn_ms,
            };

            match TriageLayer::new(triage_config) {
                Ok(t) => {
                    if let Err(e) = t.warmup().await {
                        tracing::warn!(
                            layer = "triage",
                            component = "main",
                            error = %e,
                            "Triage model warmup failed"
                        );
                    }
                    tracing::info!(layer = "triage", component = "main", "Triage layer ready");
                    Some(t)
                }
                Err(e) => {
                    tracing::error!(
                        layer = "triage",
                        component = "main",
                        error = %e,
                        "Failed to initialize triage layer, running perception-only"
                    );
                    None
                }
            }
        }
    } else {
        None
    };

    tracing::info!(
        layer = "senses",
        component = "main",
        triage = triage.is_some(),
        "All senses watchers running. Press Ctrl+C to stop."
    );

    // Main loop: receive frames, log to DB, optionally triage, print summary.
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

                // Run triage if enabled.
                let triage_str = if let Some(ref triage_layer) = triage {
                    let triage_start = Instant::now();
                    let decision = triage_layer.evaluate(&frame, "").await;
                    let triage_ms = triage_start.elapsed().as_millis();

                    tracing::debug!(
                        layer = "triage",
                        component = "main",
                        frame_id = %frame.id,
                        decision = decision.variant_name(),
                        latency_ms = triage_ms as u64,
                        "Triage decision"
                    );

                    // Execute the handler.
                    if let Err(e) = handle_decision(&decision) {
                        tracing::warn!(
                            layer = "triage",
                            component = "handler",
                            error = %e,
                            decision = %decision,
                            "Handler failed"
                        );
                    }

                    format!(" | triage={}", decision.variant_name())
                } else {
                    String::new()
                };

                println!(
                    "[{ts}] app={app} | screen=\"{desc}\" | audio=\"{audio}\" | salience={sal:.2}{triage_str}",
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
        "continuum-perception stopped cleanly"
    );

    Ok(())
}

/// Initialize the vision model, falling back to a stub if loading fails.
///
/// Honours the resolved resource plan: when `plan.vision_enabled` is false
/// (e.g. very low RAM), perception runs text-only and we return the stub.
/// `plan.vision_gpu` selects the ONNX CUDA execution provider (with CPU
/// fallback baked into the model loader).
async fn init_vision_model(
    config: &ContinuumConfig,
    plan: &continuum_core::hardware::ResolvedResourcePlan,
) -> Arc<dyn continuum_vision::VisionModel> {
    if !plan.vision_enabled {
        tracing::info!(
            layer = "senses",
            component = "main",
            "Vision disabled by resource policy; using stub (text-only perception)"
        );
        return Arc::new(StubVisionModel);
    }

    let model_path = &config.vision.model_path;

    match continuum_vision::onnx::OnnxVisionModel::new(model_path, plan.vision_gpu).await {
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
impl continuum_vision::VisionModel for StubVisionModel {
    async fn describe(
        &self,
        _image: &image::DynamicImage,
    ) -> Result<continuum_vision::VisionOutput> {
        Ok(continuum_vision::VisionOutput {
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

/// Truncates a string to at most `max_len` bytes, ending on a UTF-8
/// char boundary, adding "..." if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let target = max_len.saturating_sub(3);
    let mut cut = target.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &s[..cut])
}
