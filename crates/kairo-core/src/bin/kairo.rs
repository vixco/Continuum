//! # kairo
//!
//! The complete Kairo runtime: perception + triage + orchestrator in one binary.
//! This is the first time the full system runs end-to-end.
//!
//! When triage decides `wake_orchestrator`, this binary actually spawns Claude
//! Opus 4.6 and streams the response to the terminal.
//!
//! # Usage
//!
//! ```bash
//! cargo run --release --bin kairo
//! ```

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch, Mutex};
use tracing_subscriber::EnvFilter;

use kairo_vision::VisionModel;

use kairo_core::config::{kairo_dev_dir, load_config, KairoConfig};
use kairo_core::memory::episodic::{EpisodicEvent, EpisodicStore, EventKind};
use kairo_core::memory::raw_log::RawLog;
use kairo_core::memory::retrieval::retrieve_context;
use kairo_core::memory::semantic::SemanticStore;
use kairo_core::orchestrator::spawn::{wake_orchestrator, OrchestratorConfig, OrchestratorEvent};
use kairo_core::orchestrator::wake_context::build_wake_message;
use kairo_core::senses::audio::AudioWatcher;
use kairo_core::senses::context::ContextWatcher;
use kairo_core::senses::frame::PerceptionFrameBuilder;
use kairo_core::senses::types::{
    AudioObservation, ContextObservation, PerceptionFrame, ScreenObservation,
};
use kairo_core::senses::vision::VisionWatcher;
use kairo_core::triage::handlers::handle_decision;
use kairo_core::triage::llm::{TriageConfig, TriageLayer};
use kairo_core::triage::TriageDecision;

#[tokio::main]
async fn main() -> Result<()> {
    // Flags.
    let force_wake = std::env::args().any(|a| a == "--force-wake");

    // Structured logging.
    let default_filter = "info,kairo_core=debug,kairo_vision=info,kairo_llm=info";
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_target(false)
        .compact()
        .init();

    tracing::info!(
        layer = "system",
        component = "kairo",
        "Starting Kairo — perception + triage + orchestrator"
    );

    // --- Config ---
    let dev_dir = kairo_dev_dir();
    let config_path = dev_dir.join("config.toml");
    let config = load_config(&config_path).context("Failed to load configuration")?;

    std::fs::create_dir_all(&dev_dir).context("Failed to create ~/.kairo-dev/")?;
    std::fs::create_dir_all(&config.storage.screenshots_dir)
        .context("Failed to create screenshots directory")?;

    // --- Memory stores ---
    let raw_log = RawLog::open(&config.storage.db_path)
        .await
        .context("Failed to open raw log database")?;

    let semantic_path = dev_dir.join("semantic.sqlite");
    let semantic = Arc::new(
        SemanticStore::open(&semantic_path.to_string_lossy())
            .await
            .context("Failed to open semantic memory")?,
    );

    let episodic_dir = dev_dir.join("episodic_db");
    let episodic = Arc::new(Mutex::new(
        EpisodicStore::open(&episodic_dir.to_string_lossy())
            .await
            .context("Failed to open episodic memory")?,
    ));

    // --- Orchestrator config ---
    let prompt_path = find_system_prompt(&dev_dir);
    let orch_config = OrchestratorConfig {
        model: "claude-opus-4-6".to_string(),
        system_prompt_path: prompt_path,
        timeout_secs: 60,
        bare_mode: false,
    };

    tracing::info!(
        layer = "orchestrator",
        component = "kairo",
        model = %orch_config.model,
        "Orchestrator configured"
    );

    // --- Shutdown signal ---
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let ctrl_c_shutdown = shutdown_tx.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!(
            layer = "system",
            component = "kairo",
            "Ctrl+C received, shutting down..."
        );
        let _ = ctrl_c_shutdown.send(true);
    });

    // --- Perception channels ---
    let (screen_tx, screen_rx) = mpsc::channel::<ScreenObservation>(16);
    let (audio_tx, audio_rx) = mpsc::channel::<AudioObservation>(16);
    let (ctx_tx, ctx_rx) = mpsc::channel::<ContextObservation>(64);
    let (frame_tx, mut frame_rx) = mpsc::channel::<PerceptionFrame>(32);

    // --- Vision ---
    let vision_model = init_vision_model(&config).await;

    let vision_watcher = VisionWatcher::new(
        config.screen.clone(),
        vision_model,
        PathBuf::from(&config.storage.screenshots_dir),
    );
    let vision_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        vision_watcher.run(screen_tx, vision_shutdown).await;
    });

    // --- Audio ---
    let audio_watcher = AudioWatcher::new(config.audio.clone());
    let audio_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        audio_watcher.run(audio_tx, audio_shutdown).await;
    });

    // --- Context ---
    let context_watcher = ContextWatcher::new(config.context.clone());
    let context_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        let _ = context_watcher.run(ctx_tx, context_shutdown).await;
    });

    // --- Frame builder ---
    let frame_builder = PerceptionFrameBuilder::new(config.frame.clone());
    let builder_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        frame_builder
            .run(screen_rx, audio_rx, ctx_rx, frame_tx, builder_shutdown)
            .await;
    });

    // --- Triage ---
    let triage: Option<TriageLayer> = {
        let model_path = dev_dir
            .join("models")
            .join("triage")
            .join("qwen3-8b-q4_k_m.gguf");

        if !model_path.exists() {
            tracing::error!(
                layer = "triage",
                component = "kairo",
                path = %model_path.display(),
                "Triage model not found. Run: powershell scripts/download-models.ps1"
            );
            None
        } else {
            tracing::info!(
                layer = "triage",
                component = "kairo",
                "Initializing triage..."
            );

            let n_threads = std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(4)
                .max(4)
                .min(14);

            let triage_config = TriageConfig {
                model_path: model_path.to_string_lossy().into_owned(),
                context_size: 2048,
                n_threads,
                gpu_layers: 999,
                max_tokens: 256,
                temperature: 0.0,
                latency_warn_ms: 2000,
            };

            match TriageLayer::new(triage_config) {
                Ok(t) => {
                    if let Err(e) = t.warmup().await {
                        tracing::warn!(
                            layer = "triage",
                            component = "kairo",
                            error = %e,
                            "Triage warmup failed"
                        );
                    }
                    tracing::info!(layer = "triage", component = "kairo", "Triage ready");
                    Some(t)
                }
                Err(e) => {
                    tracing::error!(
                        layer = "triage",
                        component = "kairo",
                        error = %e,
                        "Triage failed to init — orchestrator will not be woken"
                    );
                    None
                }
            }
        }
    };

    tracing::info!(
        layer = "system",
        component = "kairo",
        triage = triage.is_some(),
        "All layers running. Press Ctrl+C to stop."
    );

    // --- Main loop ---
    let mut frame_count: u64 = 0;
    let mut recent_frames: Vec<PerceptionFrame> = Vec::new();
    let mut main_shutdown = shutdown_rx.clone();

    loop {
        tokio::select! {
            Some(frame) = frame_rx.recv() => {
                frame_count += 1;

                let audio_text = frame.audio.as_ref().map(|a| a.transcript.as_str()).unwrap_or("");
                let ts = frame.ts.format("%H:%M:%S");

                // Triage.
                let decision: Option<TriageDecision> = if let Some(ref triage_layer) = triage {
                    let triage_start = Instant::now();
                    let d = triage_layer.evaluate(&frame, "").await;
                    let triage_ms = triage_start.elapsed().as_millis();

                    tracing::debug!(
                        layer = "triage",
                        component = "kairo",
                        decision = d.variant_name(),
                        latency_ms = triage_ms as u64,
                        "Triage decision"
                    );

                    println!(
                        "[{ts}] {app} | \"{desc}\" | audio=\"{audio}\" | triage={decision}",
                        app = frame.context.foreground_process_name,
                        desc = truncate(&frame.screen.description, 50),
                        audio = truncate(audio_text, 30),
                        decision = d.variant_name(),
                    );

                    Some(d)
                } else {
                    println!(
                        "[{ts}] {app} | \"{desc}\" | sal={sal:.2}",
                        app = frame.context.foreground_process_name,
                        desc = truncate(&frame.screen.description, 50),
                        sal = frame.salience_hint,
                    );
                    None
                };

                // Write to raw log.
                if let Err(e) = raw_log.write_frame(&frame).await {
                    tracing::error!(
                        layer = "senses",
                        component = "kairo",
                        error = %e,
                        "Raw log write failed"
                    );
                }

                // Keep recent frames for wake context.
                recent_frames.push(frame.clone());
                if recent_frames.len() > 10 {
                    recent_frames.remove(0);
                }

                // --force-wake: override triage on the first frame to test the pipeline.
                let effective_decision = if force_wake && frame_count == 1 {
                    println!("[--force-wake: forcing wake on frame 1]");
                    Some(TriageDecision::WakeOrchestrator {
                        reason: "Force wake for testing — user wants to verify the orchestrator pipeline works end-to-end".to_string(),
                    })
                } else {
                    decision.clone()
                };

                // Handle decision.
                if let Some(ref decision) = effective_decision {
                    match decision {
                        TriageDecision::WakeOrchestrator { reason } => {
                            let history = recent_frames[..recent_frames.len().saturating_sub(1)].to_vec();
                            if let Err(e) = do_wake(
                                &frame,
                                &history,
                                reason,
                                &orch_config,
                                &semantic,
                                &episodic,
                            ).await {
                                tracing::error!(
                                    layer = "orchestrator",
                                    component = "kairo",
                                    error = %e,
                                    "Orchestrator wake failed"
                                );
                                println!("[ORCHESTRATOR ERROR: {e}]");
                            }
                        }
                        _ => {
                            if let Err(e) = handle_decision(decision) {
                                tracing::warn!(
                                    layer = "triage",
                                    component = "kairo",
                                    error = %e,
                                    "Handler failed"
                                );
                            }
                        }
                    }
                }
            }
            _ = main_shutdown.changed() => {
                if *main_shutdown.borrow() {
                    break;
                }
            }
        }
    }

    // --- Graceful shutdown ---
    tracing::info!(
        layer = "system",
        component = "kairo",
        frames = frame_count,
        "Shutting down..."
    );

    raw_log.close().await;
    semantic.close().await;

    tracing::info!(layer = "system", component = "kairo", "Kairo stopped cleanly");
    Ok(())
}

/// Performs a full orchestrator wake cycle.
async fn do_wake(
    trigger_frame: &PerceptionFrame,
    history_frames: &[PerceptionFrame],
    reason: &str,
    config: &OrchestratorConfig,
    semantic: &Arc<SemanticStore>,
    episodic: &Arc<Mutex<EpisodicStore>>,
) -> Result<()> {
    let wake_start = Instant::now();

    println!("\n--- KAIRO WAKING ---");

    // 1. Memory context.
    let memory_context = {
        let mut ep = episodic.lock().await;
        retrieve_context(trigger_frame, &mut *ep, semantic).await?
    };

    tracing::debug!(
        layer = "orchestrator",
        component = "kairo",
        retrieval_ms = wake_start.elapsed().as_millis() as u64,
        "Memory retrieved"
    );

    // 2. Wake message.
    let user_message = build_wake_message(trigger_frame, history_frames, &memory_context, reason);

    // 3. Spawn orchestrator + stream.
    print!("KAIRO: ");
    std::io::stdout().flush().ok();

    let mut full_response = String::new();
    let result = wake_orchestrator(config, &user_message, |event| match &event {
        OrchestratorEvent::TextDelta(text) => {
            print!("{text}");
            std::io::stdout().flush().ok();
            full_response.push_str(text);
        }
        OrchestratorEvent::ResponseComplete {
            full_text,
            cost_usd,
            duration_ms,
            ..
        } => {
            // If no text_delta events came through, print the full text now.
            if full_response.is_empty() && !full_text.is_empty() {
                print!("{full_text}");
                std::io::stdout().flush().ok();
                full_response.push_str(full_text);
            }
            println!();
            let cost_str = cost_usd.map(|c| format!(" ${c:.4}")).unwrap_or_default();
            let dur_str = duration_ms.map(|d| format!(" {d}ms")).unwrap_or_default();
            println!("--- [{dur_str}{cost_str}] ---\n");
        }
        OrchestratorEvent::Error(msg) => {
            println!("\n[ERROR: {msg}]");
        }
        OrchestratorEvent::SessionReady { session_id } => {
            tracing::debug!(
                layer = "orchestrator",
                component = "kairo",
                session_id = %session_id,
                "Session ready"
            );
        }
    })
    .await?;

    // 4. Store in episodic memory.
    if result.success && !full_response.is_empty() {
        let mut ep = episodic.lock().await;

        let wake_event = EpisodicEvent {
            id: uuid::Uuid::new_v4().to_string(),
            ts: trigger_frame.ts,
            kind: EventKind::Wake,
            summary: format!("Woken: {reason}"),
            importance: 0.8,
            tags: vec!["wake".to_string()],
            source_frame_id: Some(trigger_frame.id.to_string()),
        };
        if let Err(e) = ep.insert_event(&wake_event).await {
            tracing::warn!(
                layer = "memory",
                component = "kairo",
                error = %e,
                "Failed to store wake event"
            );
        }

        let response_event = EpisodicEvent {
            id: uuid::Uuid::new_v4().to_string(),
            ts: chrono::Utc::now(),
            kind: EventKind::KairoResponse,
            summary: truncate(&full_response, 200),
            importance: 0.7,
            tags: vec!["response".to_string()],
            source_frame_id: Some(trigger_frame.id.to_string()),
        };
        if let Err(e) = ep.insert_event(&response_event).await {
            tracing::warn!(
                layer = "memory",
                component = "kairo",
                error = %e,
                "Failed to store response event"
            );
        }
    }

    tracing::info!(
        layer = "orchestrator",
        component = "kairo",
        total_ms = wake_start.elapsed().as_millis() as u64,
        cost_usd = result.cost_usd,
        success = result.success,
        "Wake cycle complete"
    );

    Ok(())
}

/// Initialize the vision model with stub fallback.
async fn init_vision_model(config: &KairoConfig) -> Arc<dyn kairo_vision::VisionModel> {
    let model_path = &config.vision.model_path;

    match kairo_vision::onnx::OnnxVisionModel::new(model_path).await {
        Ok(model) => {
            if let Err(e) = model.warmup().await {
                tracing::warn!(
                    layer = "senses",
                    component = "kairo",
                    error = %e,
                    "Vision warmup failed, using stub"
                );
            }
            Arc::new(model)
        }
        Err(e) => {
            tracing::warn!(
                layer = "senses",
                component = "kairo",
                model_path = model_path,
                error = %e,
                "Failed to load vision model, using stub"
            );
            Arc::new(StubVisionModel)
        }
    }
}

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

/// Finds the system prompt file.
fn find_system_prompt(dev_dir: &std::path::Path) -> String {
    let dev_prompt = dev_dir.join("orchestrator-system.md");
    if dev_prompt.exists() {
        return dev_prompt.to_string_lossy().to_string();
    }

    let project_prompt = std::path::PathBuf::from("prompts/orchestrator-system.md");
    if project_prompt.exists() {
        return project_prompt.to_string_lossy().to_string();
    }

    tracing::warn!(
        layer = "orchestrator",
        component = "kairo",
        "System prompt file not found — orchestrator will use default behavior"
    );
    String::new()
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}
