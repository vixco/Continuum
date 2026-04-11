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
//!
//! Requires:
//! - Claude Code CLI installed and authenticated (`claude login`)
//! - Triage model downloaded (`scripts/download-models.ps1`)
//! - ONNX Runtime DLL in PATH or `~/.kairo-dev/lib/`

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

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
use kairo_core::senses::types::{AudioObservation, ContextObservation, PerceptionFrame, ScreenObservation};
use kairo_core::senses::vision::VisionWatcher;
use kairo_core::triage::handlers::handle_decision;
use kairo_core::triage::llm::{TriageConfig, TriageLayer};
use kairo_core::triage::TriageDecision;
use kairo_vision::VisionModel;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize structured logging.
    let default_filter = "info,kairo_core=debug,kairo_vision=info,kairo_llm=info";
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_target(false)
        .init();

    tracing::info!(
        layer = "system",
        component = "kairo",
        "Starting Kairo — perception + triage + orchestrator"
    );

    let config = load_config().unwrap_or_default();
    let dev_dir = kairo_dev_dir();

    // --- Memory stores ---
    let raw_log_path = format!(
        "sqlite:{}/raw_log.sqlite",
        dev_dir.to_str().unwrap_or("~/.kairo-dev")
    );
    let raw_log = RawLog::open(&raw_log_path).await?;

    let semantic_path = format!(
        "sqlite:{}/semantic.sqlite",
        dev_dir.to_str().unwrap_or("~/.kairo-dev")
    );
    let semantic = Arc::new(SemanticStore::open(&semantic_path).await?);

    let episodic_dir = dev_dir.join("episodic_db");
    let episodic = Arc::new(tokio::sync::Mutex::new(
        EpisodicStore::open(episodic_dir.to_str().unwrap_or("~/.kairo-dev/episodic_db")).await?,
    ));

    // --- Orchestrator config ---
    let prompt_path = find_system_prompt(&dev_dir);
    let orch_config = OrchestratorConfig {
        model: config
            .get("orchestrator")
            .and_then(|o| o.get("model"))
            .and_then(|v| v.as_str())
            .unwrap_or("claude-opus-4-6")
            .to_string(),
        system_prompt_path: prompt_path,
        timeout_secs: 60,
        bare_mode: true,
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

    // --- Perception layer ---
    let (frame_tx, mut frame_rx) = mpsc::channel::<PerceptionFrame>(32);

    // Vision watcher.
    let vision_model = match VisionModel::load_default().await {
        Ok(m) => Some(Arc::new(m)),
        Err(e) => {
            tracing::warn!(
                layer = "senses",
                component = "vision",
                error = %e,
                "Vision model unavailable — running without screen descriptions"
            );
            None
        }
    };

    let interval_secs = config
        .get("senses")
        .and_then(|s| s.get("interval_seconds"))
        .and_then(|v| v.as_integer())
        .unwrap_or(3) as u64;

    let frame_builder = PerceptionFrameBuilder::new(frame_tx.clone(), interval_secs);
    let vision_watcher = VisionWatcher::new(vision_model.clone());
    let context_watcher = ContextWatcher::new();

    // Start watchers.
    let fb_shutdown = shutdown_rx.clone();
    let vw = vision_watcher.clone();
    let cw = context_watcher.clone();
    tokio::spawn(async move {
        frame_builder.run(vw, cw, fb_shutdown).await;
    });

    // Audio watcher (best-effort).
    let audio_tx = frame_tx.clone();
    let audio_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        match AudioWatcher::new() {
            Ok(aw) => aw.run(audio_tx, audio_shutdown).await,
            Err(e) => {
                tracing::warn!(
                    layer = "senses",
                    component = "audio",
                    error = %e,
                    "Audio watcher unavailable"
                );
            }
        }
    });

    // --- Triage layer ---
    let triage = {
        let triage_config = TriageConfig::from_kairo_config(&config);
        match TriageLayer::new(triage_config).await {
            Ok(t) => {
                tracing::info!(layer = "triage", component = "kairo", "Triage ready");
                Some(t)
            }
            Err(e) => {
                tracing::error!(
                    layer = "triage",
                    component = "kairo",
                    error = %e,
                    "Triage unavailable — orchestrator will not be woken automatically"
                );
                None
            }
        }
    };

    tracing::info!(
        layer = "system",
        component = "kairo",
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

                // Print one-line status.
                let audio_text = frame.audio.as_ref().map(|a| a.transcript.as_str()).unwrap_or("");
                let ts = frame.ts.format("%H:%M:%S");

                // Run triage if available.
                let decision = if let Some(ref triage_layer) = triage {
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
                        "[{ts}] {app} | \"{desc}\" | triage={decision}",
                        app = frame.context.foreground_process_name,
                        desc = truncate(&frame.screen.description, 50),
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

                // Handle decision.
                if let Some(ref decision) = decision {
                    match decision {
                        TriageDecision::WakeOrchestrator { reason } => {
                            // THE REAL THING: wake Opus.
                            let wake_result = do_wake(
                                &frame,
                                &recent_frames,
                                reason,
                                &orch_config,
                                &semantic,
                                &episodic,
                            ).await;

                            if let Err(e) = wake_result {
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
                            // Handle non-wake decisions the old way.
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
        frames_processed = frame_count,
        "Shutting down..."
    );

    raw_log.close().await;
    semantic.close().await;

    tracing::info!(layer = "system", component = "kairo", "Kairo stopped.");
    Ok(())
}

/// Performs a full orchestrator wake cycle.
///
/// 1. Retrieves memory context
/// 2. Builds wake message
/// 3. Spawns claude process
/// 4. Streams response to terminal
/// 5. Stores interaction in episodic memory
async fn do_wake(
    trigger_frame: &PerceptionFrame,
    history_frames: &[PerceptionFrame],
    reason: &str,
    config: &OrchestratorConfig,
    semantic: &Arc<SemanticStore>,
    episodic: &Arc<tokio::sync::Mutex<EpisodicStore>>,
) -> Result<()> {
    let wake_start = Instant::now();

    println!("\n--- KAIRO WAKING ---");

    // 1. Retrieve memory context.
    let memory_context = {
        let mut ep = episodic.lock().await;
        retrieve_context(trigger_frame, &mut *ep, semantic).await?
    };

    let retrieval_ms = wake_start.elapsed().as_millis();
    tracing::debug!(
        layer = "orchestrator",
        component = "kairo",
        retrieval_ms = retrieval_ms as u64,
        "Memory retrieval complete"
    );

    // 2. Build wake message.
    let user_message = build_wake_message(trigger_frame, history_frames, &memory_context, reason);

    tracing::debug!(
        layer = "orchestrator",
        component = "kairo",
        message_len = user_message.len(),
        "Wake message built"
    );

    // 3. Spawn orchestrator and stream response.
    print!("KAIRO: ");
    std::io::stdout().flush()?;

    let mut full_response = String::new();

    let result = wake_orchestrator(config, &user_message, |event| {
        match &event {
            OrchestratorEvent::TextDelta(text) => {
                print!("{text}");
                std::io::stdout().flush().ok();
                full_response.push_str(text);
            }
            OrchestratorEvent::ResponseComplete { cost_usd, duration_ms, .. } => {
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
        }
    })
    .await?;

    // 4. Store in episodic memory.
    if result.success && !full_response.is_empty() {
        let mut ep = episodic.lock().await;

        // Store the wake event.
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

        // Store Kairo's response.
        let response_event = EpisodicEvent {
            id: uuid::Uuid::new_v4().to_string(),
            ts: chrono::Utc::now(),
            kind: EventKind::KairoResponse,
            summary: truncate(&full_response, 200).to_string(),
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

    let total_ms = wake_start.elapsed().as_millis();
    tracing::info!(
        layer = "orchestrator",
        component = "kairo",
        total_ms = total_ms as u64,
        cost_usd = result.cost_usd,
        success = result.success,
        "Wake cycle complete"
    );

    Ok(())
}

/// Finds the system prompt file, checking several locations.
fn find_system_prompt(dev_dir: &PathBuf) -> String {
    // Check dev dir first.
    let dev_prompt = dev_dir.join("orchestrator-system.md");
    if dev_prompt.exists() {
        return dev_prompt.to_string_lossy().to_string();
    }

    // Check project prompts/ directory (for development).
    let project_prompt = PathBuf::from("prompts/orchestrator-system.md");
    if project_prompt.exists() {
        return project_prompt.to_string_lossy().to_string();
    }

    // Fallback: empty string means no system prompt file.
    tracing::warn!(
        layer = "orchestrator",
        component = "kairo",
        "System prompt file not found — orchestrator will use default behavior"
    );
    String::new()
}

/// Truncates a string to the given max length, appending "..." if truncated.
fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..max_len]
    }
}
