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
//! cargo run --bin continuum-perception -- --record ~/.continuum-dev/recordings/today.jsonl
//! ```
//!
//! Configuration is loaded from `~/.continuum-dev/config.toml`. If no config
//! file exists, sensible defaults are used.
//!
//! # Record mode (context engine spec §9, Task C6)
//!
//! `--record <path>` writes every **post-privacy** perception frame and
//! every collector event to newline-delimited JSON with a relative `t_ms`
//! offset, so the [`continuum_core::bench::replay`] harness can drive the
//! pipeline over a real session without any watcher running.
//!
//! **A recording is LOCAL-ONLY.** It contains exactly the content the
//! privacy work protects — real window titles, captions, transcripts,
//! project paths, commit subjects. Keep recordings outside the repository
//! (`~/.continuum-dev/recordings/` is the suggested home); never commit
//! one, never attach one to an issue, never hand one to a cloud model. The
//! only JSONL under version control is the hand-authored synthetic fixture
//! in `crates/continuum-core/benches/data/`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

use continuum_core::bench::record::Recorder;
use continuum_core::config::{continuum_dev_dir, load_config, ContinuumConfig};
use continuum_core::context::project::{FrameInput, ProjectResolver};
use continuum_core::memory::events::{project_switch_event, ContextEvent, EventSender};
use continuum_core::memory::raw_log::RawLog;
use continuum_core::senses::audio::AudioWatcher;
use continuum_core::senses::context::ContextWatcher;
use continuum_core::senses::frame::PerceptionFrameBuilder;
use continuum_core::senses::git_watch::GitWatcher;
use continuum_core::senses::live_context::{self, LiveContextHub};
use continuum_core::senses::privacy::{emit_system_event, PrivacyFilter};
use continuum_core::senses::types::{AudioObservation, ContextObservation, ScreenObservation};
use continuum_core::senses::vision::VisionWatcher;
use continuum_core::triage::handlers::handle_decision;
use continuum_core::triage::llm::{TriageConfig, TriageLayer};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let triage_enabled = args.iter().any(|a| a == "--triage");
    // `--record <path>`: post-privacy frames + collector events → JSONL
    // (spec §9). See the module docs — recordings are local-only.
    let record_path = args
        .iter()
        .position(|a| a == "--record")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);

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

    // Privacy filter (context engine spec §4.1): constructed once at senses
    // spawn and shared into every watcher — same wiring as the runtime bin.
    let privacy_filter = Arc::new(PrivacyFilter::from_config(&config.context, &config.privacy));
    let observation_toggles = config.privacy.toggles.clone();

    // Project resolver (Task A4, spec §4.3): same wiring as the runtime
    // bin, but purely in-memory — this bin never persists the Projects
    // table (discovery proposals live only for the process lifetime).
    let mut project_resolver = ProjectResolver::from_config(&config.projects);
    let project_handle = project_resolver.handle();

    // Record mode (spec §9, Task C6): everything below the privacy choke
    // point can be serialized to JSONL for the replay harness. Created
    // before the watchers so the very first frame lands in the file, and
    // timestamped relative to *now* so `t_ms` starts at zero.
    let recorder: Option<Arc<Recorder>> = match &record_path {
        Some(path) => match Recorder::create(path, chrono::Utc::now()) {
            Ok(recorder) => {
                tracing::info!(
                    layer = "senses",
                    component = "recorder",
                    path = %path.display(),
                    "Recording post-privacy frames and events (LOCAL-ONLY — never commit a recording)"
                );
                Some(Arc::new(recorder))
            }
            Err(e) => {
                tracing::error!(
                    layer = "senses",
                    component = "recorder",
                    path = %path.display(),
                    error = %e,
                    "Failed to open the recording; continuing without it"
                );
                None
            }
        },
        None => None,
    };

    // Events sink (Task A6): the perception bin runs NO events-writer —
    // context_events belongs to the runtime binary (single-writer). All
    // collector events land in a log-only sink here; system events fall
    // back to log-only automatically because no global sender is
    // installed in this process.
    //
    // Record mode taps the same observer seam session state uses (Task
    // B5): synchronous, after the registry check, before the (absent)
    // queue — so a recording sees exactly the events a runtime would
    // persist.
    let event_sender = match recorder.clone() {
        Some(recorder) => {
            EventSender::log_only().with_observer(Arc::new(move |event: &ContextEvent| {
                recorder.record_event(event)
            }))
        }
        None => EventSender::log_only(),
    };

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

    if observation_toggles.pause_all {
        // pause_all gates the entire frame loop (spec §4.1): no watchers,
        // no frame builder, no frames built or persisted.
        emit_system_event(
            "toggle_change",
            "pause_all set in [privacy.toggles]; observation fully paused — no frames will be built",
        );
        drop(screen_tx);
        drop(audio_tx);
        drop(ctx_tx);
        drop(screen_rx);
        drop(audio_rx);
        drop(ctx_rx);
        drop(frame_tx);
    } else {
        // Initialize the vision model.
        let vision_model = init_vision_model(&config, &resource_plan).await;

        // Spawn the three senses watchers.
        let vision_watcher = VisionWatcher::new_with_live_context(
            config.screen.clone(),
            vision_model,
            PathBuf::from(&config.storage.screenshots_dir),
            live_context.clone(),
        )
        .with_privacy(privacy_filter.clone(), observation_toggles.clone());
        let vision_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            vision_watcher.run(screen_tx, vision_shutdown).await;
        });

        let audio_watcher = AudioWatcher::new(config.audio.clone())
            .with_privacy(privacy_filter.clone(), observation_toggles.clone());
        let audio_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            audio_watcher.run(audio_tx, audio_shutdown, None).await;
        });

        let context_watcher = ContextWatcher::new(config.context.clone())
            .with_privacy(privacy_filter.clone(), observation_toggles.clone())
            .with_project_handle(project_handle.clone())
            .with_event_sender(event_sender.clone());
        let context_shutdown = shutdown_rx.clone();
        let context_live_context = live_context.clone();
        tokio::spawn(async move {
            let _ = context_watcher
                .run_with_live_context(ctx_tx, context_shutdown, context_live_context)
                .await;
        });

        // Git collector (Task A5, spec §4.4): same wiring as the runtime
        // bin — active confirmed project only, disabled-with-reason parks.
        let git_watcher = GitWatcher::new(config.git_context.clone())
            .with_privacy(privacy_filter.clone(), observation_toggles.clone())
            .with_project_handle(project_handle.clone())
            .with_event_sender(event_sender.clone());
        let git_shutdown = shutdown_rx.clone();
        let git_live_context = live_context.clone();
        tokio::spawn(async move {
            git_watcher.run(git_shutdown, Some(git_live_context)).await;
        });

        // Spawn the frame builder.
        let frame_builder = PerceptionFrameBuilder::new(config.frame.clone());
        let builder_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            frame_builder
                .run(screen_rx, audio_rx, ctx_rx, frame_tx, builder_shutdown)
                .await;
        });
    }

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

                // Record mode (spec §9): the frame is already post-privacy
                // — the frame builder receives observations the watchers
                // emitted through the §4.1 choke point.
                if let Some(recorder) = &recorder {
                    recorder.record_frame(&frame);
                }

                // Project resolver (Task A4): resolve once per frame and
                // emit project_switch events, same as the runtime bin —
                // but into this bin's log-only sink (no events DB here).
                let project_outcome = project_resolver.observe(&FrameInput {
                    process_name: &frame.context.foreground_process_name,
                    window_title: &frame.context.foreground_window_title,
                    recent_file_path: None,
                    ts: frame.ts,
                });
                if let Some(switch) = &project_outcome.switched {
                    event_sender.send(project_switch_event(
                        switch.from.as_deref(),
                        &switch.to,
                        &frame.context.foreground_process_name,
                        &frame.context.foreground_window_title,
                        project_outcome.current.as_ref().and_then(|p| p.zone),
                        switch.ts,
                    ));
                }

                // Print one-line summary.
                let audio_text = frame.audio
                    .as_ref()
                    .map(|a| a.transcript.as_str())
                    .unwrap_or("");
                let ts = frame.ts.format("%H:%M:%S");

                // Run triage if enabled.
                let triage_str = if let Some(ref triage_layer) = triage {
                    let triage_start = Instant::now();
                    let output = triage_layer.evaluate(&frame, "").await;
                    let triage_ms = triage_start.elapsed().as_millis();
                    // Perception bin only exercises the decision; the §4.7
                    // classification block is consumed by the runtime (B3).
                    let decision = output.decision;

                    tracing::debug!(
                        layer = "triage",
                        component = "main",
                        frame_id = %frame.id,
                        decision = decision.variant_name(),
                        classification = output.classification.is_some(),
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

    if let (Some(recorder), Some(path)) = (&recorder, &record_path) {
        tracing::info!(
            layer = "senses",
            component = "recorder",
            path = %path.display(),
            lines = recorder.written(),
            failed = recorder.failed(),
            "Recording closed — keep it local, never commit it"
        );
    }

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
/// `plan.vision_gpu` requests an available GGUF or ONNX GPU backend; CPU and
/// ONNX model fallback are handled by the shared loader.
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

    match continuum_core::senses::vision::load_configured_vision_model(
        &config.vision,
        plan.vision_gpu,
    )
    .await
    {
        Ok(model) => model,
        Err(e) => {
            tracing::warn!(
                layer = "senses",
                component = "main",
                model_path = config.vision.model_path,
                error = %e,
                "Primary and fallback vision models failed, using stub. Download models with \
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
