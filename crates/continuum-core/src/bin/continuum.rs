//! # continuum
//!
//! The complete Continuum runtime: perception + triage + orchestrator in one binary.
//! This is the first time the full system runs end-to-end.
//!
//! When triage decides `wake_orchestrator`, this binary actually spawns Claude
//! Opus 4.6 and streams the response to the terminal.
//!
//! # Usage
//!
//! ```bash
//! cargo run --release --bin continuum
//! ```

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch, Mutex};
use tracing_subscriber::EnvFilter;

use continuum_vision::VisionModel;

use continuum_core::config::{
    continuum_dev_dir, env_or_legacy, load_config, ContinuumConfig, CuratorConfig,
};
use continuum_core::curator;
use continuum_core::memory::distill::run_memory_distiller;
use continuum_core::memory::episodic::{EpisodicEvent, EpisodicStore, EventKind};
use continuum_core::memory::raw_log::RawLog;
use continuum_core::memory::retrieval::{
    filter_pending, infer_project_hint, retrieve_context, retrieve_vault_context,
};
use continuum_core::memory::semantic::SemanticStore;
use continuum_core::orchestrator::spawn::{
    wake_orchestrator, OrchestratorConfig, OrchestratorEvent,
};
use continuum_core::orchestrator::wake_context::build_wake_message;
use continuum_core::senses::audio::AudioWatcher;
use continuum_core::senses::context::ContextWatcher;
use continuum_core::senses::frame::PerceptionFrameBuilder;
use continuum_core::senses::live_context::{self, LiveContextHub};
use continuum_core::senses::types::{
    AudioObservation, ContextObservation, PerceptionFrame, ScreenObservation,
};
use continuum_core::senses::vision::VisionWatcher;
use continuum_core::skills::{MatchContext, SkillLoader, SkillMatcher};
use continuum_core::triage::handlers::handle_decision;
use continuum_core::triage::llm::{TriageConfig, TriageLayer};
use continuum_core::triage::TriageDecision;
use continuum_core::voice::intent::{self as voice_intent, VoiceIntent};
use continuum_core::voice::playback::PlaybackStream;
use continuum_core::voice::sounds::{FeedbackCue, FeedbackPlayer};
use continuum_core::voice::streaming::SpeechController;
use continuum_core::voice::stt::{EndpointDecision, SemanticEndpointDetector, VoiceSession};
use continuum_core::voice::tts::{
    set_espeak_data_dir, ElevenLabsEngine, KokorosEngine, PiperEngine, TtsEngine,
};
// Phase 3 tier-split: the Moshi front-end exposes `interrupt()` via the
// `VoiceFrontend` trait (and `resume()` as a concrete method). Only needed
// when the `moshi` feature is on.
#[cfg(feature = "moshi")]
use continuum_core::voice::frontend::VoiceFrontend;
use continuum_core::voice::wake::TranscriptWakeDetector;
use continuum_core::workers::{EventSink, FinishSink, WorkerPool, WorkerPoolOptions};

#[cfg(windows)]
use continuum_core::voice::hotkey::spawn_hotkey_listener;

#[tokio::main]
async fn main() -> Result<()> {
    // Flags.
    let args: Vec<String> = std::env::args().collect();

    // `continuum setup` — idempotent first-run / repair pass. Short-circuits
    // before any runtime bring-up so it's safe to run on a fresh install
    // even when models aren't downloaded yet.
    if args.get(1).map(|s| s.as_str()) == Some("setup") {
        return run_setup_command().await;
    }
    // `continuum --version` — prints version and exits.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("continuum {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let force_wake = args.iter().any(|a| a == "--force-wake");
    let reset_audio = args.iter().any(|a| a == "--reset-audio");
    let no_tts = args.iter().any(|a| a == "--no-tts");

    // --- Config ---
    let dev_dir = continuum_dev_dir();
    std::fs::create_dir_all(&dev_dir).context("Failed to create ~/.continuum-dev/")?;
    let config_path = dev_dir.join("config.toml");
    let mut config = load_config(&config_path).context("Failed to load configuration")?;

    // --- Adaptive resource policy ---
    // Probe the host once and resolve concrete resource knobs from the
    // detected specs + the user's [resources] config. We mutate the loaded
    // config in place so every downstream consumer (triage LLM, vision, screen
    // poller, worker pool) picks up the adapted values without each needing to
    // know about hardware detection. See `continuum_core::hardware`.
    let hardware_specs = continuum_core::hardware::probe_hardware();
    let resource_plan =
        continuum_core::hardware::resolve_resource_policy(&hardware_specs, &config.resources);
    tracing::info!(
        layer = "hardware",
        component = "continuum",
        plan = ?resource_plan,
        "Resolved adaptive resource plan"
    );
    config.triage.gpu_layers = resource_plan.triage_gpu_layers;
    config.screen.interval_secs = resource_plan.screen_interval_secs;
    config.context.poll_interval_secs = resource_plan.context_interval_secs;
    config.workers.max_concurrent = resource_plan.workers_max_concurrent as usize;
    config.audio.whisper_threads = resource_plan.whisper_threads as i32;
    config.audio.whisper_use_gpu = resource_plan.vision_gpu;

    // Audio device: always use the Windows default input device.
    // `--reset-audio` just clears any stale picker fields from config.toml so
    // they don't linger — the selection itself always comes from Windows.
    if reset_audio {
        if let Err(e) = continuum_core::senses::audio::clear_audio_config(&config_path) {
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
    let default_filter = "info,continuum_core=debug,continuum_vision=info,continuum_llm=info";
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_target(false)
        .compact()
        .init();

    tracing::info!(
        layer = "system",
        component = "continuum",
        "Starting Continuum — perception + triage + orchestrator"
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

    // Memory vault (Plan B): markdown source of truth + derived SQLite
    // index. Opened once here; the distiller and the watcher-drain task
    // below both hold a clone of this `Arc`.
    let vault_dir = config.memory.vault.resolve_vault_dir(&dev_dir);
    let vault = Arc::new(
        continuum_memory::Vault::open_with(
            &vault_dir,
            continuum_memory::VaultOptions {
                watcher_debounce_ms: config.memory.vault.watcher_debounce_ms,
                graph_max_nodes: config.memory.vault.graph_max_nodes,
            },
        )
        .await
        .context("open memory vault")?,
    );

    // --- TTS + feedback ---
    let (speech, feedback): (Option<Arc<SpeechController>>, FeedbackPlayer) = if no_tts {
        tracing::info!(
            layer = "voice",
            component = "continuum",
            "--no-tts: speech output disabled"
        );
        (None, FeedbackPlayer::disabled())
    } else {
        init_tts_and_feedback(&config)
    };

    // --- Orchestrator config ---
    let prompt_path = find_system_prompt(&dev_dir);
    let orch_config = OrchestratorConfig {
        model: config.orchestrator.model_id.clone(),
        system_prompt_path: prompt_path,
        timeout_secs: config.orchestrator.wake_timeout_secs,
        bare_mode: config.orchestrator.bare_mode,
        // Phase 4: enable MCP tools. Data dir is the same ~/.continuum-dev/
        // the main runtime uses, so the MCP server reads/writes the same
        // semantic + episodic stores.
        mcp_enabled: true,
        mcp_server_path: None,
        mcp_config_path: None,
        mcp_data_dir: Some(dev_dir.clone()),
    };

    tracing::info!(
        layer = "orchestrator",
        component = "continuum",
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
            component = "continuum",
            "Ctrl+C received, shutting down..."
        );
        let _ = ctrl_c_shutdown.send(true);
    });

    // --- Memory vault watcher ---
    // Drains the vault's debounced file-watcher so external edits (the
    // user editing a note by hand, or the desktop dashboard writing to the
    // same vault directory) keep the runtime's derived index fresh.
    // Mirrors `apps/desktop/src-tauri/src/memory.rs`'s watcher bridge, minus
    // the Tauri event emit — there is no frontend in this process to notify.
    {
        let vault = vault.clone();
        let mut shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut watcher = match vault.watch() {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(layer = "memory", component = "runtime",
                        error = %e.user_message(), "vault watcher unavailable");
                    return;
                }
            };
            loop {
                tokio::select! {
                    Some(paths) = watcher.rx.recv() => {
                        if let Err(e) = vault.reindex_paths(&paths).await {
                            tracing::warn!(layer = "memory", component = "runtime",
                                error = %e.user_message(), "vault reindex failed");
                        }
                    }
                    _ = shutdown.changed() => { if *shutdown.borrow() { break; } }
                }
            }
        });
    }

    // --- Phase 8: Skills + Worker pool ---
    let skill_loader = SkillLoader::new(resolve_skills_root(&config));
    if config.skills.enabled {
        skill_loader.set_disabled(config.skills.disabled.clone());
        if let Err(e) = skill_loader.reload() {
            tracing::warn!(
                layer = "skills",
                component = "continuum",
                error = %e,
                "Initial skill load failed; continuing without skills"
            );
        } else {
            let names: Vec<String> = skill_loader
                .enabled()
                .into_iter()
                .map(|s| s.frontmatter.name)
                .collect();
            tracing::info!(
                layer = "skills",
                component = "continuum",
                count = names.len(),
                names = ?names,
                "Skills loaded"
            );
        }
        if config.skills.hot_reload {
            skill_loader.spawn_watcher(std::time::Duration::from_secs(3), shutdown_rx.clone());
        }
    }

    let worker_base_prompt = std::fs::read_to_string(
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join("prompts/worker-system.md"),
    )
    .ok();

    // The pool's background task holds its own Arc, so once spawned the
    // `_worker_pool` binding is only needed to keep future health-check
    // additions ergonomic. Prefix with `_` to signal "not read today".
    let _worker_pool = {
        let episodic_for_sink = episodic.clone();
        let event_sink: EventSink = Arc::new(move |id, event| {
            use continuum_core::workers::WorkerEvent;
            if let WorkerEvent::ToolCall { name, .. } = event {
                let id = id.clone();
                let episodic = episodic_for_sink.clone();
                tokio::spawn(async move {
                    let mut ep = episodic.lock().await;
                    let event = EpisodicEvent {
                        id: uuid::Uuid::new_v4().to_string(),
                        ts: chrono::Utc::now(),
                        kind: EventKind::ToolCall,
                        summary: format!("worker[{id}] tool: {name}"),
                        importance: 0.4,
                        tags: vec!["worker".into(), format!("worker:{id}"), name],
                        source_frame_id: None,
                    };
                    if let Err(e) = ep.insert_event(&event).await {
                        tracing::debug!(
                            layer = "workers",
                            component = "audit",
                            error = %e,
                            "Failed to write worker tool-call audit event"
                        );
                    }
                });
            }
        });

        let episodic_for_finish = episodic.clone();
        let finish_sink: FinishSink = Arc::new(move |snapshot| {
            let snap = snapshot.clone();
            let episodic = episodic_for_finish.clone();
            tokio::spawn(async move {
                let mut ep = episodic.lock().await;
                let status = snap.status.as_str();
                let mut summary = format!(
                    "worker[{}] {}: {}",
                    snap.id,
                    status,
                    snap.task.chars().take(200).collect::<String>()
                );
                if let Some(cost) = snap.cost_usd {
                    summary.push_str(&format!(" — cost ${:.4}", cost));
                }
                if let Some(err) = &snap.error {
                    summary.push_str(&format!(" — error: {}", err));
                }
                let mut tags = vec!["worker".to_string(), format!("worker:{}", snap.id)];
                tags.extend(snap.tags.iter().cloned());
                tags.extend(snap.skills.iter().cloned());
                let event = EpisodicEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    ts: chrono::Utc::now(),
                    kind: EventKind::ToolCall,
                    summary,
                    importance: match status {
                        "completed" => 0.5,
                        "failed" | "timed_out" => 0.7,
                        _ => 0.3,
                    },
                    tags,
                    source_frame_id: None,
                };
                let _ = ep.insert_event(&event).await;
            });
        });

        let opts = WorkerPoolOptions {
            config: config.workers.clone(),
            data_dir: dev_dir.clone(),
            claude_bin: "claude".into(),
            skill_loader: Some(skill_loader.clone()),
            skill_token_budget: config.skills.token_budget,
            mcp_config_path: None, // workers use their own MCP config if needed
            base_system_prompt: worker_base_prompt,
        };
        let pool = WorkerPool::new(opts)
            .with_event_sink(event_sink)
            .with_finish_sink(finish_sink);
        pool.spawn_background(shutdown_rx.clone());
        pool
    };
    tracing::info!(
        layer = "workers",
        component = "continuum",
        max_concurrent = config.workers.max_concurrent,
        "Worker pool ready"
    );

    // Phase 3: background raw-log to episodic-memory distillation.
    let distiller_shutdown = shutdown_rx.clone();
    let distiller_raw_log = raw_log.clone();
    let distiller_episodic = episodic.clone();
    let distiller_vault = vault.clone();
    let distiller_config = config.memory.clone();
    tokio::spawn(async move {
        run_memory_distiller(
            distiller_raw_log,
            distiller_episodic,
            distiller_vault,
            distiller_config,
            distiller_shutdown,
        )
        .await;
    });

    // --- Perception channels ---
    let live_context = LiveContextHub::new(config.screen.buffer_capacity.saturating_mul(4));
    live_context::spawn_publisher(
        live_context.clone(),
        dev_dir.join("live-context.json"),
        Duration::from_millis(200),
        shutdown_rx.clone(),
    );
    let (screen_tx, screen_rx) = mpsc::channel::<ScreenObservation>(64);
    let (audio_tx, audio_rx) = mpsc::channel::<AudioObservation>(16);
    let (ctx_tx, ctx_rx) = mpsc::channel::<ContextObservation>(64);
    let (frame_tx, mut frame_rx) = mpsc::channel::<PerceptionFrame>(32);

    // --- Vision ---
    let vision_model = init_vision_model(&config, &resource_plan).await;

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

    // --- Audio ---
    let audio_watcher = AudioWatcher::new(config.audio.clone());
    let audio_shutdown = shutdown_rx.clone();
    // Moshi S2S front-end (feature = "moshi"): when `voice.frontend.mode =
    // "moshi"`, the audio watcher forks 16 kHz mono PCM into a tap channel
    // that the Moshi front-end consumes. The channel is created here (cheap,
    // no runtime_state needed); the MoshiFrontend + its event consumer are
    // wired later (`wire_moshi_voice`) once `runtime_state` is in scope, so
    // the consumer can publish `partial_transcript` / `moshi_loaded`. The
    // pipeline path (`mode = "pipeline"`) passes `None` and is unchanged.
    #[cfg(feature = "moshi")]
    let moshi_active: bool =
        config.voice.frontend.mode == "moshi" && config.voice.frontend.moshi_tap_enabled;
    #[cfg(not(feature = "moshi"))]
    let moshi_active: bool = false;
    #[cfg(feature = "moshi")]
    #[allow(clippy::type_complexity)]
    let (moshi_tap, moshi_tap_rx): (
        Option<tokio::sync::mpsc::UnboundedSender<Vec<f32>>>,
        Option<tokio::sync::mpsc::UnboundedReceiver<Vec<f32>>>,
    ) = if moshi_active {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<f32>>();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    #[cfg(not(feature = "moshi"))]
    let moshi_tap: Option<tokio::sync::mpsc::UnboundedSender<Vec<f32>>> = None;
    tokio::spawn(async move {
        audio_watcher.run(audio_tx, audio_shutdown, moshi_tap).await;
    });

    // --- Context ---
    let context_watcher = ContextWatcher::new(config.context.clone());
    let context_shutdown = shutdown_rx.clone();
    let context_live_context = live_context.clone();
    tokio::spawn(async move {
        let _ = context_watcher
            .run_with_live_context(ctx_tx, context_shutdown, context_live_context)
            .await;
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
        let model_path = config.triage.resolve_model_path(&dev_dir);

        if !model_path.exists() {
            tracing::error!(
                layer = "triage",
                component = "continuum",
                path = %model_path.display(),
                "Triage model not found. Run: powershell scripts/download-models.ps1"
            );
            None
        } else {
            tracing::info!(
                layer = "triage",
                component = "continuum",
                "Initializing triage..."
            );

            // Threads come from the resolved adaptive resource plan — a
            // fraction of logical cores with headroom, overridable via
            // [resources] in config.toml (see `hardware::resolve_resource_policy`).
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
                            component = "continuum",
                            error = %e,
                            "Triage warmup failed"
                        );
                    }
                    tracing::info!(layer = "triage", component = "continuum", "Triage ready");
                    Some(t)
                }
                Err(e) => {
                    tracing::error!(
                        layer = "triage",
                        component = "continuum",
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
        component = "continuum",
        triage = triage.is_some(),
        "All layers running. Press Ctrl+C to stop."
    );

    // --- Curator (Plan B): background memory extraction ---
    // Watch channel carrying the latest per-frame activity signal — created
    // before the main loop so the frame-handling arm can publish into it on
    // every frame. Reuses the triage local model for extraction completions
    // (never wakes the orchestrator for routine memory bookkeeping — see
    // non-negotiable #4), so the curator only runs when triage loaded.
    let (activity_tx, activity_rx) = watch::channel(curator::run::ActivitySignal::default());
    // Task 11: cloned into the runtime snapshot publisher below so the
    // dashboard can surface curator health (last pass, pending count,
    // consecutive failures) from `~/.continuum-dev/state.json`.
    let curator_status: Option<curator::SharedCuratorStatus> = if let Some(triage) = triage.clone()
    {
        let status: curator::SharedCuratorStatus = Default::default();
        let llm: Arc<dyn curator::CuratorLlm> = Arc::new(triage);
        // C1 fix: session-summary writes are delayed by
        // `distillation_interval_minutes + 1` past their boundary so the
        // memory distiller has time to land any tail events before
        // `write_session_summary` queries the vault's timeline — see
        // `curator::run::run_curator`'s doc comment.
        let distill_lag_minutes = config.memory.distillation_interval_minutes + 1;
        tokio::spawn(curator::run::run_curator(
            vault.clone(),
            llm,
            config.memory.curator.clone(),
            config.memory.vault.clone(),
            status.clone(),
            activity_rx.clone(),
            shutdown_rx.clone(),
            distill_lag_minutes,
            dev_dir.clone(),
            raw_log.clone(),
            episodic.clone(),
        ));
        Some(status)
    } else {
        tracing::info!(
            layer = "memory",
            component = "curator",
            "curator disabled: no triage model loaded"
        );
        None
    };

    // `followup_until` is shared between the main loop (reads + clears) and
    // the spawned do_wake tasks (writes after a wake completes), so it lives
    // behind a std::sync::Mutex — locks are held for microseconds.
    let followup_until: Arc<std::sync::Mutex<Option<Instant>>> =
        Arc::new(std::sync::Mutex::new(None));
    // `orchestrator_busy` gates new wakes. When true, triage keeps running
    // but the wake_orchestrator / whisper decisions are suppressed so we
    // don't stack multiple Opus calls on top of each other. Cleared by the
    // spawned wake task when do_wake completes. Declared here (rather than
    // down in the main loop) so the daily memory-maintenance ticker below
    // can share it.
    let orchestrator_busy: Arc<std::sync::atomic::AtomicBool> =
        Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Task 10: most recent perception frame, updated once per frame in the
    // main loop below. The maintenance-wake ticker uses this as the trigger
    // frame for its `do_wake` call instead of fabricating one — per
    // non-negotiable design, a wake always carries a real observation.
    let last_frame: Arc<std::sync::Mutex<Option<PerceptionFrame>>> =
        Arc::new(std::sync::Mutex::new(None));

    // --- Runtime state publisher ---
    //
    // Writes `~/.continuum-dev/state.json` every 2 s so the dashboard (a
    // separate Tauri process) can render component statuses without
    // needing IPC into this process. Flags are updated inline as each
    // subsystem initialises below and as the main loop mutates voice /
    // orchestrator state.
    let runtime_state: Arc<std::sync::Mutex<continuum_core::runtime_publish::RuntimeSnapshot>> =
        Arc::new(std::sync::Mutex::new(
            continuum_core::runtime_publish::RuntimeSnapshot {
                triage_model_loaded: triage.is_some(),
                vision_model_loaded: true,
                tts_loaded: speech.is_some(),
                stt_loaded: config.audio.enabled,
                orchestrator_ready: !orch_config.system_prompt_path.is_empty(),
                voice_mode: Some("idle".to_string()),
                partial_transcript: None,
                voice_volume: Some(config.voice.volume),
                tts_queue_len: Some(0),
                ambient_mute_active: Some(false),
                detected_call_app: None,
                wake_word_enabled: Some(config.voice.wake_word_enabled),
                voice_frontend_mode: Some(config.voice.frontend.mode.clone()),
                moshi_loaded: Some(false),
                frame_count: 0,
                monitor_count: 0,
                capture_event_count: 0,
                dropped_capture_event_count: 0,
                last_capture_at: None,
                wake_count: 0,
                hardware_specs: Some(hardware_specs.clone()),
                resource_plan: Some(resource_plan.clone()),
                last_update: chrono::Utc::now().to_rfc3339(),
                // Overwritten every tick by the publisher closure below
                // (via `build_curator_snapshot`); `None` here is never
                // actually published.
                curator: None,
            },
        ));
    // Moshi S2S front-end (feature = "moshi"): now that `runtime_state` is in
    // scope, build the Moshi backend + wire the tap receiver + event consumer
    // created earlier at the audio-spawn site. The consumer publishes
    // `partial_transcript` / `moshi_loaded` into the runtime snapshot. The
    // returned handle drives the Phase 3 tier-split bridge: on a
    // `WakeOrchestrator` triage decision we `interrupt()` Moshi (mute its
    // output + send EndTurn) while the orchestrator + Kokoros speak, then
    // `resume()` it when the wake completes.
    #[cfg(feature = "moshi")]
    let moshi_frontend: Option<std::sync::Arc<continuum_core::voice::moshi::MoshiFrontend>> =
        if moshi_active {
            match wire_moshi_voice(&config, moshi_tap_rx, runtime_state.clone()) {
                Ok(handle) => {
                    tracing::info!(
                        layer = "voice",
                        component = "moshi",
                        "Moshi S2S front-end wired"
                    );
                    Some(handle)
                }
                Err(e) => {
                    tracing::error!(
                        layer = "voice",
                        component = "moshi",
                        error = %e,
                        "Moshi front-end failed to start; S2S tap will be inert"
                    );
                    None
                }
            }
        } else {
            None
        };
    #[cfg(not(feature = "moshi"))]
    let _moshi_frontend: Option<()> = None;
    {
        let state_clone = runtime_state.clone();
        // Task 11: the curator only actually runs when both the triage
        // model loaded (so `curator_status` is `Some`) and
        // `[memory.curator] enabled = true` — see `build_curator_snapshot`.
        // Capturing the bool by value (not `config`) keeps `config` usable
        // for the rest of `main` below this block.
        let curator_status_for_publisher = curator_status.clone();
        let curator_enabled = config.memory.curator.enabled;
        let speech_clone = speech.clone();
        continuum_core::runtime_publish::spawn_publisher(
            dev_dir.join("state.json"),
            2,
            shutdown_rx.clone(),
            move || {
                let guard = state_clone.lock().unwrap_or_else(|p| p.into_inner());
                let mut snap = guard.clone();
                // M4 fix: release `runtime_state`'s lock before calling
                // build_curator_snapshot, which locks a *different* mutex
                // (`curator_status_for_publisher`) — holding the first
                // across the second is a latent nested-lock that doesn't
                // deadlock today only because nothing else acquires them in
                // the opposite order, but there's no reason to hold it a
                // moment longer than the `.clone()` above needs.
                drop(guard);
                if let Some(controller) = speech_clone.as_ref() {
                    snap.tts_queue_len = Some(controller.pending_count());
                    if controller.is_speaking() {
                        snap.voice_mode = Some("speaking".to_string());
                    }
                }
                snap.last_update = chrono::Utc::now().to_rfc3339();
                snap.curator = Some(build_curator_snapshot(
                    curator_status_for_publisher.as_ref(),
                    curator_enabled,
                ));
                snap
            },
        );
    }

    // --- Task 10: daily memory-maintenance wake ---
    // Quiet-day drain: if nothing else ever wakes the orchestrator on a
    // given day, this ticker fires once at the configured local hour and
    // reviews any vault decisions that piled up unattended. It reuses the
    // exact same `do_wake` path the triage `WakeOrchestrator` arm uses.
    spawn_maintenance_wake_ticker(
        config.memory.curator.clone(),
        orch_config.clone(),
        vault.clone(),
        semantic.clone(),
        episodic.clone(),
        skill_loader.clone(),
        config.skills.token_budget,
        dev_dir.clone(),
        speech.clone(),
        feedback.clone(),
        last_frame.clone(),
        followup_until.clone(),
        config.voice.conversation_followup_seconds,
        config.voice.ambient_mute_enabled,
        orchestrator_busy.clone(),
        runtime_state.clone(),
        shutdown_rx.clone(),
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

    // --- Dashboard push-to-talk intents ---
    // Dashboard-process writes TalkNow intents to `~/.continuum-dev/voice-intents/`;
    // we drain them here and treat each as equivalent to a hotkey press.
    if let Err(e) = voice_intent::ensure_intents_dir(&dev_dir) {
        tracing::warn!(
            layer = "voice",
            component = "intent",
            error = %e,
            "Failed to create voice-intents dir; push-to-talk from dashboard will be ignored"
        );
    }
    let mut voice_intent_tick = tokio::time::interval(std::time::Duration::from_millis(250));
    // Skip the first immediate tick so we don't drain before the loop even starts.
    voice_intent_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // --- Main loop ---
    let mut frame_count: u64 = 0;
    let mut recent_frames: Vec<PerceptionFrame> = Vec::new();
    let mut main_shutdown = shutdown_rx.clone();
    let wake_detector = TranscriptWakeDetector::new(config.voice.wake_keyword.clone());
    let mut voice_session: Option<VoiceSession> = None;
    let mut hotkey_pending: bool = false;

    loop {
        tokio::select! {
            Some(frame) = frame_rx.recv() => {
                frame_count += 1;
                if let Ok(mut s) = runtime_state.lock() {
                    s.frame_count = frame_count;
                    let world = live_context.snapshot();
                    s.monitor_count = world.monitors.len();
                    s.capture_event_count = world.health.capture_events;
                    s.dropped_capture_event_count = world.health.dropped_capture_events;
                    s.last_capture_at = world.health.last_capture_at;
                    let ambient_active = config.voice.ambient_mute_enabled && frame.context.in_call;
                    s.ambient_mute_active = Some(ambient_active);
                    s.detected_call_app = ambient_active
                        .then(|| frame.context.foreground_process_name.clone());
                }

                // Task 10: publish the latest frame for the daily
                // memory-maintenance ticker. It never fabricates a frame —
                // it waits for this to be `Some` before it can fire at all.
                {
                    let mut lf = match last_frame.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    *lf = Some(frame.clone());
                }

                // Curator (Plan B): publish the latest activity signal so the
                // curator's project-aware context (Task 9) has fresh data
                // between ticks. Send is a no-op if the curator never spawned
                // (no triage model) — nothing is listening, that's fine.
                let _ = activity_tx.send(curator::run::ActivitySignal {
                    project_hint: infer_project_hint(&frame),
                    process: frame.context.foreground_process_name.clone(),
                    idle_seconds: frame.context.idle_seconds,
                    ts: Some(frame.context.ts),
                });

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
                            component = "continuum",
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
                            component = "continuum",
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
                        component = "continuum",
                        error = %e,
                        "Raw log write failed"
                    );
                }

                // Keep recent frames for wake context.
                recent_frames.push(frame.clone());
                if recent_frames.len() > 10 {
                    recent_frames.remove(0);
                }

                // In Moshi S2S mode the front-end is always-listening — skip
                // the wake-word / pipeline voice-session machinery. Triage
                // still runs on the whisper user transcript (above) and
                // drives escalation via the `WakeOrchestrator` arm, which
                // `interrupt()`s Moshi while the orchestrator speaks.
                #[cfg(feature = "moshi")]
                let voice_decision = if moshi_frontend.is_some() {
                    None
                } else {
                    update_voice_session(
                        &frame,
                        &config,
                        &wake_detector,
                        &mut voice_session,
                        &followup_until,
                        &mut hotkey_pending,
                        speech.as_ref(),
                        &feedback,
                    )
                };
                #[cfg(not(feature = "moshi"))]
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
                if !moshi_active
                    && !orchestrator_busy.load(std::sync::atomic::Ordering::Acquire)
                {
                    if let Ok(mut s) = runtime_state.lock() {
                        if let Some(session) = voice_session.as_ref() {
                            s.voice_mode = Some("listening".to_string());
                            s.partial_transcript = Some(session.text().to_string());
                        } else {
                            s.voice_mode = Some("idle".to_string());
                            s.partial_transcript = Some(String::new());
                        }
                    }
                }

                // --force-wake: override triage on the first frame to test the pipeline.
                let effective_decision = if force_wake && frame_count == 1 {
                    println!("[--force-wake: forcing wake on frame 1]");
                    Some(TriageDecision::WakeOrchestrator {
                        reason: "Force wake for testing — user wants to verify the orchestrator pipeline works end-to-end".to_string(),
                        suggested_skill: None,
                    })
                } else {
                    voice_decision.or_else(|| decision.clone())
                };

                // Handle decision.
                if let Some(ref decision) = effective_decision {
                    match decision {
                        TriageDecision::WakeOrchestrator { reason, suggested_skill } => {
                            // If we're already inside a wake (orchestrator still
                            // streaming from a previous trigger), don't stack — log
                            // and skip. The user's latest utterance still landed in
                            // the raw log; they can ask again.
                            if !try_claim_busy(&orchestrator_busy) {
                                tracing::warn!(
                                    layer = "orchestrator",
                                    component = "continuum",
                                    reason = %reason,
                                    "Orchestrator already busy — skipping new wake"
                                );
                            } else {
                                let history = recent_frames[..recent_frames.len().saturating_sub(1)].to_vec();
                                let wake_speech_opt = if config.voice.ambient_mute_enabled && frame.context.in_call {
                                    tracing::info!(
                                        layer = "voice",
                                        component = "continuum",
                                        "Quiet mode active during call; orchestrator response will not be spoken"
                                    );
                                    None
                                } else {
                                    speech.clone()
                                };

                                // Phase 3 tier-split: mute Moshi's assistant
                                // output + send EndTurn so the orchestrator +
                                // Kokoros speak unobstructed. `resume()` runs in
                                // the spawned task once the wake finishes.
                                #[cfg(feature = "moshi")]
                                if let Some(fe) = moshi_frontend.as_ref() {
                                    fe.interrupt();
                                    tracing::info!(
                                        layer = "voice",
                                        component = "moshi",
                                        "Moshi interrupted for orchestrator escalation"
                                    );
                                }

                                // Spawn the wake as a tokio task so the main loop
                                // keeps draining frame_rx. If we awaited here,
                                // triage would queue up 5–15 stale frames while
                                // Opus streams for 5–10 s, and every subsequent
                                // response would be that many seconds behind reality.
                                // (Busy flag already claimed above by try_claim_busy.)
                                if let Ok(mut s) = runtime_state.lock() {
                                    s.wake_count += 1;
                                    s.voice_mode = Some("thinking".to_string());
                                }
                                let busy_flag = orchestrator_busy.clone();
                                let followup_shared = followup_until.clone();
                                let orch_cfg_clone = orch_config.clone();
                                let semantic_clone = semantic.clone();
                                let episodic_clone = episodic.clone();
                                let vault_clone = vault.clone();
                                let curator_cfg_clone = config.memory.curator.clone();
                                let feedback_clone = feedback.clone();
                                let frame_clone = frame.clone();
                                let reason_clone = reason.clone();
                                let followup_secs = config.voice.conversation_followup_seconds;
                                let runtime_state_clone = runtime_state.clone();
                                let skill_loader_clone = skill_loader.clone();
                                let suggested_skill_clone = suggested_skill.clone();
                                let skill_budget = config.skills.token_budget;
                                let dev_dir_clone = dev_dir.clone();
                                let mut wake_shutdown = shutdown_rx.clone();
                                #[cfg(feature = "moshi")]
                                let moshi_frontend_clone = moshi_frontend.clone();

                                tokio::spawn(async move {
                                    // Race the wake against the runtime's shutdown
                                    // signal: on Ctrl-C we abort the in-flight
                                    // wake so the claude subprocess is killed
                                    // (via kill_on_drop on the tokio Command)
                                    // rather than orphaned.
                                    let result = tokio::select! {
                                        r = do_wake(
                                            &frame_clone,
                                            &history,
                                            &reason_clone,
                                            &orch_cfg_clone,
                                            &semantic_clone,
                                            &episodic_clone,
                                            &vault_clone,
                                            &curator_cfg_clone,
                                            wake_speech_opt.as_ref(),
                                            &skill_loader_clone,
                                            suggested_skill_clone.as_deref(),
                                            skill_budget,
                                            &dev_dir_clone,
                                        ) => r,
                                        _ = wake_shutdown.changed() => {
                                            if *wake_shutdown.borrow() {
                                                tracing::info!(
                                                    layer = "orchestrator",
                                                    component = "continuum",
                                                    "Shutdown received — aborting in-flight wake"
                                                );
                                                Ok(())
                                            } else {
                                                // Spurious change (sender still 'false') — continue waiting.
                                                do_wake(
                                                    &frame_clone,
                                                    &history,
                                                    &reason_clone,
                                                    &orch_cfg_clone,
                                                    &semantic_clone,
                                                    &episodic_clone,
                                                    &vault_clone,
                                                    &curator_cfg_clone,
                                                    wake_speech_opt.as_ref(),
                                                    &skill_loader_clone,
                                                    suggested_skill_clone.as_deref(),
                                                    skill_budget,
                                                    &dev_dir_clone,
                                                )
                                                .await
                                            }
                                        }
                                    };

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
                                                    component = "continuum",
                                                    seconds = followup_secs,
                                                    "Follow-up window open"
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::error!(
                                                layer = "orchestrator",
                                                component = "continuum",
                                                error = %e,
                                                "Orchestrator wake failed"
                                            );
                                            println!("[ORCHESTRATOR ERROR: {e}]");
                                            feedback_clone.play(FeedbackCue::Error);
                                        }
                                    }

                                    busy_flag.store(false, std::sync::atomic::Ordering::Release);
                                    if let Ok(mut s) = runtime_state_clone.lock() {
                                        s.voice_mode = Some("idle".to_string());
                                    }
                                    // Phase 3 tier-split: hand the conversation back
                                    // to Moshi — unmute its assistant output so
                                    // the S2S front-end resumes natural turn-taking.
                                    #[cfg(feature = "moshi")]
                                    if let Some(fe) = moshi_frontend_clone.as_ref() {
                                        fe.resume();
                                        tracing::info!(
                                            layer = "voice",
                                            component = "moshi",
                                            "Moshi resumed after orchestrator turn"
                                        );
                                    }
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
                                    component = "continuum",
                                    decision = "whisper",
                                    text = %text,
                                    "Suppressed whisper — orchestrator already speaking"
                                );
                            } else {
                            // Phase 5.1: triage-driven local speech, no orchestrator wake.
                            tracing::info!(
                                layer = "triage",
                                component = "continuum",
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
                                    component = "continuum",
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
            _ = voice_intent_tick.tick() => {
                drain_voice_intents_tick(&dev_dir, &mut hotkey_pending, &feedback);
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
        component = "continuum",
        frames = frame_count,
        "Shutting down..."
    );

    raw_log.close().await;
    semantic.close().await;

    tracing::info!(
        layer = "system",
        component = "continuum",
        "Continuum stopped cleanly"
    );
    Ok(())
}

/// Performs a full orchestrator wake cycle.
#[allow(clippy::too_many_arguments)]
async fn do_wake(
    trigger_frame: &PerceptionFrame,
    history_frames: &[PerceptionFrame],
    reason: &str,
    config: &OrchestratorConfig,
    semantic: &Arc<SemanticStore>,
    episodic: &Arc<Mutex<EpisodicStore>>,
    vault: &Arc<continuum_memory::Vault>,
    curator_cfg: &CuratorConfig,
    speech: Option<&Arc<SpeechController>>,
    skill_loader: &SkillLoader,
    suggested_skill: Option<&str>,
    skill_token_budget: usize,
    dev_dir: &std::path::Path,
) -> Result<()> {
    let wake_start = Instant::now();

    println!("\n--- CONTINUUM WAKING ---");

    // 1. Memory context: episodic/semantic retrieval plus the memory vault
    // (Plan B curator) — confirmed notes relevant to this frame, and any
    // candidate notes that have sat unresolved long enough to nudge the
    // orchestrator to review them. Vault retrieval never fails the wake —
    // `retrieve_vault_context` swallows and logs its own errors.
    let mut memory_context = {
        let mut ep = episodic.lock().await;
        retrieve_context(trigger_frame, &mut ep, semantic).await?
    };
    let (vault_notes, pending_decisions) =
        retrieve_vault_context(vault, trigger_frame, curator_cfg).await;
    memory_context.vault_notes = vault_notes;
    memory_context.pending_decisions = pending_decisions;

    tracing::debug!(
        layer = "orchestrator",
        component = "continuum",
        retrieval_ms = wake_start.elapsed().as_millis() as u64,
        vault_notes = memory_context.vault_notes.len(),
        pending_decisions = memory_context.pending_decisions.len(),
        "Memory retrieved"
    );

    // 2. Wake message.
    let user_message = build_wake_message(trigger_frame, history_frames, &memory_context, reason);

    // Spec-gap 1 (cheap timeline win): record the wake itself on the
    // vault's own event timeline so the Memory tab's timeline strip shows
    // wakes alongside curator-written events, not just curator activity.
    // Best-effort — a vault hiccup here must never block the wake.
    let _ = vault
        .append_event(continuum_memory::NewEvent {
            ts: None,
            kind: "wake".to_string(),
            text: reason.to_string(),
            project: None,
            node_id: None,
            reference: None,
        })
        .await
        .map_err(|e| {
            tracing::warn!(
                layer = "memory",
                component = "continuum",
                error = %e.user_message(),
                "Failed to append wake event to vault timeline"
            );
        });

    // Best-effort: mark the injected vault notes as recently used. Spawned
    // so a slow/failing vault write never delays the wake itself; errors
    // are logged inside `touch_last_used`/swallowed here.
    if !memory_context.vault_notes.is_empty() {
        let ids: Vec<String> = memory_context
            .vault_notes
            .iter()
            .map(|n| n.id.clone())
            .collect();
        let vault_touch = vault.clone();
        tokio::spawn(async move {
            if let Err(e) = vault_touch.touch_last_used(&ids).await {
                tracing::warn!(
                    layer = "memory",
                    component = "continuum",
                    error = %e.user_message(),
                    "touch_last_used failed for wake-injected vault notes"
                );
            }
        });
    }

    // 3. Compose a dynamic system prompt — base file + matched skills +
    // any `suggested_skill` hint from the triage layer.
    let wake_config = compose_wake_config(
        config,
        skill_loader,
        reason,
        trigger_frame,
        suggested_skill,
        skill_token_budget,
        dev_dir,
    )
    .unwrap_or_else(|| config.clone());

    // 4. Spawn orchestrator + stream.
    print!("CONTINUUM: ");
    std::io::stdout().flush().ok();

    let mut full_response = String::new();
    let mut final_info: Option<(Option<u64>, Option<f64>)> = None;
    let result = wake_orchestrator(&wake_config, &user_message, |event| match &event {
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
                component = "continuum",
                session_id = %session_id,
                "Session ready"
            );
        }
    })
    .await?;

    // Block until the synthesised audio has finished playing so the
    // cost/duration summary prints *after* Continuum stops talking. Uses a
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
                component = "continuum",
                error = %e,
                "Failed to store wake event"
            );
        }

        let response_event = EpisodicEvent {
            id: uuid::Uuid::new_v4().to_string(),
            ts: chrono::Utc::now(),
            kind: EventKind::ContinuumResponse,
            summary: truncate(&full_response, 200),
            importance: 0.7,
            tags: vec!["response".to_string()],
            source_frame_id: Some(trigger_frame.id.to_string()),
        };
        if let Err(e) = ep.insert_event(&response_event).await {
            tracing::warn!(
                layer = "memory",
                component = "continuum",
                error = %e,
                "Failed to store response event"
            );
        }
    }

    tracing::info!(
        layer = "orchestrator",
        component = "continuum",
        total_ms = wake_start.elapsed().as_millis() as u64,
        cost_usd = result.cost_usd,
        success = result.success,
        "Wake cycle complete"
    );

    Ok(())
}

/// Builds the [`continuum_core::runtime_publish::CuratorSnapshot`] published
/// into `state.json` (Task 11).
///
/// `status` is `None` when the curator never spawned at all (no triage
/// model loaded at boot — see the `curator_status` binding in `main`); that
/// always publishes `enabled: false` with zeroed counters regardless of the
/// `[memory.curator] enabled` config value, since nothing is running to
/// report on. When `status` is `Some`, `enabled` reflects `curator_enabled`
/// (the config flag) directly: `run_curator` itself no-ops forever without
/// touching `status` when the config disables it (see
/// `curator::run::run_curator`'s early return), so a disabled-but-spawned
/// curator also correctly reports zeroed counters via `status`'s untouched
/// defaults.
fn build_curator_snapshot(
    status: Option<&curator::SharedCuratorStatus>,
    curator_enabled: bool,
) -> continuum_core::runtime_publish::CuratorSnapshot {
    use continuum_core::runtime_publish::CuratorSnapshot;
    match status {
        Some(status) => {
            let guard = status.lock().unwrap_or_else(|p| p.into_inner());
            CuratorSnapshot {
                last_pass_at: guard.last_pass_at.map(|t| t.to_rfc3339()),
                consecutive_failures: guard.consecutive_failures,
                candidates_written_total: guard.candidates_written_total,
                pending_count: guard.pending_count,
                enabled: curator_enabled,
            }
        }
        None => CuratorSnapshot {
            enabled: false,
            ..CuratorSnapshot::default()
        },
    }
}

/// Atomically claims `orchestrator_busy`, returning `true` only if this
/// call was the one that flipped it from `false` to `true`.
///
/// Shared by two concurrent producers that gate new orchestrator wakes on
/// the same flag: the `WakeOrchestrator` triage arm in `main`'s frame loop,
/// and `spawn_maintenance_wake_ticker`'s daily ticker task. A naive
/// load-then-store (check `load()`, then separately `store(true)`) is safe
/// for a single producer but not for two independent tasks — both could
/// observe "not busy" in the gap between the load and the store and each
/// spawn a `do_wake`, yielding two concurrent Opus subprocesses (double
/// cost, overlapping state writes). The single `compare_exchange` closes
/// that window.
fn try_claim_busy(busy: &std::sync::atomic::AtomicBool) -> bool {
    busy.compare_exchange(
        false,
        true,
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire,
    )
    .is_ok()
}

/// Spawns the daily memory-maintenance wake ticker (Task 10 of the memory
/// vault curator plan).
///
/// Once per local day, at `curator_cfg.maintenance_wake_hour`, this checks
/// whether the vault has pending memory decisions that would otherwise sit
/// unreviewed on a day when nothing else triggers a wake — the "queue
/// drains on quiet days" guarantee — and, if so, fires the same [`do_wake`]
/// path the triage `WakeOrchestrator` arm in `main` uses (see the argument
/// construction there, which this mirrors).
///
/// Disabled entirely when `maintenance_wake_hour < 0`. Values >= 24 clamp
/// to 23 with a `warn` logged once at spawn time. Never fabricates a
/// [`PerceptionFrame`]: skips (debug log) until `last_frame` has observed
/// at least one real frame from the main loop. Also skips when the curator
/// is disabled, when the vault has no pending decisions, or when
/// [`try_claim_busy`] can't claim the orchestrator — shared with the
/// `WakeOrchestrator` arm precisely to avoid a double-wake race between
/// the two (see that function's doc comment).
///
/// `history_frames` is passed as `&[]` to `do_wake` — this ticker only
/// tracks the single most recent frame, not the frame loop's rolling
/// history, so there is no synthetic history to hand the orchestrator.
#[allow(clippy::too_many_arguments)]
fn spawn_maintenance_wake_ticker(
    curator_cfg: CuratorConfig,
    orch_config: OrchestratorConfig,
    vault: Arc<continuum_memory::Vault>,
    semantic: Arc<SemanticStore>,
    episodic: Arc<Mutex<EpisodicStore>>,
    skill_loader: SkillLoader,
    skill_token_budget: usize,
    dev_dir: PathBuf,
    speech: Option<Arc<SpeechController>>,
    feedback: FeedbackPlayer,
    last_frame: Arc<std::sync::Mutex<Option<PerceptionFrame>>>,
    followup_until: Arc<std::sync::Mutex<Option<Instant>>>,
    followup_secs: u64,
    ambient_mute_enabled: bool,
    orchestrator_busy: Arc<std::sync::atomic::AtomicBool>,
    runtime_state: Arc<std::sync::Mutex<continuum_core::runtime_publish::RuntimeSnapshot>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let configured_hour = curator_cfg.maintenance_wake_hour;
    if configured_hour < 0 {
        tracing::info!(
            layer = "memory",
            component = "maintenance_wake",
            "daily memory-maintenance wake disabled (maintenance_wake_hour < 0)"
        );
        return;
    }
    let hour = if configured_hour >= 24 {
        tracing::warn!(
            layer = "memory",
            component = "maintenance_wake",
            configured = configured_hour,
            "maintenance_wake_hour >= 24, clamping to 23"
        );
        23u32
    } else {
        configured_hour as u32
    };

    tokio::spawn(async move {
        loop {
            let delay = continuum_core::health::backup::seconds_until_next_local(hour);
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(delay)) => {}
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    } else {
                        continue;
                    }
                }
            }

            if !curator_cfg.enabled {
                tracing::debug!(
                    layer = "memory",
                    component = "maintenance_wake",
                    "skip — curator disabled"
                );
                continue;
            }

            let pending = match vault.pending().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        layer = "memory",
                        component = "maintenance_wake",
                        error = %e.user_message(),
                        "skip — vault.pending() failed"
                    );
                    continue;
                }
            };
            // I4 fix: gate on the same filtered view `retrieve_vault_context`
            // uses for the wake message itself (age + sensitivity), not the
            // raw unfiltered list — otherwise a vault whose only pending
            // items are too-fresh or sensitivity-excluded could fire a
            // no-op wake (nothing to show) every single day, forever.
            let pending = filter_pending(pending, &curator_cfg, chrono::Utc::now());
            if pending.is_empty() {
                tracing::debug!(
                    layer = "memory",
                    component = "maintenance_wake",
                    "skip — no pending memory decisions after age/sensitivity filtering"
                );
                continue;
            }

            let frame = {
                let guard = match last_frame.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.clone()
            };
            let Some(frame) = frame else {
                tracing::debug!(
                    layer = "memory",
                    component = "maintenance_wake",
                    "skip — no frame observed yet"
                );
                continue;
            };

            // Claim the busy flag right before firing (not earlier) so the
            // window where we could race the frame loop's own wake is as
            // small as possible. Shared helper with the WakeOrchestrator
            // arm — see try_claim_busy's doc comment for why this must be
            // a single CAS rather than load-then-store.
            if !try_claim_busy(&orchestrator_busy) {
                tracing::debug!(
                    layer = "memory",
                    component = "maintenance_wake",
                    "skip — orchestrator busy"
                );
                continue;
            }

            let reason = format!(
                "daily memory maintenance: {} pending memory decisions",
                pending.len()
            );
            tracing::info!(
                layer = "memory",
                component = "maintenance_wake",
                pending = pending.len(),
                "Firing daily memory-maintenance wake"
            );

            if let Ok(mut s) = runtime_state.lock() {
                s.wake_count += 1;
                s.voice_mode = Some("thinking".to_string());
            }

            // Mirrors the WakeOrchestrator arm's ambient-mute check: a live
            // call suppresses spoken output, but the wake itself still runs.
            let wake_speech_opt = if ambient_mute_enabled && frame.context.in_call {
                None
            } else {
                speech.clone()
            };

            // Unlike the WakeOrchestrator arm, this does not race against
            // `shutdown` via tokio::select! — an in-flight maintenance wake
            // is simply reaped at process exit, which is acceptable for a
            // once-daily best-effort job (not worth the extra branching).
            let result = do_wake(
                &frame,
                &[],
                &reason,
                &orch_config,
                &semantic,
                &episodic,
                &vault,
                &curator_cfg,
                wake_speech_opt.as_ref(),
                &skill_loader,
                None,
                skill_token_budget,
                &dev_dir,
            )
            .await;

            match result {
                Ok(()) => {
                    if followup_secs > 0 {
                        if let Ok(mut slot) = followup_until.lock() {
                            *slot = Some(Instant::now() + Duration::from_secs(followup_secs));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(
                        layer = "memory",
                        component = "maintenance_wake",
                        error = %e,
                        "daily memory-maintenance wake failed"
                    );
                    feedback.play(FeedbackCue::Error);
                }
            }

            orchestrator_busy.store(false, std::sync::atomic::Ordering::Release);
            if let Ok(mut s) = runtime_state.lock() {
                s.voice_mode = Some("idle".to_string());
            }
        }
    });
}

/// Build + start the Moshi S2S front-end and wire the tap receiver (created
/// at the audio-spawn site) + event consumer. Only compiled with the `moshi`
/// cargo feature; the runtime also requires `voice.frontend.mode = "moshi"`,
/// a CUDA-built `moshi-backend.exe` on PATH / `CONTINUUM_MOSHI_BIN`, and (for
/// audio) the `moshi-opus` feature + libopus.
///
/// Returns the `Arc<MoshiFrontend>` handle for the main loop / Phase 3 bridge.
///
/// **Not runtime-verified in this environment** (no CUDA / moshi-backend.exe /
/// libopus). Compile-verified with `cargo check --features moshi`.
#[cfg(feature = "moshi")]
fn wire_moshi_voice(
    config: &ContinuumConfig,
    moshi_tap_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<f32>>>,
    runtime_state: Arc<std::sync::Mutex<continuum_core::runtime_publish::RuntimeSnapshot>>,
) -> Result<std::sync::Arc<continuum_core::voice::moshi::MoshiFrontend>> {
    use continuum_core::voice::frontend::VoiceFrontend;
    use continuum_core::voice::moshi::{MoshiConfig, MoshiEvent, MoshiFrontend};

    let fc = &config.voice.frontend;
    let cfg = MoshiConfig {
        host: "127.0.0.1".to_string(),
        port: fc.moshi_port,
        model_repo: fc.moshi_model_repo.clone(),
        device: fc.moshi_device.clone(),
        bin: if fc.moshi_bin.is_empty() {
            PathBuf::new()
        } else {
            PathBuf::from(&fc.moshi_bin)
        },
    };
    let frontend = std::sync::Arc::new(MoshiFrontend::new(cfg));

    if let Err(e) = frontend.start() {
        tracing::error!(layer = "voice", component = "moshi", error = %e, "moshi-backend start failed");
        // Fall through: health reports moshi_loaded=false; the tap (if any)
        // just drains to nothing.
    }
    if let Ok(mut s) = runtime_state.lock() {
        s.moshi_loaded = Some(frontend.loaded());
    }

    // Tap consumer: drain the 16 kHz mono PCM the audio watcher forks in, and
    // feed it to the frontend. Only spawned when the tap channel was created.
    if let Some(mut tap_rx) = moshi_tap_rx {
        let fe_tap = frontend.clone();
        tokio::spawn(async move {
            while let Some(samples) = tap_rx.recv().await {
                fe_tap.feed_pcm(&samples);
            }
        });
    }

    // Event consumer: route assistant text deltas to `partial_transcript` so
    // the dashboard shows the live Moshi transcript. Audio events (only with
    // `moshi-opus`) are Phase-3-routed to PlaybackStream; for now logged.
    let fe_events = frontend.clone();
    let rs = runtime_state.clone();
    tokio::spawn(async move {
        let mut rx = match fe_events.events() {
            Some(r) => r,
            None => return, // already taken — shouldn't happen
        };
        let mut text_acc = String::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                MoshiEvent::Text(t) => {
                    text_acc.push_str(&t);
                    if let Ok(mut s) = rs.lock() {
                        s.partial_transcript = Some(text_acc.clone());
                        s.voice_mode = Some("speaking".to_string());
                        s.moshi_loaded = Some(true);
                    }
                }
                MoshiEvent::Audio(_pcm) => {
                    // Phase 3: route 24 kHz mono f32 to PlaybackStream
                    // (resample 24k→device rate). Only produced with the
                    // `moshi-opus` feature; no-op otherwise.
                }
                MoshiEvent::Error(e) => {
                    tracing::warn!(layer = "voice", component = "moshi", "server error: {e}");
                    if let Ok(mut s) = rs.lock() {
                        s.moshi_loaded = Some(false);
                        s.voice_mode = Some("error".to_string());
                    }
                }
                MoshiEvent::Disconnected(msg) => {
                    tracing::warn!(layer = "voice", component = "moshi", "disconnected: {msg}");
                    if let Ok(mut s) = rs.lock() {
                        s.moshi_loaded = Some(false);
                    }
                }
            }
        }
    });

    Ok(frontend)
}

/// Updates the post-wake voice session and returns an explicit wake decision
/// once the spoken command is complete. Also manages the conversation
/// follow-up window and hotkey push-to-talk trigger.
#[allow(clippy::too_many_arguments)]
fn update_voice_session(
    frame: &PerceptionFrame,
    config: &ContinuumConfig,
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

    // While Continuum is actively speaking, the mic is likely picking up its
    // own TTS output. Drop those transcripts — they almost always contain
    // fragments of what Continuum just said and will trigger spurious wakes
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
                "Dropped transcript — Continuum is currently speaking"
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
                suggested_skill: None,
            });
        }
    }

    None
}

/// Resolve the skills directory: first checks the repo-relative `skills/`,
/// then the user's dev-dir fallback. Never panics.
fn resolve_skills_root(cfg: &ContinuumConfig) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(&cfg.skills.dir);
    if p.is_absolute() {
        return p;
    }
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(&cfg.skills.dir);
        if candidate.exists() {
            return candidate;
        }
    }
    continuum_dev_dir().join(&cfg.skills.dir)
}

/// If any skills match the wake reason, write a dynamic prompt combining
/// the base system prompt with matched skill content and return a config
/// clone that points at it. Returns `None` (falling back to the base
/// config) when nothing matches.
fn compose_wake_config(
    base: &OrchestratorConfig,
    skill_loader: &SkillLoader,
    reason: &str,
    frame: &PerceptionFrame,
    suggested_skill: Option<&str>,
    token_budget: usize,
    dev_dir: &std::path::Path,
) -> Option<OrchestratorConfig> {
    let skills = skill_loader.enabled();
    if skills.is_empty() {
        return None;
    }
    let audio_text = frame
        .audio
        .as_ref()
        .map(|a| a.transcript.clone())
        .unwrap_or_default();
    let ctx = MatchContext {
        wake_reason: Some(reason.to_string()),
        task: None,
        project: None,
        audio_transcript: if audio_text.is_empty() {
            None
        } else {
            Some(audio_text)
        },
        foreground_app: Some(frame.context.foreground_process_name.clone()),
        tags: Vec::new(),
        forced: suggested_skill
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
    };
    let (skill_prompt, names) = SkillMatcher::render_prompt(&skills, &ctx, token_budget);
    if skill_prompt.is_empty() {
        return None;
    }
    let base_text = if base.system_prompt_path.is_empty() {
        String::new()
    } else {
        std::fs::read_to_string(&base.system_prompt_path).unwrap_or_default()
    };
    let combined = format!("{base_text}\n\n{skill_prompt}");
    let dyn_path = dev_dir.join("orchestrator-dynamic.md");
    std::fs::write(&dyn_path, combined).ok()?;
    tracing::info!(
        layer = "skills",
        component = "continuum",
        skills = ?names,
        "Dynamic orchestrator prompt assembled"
    );
    let mut new_cfg = base.clone();
    new_cfg.system_prompt_path = dyn_path.to_string_lossy().into_owned();
    Some(new_cfg)
}

/// Spawn the global hotkey listener if configured. Returns `None` when
/// the config disables the hotkey (empty string) or registration fails
/// (chord already owned by another app) — Continuum logs a warning and
/// continues without push-to-talk.
#[cfg(windows)]
fn spawn_hotkey(
    config: &ContinuumConfig,
) -> Option<(
    continuum_core::voice::hotkey::HotkeyHandle,
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

/// Drain dashboard push-to-talk intents and apply the same effect as a
/// hotkey press. Multiple intents in the same tick collapse into one
/// listen-trigger because `hotkey_pending` is a boolean — we only play
/// the feedback cue once per drain to avoid stutter when several clicks
/// arrive in flight.
fn drain_voice_intents_tick(
    dev_dir: &std::path::Path,
    hotkey_pending: &mut bool,
    feedback: &FeedbackPlayer,
) {
    let intents = match voice_intent::drain_intents(dev_dir) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(
                layer = "voice",
                component = "intent",
                error = %e,
                "Failed to drain voice intents"
            );
            return;
        }
    };
    if intents.is_empty() {
        return;
    }
    let mut had_talk_now = false;
    for intent in intents {
        match intent {
            VoiceIntent::TalkNow { .. } => {
                had_talk_now = true;
            }
        }
    }
    if had_talk_now {
        tracing::info!(
            layer = "voice",
            component = "intent",
            "Push-to-talk intent received — next transcript opens a session"
        );
        *hotkey_pending = true;
        feedback.play(FeedbackCue::Listen);
    }
}

/// Initialize the TTS pipeline and feedback player together so they share
/// a single [`PlaybackStream`] — one queue means TTS audio and UI cues
/// naturally order behind each other. Returns a disabled [`FeedbackPlayer`]
/// when the audio device is unavailable, so callers don't need to branch.
fn init_tts_and_feedback(
    config: &ContinuumConfig,
) -> (Option<Arc<SpeechController>>, FeedbackPlayer) {
    let cfg = &config.tts;
    if !cfg.enabled {
        tracing::info!(
            layer = "voice",
            component = "continuum",
            "TTS disabled in config"
        );
        return (None, FeedbackPlayer::disabled());
    }

    let espeak_dir = expand_home(&cfg.espeak_data_dir);
    set_espeak_data_dir(&espeak_dir);

    // Piper voice resolution is only needed for the piper / elevenlabs paths.
    // Kokoros resolves its own model + voices files inside its arm below.
    let piper_voice = if cfg.engine != "kokoros" {
        let voice_cfg = match cfg.voices.get(&cfg.primary) {
            Some(v) => v,
            None => {
                tracing::warn!(
                    layer = "voice",
                    component = "continuum",
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
                component = "continuum",
                model = %model_path.display(),
                config = %config_path.display(),
                "Piper voice files missing — run scripts/download-models.ps1. TTS disabled."
            );
            return (None, FeedbackPlayer::disabled());
        }
        Some((model_path, config_path, voice_cfg.speaker_id))
    } else {
        None
    };

    let engine: Arc<dyn TtsEngine> = match cfg.engine.as_str() {
        "kokoros" => {
            let kcfg = &cfg.kokoros;
            let model_path = expand_home(&kcfg.model_path);
            let voices_path = expand_home(&kcfg.voices_path);
            if !model_path.exists() || !voices_path.exists() {
                tracing::warn!(
                    layer = "voice",
                    component = "continuum",
                    model = %model_path.display(),
                    voices = %voices_path.display(),
                    "Kokoros model/voices files missing — run scripts/download-models.ps1. \
                     TTS disabled."
                );
                return (None, FeedbackPlayer::disabled());
            }
            match KokorosEngine::new(
                &model_path,
                &voices_path,
                kcfg.voice_name.clone(),
                kcfg.speed,
            ) {
                Ok(e) => Arc::new(e),
                Err(e) => {
                    tracing::error!(
                        layer = "voice",
                        component = "continuum",
                        error = %e,
                        "Kokoros engine init failed; TTS disabled"
                    );
                    return (None, FeedbackPlayer::disabled());
                }
            }
        }
        "elevenlabs" => {
            tracing::warn!(
                layer = "voice",
                component = "continuum",
                "tts.engine = \"elevenlabs\" is a Phase 5 extension point \
                 (stub). Falling back to Piper."
            );
            let _stub = ElevenLabsEngine::new(
                cfg.elevenlabs.voice_id.clone(),
                cfg.elevenlabs.model_id.clone(),
            );
            let (model_path, config_path, speaker_id) =
                piper_voice.expect("piper_voice is resolved for non-kokoros engines");
            match PiperEngine::new(&model_path, &config_path, cfg.length_scale, speaker_id) {
                Ok(e) => Arc::new(e),
                Err(e) => {
                    tracing::error!(
                        layer = "voice",
                        component = "continuum",
                        error = %e,
                        "Piper fallback init failed; TTS disabled"
                    );
                    return (None, FeedbackPlayer::disabled());
                }
            }
        }
        _ => {
            // Default: Piper local.
            let (model_path, config_path, speaker_id) =
                piper_voice.expect("piper_voice is resolved for non-kokoros engines");
            match PiperEngine::new(&model_path, &config_path, cfg.length_scale, speaker_id) {
                Ok(e) => Arc::new(e),
                Err(e) => {
                    tracing::error!(
                        layer = "voice",
                        component = "continuum",
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
                component = "continuum",
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
                component = "continuum",
                warmup_ms = start.elapsed().as_millis() as u64,
                "Piper warmup done"
            ),
            Err(e) => tracing::warn!(
                layer = "voice",
                component = "continuum",
                error = %e,
                "Piper warmup failed — first real utterance will be slow"
            ),
        }
    });

    tracing::info!(
        layer = "voice",
        component = "continuum",
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
///
/// Honours the resolved resource plan: when `plan.vision_enabled` is false
/// (e.g. very low RAM), perception runs text-only and we return the stub. When
/// enabled, `plan.vision_gpu` selects the ONNX CUDA execution provider (with
/// CPU fallback baked into the model loader).
async fn init_vision_model(
    config: &ContinuumConfig,
    plan: &continuum_core::hardware::ResolvedResourcePlan,
) -> Arc<dyn continuum_vision::VisionModel> {
    if !plan.vision_enabled {
        tracing::info!(
            layer = "senses",
            component = "continuum",
            "Vision disabled by resource policy; using stub (text-only perception)"
        );
        return Arc::new(StubVisionModel);
    }

    let model_path = &config.vision.model_path;

    match continuum_vision::onnx::OnnxVisionModel::new(model_path, plan.vision_gpu).await {
        Ok(model) => {
            if let Err(e) = model.warmup().await {
                tracing::warn!(
                    layer = "senses",
                    component = "continuum",
                    error = %e,
                    "Vision warmup failed, using stub"
                );
            }
            Arc::new(model)
        }
        Err(e) => {
            tracing::warn!(
                layer = "senses",
                component = "continuum",
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
        component = "continuum",
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

// ---- `continuum setup` subcommand ---------------------------------------------

/// Prereq / model / diagnostic check. Prints a status report and exits 0 if
/// everything is OK, 1 otherwise. Designed to be safe to run repeatedly.
async fn run_setup_command() -> Result<()> {
    println!("continuum setup — checking install and runtime prerequisites");
    println!();

    let dev_dir = continuum_dev_dir();
    let mut all_ok = true;

    // 1. Claude Code CLI.
    print!("  [..] Claude Code CLI .................. ");
    let _ = std::io::stdout().flush();
    match tokio::process::Command::new("claude")
        .arg("--version")
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!("OK ({v})");
        }
        _ => {
            println!("MISSING");
            println!("       -> npm install -g @anthropic-ai/claude-code && claude login");
            all_ok = false;
        }
    }

    // 2. Data directory structure.
    for sd in &["config", "models", "logs", "memory", "backups", "bin"] {
        let p = dev_dir.join(sd);
        if !p.exists() {
            if let Err(e) = std::fs::create_dir_all(&p) {
                println!("  [FAIL] Could not create {}: {}", p.display(), e);
                all_ok = false;
            } else {
                println!("  [OK]   Created {}", p.display());
            }
        }
    }

    // 3. Vision model.
    let vision = dev_dir
        .join("models")
        .join("vision")
        .join("vision_encoder.onnx");
    report_file("Vision model (SmolVLM-256M)", &vision, &mut all_ok);

    // 4. Triage model (8B preferred, 4B fallback).
    let triage_8b = dev_dir
        .join("models")
        .join("triage")
        .join("qwen3-8b-q4_k_m.gguf");
    let triage_4b = dev_dir
        .join("models")
        .join("triage")
        .join("qwen3-4b-q4_k_m.gguf");
    if triage_8b.exists() {
        println!("  [OK]   Triage model ..................... Qwen 3 8B");
    } else if triage_4b.exists() {
        println!("  [WARN] Triage model ..................... Qwen 3 4B fallback");
        println!("       -> 8B recommended; run scripts/download-models.ps1 to fetch");
    } else {
        println!("  [FAIL] Triage model ..................... MISSING");
        println!("       -> scripts/download-models.ps1");
        all_ok = false;
    }

    // 5. Whisper STT.
    let whisper = dev_dir
        .join("models")
        .join("stt")
        .join("whisper-medium.bin");
    report_file("Whisper STT (medium)", &whisper, &mut all_ok);

    // 6. Piper TTS.
    let piper_env = env_or_legacy("CONTINUUM_PIPER_BIN", "KAIRO_PIPER_BIN").map(PathBuf::from);
    let piper_default = dev_dir.join("bin").join("piper").join("piper.exe");
    let piper = piper_env.unwrap_or(piper_default);
    if piper.exists() {
        println!(
            "  [OK]   Piper TTS .......................... {}",
            piper.display()
        );
    } else {
        println!("  [FAIL] Piper TTS .......................... MISSING");
        println!(
            "       -> scripts/download-models.ps1 installs it under ~/.continuum-dev/bin/piper/"
        );
        all_ok = false;
    }

    // 7. Config file.
    let config_path = dev_dir.join("config.toml");
    if config_path.exists() {
        println!(
            "  [OK]   Config ............................ {}",
            config_path.display()
        );
    } else {
        println!("  [INFO] Config file not found. Continuum will seed defaults on first launch.");
    }

    println!();
    if all_ok {
        println!(
            "continuum setup: all required components present. You're ready to run 'continuum'."
        );
        Ok(())
    } else {
        println!("continuum setup: some components are missing (see above).");
        std::process::exit(1);
    }
}

fn report_file(label: &str, path: &std::path::Path, all_ok: &mut bool) {
    if path.exists() {
        let bytes = path.metadata().map(|m| m.len()).unwrap_or(0);
        let mb = bytes / 1_048_576;
        println!("  [OK]   {label} ({mb} MB)");
    } else {
        println!("  [FAIL] {label} MISSING");
        println!("       -> scripts/download-models.ps1");
        *all_ok = false;
    }
}
