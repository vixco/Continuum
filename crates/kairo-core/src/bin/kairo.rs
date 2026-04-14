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
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch, Mutex};
use tracing_subscriber::EnvFilter;

use kairo_vision::VisionModel;

use kairo_core::config::{kairo_dev_dir, load_config, KairoConfig};
use kairo_core::memory::distill::run_memory_distiller;
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
use kairo_core::voice::playback::PlaybackStream;
use kairo_core::voice::sounds::{FeedbackCue, FeedbackPlayer};
use kairo_core::voice::stt::{EndpointDecision, SemanticEndpointDetector, VoiceSession};
use kairo_core::voice::streaming::SpeechController;
use kairo_core::voice::tts::{set_espeak_data_dir, ElevenLabsEngine, PiperEngine, TtsEngine};
use kairo_core::voice::wake::TranscriptWakeDetector;

#[cfg(windows)]
use kairo_core::voice::hotkey::spawn_hotkey_listener;

#[tokio::main]
async fn main() -> Result<()> {
    // Flags.
    let args: Vec<String> = std::env::args().collect();
    let force_wake = args.iter().any(|a| a == "--force-wake");
    let reset_audio = args.iter().any(|a| a == "--reset-audio");
    let no_tts = args.iter().any(|a| a == "--no-tts");

    // --- Config ---
    let dev_dir = kairo_dev_dir();
    std::fs::create_dir_all(&dev_dir).context("Failed to create ~/.kairo-dev/")?;
    let config_path = dev_dir.join("config.toml");
    let mut config = load_config(&config_path).context("Failed to load configuration")?;

    // Audio device: always use the Windows default input device.
    // `--reset-audio` just clears any stale picker fields from config.toml so
    // they don't linger — the selection itself always comes from Windows.
    if reset_audio {
        if let Err(e) = kairo_core::senses::audio::clear_audio_config(&config_path) {
            eprintln!("--reset-audio: failed to clear saved device: {e}");
        } else {
            println!("--reset-audio: cleared saved picker fields. Using Windows default.");
        }
        config.audio.device_name.clear();
        config.audio.device_index = None;
    }

    std::fs::create_dir_all(&config.storage.screenshots_dir)
        .context("Failed to create screenshots directory")?;

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

    // --- TTS + feedback ---
    let (speech, feedback): (Option<Arc<SpeechController>>, FeedbackPlayer) = if no_tts {
        tracing::info!(
            layer = "voice",
            component = "kairo",
            "--no-tts: speech output disabled"
        );
        (None, FeedbackPlayer::disabled())
    } else {
        init_tts_and_feedback(&config)
    };

    // --- Orchestrator config ---
    let prompt_path = find_system_prompt(&dev_dir);
    let orch_config = OrchestratorConfig {
        model: "claude-opus-4-6".to_string(),
        system_prompt_path: prompt_path,
        timeout_secs: 60,
        bare_mode: false,
        // Phase 4: enable MCP tools. Data dir is the same ~/.kairo-dev/
        // the main runtime uses, so the MCP server reads/writes the same
        // semantic + episodic stores.
        mcp_enabled: true,
        mcp_server_path: None,
        mcp_config_path: None,
        mcp_data_dir: Some(dev_dir.clone()),
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

    // Phase 3: background raw-log to episodic-memory distillation.
    let distiller_shutdown = shutdown_rx.clone();
    let distiller_raw_log = raw_log.clone();
    let distiller_episodic = episodic.clone();
    let distiller_config = config.memory.clone();
    tokio::spawn(async move {
        run_memory_distiller(
            distiller_raw_log,
            distiller_episodic,
            distiller_config,
            distiller_shutdown,
        )
        .await;
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
                .clamp(4, 14);

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

    // --- Hotkey (Windows only) ---
    // Drop guard: _hotkey_handle must stay in scope for the listener thread
    // to keep running. Dropping it at end of main unregisters the hotkey.
    #[cfg(windows)]
    let (_hotkey_handle, mut hotkey_rx) = match spawn_hotkey(&config) {
        Some((handle, rx)) => (Some(handle), Some(rx)),
        None => (None, None),
    };
    #[cfg(not(windows))]
    let mut hotkey_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>> = None;

    // --- Main loop ---
    let mut frame_count: u64 = 0;
    let mut recent_frames: Vec<PerceptionFrame> = Vec::new();
    let mut main_shutdown = shutdown_rx.clone();
    let wake_detector = TranscriptWakeDetector::new(config.voice.wake_keyword.clone());
    let mut voice_session: Option<VoiceSession> = None;
    // `followup_until` is shared between the main loop (reads + clears) and
    // the spawned do_wake tasks (writes after a wake completes), so it lives
    // behind a std::sync::Mutex — locks are held for microseconds.
    let followup_until: Arc<std::sync::Mutex<Option<Instant>>> =
        Arc::new(std::sync::Mutex::new(None));
    // `orchestrator_busy` gates new wakes. When true, triage keeps running
    // but the wake_orchestrator / whisper decisions are suppressed so we
    // don't stack multiple Opus calls on top of each other. Cleared by the
    // spawned wake task when do_wake completes.
    let orchestrator_busy: Arc<std::sync::atomic::AtomicBool> =
        Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut hotkey_pending: bool = false;

    loop {
        tokio::select! {
            Some(frame) = frame_rx.recv() => {
                frame_count += 1;

                let audio_text = frame.audio.as_ref().map(|a| a.transcript.as_str()).unwrap_or("");
                let ts = frame.ts.format("%H:%M:%S");

                // Triage gate — skip the Qwen call when the frame carries
                // no reason to wake the orchestrator anyway. Saves a full
                // GPU burst (~800 ms on Qwen 3 8B) per skipped frame,
                // which is most frames in steady state. Quality is
                // unchanged: these frames would have produced
                // `Ignore` anyway.
                //
                // Skip when ALL of:
                //   - salience < threshold (nothing new happened), AND
                //   - no audio transcript (user said nothing), AND
                //   - no error visible on screen, AND
                //   - orchestrator is either idle, or busy but we still
                //     have no audio (voice arms first-class — they force
                //     a triage call because the user might be speaking
                //     follow-up).
                let has_audio = frame
                    .audio
                    .as_ref()
                    .is_some_and(|a| !a.transcript.trim().is_empty());
                let skip_triage = frame.salience_hint < config.frame.salience_threshold
                    && !has_audio
                    && !frame.screen.has_error_visible;

                let decision: Option<TriageDecision> = if let Some(ref triage_layer) = triage {
                    if skip_triage {
                        tracing::trace!(
                            layer = "triage",
                            component = "kairo",
                            frame_id = %frame.id,
                            salience = frame.salience_hint,
                            "Skipped triage — low-salience idle frame"
                        );
                        Some(TriageDecision::Ignore)
                    } else {
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
                    }
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

                let voice_decision = update_voice_session(
                    &frame,
                    &config,
                    &wake_detector,
                    &mut voice_session,
                    &followup_until,
                    &mut hotkey_pending,
                    speech.as_ref(),
                    &feedback,
                );

                // --force-wake: override triage on the first frame to test the pipeline.
                let effective_decision = if force_wake && frame_count == 1 {
                    println!("[--force-wake: forcing wake on frame 1]");
                    Some(TriageDecision::WakeOrchestrator {
                        reason: "Force wake for testing — user wants to verify the orchestrator pipeline works end-to-end".to_string(),
                    })
                } else {
                    voice_decision.or_else(|| decision.clone())
                };

                // Handle decision.
                if let Some(ref decision) = effective_decision {
                    match decision {
                        TriageDecision::WakeOrchestrator { reason } => {
                            // If we're already inside a wake (orchestrator still
                            // streaming from a previous trigger), don't stack — log
                            // and skip. The user's latest utterance still landed in
                            // the raw log; they can ask again.
                            if orchestrator_busy.load(std::sync::atomic::Ordering::Acquire) {
                                tracing::warn!(
                                    layer = "orchestrator",
                                    component = "kairo",
                                    reason = %reason,
                                    "Orchestrator already busy — skipping new wake"
                                );
                            } else {
                                let history = recent_frames[..recent_frames.len().saturating_sub(1)].to_vec();
                                let wake_speech_opt = if config.voice.ambient_mute_enabled && frame.context.in_call {
                                    tracing::info!(
                                        layer = "voice",
                                        component = "kairo",
                                        "Quiet mode active during call; orchestrator response will not be spoken"
                                    );
                                    None
                                } else {
                                    speech.clone()
                                };

                                // Spawn the wake as a tokio task so the main loop
                                // keeps draining frame_rx. If we awaited here,
                                // triage would queue up 5–15 stale frames while
                                // Opus streams for 5–10 s, and every subsequent
                                // response would be that many seconds behind reality.
                                orchestrator_busy.store(true, std::sync::atomic::Ordering::Release);
                                let busy_flag = orchestrator_busy.clone();
                                let followup_shared = followup_until.clone();
                                let orch_cfg_clone = orch_config.clone();
                                let semantic_clone = semantic.clone();
                                let episodic_clone = episodic.clone();
                                let feedback_clone = feedback.clone();
                                let frame_clone = frame.clone();
                                let reason_clone = reason.clone();
                                let followup_secs = config.voice.conversation_followup_seconds;

                                tokio::spawn(async move {
                                    let result = do_wake(
                                        &frame_clone,
                                        &history,
                                        &reason_clone,
                                        &orch_cfg_clone,
                                        &semantic_clone,
                                        &episodic_clone,
                                        wake_speech_opt.as_ref(),
                                    )
                                    .await;

                                    match result {
                                        Ok(()) => {
                                            if followup_secs > 0 {
                                                if let Ok(mut slot) = followup_shared.lock() {
                                                    *slot = Some(
                                                        Instant::now()
                                                            + Duration::from_secs(followup_secs),
                                                    );
                                                }
                                                tracing::debug!(
                                                    layer = "voice",
                                                    component = "kairo",
                                                    seconds = followup_secs,
                                                    "Follow-up window open"
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::error!(
                                                layer = "orchestrator",
                                                component = "kairo",
                                                error = %e,
                                                "Orchestrator wake failed"
                                            );
                                            println!("[ORCHESTRATOR ERROR: {e}]");
                                            feedback_clone.play(FeedbackCue::Error);
                                        }
                                    }

                                    busy_flag.store(false, std::sync::atomic::Ordering::Release);
                                });
                            }
                        }
                        TriageDecision::Whisper { text } => {
                            // Don't step on the orchestrator's speech — if a wake
                            // is already streaming through TTS, a concurrent
                            // whisper would either queue behind it (adding delay)
                            // or race with it (interrupt noise). Silently drop.
                            if orchestrator_busy.load(std::sync::atomic::Ordering::Acquire) {
                                tracing::debug!(
                                    layer = "triage",
                                    component = "kairo",
                                    decision = "whisper",
                                    text = %text,
                                    "Suppressed whisper — orchestrator already speaking"
                                );
                            } else {
                            // Phase 5.1: triage-driven local speech, no orchestrator wake.
                            tracing::info!(
                                layer = "triage",
                                component = "kairo",
                                decision = "whisper",
                                text = %text,
                                "Whispering via TTS"
                            );
                            if config.voice.ambient_mute_enabled && frame.context.in_call {
                                println!("[quiet mode: would say via TTS: {text}]");
                            } else if let Some(ref sc) = speech {
                                let language = frame.audio.as_ref().map(|a| a.language.as_str());
                                sc.say_with_language(text, language);
                            } else {
                                println!("[would say via TTS: {text}]");
                            }
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
            Some(()) = recv_hotkey(&mut hotkey_rx) => {
                tracing::info!(
                    layer = "voice",
                    component = "hotkey",
                    "Hotkey pressed — next transcript opens a session"
                );
                hotkey_pending = true;
                feedback.play(FeedbackCue::Listen);
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

    tracing::info!(
        layer = "system",
        component = "kairo",
        "Kairo stopped cleanly"
    );
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
    speech: Option<&Arc<SpeechController>>,
) -> Result<()> {
    let wake_start = Instant::now();

    println!("\n--- KAIRO WAKING ---");

    // 1. Memory context.
    let memory_context = {
        let mut ep = episodic.lock().await;
        retrieve_context(trigger_frame, &mut ep, semantic).await?
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
    let mut final_info: Option<(Option<u64>, Option<f64>)> = None;
    let result = wake_orchestrator(config, &user_message, |event| match &event {
        OrchestratorEvent::TextDelta(text) => {
            print!("{text}");
            std::io::stdout().flush().ok();
            full_response.push_str(text);
            if let Some(sc) = speech {
                sc.push_delta(text);
            }
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
                if let Some(sc) = speech {
                    // No deltas arrived (some CLI configurations) — speak
                    // the aggregated text instead.
                    sc.say(full_text);
                }
            } else if let Some(sc) = speech {
                // Flush any trailing text that didn't terminate with a
                // sentence boundary so the last words get spoken.
                sc.flush();
            }
            println!();
            final_info = Some((*duration_ms, *cost_usd));
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

    // Block until the synthesised audio has finished playing so the
    // cost/duration summary prints *after* Kairo stops talking. Uses a
    // blocking wait off the async runtime because the playback thread
    // is not tokio-aware.
    if let Some(sc) = speech {
        let sc = sc.clone();
        tokio::task::spawn_blocking(move || sc.wait_idle())
            .await
            .ok();
    }

    if let Some((duration_ms, cost_usd)) = final_info {
        let cost_str = cost_usd.map(|c| format!(" ${c:.4}")).unwrap_or_default();
        let dur_str = duration_ms.map(|d| format!(" {d}ms")).unwrap_or_default();
        println!("--- [{dur_str}{cost_str}] ---\n");
    }

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

/// Updates the post-wake voice session and returns an explicit wake decision
/// once the spoken command is complete. Also manages the conversation
/// follow-up window and hotkey push-to-talk trigger.
#[allow(clippy::too_many_arguments)]
fn update_voice_session(
    frame: &PerceptionFrame,
    config: &KairoConfig,
    wake_detector: &TranscriptWakeDetector,
    voice_session: &mut Option<VoiceSession>,
    followup_until: &Arc<std::sync::Mutex<Option<Instant>>>,
    hotkey_pending: &mut bool,
    speech: Option<&Arc<SpeechController>>,
    feedback: &FeedbackPlayer,
) -> Option<TriageDecision> {
    if !config.voice.enabled {
        return None;
    }

    let Some(audio) = &frame.audio else {
        return None;
    };

    let transcript = audio.transcript.trim();
    if transcript.is_empty() {
        return None;
    }

    // While Kairo is actively speaking, the mic is likely picking up its
    // own TTS output. Drop those transcripts — they almost always contain
    // fragments of what Kairo just said and will trigger spurious wakes
    // through the follow-up window. Barge-in is separately handled by the
    // speech controller's interrupt() call below.
    if let Some(sc) = speech {
        if sc.is_speaking() {
            if config.voice.barge_in_enabled {
                sc.interrupt();
            }
            tracing::debug!(
                layer = "voice",
                component = "stt",
                transcript = %transcript,
                "Dropped transcript — Kairo is currently speaking"
            );
            return None;
        }
    }

    // Expire the follow-up window before we check it.
    let now = Instant::now();
    let followup_active = {
        let mut slot = match followup_until.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match *slot {
            Some(deadline) if now < deadline => true,
            _ => {
                *slot = None;
                false
            }
        }
    };

    let mut consumed_by_wake = false;
    if voice_session.is_none() {
        // Hotkey press → skip wake word on the next transcript.
        if *hotkey_pending {
            tracing::info!(
                layer = "voice",
                component = "hotkey",
                "Starting voice session via hotkey"
            );
            *voice_session = Some(VoiceSession::new(transcript, &audio.language));
            *hotkey_pending = false;
            consumed_by_wake = true;
        } else if followup_active {
            tracing::info!(
                layer = "voice",
                component = "stt",
                "Starting follow-up voice session without wake word"
            );
            *voice_session = Some(VoiceSession::new(transcript, &audio.language));
            if let Ok(mut slot) = followup_until.lock() {
                *slot = None;
            }
            feedback.play(FeedbackCue::Listen);
            consumed_by_wake = true;
        } else if config.voice.wake_word_enabled {
            if let Some(detection) = wake_detector.detect(transcript) {
                tracing::info!(
                    layer = "voice",
                    component = "wake",
                    keyword = %detection.keyword,
                    after_len = detection.utterance_after_wake.len(),
                    "Wake phrase detected"
                );
                *voice_session = Some(VoiceSession::new(
                    &detection.utterance_after_wake,
                    &audio.language,
                ));
                feedback.play(FeedbackCue::Wake);
                consumed_by_wake = true;
            }
        } else {
            *voice_session = Some(VoiceSession::new(transcript, &audio.language));
            feedback.play(FeedbackCue::Listen);
            consumed_by_wake = true;
        }
    }

    if let Some(session) = voice_session.as_mut() {
        if !consumed_by_wake {
            session.push_transcript(transcript, &audio.language);
        }

        let endpoint_detector = SemanticEndpointDetector::new(
            Duration::from_millis(config.voice.endpoint_silence_ms),
            Duration::from_millis(config.voice.listen_timeout_ms),
            config.voice.min_utterance_chars,
        );

        if matches!(
            endpoint_detector.decide(session),
            EndpointDecision::Complete | EndpointDecision::TimedOut
        ) {
            let text = session.text().trim().to_string();
            let language = session.language().to_string();
            *voice_session = None;

            if text.is_empty() {
                return None;
            }

            tracing::info!(
                layer = "voice",
                component = "stt",
                text_len = text.len(),
                "Voice command endpoint detected"
            );
            return Some(TriageDecision::WakeOrchestrator {
                reason: format!("Voice command ({language}): {text}"),
            });
        }
    }

    None
}

/// Spawn the global hotkey listener if configured. Returns `None` when
/// the config disables the hotkey (empty string) or registration fails
/// (chord already owned by another app) — Kairo logs a warning and
/// continues without push-to-talk.
#[cfg(windows)]
fn spawn_hotkey(
    config: &KairoConfig,
) -> Option<(
    kairo_core::voice::hotkey::HotkeyHandle,
    tokio::sync::mpsc::UnboundedReceiver<()>,
)> {
    let spec = config.voice.hotkey.trim();
    if spec.is_empty() {
        tracing::info!(
            layer = "voice",
            component = "hotkey",
            "Hotkey disabled in config"
        );
        return None;
    }
    match spawn_hotkey_listener(spec) {
        Ok((handle, rx)) => {
            tracing::info!(
                layer = "voice",
                component = "hotkey",
                spec = %spec,
                "Hotkey listener started"
            );
            Some((handle, rx))
        }
        Err(e) => {
            tracing::warn!(
                layer = "voice",
                component = "hotkey",
                spec = %spec,
                error = %e,
                "Hotkey registration failed — push-to-talk disabled"
            );
            None
        }
    }
}

/// Poll an optional hotkey channel inside a `tokio::select!` arm. Returning
/// `None` makes the select branch effectively pend forever, which is what
/// we want when hotkey is disabled.
async fn recv_hotkey(rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<()>>) -> Option<()> {
    match rx.as_mut() {
        Some(ch) => ch.recv().await,
        None => std::future::pending().await,
    }
}

/// Initialize the TTS pipeline and feedback player together so they share
/// a single [`PlaybackStream`] — one queue means TTS audio and UI cues
/// naturally order behind each other. Returns a disabled [`FeedbackPlayer`]
/// when the audio device is unavailable, so callers don't need to branch.
fn init_tts_and_feedback(config: &KairoConfig) -> (Option<Arc<SpeechController>>, FeedbackPlayer) {
    let cfg = &config.tts;
    if !cfg.enabled {
        tracing::info!(
            layer = "voice",
            component = "kairo",
            "TTS disabled in config"
        );
        return (None, FeedbackPlayer::disabled());
    }

    let espeak_dir = expand_home(&cfg.espeak_data_dir);
    set_espeak_data_dir(&espeak_dir);

    let voice_cfg = match cfg.voices.get(&cfg.primary) {
        Some(v) => v,
        None => {
            tracing::warn!(
                layer = "voice",
                component = "kairo",
                primary = %cfg.primary,
                "Primary voice key missing from [tts.voices]; TTS disabled"
            );
            return (None, FeedbackPlayer::disabled());
        }
    };

    let model_path = expand_home(&voice_cfg.model_path);
    let config_path = expand_home(&voice_cfg.config_path);

    if !model_path.exists() || !config_path.exists() {
        tracing::warn!(
            layer = "voice",
            component = "kairo",
            model = %model_path.display(),
            config = %config_path.display(),
            "Piper voice files missing — run scripts/download-models.ps1. TTS disabled."
        );
        return (None, FeedbackPlayer::disabled());
    }

    let engine: Arc<dyn TtsEngine> = match cfg.engine.as_str() {
        "elevenlabs" => {
            tracing::warn!(
                layer = "voice",
                component = "kairo",
                "tts.engine = \"elevenlabs\" is a Phase 5 extension point \
                 (stub). Falling back to Piper."
            );
            let _stub = ElevenLabsEngine::new(
                cfg.elevenlabs.voice_id.clone(),
                cfg.elevenlabs.model_id.clone(),
            );
            match PiperEngine::new(
                &model_path,
                &config_path,
                cfg.length_scale,
                voice_cfg.speaker_id,
            ) {
                Ok(e) => Arc::new(e),
                Err(e) => {
                    tracing::error!(
                        layer = "voice",
                        component = "kairo",
                        error = %e,
                        "Piper fallback init failed; TTS disabled"
                    );
                    return (None, FeedbackPlayer::disabled());
                }
            }
        }
        _ => {
            // Default: Piper local.
            match PiperEngine::new(
                &model_path,
                &config_path,
                cfg.length_scale,
                voice_cfg.speaker_id,
            ) {
                Ok(e) => Arc::new(e),
                Err(e) => {
                    tracing::error!(
                        layer = "voice",
                        component = "kairo",
                        error = %e,
                        "Piper engine init failed; TTS disabled"
                    );
                    return (None, FeedbackPlayer::disabled());
                }
            }
        }
    };

    let playback = match PlaybackStream::open_default_with_volume(config.voice.volume) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            tracing::error!(
                layer = "voice",
                component = "kairo",
                error = %e,
                "Audio output device unavailable; TTS disabled"
            );
            return (None, FeedbackPlayer::disabled());
        }
    };

    let feedback = FeedbackPlayer::new(playback.clone(), config.voice.feedback_sounds);

    // Piper warmup: the first synth call pays ~2-3 s of Piper process
    // startup + model load even on a warm OS file cache. Run a tiny
    // dummy synth here so the first *real* utterance doesn't eat that
    // latency on the user-visible path. The audio is discarded.
    let warmup_engine = engine.clone();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        match warmup_engine.synthesize(".") {
            Ok(_) => tracing::info!(
                layer = "voice",
                component = "kairo",
                warmup_ms = start.elapsed().as_millis() as u64,
                "Piper warmup done"
            ),
            Err(e) => tracing::warn!(
                layer = "voice",
                component = "kairo",
                error = %e,
                "Piper warmup failed — first real utterance will be slow"
            ),
        }
    });

    tracing::info!(
        layer = "voice",
        component = "kairo",
        voice = %cfg.primary,
        volume = config.voice.volume,
        feedback_sounds = config.voice.feedback_sounds,
        "TTS ready"
    );
    (
        Some(Arc::new(SpeechController::new(engine, playback))),
        feedback,
    )
}

/// Expand a leading `~/` to the user's home directory. Returns the path
/// unchanged if no tilde prefix or home lookup fails.
fn expand_home(raw: &str) -> PathBuf {
    if let Some(stripped) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(raw)
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
    async fn describe(&self, _image: &image::DynamicImage) -> Result<kairo_vision::VisionOutput> {
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
        return s.to_string();
    }
    // Walk back from the byte cutoff to the nearest char boundary so we
    // don't panic when max_len lands inside a multi-byte UTF-8 codepoint
    // (happens with curly quotes "  " and other chars in vision model
    // output, e.g. `"Beer in Bordeaux, 2016"`).
    let target = max_len.saturating_sub(3);
    let mut cut = target.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &s[..cut])
}
