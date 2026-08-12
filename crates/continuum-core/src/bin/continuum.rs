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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::sync::{mpsc, watch, Mutex};
use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::EnvFilter;

use continuum_vision::VisionModel;

use continuum_core::audit::{Actor, AuditLog};
use continuum_core::config::{
    continuum_dev_dir, env_or_legacy, load_config, ContextPackageConfig, ContinuationConfig,
    ContinuumConfig, CuratorConfig,
};
use continuum_core::context::apply::{
    apply_deferred_intent, apply_intent, is_deferred, DeferredIntentContext, IntentContext,
};
use continuum_core::context::continuation::{
    self, ContinuationInputs, ContinuationOutcome, MAX_ASK_CANDIDATES,
};
use continuum_core::context::intents::{
    self as context_intent, IntentDrainer, SessionField, ToggleName,
};
use continuum_core::context::package::{parse_wake_trailer, NextStep, ToolsSection};
use continuum_core::context::project::{
    CurrentProject, CurrentProjectHandle, FrameInput, ProjectEntry, ProjectResolver, ProjectStatus,
};
use continuum_core::context::session_state::{
    self, ProjectPinGuard, SessionState, SessionStateHub, StampedText,
};
use continuum_core::curator;
use continuum_core::guard::FallbackGuard;
use continuum_core::memory::distill::run_memory_distiller;
use continuum_core::memory::episodic::{EpisodicEvent, EpisodicStore, EventKind};
use continuum_core::memory::events::{
    event_enum_token, install_global_sender, project_switch_event, send_system_event,
    spawn_event_writer, EventSensitivity, EventType,
};
use continuum_core::memory::raw_log::{ContextEventRow, RawLog};
use continuum_core::memory::retrieval::{filter_pending, retrieve_context, retrieve_vault_context};
use continuum_core::memory::semantic::SemanticStore;
use continuum_core::orchestrator::spawn::{
    wake_orchestrator, OrchestratorConfig, OrchestratorEvent,
};
use continuum_core::orchestrator::wake_context::{
    self as wake_context, build_wake_package, FrameRing, WakeContextInputs,
};
use continuum_core::runtime_publish::{
    ComponentHealthSummary, ContextEngineSnapshot, ContextEventView, ContextPageSnapshot,
    ContinuationCandidateView, ObservationTogglesView, OverrideRuleView, ProjectSummaryView,
    SessionPinView,
};
use continuum_core::senses::audio::AudioWatcher;
use continuum_core::senses::cadence::{CadenceControl, IdleController, IdleTransition};
use continuum_core::senses::context::{ContextWatcher, SharedContextWatchHealth};
use continuum_core::senses::file_watch::{
    FileWatcher, ProjectsProvider, RecentFileHandle, SharedFileWatchHealth,
};
use continuum_core::senses::frame::PerceptionFrameBuilder;
use continuum_core::senses::git_watch::{GitWatcher, SharedGitWatchHealth};
use continuum_core::senses::live_context::{self, LiveContextHub};
use continuum_core::senses::privacy::{emit_system_event, strictest, PrivacyFilter, Zone};
use continuum_core::senses::process_watch::{ProcessWatcher, SharedProcessWatchHealth};
use continuum_core::senses::screenshots::ScreenshotPolicy;
use continuum_core::senses::toggles::ToggleControl;
use continuum_core::senses::types::{
    AudioObservation, ContextObservation, PerceptionFrame, ScreenObservation,
};
use continuum_core::senses::vision::VisionWatcher;
use continuum_core::skills::{MatchContext, SkillLoader, SkillMatcher};
use continuum_core::supervisor::Supervisor;
use continuum_core::triage::coalesce::{Submitted, TriageBusyHandle, TriageCoalescer};
use continuum_core::triage::consume::ClassificationConsumer;
use continuum_core::triage::handlers::handle_decision;
use continuum_core::triage::llm::{TriageConfig, TriageLayer};
use continuum_core::triage::{Classification, TriageDecision, TriageOutput};
use continuum_core::voice::intent::{self as voice_intent, VoiceIntent};
use continuum_core::voice::playback::PlaybackStream;
use continuum_core::voice::recv_hotkey;
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
    let pause_file_exists = continuum_core::privacy_pause::pause_path(&dev_dir).exists();
    match continuum_core::privacy_pause::read_status(&dev_dir, Utc::now()) {
        Ok(status) if status.paused => config.privacy.toggles.pause_all = true,
        Ok(_) if pause_file_exists => {
            config.privacy.toggles.pause_all = false;
            let _ = continuum_core::privacy_pause::clear(&dev_dir);
            if let Err(error) =
                continuum_core::config_edit::set_toggle(&config_path, ToggleName::PauseAll, false)
            {
                tracing::warn!(
                    layer = "privacy",
                    component = "observation_pause",
                    error = %error,
                    "Expired privacy pause could not normalize config"
                );
            }
        }
        Ok(_) => {}
        Err(error) => {
            config.privacy.toggles.pause_all = true;
            tracing::error!(
                layer = "privacy",
                component = "observation_pause",
                error = %error,
                "Privacy pause record is invalid; failing closed"
            );
        }
    }

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

    // Structured logging — tee to stdout AND a fixed file at
    // `<dev_dir>/logs/continuum.log`. The repair agent tails that file for
    // diagnose→restart context (health/repair.rs `runtime_log_tail`).
    // `rolling::never` keeps a single stable filename the repair agent can
    // rely on (no per-rotation guessing); `non_blocking` decouples the write
    // path so a slow disk never stalls a sense/orchestrator task. The
    // `_log_guard` must outlive the subscriber — bound here, it lives until
    // `main` returns (an underscore-*prefixed* name suppresses the unused
    // warning without triggering an immediate drop the way a bare `_` would).
    let default_filter = "info,continuum_core=debug,continuum_vision=info,continuum_llm=info";
    let logs_dir = dev_dir.join("logs");
    std::fs::create_dir_all(&logs_dir).context("Failed to create runtime logs directory")?;
    let file_writer = tracing_appender::rolling::never(&logs_dir, "continuum.log");
    let (non_blocking_file, _log_guard) = tracing_appender::non_blocking(file_writer);
    tracing_subscriber::fmt()
        .with_writer(non_blocking_file.and(std::io::stdout))
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

    // --- Project resolver (Task A4, spec §4.3) ---
    // Boot reconciliation: [[projects.known]] config seeds/updates the
    // Projects table (config wins by id); discovered/confirmed rows and
    // persisted override rules survive restarts. A table failure degrades
    // to config-only resolution — never blocks boot.
    let config_projects: Vec<ProjectEntry> = config
        .projects
        .known
        .iter()
        .map(ProjectEntry::from_config)
        .collect();
    let table_projects = match raw_log.reconcile_projects(&config_projects).await {
        Ok(rows) => rows.into_iter().map(|row| row.entry).collect(),
        Err(e) => {
            tracing::warn!(
                layer = "context",
                component = "continuum",
                error = %e,
                "Projects-table reconciliation failed; resolver runs config-only"
            );
            config_projects.clone()
        }
    };
    let project_rules = raw_log.list_project_rules().await.unwrap_or_else(|e| {
        tracing::warn!(
            layer = "context",
            component = "continuum",
            error = %e,
            "Failed to load project override rules; resolver runs without them"
        );
        Vec::new()
    });
    let mut project_resolver =
        ProjectResolver::new(table_projects, project_rules, &config.projects);
    let project_handle: CurrentProjectHandle = project_resolver.handle();
    // Task A7 seam (spec §4.3 tier 3): the file watcher writes its most
    // recent non-ignored event path here; the frame loop reads it into
    // `FrameInput.recent_file_path` to activate git-root resolution.
    let recent_file_handle = RecentFileHandle::default();

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

    // --- Session state (Task B5, spec §4.8) ---
    // Continuum's live "what is the user doing" state. Boot rehydration
    // seeds it from the last published state.json snapshot (Plan C
    // publishes it; absent today, which degrades to "start empty") plus
    // the last hour of context_events, with confidence discounted by age
    // — this is what lets the §4.12 continuation resolver answer "ga door"
    // after a restart. Never fails: any read error degrades to empty.
    let session_state = {
        let (state, digests) = continuum_core::context::session_state::rehydrate_from_disk(
            &dev_dir,
            &raw_log,
            &config.session_state,
            Utc::now(),
        )
        .await;
        let hub = SessionStateHub::with_state(state);
        hub.seed_events(digests);
        hub
    };

    // --- Events writer (Task A6, spec §3/§4.6) ---
    // The dedicated writer task owns the context_events table: bounded
    // mpsc channel, dedupe + batch inserts, overflow coalesce, retention
    // rotation. One sender is cloned into every collector; the global
    // install covers producers without an injection path
    // (senses::privacy::emit_system_event). The writer stamps the
    // resolver's current project onto events that arrive unattributed.
    // Health (Task A8, spec §7): `event_writer_health` (queue depth +
    // last-flush ts) is published into RuntimeSnapshot.context_engine by
    // the state publisher below; should_restart() fires only if the
    // writer dies unexpectedly.
    let (event_sender, event_writer_health) = spawn_event_writer(
        raw_log.clone(),
        &config.events,
        Some(project_handle.clone()),
        shutdown_rx.clone(),
    );
    // Task B5 (spec §4.8): session state's mechanical fields
    // (last_error/last_success/open_files) and its inference window read
    // the SAME event stream the writer persists. The tap runs before the
    // queue, so a full queue costs a row but never a state update. It must
    // be attached BEFORE any clone — every clone below carries it.
    let event_sender = event_sender.with_observer({
        let hub = session_state.clone();
        let cfg = config.session_state.clone();
        std::sync::Arc::new(move |ev: &continuum_core::memory::events::ContextEvent| {
            hub.apply_context_event(ev, &cfg);
        })
    });
    install_global_sender(event_sender.clone());

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
                        project: None,
                        sensitivity: continuum_core::memory::events::EventSensitivity::CloudAllowed,
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
                    project: None,
                    sensitivity: continuum_core::memory::events::EventSensitivity::CloudAllowed,
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

    // Phase 3: background raw-log to episodic-memory distillation, plus
    // raw-log retention rotation on the same ticker (Task B6).
    let distiller_shutdown = shutdown_rx.clone();
    let distiller_raw_log = raw_log.clone();
    let distiller_episodic = episodic.clone();
    let distiller_vault = vault.clone();
    let distiller_config = config.memory.clone();
    let distiller_storage = config.storage.clone();
    tokio::spawn(async move {
        run_memory_distiller(
            distiller_raw_log,
            distiller_episodic,
            distiller_vault,
            distiller_config,
            distiller_storage,
            distiller_shutdown,
        )
        .await;
    });

    // --- Privacy filter (context engine spec §4.1) ---
    // Constructed once at senses spawn and shared (Arc) into every watcher.
    // Every observed byte — titles, captions, transcripts, paths — passes
    // through it at collector emit, before the live-context hub, the frame
    // channel, and persistence.
    let privacy_filter = Arc::new(PrivacyFilter::from_config(&config.context, &config.privacy));
    let observation_toggles = config.privacy.toggles.clone();
    // --- Live observation toggles (Task C5, spec §4.1/§4.13) ---
    // The shared-atomics control every watcher re-reads each iteration, so
    // a Context-page switch takes effect without a restart. Seeded from the
    // same boot config the watchers get; `set_toggle` intents store into
    // it and persist the value to config.toml.
    //
    // Watchers always spawn and park behind these atomics while paused, so
    // pause and resume both work live, including after a paused boot.
    let toggle_control = ToggleControl::new(&observation_toggles);
    // --- Action audit log (Task C5, spec §4.13) ---
    // Append-only JSONL of wakes, toggle changes, corrections and
    // deletions at `<data_dir>/logs/actions.jsonl`.
    let audit = AuditLog::new(&dev_dir);

    // --- Cadence control + idle controller (Task A8, spec §3/§4.11) ---
    // The shared-AtomicU64 cadence handle is the sanctioned pattern for
    // runtime-adjustable cadences: capture workers and the vision gate
    // read it each iteration; the idle controller below adjusts it when
    // `idle_seconds` crosses `[performance].idle_pause_after_secs` and
    // restores it on input activity, voice wake, hotkey, or any
    // orchestrator wake. Shared behind a std Mutex because the daily
    // maintenance-wake ticker is a second wake producer (locks held for
    // microseconds).
    let cadence = CadenceControl::new(
        config.screen.capture_interval_ms,
        config.screen.vision_min_interval_ms,
    );
    let idle_controller: Arc<std::sync::Mutex<IdleController>> = Arc::new(std::sync::Mutex::new(
        IdleController::new(&config.performance),
    ));

    // --- Perception channels ---
    let live_context = LiveContextHub::new(config.screen.buffer_capacity.saturating_mul(4));
    // Updated by the supervised vision task after the real ONNX model has
    // loaded and warmed up. This must never be inferred from "the watcher is
    // running": the watcher deliberately stays alive with a stub when model
    // files or the runtime are unavailable.
    let vision_model_loaded = Arc::new(AtomicBool::new(false));
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

    // Optional Moshi S2S audio fork. The privacy/toggle-aware watcher remains
    // the single capture path; Moshi only receives the same already-admitted
    // 16 kHz mono samples through this in-process tap.
    #[cfg(feature = "moshi")]
    let moshi_active = !observation_toggles.pause_all
        && config.voice.frontend.mode == "moshi"
        && config.voice.frontend.moshi_tap_enabled;
    #[cfg(not(feature = "moshi"))]
    let moshi_active = false;
    #[cfg(feature = "moshi")]
    #[allow(clippy::type_complexity)]
    let (moshi_tap, moshi_tap_rx): (
        Option<tokio::sync::mpsc::UnboundedSender<Vec<f32>>>,
        Option<tokio::sync::mpsc::UnboundedReceiver<Vec<f32>>>,
    ) = if moshi_active {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    #[cfg(not(feature = "moshi"))]
    let moshi_tap: Option<tokio::sync::mpsc::UnboundedSender<Vec<f32>>> = None;

    // Health-snapshot registration (Task A8, spec §7): shared health
    // handles grabbed before each watcher is moved into its task. For the
    // supervisor-managed watchers these are created once here and passed into
    // every (re)spawn via `with_health`, so the published snapshot stays valid
    // across restarts instead of orphaning on the dead instance.
    let context_watch_health: Option<SharedContextWatchHealth>;
    let git_watch_health: Option<SharedGitWatchHealth>;
    let file_watch_health: Option<SharedFileWatchHealth>;
    let process_watch_health: Option<SharedProcessWatchHealth>;

    // Runtime component supervisor (self-healing, spec §7): owns the lifecycle
    // of every long-running sense task. A watch loop reaps dead `JoinHandle`s
    // and respawns them via the registered `restarter` closures (up to a
    // per-hour backstop), and drains `~/.continuum-dev/repair-intents/` so a
    // `repair_restart_component` intent for vision/audio/context_watcher
    // aborts the live task and respawns it. `supervisor_stats` is the clone
    // the health publisher reads; `supervisor` is moved into the run loop.
    let supervisor = Supervisor::new();
    let supervisor_stats = supervisor.clone();

    if observation_toggles.pause_all {
        emit_system_event(
            "toggle_change",
            "pause_all set in [privacy.toggles]; observation workers parked",
        );
    }
    {
        // Each sense watcher is registered with the runtime supervisor
        // instead of bare `tokio::spawn`. The supervisor reaps dead tasks and
        // respawns them via the `restarter` closures below, and drains
        // repair-restart intents — closing the self-healing loop (spec §7).
        //
        // Restarters capture clones of the shared state each task needs and
        // re-clone per invocation (they are `Fn`, callable many times), so a
        // respawn faithfully reconstructs the task without re-running one-shot
        // boot logic. Vision/audio have no shared health handle; context, git,
        // file and process publish one, so their health Arc is created here and
        // threaded into every (re)spawn via `with_health` — the published
        // snapshot then stays valid across restarts instead of orphaning on
        // the dead instance.

        // --- Vision --- (repair target "vision")
        {
            let config = config.clone();
            let resource_plan = resource_plan.clone();
            let live_context = live_context.clone();
            let privacy = privacy_filter.clone();
            let toggles = observation_toggles.clone();
            let toggle_control = toggle_control.clone();
            let cadence = cadence.clone();
            let vision_model_loaded = vision_model_loaded.clone();
            let screen_tx = screen_tx.clone();
            let shutdown = shutdown_rx.clone();
            let restarter: Box<dyn Fn() -> tokio::task::JoinHandle<()> + Send + Sync> =
                Box::new(move || {
                    // Clone every captured handle into locals for THIS
                    // invocation before `async move` takes them — the closure
                    // is `Fn` (callable many times), so it cannot move the
                    // shared captures into the spawned task directly.
                    let config = config.clone();
                    let resource_plan = resource_plan.clone();
                    let live_context = live_context.clone();
                    let privacy = privacy.clone();
                    let toggles = toggles.clone();
                    let toggle_control = toggle_control.clone();
                    let cadence = cadence.clone();
                    let vision_model_loaded = vision_model_loaded.clone();
                    let screen_tx = screen_tx.clone();
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        // Re-init the vision model on each (re)spawn: the prior
                        // Arc may belong to a dead task, and a fresh load is the
                        // faithful reconstruction. Runs in the task, so boot no
                        // longer blocks on vision model load.
                        let vision_model = init_vision_model(&config, &resource_plan).await;
                        vision_model_loaded
                            .store(vision_model.model_name() != "stub", Ordering::Release);
                        let vision_watcher = VisionWatcher::new_with_live_context(
                            config.screen.clone(),
                            vision_model,
                            PathBuf::from(&config.storage.screenshots_dir),
                            live_context,
                        )
                        .with_privacy(privacy, toggles)
                        .with_toggle_control(toggle_control)
                        .with_cadence(cadence);
                        vision_watcher.run(screen_tx, shutdown).await;
                        vision_model_loaded.store(false, Ordering::Release);
                    })
                });
            supervisor.register("vision", Some("vision"), restarter);
        }

        // --- Audio --- (repair target "audio")
        {
            let config = config.clone();
            let privacy = privacy_filter.clone();
            let toggles = observation_toggles.clone();
            let toggle_control = toggle_control.clone();
            let audio_tx = audio_tx.clone();
            let moshi_tap = moshi_tap.clone();
            let shutdown = shutdown_rx.clone();
            let restarter: Box<dyn Fn() -> tokio::task::JoinHandle<()> + Send + Sync> =
                Box::new(move || {
                    let config = config.clone();
                    let privacy = privacy.clone();
                    let toggles = toggles.clone();
                    let toggle_control = toggle_control.clone();
                    let audio_tx = audio_tx.clone();
                    let moshi_tap = moshi_tap.clone();
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        let audio_watcher = AudioWatcher::new(config.audio.clone())
                            .with_privacy(privacy, toggles)
                            .with_toggle_control(toggle_control);
                        audio_watcher.run(audio_tx, shutdown, moshi_tap).await;
                    })
                });
            supervisor.register("audio", Some("audio"), restarter);
        }

        // --- Context --- (repair target "context_watcher")
        {
            let health = SharedContextWatchHealth::default();
            context_watch_health = Some(health.clone());
            let config = config.clone();
            let privacy = privacy_filter.clone();
            let toggles = observation_toggles.clone();
            let toggle_control = toggle_control.clone();
            let project_handle = project_handle.clone();
            let event_sender = event_sender.clone();
            let live_context = live_context.clone();
            let ctx_tx = ctx_tx.clone();
            let shutdown = shutdown_rx.clone();
            let restarter: Box<dyn Fn() -> tokio::task::JoinHandle<()> + Send + Sync> =
                Box::new(move || {
                    let config = config.clone();
                    let privacy = privacy.clone();
                    let toggles = toggles.clone();
                    let toggle_control = toggle_control.clone();
                    let project_handle = project_handle.clone();
                    let event_sender = event_sender.clone();
                    let ctx_tx = ctx_tx.clone();
                    let shutdown = shutdown.clone();
                    let live_context = live_context.clone();
                    let health = health.clone();
                    tokio::spawn(async move {
                        let context_watcher = ContextWatcher::new(config.context.clone())
                            .with_privacy(privacy, toggles)
                            .with_toggle_control(toggle_control)
                            .with_project_handle(project_handle)
                            .with_event_sender(event_sender)
                            .with_health(health);
                        let _ = context_watcher
                            .run_with_live_context(ctx_tx, shutdown, live_context)
                            .await;
                    })
                });
            supervisor.register("context_watcher", Some("context_watcher"), restarter);
        }

        // --- Git collector (Task A5, spec §4.4) --- (auto-heal only; no
        // repair target). Watches the resolver's active confirmed project
        // only; parks in disabled-with-reason when git is absent or the
        // source is off.
        {
            let health = SharedGitWatchHealth::default();
            git_watch_health = Some(health.clone());
            let config = config.clone();
            let privacy = privacy_filter.clone();
            let toggles = observation_toggles.clone();
            let toggle_control = toggle_control.clone();
            let project_handle = project_handle.clone();
            let event_sender = event_sender.clone();
            let live_context = live_context.clone();
            let shutdown = shutdown_rx.clone();
            let restarter: Box<dyn Fn() -> tokio::task::JoinHandle<()> + Send + Sync> =
                Box::new(move || {
                    let config = config.clone();
                    let privacy = privacy.clone();
                    let toggles = toggles.clone();
                    let toggle_control = toggle_control.clone();
                    let project_handle = project_handle.clone();
                    let event_sender = event_sender.clone();
                    let shutdown = shutdown.clone();
                    let live_context = live_context.clone();
                    let health = health.clone();
                    tokio::spawn(async move {
                        let git_watcher = GitWatcher::new(config.git_context.clone())
                            .with_privacy(privacy, toggles)
                            .with_toggle_control(toggle_control)
                            .with_project_handle(project_handle)
                            .with_event_sender(event_sender)
                            .with_health(health);
                        git_watcher.run(shutdown, Some(live_context)).await;
                    })
                });
            supervisor.register("git", None, restarter);
        }

        // --- File watcher (Task A7, spec §4.5) --- opt-in, default OFF.
        // (auto-heal only; no repair target). Watches every
        // confirmed/configured project root whose zone allows; re-reads the
        // Projects table each rearm tick so projects confirmed at runtime get
        // watched without a restart. Parks in disabled-with-reason when
        // [file_watcher].enabled or the files toggle is off — no notify watch
        // is ever armed then.
        {
            let health = SharedFileWatchHealth::default();
            file_watch_health = Some(health.clone());
            let file_watch_log = raw_log.clone();
            let projects_provider: ProjectsProvider = Arc::new(move || {
                let raw_log = file_watch_log.clone();
                Box::pin(async move {
                    match raw_log.list_projects().await {
                        Ok(rows) => Some(rows.into_iter().map(|row| row.entry).collect()),
                        Err(e) => {
                            tracing::warn!(
                                layer = "senses",
                                component = "file_watch",
                                error = %e,
                                "Projects table read failed; keeping current watch set"
                            );
                            None
                        }
                    }
                })
            });
            let config = config.clone();
            let privacy = privacy_filter.clone();
            let toggles = observation_toggles.clone();
            let toggle_control = toggle_control.clone();
            let event_sender = event_sender.clone();
            let recent_file_handle = recent_file_handle.clone();
            let shutdown = shutdown_rx.clone();
            let restarter: Box<dyn Fn() -> tokio::task::JoinHandle<()> + Send + Sync> =
                Box::new(move || {
                    let config = config.clone();
                    let privacy = privacy.clone();
                    let toggles = toggles.clone();
                    let toggle_control = toggle_control.clone();
                    let projects_provider = projects_provider.clone();
                    let event_sender = event_sender.clone();
                    let recent_file_handle = recent_file_handle.clone();
                    let shutdown = shutdown.clone();
                    let health = health.clone();
                    tokio::spawn(async move {
                        let file_watcher = FileWatcher::new(config.file_watcher.clone())
                            .with_privacy(privacy, toggles)
                            .with_toggle_control(toggle_control)
                            .with_projects_provider(projects_provider)
                            .with_event_sender(event_sender)
                            .with_recent_file(recent_file_handle)
                            .with_health(health);
                        file_watcher.run(shutdown).await;
                    })
                });
            supervisor.register("file", None, restarter);
        }

        // --- Background-process collector --- opt-in, default OFF.
        // (auto-heal only; no repair target). Emits only configured lifecycle
        // events and sustained resource pressure; command lines, environment
        // and process memory are never read. A compact current snapshot backs
        // `context_processes`.
        {
            let health = SharedProcessWatchHealth::default();
            process_watch_health = Some(health.clone());
            let config = config.clone();
            let dev_dir = dev_dir.clone();
            let privacy = privacy_filter.clone();
            let toggle_control = toggle_control.clone();
            let event_sender = event_sender.clone();
            let shutdown = shutdown_rx.clone();
            let restarter: Box<dyn Fn() -> tokio::task::JoinHandle<()> + Send + Sync> =
                Box::new(move || {
                    let config = config.clone();
                    let dev_dir = dev_dir.clone();
                    let privacy = privacy.clone();
                    let toggle_control = toggle_control.clone();
                    let event_sender = event_sender.clone();
                    let shutdown = shutdown.clone();
                    let health = health.clone();
                    tokio::spawn(async move {
                        let process_watcher =
                            ProcessWatcher::new(config.process_watcher.clone(), dev_dir)
                                .with_privacy(privacy)
                                .with_toggle_control(toggle_control)
                                .with_event_sender(event_sender)
                                .with_health(health);
                        process_watcher.run(shutdown).await;
                    })
                });
            supervisor.register("process", None, restarter);
        }

        // --- Frame builder ---
        let frame_builder = PerceptionFrameBuilder::new(config.frame.clone());
        let builder_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            frame_builder
                .run(screen_rx, audio_rx, ctx_rx, frame_tx, builder_shutdown)
                .await;
        });
    }

    // Supervisor watch loop: reaps dead sense tasks + drains repair-restart
    // intents every `WATCH_TICK_SECS`. Runs until shutdown. The frame builder
    // above stays a plain spawn — its channel receivers are single-consumer
    // and cannot be reconstructed across a respawn, so it is not
    // supervisor-managed (a frame-builder death breaks perception
    // irrecoverably without rebuilding the channel topology).
    {
        let sup = supervisor;
        let sup_dev = dev_dir.clone();
        let sup_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            sup.run(sup_dev, sup_shutdown).await;
        });
    }

    // Expire timed privacy pauses even when the dashboard is closed.
    {
        let pause_dir = dev_dir.clone();
        let pause_config = config_path.clone();
        let pause_toggles = toggle_control.clone();
        let pause_hub = live_context.clone();
        let mut pause_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut was_paused = false;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        let currently_paused = pause_toggles.paused();
                        if currently_paused && !was_paused {
                            pause_hub.clear_observed_data();
                        }
                        was_paused = currently_paused;
                        let file_exists = continuum_core::privacy_pause::pause_path(&pause_dir).exists();
                        match continuum_core::privacy_pause::read_status(&pause_dir, Utc::now()) {
                            Ok(status) if status.paused && !pause_toggles.paused() => {
                                if let Err(error) = continuum_core::config_edit::set_toggle(
                                    &pause_config,
                                    ToggleName::PauseAll,
                                    true,
                                ) {
                                    tracing::error!(
                                        layer = "privacy",
                                        component = "observation_pause",
                                        error = %error,
                                        "Privacy pause could not be persisted; live observation still stopping"
                                    );
                                }
                                pause_toggles.set(ToggleName::PauseAll, true);
                                pause_hub.clear_observed_data();
                                emit_system_event(
                                    "toggle_change",
                                    "Durable privacy pause applied; all observation stopped",
                                );
                            }
                            Ok(status) if file_exists && !status.paused && pause_toggles.paused() => {
                                if let Err(error) = continuum_core::config_edit::set_toggle(
                                    &pause_config,
                                    ToggleName::PauseAll,
                                    false,
                                ) {
                                    tracing::warn!(
                                        layer = "privacy",
                                        component = "observation_pause",
                                        error = %error,
                                        "Timed pause expired but config could not be normalized"
                                    );
                                    continue;
                                }
                                pause_toggles.set(ToggleName::PauseAll, false);
                                let _ = continuum_core::privacy_pause::clear(&pause_dir);
                                emit_system_event(
                                    "toggle_change",
                                    "Timed privacy pause expired; observation resumed",
                                );
                            }
                            Err(error) => {
                                pause_toggles.set(ToggleName::PauseAll, true);
                                tracing::error!(
                                    layer = "privacy",
                                    component = "observation_pause",
                                    error = %error,
                                    "Privacy pause record became unreadable; failing closed"
                                );
                            }
                            _ => {}
                        }
                    }
                    _ = pause_shutdown.changed() => {
                        if *pause_shutdown.borrow() { break; }
                    }
                }
            }
        });
    }

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
        // Task B5 (spec §4.8): goal/task inference runs in its OWN spawned
        // task — never awaited by the frame loop. It goes through the
        // background LLM tier (Task B2: try-acquire behind interactive
        // triage, max_tokens clamped) and pauses entirely while idle
        // (spec §4.11), so its whole cost is "≤ 1 call / 2 min, ≤ 256
        // tokens, only when the user is actually here".
        session_state::spawn_inference_task(
            session_state.clone(),
            llm.clone(),
            config.session_state.clone(),
            cadence.clone(),
            shutdown_rx.clone(),
        );
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
    // Task 10 / Task B7 (spec §4.9): the rolling recent-frames ring. The
    // frame loop pushes every frame; every wake path snapshot-clones it
    // under a short guard (never held across an await — see [`FrameRing`]).
    //
    // Pre-B7 this was a loop-local `Vec` plus a separate `last_frame`
    // slot for the maintenance-wake ticker, which therefore had to pass
    // `&[]` as history. One shared ring replaces both: the ticker's
    // trigger frame is `latest()` and its history is
    // `snapshot_excluding(trigger.id)` — a maintenance wake now sees the
    // same "just before" the triage path does. It still never fabricates a
    // frame: an empty ring skips the wake.
    let recent_frames = FrameRing::new(wake_context::FRAME_RING_CAP);

    // Busy CAS + one-deep latest-wins slot for off-loop triage (Task B2;
    // mirrors the do_wake `try_claim_busy` pattern). Only the main loop's
    // thread touches the coalescer itself; its cloneable health handle is
    // read by the publish closure below (C1 stuck detection), which is why
    // it is constructed here rather than next to the loop.
    let triage_coalescer: TriageCoalescer<(PerceptionFrame, Option<CurrentProject>)> =
        TriageCoalescer::new();
    let triage_busy_health = triage_coalescer.health();

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
                vision_model_loaded: false,
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
                // (via `build_curator_snapshot` /
                // `build_context_engine_snapshot`); `None` here is never
                // actually published.
                curator: None,
                paused: None,
                context_engine: None,
                session_state: None,
                context_page: None,
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
        // Task A8 (spec §7): the runtime has no HealthRegistry — this
        // publish closure IS the context-engine health registration.
        // Every tick it reads the shared health handles and folds them
        // into `RuntimeSnapshot.context_engine` for the repair agent.
        let pause_for_publisher = toggle_control.clone();
        let health_cadence = cadence.clone();
        let health_hub = live_context.clone();
        let health_context = context_watch_health.clone();
        let health_git = git_watch_health.clone();
        let health_file = file_watch_health.clone();
        let health_process = process_watch_health.clone();
        // Supervisor liveness/restart counts — logged each tick at debug so a
        // death or restart-loop is observable in the runtime log (which the
        // repair agent tails). Per-task `alive`/`restarts` mirror what the
        // supervisor's watch loop is acting on.
        let health_supervisor = supervisor_stats.clone();
        let health_writer = event_writer_health.clone();
        let health_triage = triage_busy_health.clone();
        let vision_model_loaded_for_publisher = vision_model_loaded.clone();
        let triage_enabled = triage.is_some();
        let screen_capture_configured = config.screen.enabled;
        let context_poll_interval = Duration::from_secs(config.context.poll_interval_secs.max(1));
        // Task C1 (spec §4.8 consumers): publish the live session state on
        // every tick. Three consumers read this key straight out of
        // `state.json` — boot rehydration (B5), the desktop chat profile
        // (B8) and the `context_session` MCP tool (C3). The RAW state is
        // published, never `cloud_view()`: each consumer owns its own
        // cloud gate, and a local reader may see the real text (§4.1).
        let session_for_publisher = session_state.clone();
        // Task C5 (spec §4.13): the Context page's list data is refreshed
        // on its own slower ticker — four SQLite reads are far too heavy
        // for the 2 s publish tick — into a shared cell the publish
        // closure clones.
        let context_page: Arc<parking_lot::RwLock<ContextPageSnapshot>> =
            Arc::new(parking_lot::RwLock::new(ContextPageSnapshot::default()));
        {
            let cell = context_page.clone();
            let sources = ContextPageSources {
                raw_log: raw_log.clone(),
                session: session_state.clone(),
                project_handle: project_handle.clone(),
                toggles: toggle_control.clone(),
                privacy: privacy_filter.clone(),
                continuation: config.continuation.clone(),
            };
            let mut shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                let mut ticker =
                    tokio::time::interval(Duration::from_secs(CONTEXT_PAGE_REFRESH_SECS));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            let snapshot = build_context_page_snapshot(&sources).await;
                            *cell.write() = snapshot;
                        }
                        _ = shutdown.changed() => {
                            if *shutdown.borrow() {
                                break;
                            }
                        }
                    }
                }
            });
        }
        let context_page_for_publisher = context_page.clone();
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
                snap.vision_model_loaded =
                    vision_model_loaded_for_publisher.load(Ordering::Acquire);
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
                // Supervisor liveness snapshot — debug-level so it doesn't
                // spam the log every tick, but a death/restart-loop surfaces
                // here and in the supervisor's own warn lines (which the
                // repair agent's log tail reads).
                let sup_stats = health_supervisor.stats();
                if sup_stats.iter().any(|s| !s.alive || s.restarts > 0) {
                    tracing::debug!(
                        layer = "system",
                        component = "supervisor",
                        stats = ?sup_stats,
                        "supervisor snapshot"
                    );
                }
                let paused = pause_for_publisher.paused();
                snap.paused = Some(paused);
                snap.session_state = Some(session_for_publisher.snapshot());
                snap.context_page = Some(context_page_for_publisher.read().clone());
                snap.context_engine = Some(build_context_engine_snapshot(
                    &health_cadence,
                    &health_hub,
                    health_context.as_ref(),
                    health_git.as_ref(),
                    health_file.as_ref(),
                    health_process.as_ref(),
                    &health_writer,
                    &health_triage,
                    triage_enabled,
                    screen_capture_configured && !paused,
                    context_poll_interval,
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
        recent_frames.clone(),
        WakeSources {
            raw_log: raw_log.clone(),
            package: config.context_package.clone(),
            continuation: config.continuation.clone(),
        },
        followup_until.clone(),
        config.voice.conversation_followup_seconds,
        config.voice.ambient_mute_enabled,
        orchestrator_busy.clone(),
        runtime_state.clone(),
        project_handle.clone(),
        idle_controller.clone(),
        cadence.clone(),
        live_context.clone(),
        session_state.clone(),
        config.session_state.confidence_floor,
        audit.clone(),
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

    // --- Task B2 (spec §4.7): off-loop triage plumbing ---
    // Decision consumption runs from two select arms (voice decisions in
    // the frame arm, off-loop triage results in the triage arm), so its
    // shared dependencies are bundled once here for `handle_triage_decision`
    // instead of being cloned at two call sites.
    // --- Task B3 (spec §4.7 consumption): classification consumer ---
    // Turns each triage output's classification into a context event, a
    // vault memory candidate, and the frame's `triage_decision` column.
    // `known_projects` is the resolver's id set: a classifier-emitted
    // project outside it is dropped to the resolver's value (spec §4.6).
    let classification_consumer = ClassificationConsumer::new(
        event_sender.clone(),
        vault.clone(),
        raw_log.clone(),
        privacy_filter.clone(),
        config.memory.candidate_ttl_days,
        project_resolver
            .projects()
            .iter()
            .map(|p| p.id.clone())
            .collect(),
        // The candidate gate follows the events writer's collapse window:
        // one memory candidate per collapsed row, not one per occurrence.
        config.events.collapse_window_minutes,
    );
    // Task B7 (spec §4.9): the runtime-full packager's own sources —
    // deduped `context_events` for the just-before/changes/failures
    // sections, plus the `[context_package]` budget + caps.
    let wake_sources = WakeSources {
        raw_log: raw_log.clone(),
        package: config.context_package.clone(),
        continuation: config.continuation.clone(),
    };
    let decision_ctx = DecisionCtx {
        classification: classification_consumer,
        frames: recent_frames.clone(),
        wake_sources: wake_sources.clone(),
        orchestrator_busy: orchestrator_busy.clone(),
        idle_controller: idle_controller.clone(),
        cadence: cadence.clone(),
        live_context: live_context.clone(),
        speech: speech.clone(),
        feedback: feedback.clone(),
        followup_until: followup_until.clone(),
        followup_secs: config.voice.conversation_followup_seconds,
        ambient_mute_enabled: config.voice.ambient_mute_enabled,
        orch_config: orch_config.clone(),
        semantic: semantic.clone(),
        episodic: episodic.clone(),
        vault: vault.clone(),
        curator_cfg: config.memory.curator.clone(),
        runtime_state: runtime_state.clone(),
        skill_loader: skill_loader.clone(),
        skill_token_budget: config.skills.token_budget,
        dev_dir: dev_dir.clone(),
        session_state: session_state.clone(),
        session_confidence_floor: config.session_state.confidence_floor,
        audit: audit.clone(),
        #[cfg(feature = "moshi")]
        moshi_frontend: moshi_frontend.clone(),
        shutdown_rx: shutdown_rx.clone(),
    };
    // Spawned evaluations send their TriageOutput back into the loop here.
    // Unbounded is safe: the coalescer admits at most one evaluation at a
    // time, so the channel never holds more than one message in practice
    // (plus, at most, one fallback from a dead evaluation task — C1).
    let (triage_result_tx, mut triage_result_rx) = mpsc::unbounded_channel::<TriageEvalResult>();
    // Frame ids whose in-flight/parked triage evaluation was superseded by
    // a voice or forced wake on the same frame — their results are dropped
    // on arrival, preserving the pre-B2 `.or_else` discard semantics.
    // Bounded by construction: at most one in-flight + one parked frame
    // can hold live entries, and every path removes what it consumes.
    let mut voice_superseded: Vec<uuid::Uuid> = Vec::new();

    // --- Main loop ---
    // Task B2 invariant (spec §4.7): NO arm of this select loop ever
    // awaits LLM work. Triage evaluations are tokio::spawn'ed (the frame
    // arm submits, the triage arm consumes their results) and orchestrator
    // wakes are spawned tasks, so the 250 ms voice-intent ticker and the
    // hotkey arm are always serviced within a scheduler tick, whatever the
    // GPU is doing. Keep it that way: any new arm that needs the local
    // model must go through the same spawn-and-channel pattern.
    let mut frame_count: u64 = 0;
    let mut main_shutdown = shutdown_rx.clone();
    let wake_detector = TranscriptWakeDetector::new(config.voice.wake_keyword.clone());
    let mut voice_session: Option<VoiceSession> = None;
    let mut hotkey_pending: bool = false;
    // Task C5 (spec §4.13): Context-page intents are drained on the same
    // 250 ms ticker as the dashboard's push-to-talk intents. Both are
    // dashboard→runtime filesystem messages, and neither may block the
    // loop: intent handlers touch SQLite/LanceDB/the vault, never the LLM.
    let mut context_intents = IntentDrainer::new();
    let mut project_pin_guard = ProjectPinGuard::new();
    if let Err(e) = context_intent::ensure_intents_dir(&dev_dir) {
        tracing::warn!(
            layer = "context",
            component = "intent",
            error = %e,
            "Failed to create context-intents dir; Context page actions will be ignored"
        );
    }
    // I1: deletion intents run on this single worker task instead of the
    // select loop. One task (not one per intent) keeps them strictly in
    // drain order — a `forget` queued before a `delete_range` still
    // applies first — while the loop stays free to service voice. The
    // worker ends when the loop drops the sender at shutdown.
    let (deferred_intent_tx, mut deferred_intent_rx) =
        mpsc::unbounded_channel::<continuum_core::context::intents::ContextIntent>();
    {
        let deferred_ctx = DeferredIntentContext {
            raw_log: raw_log.clone(),
            episodic: episodic.clone(),
            vault: vault.clone(),
            audit: audit.clone(),
            screenshot_policy: ScreenshotPolicy::from(&config.storage),
        };
        tokio::spawn(async move {
            while let Some(intent) = deferred_intent_rx.recv().await {
                let _ = apply_deferred_intent(&deferred_ctx, &intent).await;
            }
            tracing::debug!(
                layer = "context",
                component = "intent",
                "Deferred intent worker stopped"
            );
        });
    }
    // Boot rehydration of the persisted pins (spec §4.13 "all overrides and
    // pins survive restart"). Override rules were already loaded into the
    // resolver above; pins live on the session state.
    match raw_log.list_session_pins().await {
        Ok(pins) => {
            for (field, value, _project) in pins {
                if let Some(field) = SessionField::parse(&field) {
                    session_state.set_pin(field, value.as_deref(), chrono::Utc::now());
                }
            }
        }
        Err(e) => tracing::warn!(
            layer = "context",
            component = "continuum",
            error = %e,
            "Failed to load session pins; starting unpinned"
        ),
    }

    loop {
        tokio::select! {
            Some(frame) = frame_rx.recv() => {
                // A frame admitted just before the user's click may still be
                // buffered. Drop it at the central consumer while paused so
                // it cannot reach persistence, triage, or an agent.
                if toggle_control.paused() {
                    continue;
                }
                frame_count += 1;
                // M2: read the four counters through the cheap accessors
                // BEFORE taking the `runtime_state` mutex. `snapshot()`
                // cloned every monitor, the whole event ring and the
                // session state — under a mutex the publisher also takes —
                // to fill four numbers.
                let capture_health = live_context.health();
                let monitor_count = live_context.monitor_count();
                if let Ok(mut s) = runtime_state.lock() {
                    s.frame_count = frame_count;
                    s.monitor_count = monitor_count;
                    s.capture_event_count = capture_health.capture_events;
                    s.dropped_capture_event_count = capture_health.dropped_capture_events;
                    s.last_capture_at = capture_health.last_capture_at;
                    let ambient_active = config.voice.ambient_mute_enabled && frame.context.in_call;
                    s.ambient_mute_active = Some(ambient_active);
                    s.detected_call_app = ambient_active
                        .then(|| frame.context.foreground_process_name.clone());
                }

                // Idle controller (Task A8, spec §4.11): mechanical — the
                // context watcher's idle_seconds either crosses the
                // threshold (enter idle: relaxed cadences via the shared
                // CadenceControl) or drops on input activity (exit).
                // Voice wake / hotkey / do_wake restores are handled at
                // their own trigger sites below.
                {
                    let transition = {
                        let mut ctrl = idle_controller
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        ctrl.on_frame(frame.context.idle_seconds, &cadence)
                    };
                    apply_idle_transition(transition, &live_context);
                }

                // Project resolver (Task A4, spec §4.3): resolve once per
                // frame; every consumer below receives this single result.
                // The file-event path (Task A7): the file watcher's most
                // recent non-ignored event path activates tier-3 git-root
                // resolution; None (watcher off/idle) skips tier 3.
                let recent_file = recent_file_handle.read().clone();
                let project_outcome = project_resolver.observe(&FrameInput {
                    process_name: &frame.context.foreground_process_name,
                    window_title: &frame.context.foreground_window_title,
                    recent_file_path: recent_file.as_deref(),
                    ts: frame.ts,
                });
                if let Some(switch) = &project_outcome.switched {
                    // Events channel (Task A6): project_switch is emitted
                    // pre-stamped with the new project id. Unattributed
                    // producers are stamped by the events-writer from the
                    // shared project handle — no per-frame buffer pass.
                    event_sender.send(project_switch_event(
                        switch.from.as_deref(),
                        &switch.to,
                        &frame.context.foreground_process_name,
                        &frame.context.foreground_window_title,
                        // Fixwave 3b (minor): the destination project's
                        // zone decides the event's sensitivity, exactly as
                        // it does for the git and file collectors.
                        project_outcome.current.as_ref().and_then(|p| p.zone),
                        switch.ts,
                    ));
                }
                // I2: the per-frame SQLite writes below are fire-and-forget
                // bookkeeping — nothing in this arm reads their result.
                // Awaiting them inline put the whole select loop (voice
                // ticker, hotkey) behind a pool that the events writer
                // holds across `BEGIN IMMEDIATE` batches and hourly
                // rotation. They are spawned instead; the pool's 250 ms
                // busy_timeout keeps a spawned write from queueing behind
                // a lock for long, and failures are logged exactly as
                // before.
                if let Some(current) = &project_outcome.current {
                    let raw_log = raw_log.clone();
                    let project_id = current.id.clone();
                    let ts = frame.ts;
                    tokio::spawn(async move {
                        if let Err(e) = raw_log.bump_project_stats(&project_id, ts).await {
                            tracing::debug!(
                                layer = "context",
                                component = "continuum",
                                error = %e,
                                "project stats bump failed"
                            );
                        }
                    });
                }
                for candidate in &project_outcome.discovered {
                    // Persist the proposal row (status discovered). Never
                    // collected from until confirmed (spec §4.3).
                    let entry = ProjectEntry {
                        id: candidate.id.clone(),
                        name: candidate.name.clone(),
                        root_paths: vec![candidate.root_path.clone()],
                        repo: None,
                        keywords: Vec::new(),
                        zone: None,
                        status: ProjectStatus::Discovered,
                    };
                    let raw_log = raw_log.clone();
                    let candidate_id = candidate.id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = raw_log.upsert_project(&entry).await {
                            tracing::warn!(
                                layer = "context",
                                component = "continuum",
                                id = %candidate_id,
                                error = %e,
                                "failed to persist discovered project candidate"
                            );
                        }
                    });
                }
                let current_project = project_outcome.current.clone();

                // Task B5 (spec §4.8): mechanical session-state update.
                // Synchronous, lock-scoped, no I/O — project, app, window
                // title and a best-effort open-file guess from the editor
                // title. A project change here clears the inferred
                // goal/task and arms the inference trigger.
                session_state.apply_frame(&frame, current_project.as_ref());

                // Task C5 (spec §4.13): a pinned project expires once the
                // resolver has confidently disagreed for `switch_min_secs`
                // — the rule that keeps a pin from deadlocking against
                // reality. The pin never blocks resolution; only the
                // session-state field it froze.
                {
                    let pinned_project = session_state
                        .snapshot()
                        .pinned
                        .iter()
                        .any(|f| f == SessionField::Project.as_str())
                        .then(|| session_state.snapshot().active_project.clone())
                        .flatten();
                    if let Some(pinned) = pinned_project {
                        let resolved = current_project
                            .as_ref()
                            .map(|p| (p.id.as_str(), p.confidence));
                        if project_pin_guard.observe(
                            &pinned,
                            resolved,
                            chrono::Duration::seconds(config.projects.switch_min_secs as i64),
                            frame.ts,
                        ) {
                            tracing::info!(
                                layer = "context",
                                component = "continuum",
                                pinned = %pinned,
                                "Clearing the project pin — the resolver has disagreed for switch_min_secs"
                            );
                            session_state.set_pin(SessionField::Project, None, frame.ts);
                            // I2: the live pin is already cleared above;
                            // persisting that is best-effort and must not
                            // hold the loop (see the comment on the stats
                            // bump).
                            {
                                let raw_log = raw_log.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = raw_log
                                        .clear_session_pin(SessionField::Project.as_str())
                                        .await
                                    {
                                        tracing::warn!(
                                            layer = "context",
                                            component = "continuum",
                                            error = %e,
                                            "Failed to clear the persisted project pin"
                                        );
                                    }
                                });
                            }
                            audit.record(
                                "correction",
                                Actor::Agent,
                                format!(
                                    "Cleared the project pin \"{pinned}\" — the resolver \
                                     disagreed for {}s",
                                    config.projects.switch_min_secs
                                ),
                                None,
                            );
                        }
                    } else {
                        // Fixwave 3b (I4): no pin this frame — forget any
                        // divergence still being timed. Without this a
                        // brand-new pin inherited a stale start timestamp
                        // and could be cleared on its very first frame.
                        project_pin_guard.reset();
                    }
                }

                // Task C1: mirror the (just-updated) session state into the
                // live-context hub so `live-context.json` and every
                // in-process `compact_for_agents` blob carry it too. Taking
                // the whole snapshot here also picks up event- and
                // inference-driven changes since the previous frame; the
                // hub bumps its content version only when something other
                // than `updated` actually changed, so a static session
                // causes no extra publish.
                live_context.record_session_state(session_state.snapshot());

                // Curator (Plan B): publish the latest activity signal so the
                // curator's project-aware context (Task 9) has fresh data
                // between ticks. Send is a no-op if the curator never spawned
                // (no triage model) — nothing is listening, that's fine.
                //
                // Fixwave 3b (C4): the hint is the resolver's
                // post-hysteresis output or nothing. It used to fall back to
                // the legacy frame-only keyword hint, which wrote durable,
                // unvalidated project attribution into the vault — see
                // `ActivitySignal::project_hint`.
                let _ = activity_tx.send(curator::run::activity_signal(
                    &frame,
                    current_project.as_ref(),
                ));

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
                // Idle triage gate (Task A8, spec §4.11): while idle,
                // triage runs ONLY for frames with a visible error or
                // audio — the same mechanical inputs the salience gate
                // already uses, implemented as this skip predicate. No
                // triage-layer code changes.
                let idle_triage_gated =
                    cadence.is_idle() && !has_audio && !frame.screen.has_error_visible;
                let skip_triage = idle_triage_gated
                    || (frame.salience_hint < config.frame.salience_threshold
                        && !has_audio
                        && !frame.screen.has_error_visible);

                // Task B2 (spec §4.7): triage runs OFF the main loop. A
                // gated frame goes to the coalescer: if no evaluation is in
                // flight, one is tokio::spawn'ed for it (mirroring the
                // do_wake CAS pattern) and its `TriageOutput` returns
                // through the `triage_result_rx` select arm below; if one
                // IS in flight, the frame parks in a one-deep latest-wins
                // slot, so a burst of gated frames coalesces to the newest.
                // This arm never awaits the LLM.
                let mut inline_decision: Option<TriageDecision> = None;
                let mut submitted_for_triage = false;
                if let Some(ref triage_layer) = triage {
                    if skip_triage {
                        tracing::trace!(
                            layer = "triage",
                            component = "continuum",
                            frame_id = %frame.id,
                            salience = frame.salience_hint,
                            "Skipped triage — low-salience idle frame"
                        );
                        inline_decision = Some(TriageDecision::Ignore);
                    } else {
                        match triage_coalescer.submit((frame.clone(), current_project.clone())) {
                            Submitted::Evaluate((eval_frame, eval_project)) => {
                                spawn_triage_eval(
                                    triage_layer.clone(),
                                    eval_frame,
                                    eval_project,
                                    session_state.clone(),
                                    config.session_state.confidence_floor,
                                    triage_result_tx.clone(),
                                );
                            }
                            Submitted::Coalesced { replaced } => {
                                if let Some((old, _)) = replaced {
                                    // The displaced frame will never produce
                                    // a triage result — drop any supersede
                                    // bookkeeping keyed on it.
                                    voice_superseded.retain(|id| *id != old.id);
                                }
                                tracing::trace!(
                                    layer = "triage",
                                    component = "continuum",
                                    frame_id = %frame.id,
                                    "Triage busy — frame parked (latest-wins)"
                                );
                            }
                        }
                        submitted_for_triage = true;
                    }
                } else {
                    println!(
                        "[{ts}] {app} | \"{desc}\" | sal={sal:.2}",
                        app = frame.context.foreground_process_name,
                        desc = truncate(&frame.screen.description, 50),
                        sal = frame.salience_hint,
                    );
                }

                // Write to raw log. Privacy contract (spec §4.1): every
                // free-text field in the frame — window title, vision
                // captions inside the compact description, audio
                // transcript — was scrubbed at collector emit, and
                // never_observe windows arrive as the sentinel
                // observation, so the raw log never stores unfiltered
                // content by construction.
                //
                // I2: spawned, not awaited. Frames are independent rows
                // keyed on their own `ts`, and every reader orders by `ts`,
                // so a best-effort out-of-order INSERT is harmless — while
                // an inline await parked the voice ticker behind whatever
                // the events writer was holding.
                {
                    let raw_log = raw_log.clone();
                    let frame_for_log = frame.clone();
                    tokio::spawn(async move {
                        if let Err(e) = raw_log.write_frame(&frame_for_log).await {
                            tracing::error!(
                                layer = "senses",
                                component = "continuum",
                                error = %e,
                                "Raw log write failed"
                            );
                        }
                    });
                }

                // Keep recent frames for wake context (Task B7: the shared
                // ring, also read by the maintenance-wake ticker).
                recent_frames.push(frame.clone());

                let session_was_open = voice_session.is_some();
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
                        &session_state,
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
                    &session_state,
                );
                // Idle restore trigger (Task A8, spec §4.11): a voice
                // session just opened — wake phrase, hotkey-armed
                // transcript, or follow-up window — so the user is
                // audibly present even though idle_seconds may still be
                // high (speaking produces no keyboard/mouse input).
                if !session_was_open && voice_session.is_some() {
                    idle_wake_restore(&idle_controller, &cadence, &live_context);
                }
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
                    // Voice intents outrank triage (pre-B2 shape:
                    // `voice_decision.or_else(|| decision)`; triage results
                    // now arrive asynchronously in the triage arm below, so
                    // only the skip-path Ignore is still inline here).
                    voice_decision.or(inline_decision)
                };

                // Handle voice / forced / skip-path decisions. Task B2
                // precedence preservation: pre-B2, `.or_else` discarded the
                // same-frame triage decision whenever a voice decision (or
                // the forced wake) won — so a frame whose evaluation is
                // still in flight or parked is marked superseded here and
                // its off-loop result is dropped on arrival in the triage
                // arm below.
                if let Some(ref decision) = effective_decision {
                    if submitted_for_triage {
                        voice_superseded.push(frame.id);
                    }
                    // No classification on this path: voice/forced/skip
                    // decisions never came from the model (Task B3).
                    handle_triage_decision(
                        decision,
                        &frame,
                        current_project.as_ref(),
                        None,
                        &decision_ctx,
                    );
                }
            }
            Some(eval) = triage_result_rx.recv() => {
                // Task B2 (spec §4.7): off-loop triage result arrives.
                // Decision consumption is identical in order and semantics
                // to the pre-B2 inline path — the frame's own log line
                // prints, then the decision routes through the same
                // handler the voice path uses.
                let TriageEvalResult { frame_id, outcome } = eval;
                let Some(TriageEvalOutcome {
                    frame: eval_frame,
                    current_project: eval_project,
                    output,
                    latency_ms,
                }) = outcome
                else {
                    // C1: the evaluation task died (panic / cancellation)
                    // and its drop guard reported instead. Nothing to
                    // consume — drop this frame's bookkeeping and fall
                    // through to the chain below, which releases the
                    // coalescer (or starts the parked frame).
                    voice_superseded.retain(|id| *id != frame_id);
                    tracing::warn!(
                        layer = "triage",
                        component = "continuum",
                        frame_id = %frame_id,
                        "Triage evaluation produced no result; frame dropped"
                    );
                    chain_next_triage_eval(
                        &triage_coalescer,
                        &mut voice_superseded,
                        triage.as_ref(),
                        &session_state,
                        config.session_state.confidence_floor,
                        &triage_result_tx,
                    );
                    continue;
                };
                if let Some(pos) = voice_superseded.iter().position(|id| *id == eval_frame.id) {
                    voice_superseded.remove(pos);
                    tracing::debug!(
                        layer = "triage",
                        component = "continuum",
                        frame_id = %eval_frame.id,
                        decision = output.decision.variant_name(),
                        "Dropping triage result — superseded by a voice/forced wake on the same frame"
                    );
                } else {
                    tracing::debug!(
                        layer = "triage",
                        component = "continuum",
                        decision = output.decision.variant_name(),
                        classification = output.classification.is_some(),
                        latency_ms = latency_ms,
                        "Triage decision"
                    );

                    // Task B3 (spec §4.7): the classification rides into
                    // `handle_triage_decision`, which consumes it (event +
                    // vault candidate + `triage_decision` column) before
                    // acting on the decision. Superseded results return
                    // above without consuming — pre-B2 semantics dropped
                    // the whole result, and one lost observation beats
                    // double-recording a frame the voice path already
                    // handled.
                    let d = output.decision;
                    let classification = output.classification;

                    let audio_text = eval_frame
                        .audio
                        .as_ref()
                        .map(|a| a.transcript.as_str())
                        .unwrap_or("");
                    println!(
                        "[{ts}] {app} | \"{desc}\" | audio=\"{audio}\" | triage={decision}",
                        ts = eval_frame.ts.format("%H:%M:%S"),
                        app = eval_frame.context.foreground_process_name,
                        desc = truncate(&eval_frame.screen.description, 50),
                        audio = truncate(audio_text, 30),
                        decision = d.variant_name(),
                    );

                    handle_triage_decision(
                        &d,
                        &eval_frame,
                        eval_project.as_ref(),
                        classification.as_ref(),
                        &decision_ctx,
                    );
                }

                chain_next_triage_eval(
                    &triage_coalescer,
                    &mut voice_superseded,
                    triage.as_ref(),
                    &session_state,
                    config.session_state.confidence_floor,
                    &triage_result_tx,
                );
            }
            Some(()) = recv_hotkey(&mut hotkey_rx) => {
                tracing::info!(
                    layer = "voice",
                    component = "hotkey",
                    "Hotkey pressed — next transcript opens a session"
                );
                hotkey_pending = true;
                feedback.play(FeedbackCue::Listen);
                // Idle restore trigger (Task A8, spec §4.11): hotkey.
                idle_wake_restore(&idle_controller, &cadence, &live_context);
            }
            _ = voice_intent_tick.tick() => {
                let was_pending = hotkey_pending;
                drain_voice_intents_tick(&dev_dir, &mut hotkey_pending, &feedback);
                // Idle restore trigger (Task A8, spec §4.11): dashboard
                // push-to-talk equals a hotkey press. Only a *newly*
                // drained intent restores — an unconsumed pending flag
                // from an earlier tick must not re-exit idle every 250 ms.
                if hotkey_pending && !was_pending {
                    idle_wake_restore(&idle_controller, &cadence, &live_context);
                }

                // Task C5 (spec §4.13): drain Context-page intents on the
                // same tick. Each handler is contained — it logs and moves
                // on rather than propagating — so one bad project id can
                // never stop the loop or the intents behind it.
                let drained = context_intents.drain(&dev_dir);
                if !drained.is_empty() {
                    let mut intent_ctx = IntentContext {
                        raw_log: &raw_log,
                        episodic: &episodic,
                        vault: &vault,
                        session: &session_state,
                        resolver: &mut project_resolver,
                        toggles: &toggle_control,
                        audit: &audit,
                        config_path: config_path.clone(),
                        screenshot_policy: ScreenshotPolicy::from(&config.storage),
                    };
                    for intent in &drained {
                        // I1: deletion intents (forget / delete_range)
                        // await LanceDB, two SQLite DELETEs, a screenshot
                        // remove_file per row and a full vault.pending()
                        // scan. Applied here they would freeze
                        // push-to-talk and the hotkey for the length of a
                        // "delete the last hour" click, so they go to the
                        // serial deferred worker instead — which preserves
                        // their order relative to each other. Everything
                        // else is small, touches the loop-owned resolver,
                        // and stays inline.
                        if is_deferred(&intent.action) {
                            if deferred_intent_tx.send(intent.clone()).is_err() {
                                tracing::error!(
                                    layer = "context",
                                    component = "intent",
                                    intent_id = %intent.id,
                                    "Deferred intent worker is gone; deletion dropped"
                                );
                            }
                            continue;
                        }
                        let _ = apply_intent(&mut intent_ctx, intent).await;
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
        component = "continuum",
        frames = frame_count,
        "Shutting down..."
    );

    // I5: the events writer flushes its pending batch (up to 256 rows)
    // when it sees the shutdown signal, and that flush needs a pooled
    // connection. Closing the raw-log pool first made the flush fail and
    // drop the batch silently on every clean exit — so wait for the writer
    // task to actually stop, then close.
    if !event_writer_health
        .wait_for_stop(EVENT_WRITER_SHUTDOWN_TIMEOUT)
        .await
    {
        tracing::warn!(
            layer = "memory",
            component = "continuum",
            "Events writer did not confirm its final flush before shutdown"
        );
    }

    raw_log.close().await;
    semantic.close().await;

    tracing::info!(
        layer = "system",
        component = "continuum",
        "Continuum stopped cleanly"
    );
    Ok(())
}

/// The packager's runtime-only sources (Task B7, spec §4.9).
///
/// Everything the ungated [`ContextPackage`] assembler needs that lives in
/// this process: the raw log (for deduped `context_events`) and the
/// `[context_package]` budget + caps. Cheap to clone — `RawLog` is a
/// pooled handle and the config is a handful of numbers.
#[derive(Clone)]
struct WakeSources {
    raw_log: RawLog,
    package: ContextPackageConfig,
    /// Task B8 (spec §4.12): the continuation resolver's floor + trigger
    /// phrases. Only read on continue-class wakes.
    continuation: ContinuationConfig,
}

/// Reads the deduped `context_events` rows the packager renders as "just
/// before" / "recent changes" / "failed attempts" / "last success"
/// (Task B6 seam, spec §4.9).
///
/// **Never fails the wake**: any DB error logs and yields an empty slice,
/// which makes the packager fall back to raw history frames.
async fn read_package_events(sources: &WakeSources) -> Vec<ContextEventRow> {
    let window = chrono::Duration::minutes(sources.package.events_window_minutes.max(1));
    // Over-fetch: the split routes one row into several sections, so the
    // per-section caps need more candidates than any single cap.
    let limit = (sources.package.max_just_before
        + sources.package.max_recent_changes
        + sources.package.max_failed_attempts)
        .max(20)
        * 2;
    match sources
        .raw_log
        .recent_context_events(Utc::now() - window, limit)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                layer = "context",
                component = "packager",
                error = %e,
                "context_events read failed; wake continues without event sections"
            );
            Vec::new()
        }
    }
}

/// Reads the most recent `wake_result` event's next step (Task B7's
/// post-wake record), one of the continuation resolver's five candidate
/// producers (spec §4.12).
///
/// Looks back `[continuation] wake_result_lookback_hours` — deliberately
/// much further than the packager's event window: "ga door" after lunch
/// still has to find this morning's next step. **Never fails the wake**: a
/// DB error logs and yields `None`, which simply removes one candidate.
async fn read_last_wake_next_step(sources: &WakeSources) -> Option<StampedText> {
    let window = chrono::Duration::hours(sources.continuation.wake_result_lookback_hours.max(1));
    let rows = match sources
        .raw_log
        .recent_context_events(Utc::now() - window, 500)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                layer = "context",
                component = "continuation",
                error = %e,
                "wake_result read failed; that candidate is skipped"
            );
            return None;
        }
    };
    // `recent_context_events` returns oldest-first, so the last matching
    // row is the newest wake_result.
    rows.iter()
        .rev()
        .find(|row| row.event_type == EventType::WakeResult && !row.summary.trim().is_empty())
        .map(|row| StampedText::new(row.summary.clone(), row.ts_last))
}

/// Reads the `open_task:` trailer off the **last** curator session summary
/// (spec §4.9 structured trailer → §4.12 candidate).
///
/// Task B7 guarantees every session note body ends with exactly one
/// well-formed `open_task:` line, so this is a plain read — no LLM call.
/// Only the newest session note is consulted: "the open task from the last
/// session" is exactly that, and a `none` trailer genuinely means the
/// session closed with nothing open.
///
/// **Never fails the wake**: any vault error logs and yields `None`.
async fn read_last_open_task(vault: &continuum_memory::Vault) -> Option<StampedText> {
    // Fixwave 3b (I6): a targeted "newest session note" query. This used
    // to page `vault.graph()`, which orders by `importance DESC, id ASC`
    // and caps at 50 — and every curator session note carries the same
    // `importance: 0.5`, so the page was simply the 50 lowest-id notes.
    // Past ~50 sessions the newest note was never in it and a months-old
    // open task scored 0.64 (over the 0.6 floor) forever.
    let newest = match vault
        .newest_node(
            continuum_memory::NodeType::Session,
            continuum_memory::NodeStatus::Confirmed,
        )
        .await
    {
        Ok(Some(node)) => node,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                layer = "context",
                component = "continuation",
                error = %e.user_message(),
                "session-summary read failed; open_task candidate is skipped"
            );
            return None;
        }
    };
    let note = vault.get(&newest.id).await.ok()?;
    let open_task = continuum_core::context::package::read_open_task(&note.body)?;
    let at = note.frontmatter.updated.unwrap_or(note.frontmatter.created);
    Some(StampedText::new(open_task, at))
}

/// Runs the continuation resolver for a continue-class wake (spec §4.12).
///
/// Returns `None` for every ordinary wake — the trigger check is the first
/// thing that happens, so a normal wake pays nothing for this feature (no
/// extra DB read, no extra vault read).
///
/// Continue-class means the user's own words matched
/// `[continuation] trigger_phrases`, or there were no words at all (a bare
/// hotkey press / empty ask, which spec §4.12 counts as a trigger).
async fn resolve_continuation(
    reason: &str,
    session: Option<&SessionState>,
    sources: &WakeSources,
    vault: &continuum_memory::Vault,
) -> Option<ContinuationOutcome> {
    let cfg = &sources.continuation;
    let utterance = continuation::utterance_from_wake_reason(reason);
    if !continuation::is_continue_class(Some(utterance), cfg) {
        return None;
    }

    let inputs = session
        .map(ContinuationInputs::from_session)
        .unwrap_or_default()
        .with_wake_next_step(read_last_wake_next_step(sources).await)
        .with_open_task(read_last_open_task(vault).await);
    let outcome = continuation::rank(&inputs, Utc::now(), cfg);
    tracing::info!(
        layer = "context",
        component = "continuation",
        outcome = outcome.variant_name(),
        candidate = outcome.best().map(|c| c.text.as_str()).unwrap_or(""),
        confidence = outcome.best().map(|c| c.confidence).unwrap_or(0.0),
        "Continue-class wake resolved"
    );
    Some(outcome)
}

/// Turns a resolver outcome into the wake reason + the packager's
/// "## Recommended next step" section (spec §4.12 routing).
///
/// `Recommend` enriches the reason with `Continue: <text>` and fills the
/// section; `Ask` enriches the reason with a one-question disambiguation
/// instruction and fills nothing (there is no confident next step to
/// recommend); `Nothing`/not-continue-class leaves today's behavior.
fn apply_continuation(
    reason: &str,
    outcome: Option<&ContinuationOutcome>,
) -> (String, Option<NextStep>) {
    match outcome {
        Some(ContinuationOutcome::Recommend(candidate)) => (
            continuation::recommend_reason(reason, candidate),
            Some(NextStep {
                text: candidate.text.clone(),
                confidence: candidate.confidence,
                // Fixwave 2 (C1): the resolver ranks the *raw* session
                // task, so the zone rides along and the packager drops
                // this section at the cloud egress point.
                local_only: candidate.local_only,
            }),
        ),
        Some(ContinuationOutcome::Ask(candidates)) => (
            continuation::ask_reason(
                reason,
                &candidates[..candidates.len().min(MAX_ASK_CANDIDATES)],
            ),
            None,
        ),
        Some(ContinuationOutcome::Nothing) | None => (reason.to_string(), None),
    }
}

/// Summarizes the tools + permission mode this wake runs under, straight
/// off the composed wake config (spec §4.9 "available tools + permission
/// mode"). Mirrors what `orchestrator::spawn` actually passes to the CLI:
/// with MCP enabled that is `mcp__continuum__*` plus one wildcard per
/// installed external server, at `--permission-mode default`; without MCP
/// it is no tools at all, in `plan` mode.
///
/// Registry read failures degrade to the Continuum wildcard alone — a
/// wake never fails over a tool list.
fn summarize_tools(config: &OrchestratorConfig, dev_dir: &std::path::Path) -> ToolsSection {
    if !config.mcp_enabled {
        return ToolsSection {
            names: Vec::new(),
            permission_mode: "plan".to_string(),
        };
    }
    let registry_dir = config
        .mcp_data_dir
        .clone()
        .unwrap_or_else(|| dev_dir.to_path_buf());
    let mut names = vec!["mcp__continuum__*".to_string()];
    match continuum_core::mcp_registry::list_servers(&registry_dir) {
        Ok(servers) => names.extend(servers.iter().map(|s| format!("mcp__{}__*", s.name))),
        Err(e) => tracing::debug!(
            layer = "context",
            component = "packager",
            error = %e,
            "MCP registry read failed; tools section lists the Continuum server only"
        ),
    }
    ToolsSection {
        names,
        permission_mode: "default".to_string(),
    }
}

/// Writes the post-wake structured record (spec §4.9): a best-effort
/// `{action, result, next_step}` trailer parsed off the orchestrator's
/// final text becomes a `wake_result` system event (summary = next step,
/// else result, else action) plus a matching vault timeline entry, which
/// is what the continuation resolver (Task B8, spec §4.12) ranks.
///
/// Absent or unparseable trailer → nothing is written and nothing is
/// logged as an error. A wake that answers in prose is normal.
async fn record_wake_result(vault: &continuum_memory::Vault, response: &str, reason: &str) {
    let Some(trailer) = parse_wake_trailer(response) else {
        return;
    };
    if trailer.is_empty() {
        return;
    }

    // System event — the continuation resolver's queryable producer.
    // Goes straight to the events channel rather than through
    // `privacy::emit_system_event`: this is an orchestrator record, not a
    // privacy notice, and the summary is already the model's own text.
    //
    // Fixwave 3b (minor): only `next_step` is written here. A trailer with
    // just `action`/`result` describes *finished* work and must not become
    // a continuation candidate — but it still earns its timeline entry
    // below.
    if let Some(summary) = trailer.summary() {
        send_system_event("wake_result", summary);
    }

    // Vault timeline — `NewEvent` has no structured slots, so the fields
    // ride the text in a stable `key: value | …` shape (additive: no
    // schema change, no migration).
    let _ = vault
        .append_event(continuum_memory::NewEvent {
            ts: None,
            kind: "wake_result".to_string(),
            text: trailer.render_line(),
            project: None,
            node_id: None,
            reference: None,
            local_only: false,
        })
        .await
        .map_err(|e| {
            tracing::warn!(
                layer = "memory",
                component = "continuum",
                error = %e.user_message(),
                "Failed to append wake_result event to vault timeline"
            );
        });

    tracing::info!(
        layer = "orchestrator",
        component = "continuum",
        reason = %reason,
        next_step = trailer.next_step.is_some(),
        "Post-wake structured record written"
    );
}

/// Performs a full orchestrator wake cycle.
///
/// `current_project` is the project resolver's post-hysteresis output at
/// wake time (Task A4) — it drives the semantic-fact project prefix and
/// skills matching.
///
/// Task B7 (spec §4.9): the user message is now the **runtime-full context
/// package** — every section, assembled here from the in-process hubs, the
/// shared frame ring, retrieval, deduped `context_events` and the composed
/// wake config, then rendered by the ungated packager under the
/// `[context_package]` budget. Every source is wrapped: a failure logs and
/// leaves its section empty (never-fail-the-wake).
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
    current_project: Option<&CurrentProject>,
    session: Option<&SessionState>,
    confidence_floor: f32,
    sources: &WakeSources,
    audit: &AuditLog,
) -> Result<()> {
    let wake_start = Instant::now();

    // Audit (Task C5, spec §4.13): a wake is Continuum acting on its own —
    // the single most important thing an ambient assistant owes the user a
    // record of. The reason text is already privacy-gated by the packager.
    audit.record(
        "wake_start",
        Actor::Agent,
        format!("Woke the orchestrator: {reason}"),
        Some(serde_json::json!({
            "model": config.model,
            "project": current_project.map(|p| p.id.clone()),
        })),
    );

    println!("\n--- CONTINUUM WAKING ---");

    // 1. Memory context: episodic/semantic retrieval plus the memory vault
    // (Plan B curator) — confirmed notes relevant to this frame, and any
    // candidate notes that have sat unresolved long enough to nudge the
    // orchestrator to review them. Vault retrieval never fails the wake —
    // `retrieve_vault_context` swallows and logs its own errors.
    let mut memory_context = {
        let mut ep = episodic.lock().await;
        retrieve_context(
            trigger_frame,
            &mut ep,
            semantic,
            current_project.map(|p| p.id.as_str()),
        )
        .await?
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

    // 2. Compose a dynamic system prompt — base file + matched skills +
    // any `suggested_skill` hint from the triage layer. Runs before the
    // wake message (Task B7) because the package's "available tools"
    // section is summarized from the *composed* config.
    let wake_config = compose_wake_config(
        config,
        skill_loader,
        reason,
        trigger_frame,
        suggested_skill,
        skill_token_budget,
        dev_dir,
        current_project,
        session,
        confidence_floor,
    )
    .unwrap_or_else(|| config.clone());

    // 3. Wake message — the runtime-full context package (spec §4.9).
    // Deduped events are read best-effort; everything else is already in
    // hand. Rendering applies the cloud gate, the per-section caps and the
    // documented drop ladder under `[context_package] token_budget`.
    let events = read_package_events(sources).await;
    // Task B8 (spec §4.12): a continue-class trigger ("ga door", or a
    // hotkey with no words) resolves what "continue" refers to before the
    // package is assembled — either a confident recommendation, or an
    // instruction to ask one short disambiguation question. Ordinary wakes
    // short-circuit inside `resolve_continuation` and pay nothing.
    let continuation_outcome = resolve_continuation(reason, session, sources, vault).await;
    let (wake_reason, recommended_next_step) =
        apply_continuation(reason, continuation_outcome.as_ref());
    let budget = sources.package.wake_budget();
    let package = build_wake_package(WakeContextInputs {
        trigger_frame,
        history_frames,
        memory_context: &memory_context,
        wake_reason: &wake_reason,
        session,
        session_confidence_floor: confidence_floor,
        events: &events,
        tools: Some(summarize_tools(&wake_config, dev_dir)),
        recommended_next_step,
        now: Utc::now(),
        caps: budget.caps.clone(),
    });
    let render = package.render_with_report(&budget);
    let user_message = render.text;
    tracing::debug!(
        layer = "context",
        component = "packager",
        tokens = render.tokens,
        budget = budget.token_budget,
        dropped = ?render.dropped,
        events = events.len(),
        over_budget = render.over_budget,
        "Wake package rendered"
    );

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
            local_only: false,
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
            project: None,
            sensitivity: continuum_core::memory::events::EventSensitivity::CloudAllowed,
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
            project: None,
            sensitivity: continuum_core::memory::events::EventSensitivity::CloudAllowed,
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

    // Post-wake structured record (Task B7, spec §4.9). Runs on every
    // completed wake, including one whose response was empty enough to
    // skip the episodic writes above — `record_wake_result` no-ops when
    // there is no parseable trailer.
    record_wake_result(vault, &full_response, reason).await;

    let total_ms = wake_start.elapsed().as_millis() as u64;
    audit.record(
        "wake_result",
        Actor::Agent,
        format!(
            "Wake finished in {total_ms} ms ({})",
            if result.success { "ok" } else { "failed" }
        ),
        Some(serde_json::json!({
            "duration_ms": total_ms,
            "cost_usd": result.cost_usd,
            "success": result.success,
        })),
    );

    tracing::info!(
        layer = "orchestrator",
        component = "continuum",
        total_ms = total_ms,
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

/// Maps an idle-controller transition (Task A8, spec §4.11) onto its
/// side effects: the live-context hub's idle flag (suppresses unchanged
/// ring events) and the `idle_start` / `idle_end` system events (already
/// registered EventTypes — routed through the A6 global sender).
fn apply_idle_transition(transition: Option<IdleTransition>, hub: &LiveContextHub) {
    match transition {
        Some(IdleTransition::EnteredIdle) => {
            hub.set_idle(true);
            tracing::info!(
                layer = "senses",
                component = "idle",
                "Idle threshold crossed — capture/vision cadences relaxed, triage gated to error/audio"
            );
            emit_system_event(
                "idle_start",
                "user idle; capture and vision cadences relaxed, triage gated to error/audio only",
            );
        }
        Some(IdleTransition::ExitedIdle) => {
            hub.set_idle(false);
            tracing::info!(
                layer = "senses",
                component = "idle",
                "Idle ended — normal cadences restored"
            );
            emit_system_event("idle_end", "activity resumed; normal cadences restored");
        }
        None => {}
    }
}

/// Applies an explicit idle restore trigger (voice wake, hotkey,
/// push-to-talk intent, any `do_wake` entry — Task A8, spec §4.11).
/// Shared between the frame loop and the maintenance-wake ticker, hence
/// the mutex-wrapped controller; locks are held for microseconds.
fn idle_wake_restore(
    idle_controller: &Arc<std::sync::Mutex<IdleController>>,
    cadence: &CadenceControl,
    hub: &LiveContextHub,
) {
    let transition = {
        let mut ctrl = idle_controller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ctrl.on_wake(cadence)
    };
    apply_idle_transition(transition, hub);
}

/// How long shutdown waits for the events writer's final flush before
/// closing the raw-log pool (I5). Generous: the flush is one batched
/// transaction of at most 256 rows.
const EVENT_WRITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a single triage evaluation may be in flight before the health
/// snapshot calls triage wedged (C1).
///
/// A Qwen pass is ~1 s; a minute is two orders of magnitude past that, so
/// this only fires when an evaluation genuinely never reported back — the
/// state that used to park every later frame silently.
const TRIAGE_STUCK_SECS: i64 = 60;

/// How many recent context events the Context page's strip shows.
const CONTEXT_PAGE_EVENT_LIMIT: usize = 40;

/// How far back the recent-events strip looks.
const CONTEXT_PAGE_EVENT_WINDOW_MINUTES: i64 = 180;

/// How often the Context page's list data is refreshed from the database
/// (Task C5). Deliberately slower than the 2 s publish tick: these are
/// four SQLite reads plus a vault-free continuation rank, and the page is
/// a human-latency surface.
const CONTEXT_PAGE_REFRESH_SECS: u64 = 5;

/// Everything the Context-page refresher needs, bundled so the spawn site
/// stays readable.
struct ContextPageSources {
    raw_log: RawLog,
    session: SessionStateHub,
    project_handle: CurrentProjectHandle,
    toggles: ToggleControl,
    privacy: Arc<PrivacyFilter>,
    continuation: ContinuationConfig,
}

/// Builds one [`ContextPageSnapshot`] (Task C5, spec §4.13).
///
/// Never fails: every read degrades to an empty list with a warning, so a
/// locked database costs the page a section, not the runtime a tick.
///
/// **Privacy:** the recent-events strip is a *published* surface —
/// `state.json` is a file on disk that backups and support bundles copy —
/// so rows tagged `local_only` are withheld and the survivors' free-text
/// fields are re-scrubbed through the live filter, exactly like the MCP
/// tools' `event_views` gate (Task C4).
async fn build_context_page_snapshot(sources: &ContextPageSources) -> ContextPageSnapshot {
    let current = sources.project_handle.read().clone();
    let active_id = current.as_ref().map(|p| p.id.clone());

    let projects = match sources.raw_log.list_projects().await {
        Ok(rows) => rows
            .into_iter()
            .map(|row| ProjectSummaryView {
                active: Some(&row.entry.id) == active_id.as_ref(),
                id: row.entry.id,
                name: row.entry.name,
                status: row.entry.status.as_str().to_string(),
                root_paths: row
                    .entry
                    .root_paths
                    .iter()
                    .map(|p| sources.privacy.scrub_path(p))
                    .collect(),
                last_active: row.last_active_ts,
                frames_count: row.frames_count,
            })
            .collect(),
        Err(e) => {
            tracing::warn!(
                layer = "context",
                component = "page",
                error = %e,
                "Projects read failed; Context page shows no projects this tick"
            );
            Vec::new()
        }
    };

    let rules = sources
        .raw_log
        .list_project_rules()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|rule| OverrideRuleView {
            match_process: rule.match_process,
            match_title_substring: rule.match_title_substring,
            action: rule.action.as_str().to_string(),
            project_id: rule.project_id,
        })
        .collect();

    let pins = sources
        .raw_log
        .list_session_pins()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(field, value, _project)| SessionPinView { field, value })
        .collect();

    let since = chrono::Utc::now() - chrono::Duration::minutes(CONTEXT_PAGE_EVENT_WINDOW_MINUTES);
    let rows = sources
        .raw_log
        .recent_context_events(since, CONTEXT_PAGE_EVENT_LIMIT)
        .await
        .unwrap_or_default();
    let mut recent_events: Vec<ContextEventView> = rows
        .into_iter()
        .filter(|row| {
            // Recorded sensitivity first, then the LIVE zone rules over the
            // row's own text (a rule added after the row was written still
            // binds — the Task C2 fail-closed trick).
            if matches!(row.sensitivity, EventSensitivity::LocalOnly) {
                return false;
            }
            let zone = strictest([
                sources
                    .privacy
                    .resolve_zone(&row.application, &row.window_title),
                sources.privacy.resolve_zone("", &row.summary),
            ]);
            zone == Zone::CloudAllowed
        })
        .map(|row| ContextEventView {
            id: row.id,
            ts: row.ts_last.to_rfc3339(),
            source: event_enum_token(&row.source),
            event_type: event_enum_token(&row.event_type),
            application: sources.privacy.scrub_path(&row.application),
            summary: sources.privacy.scrub_text(&row.summary),
            count: row.count,
            project_id: row.project_id,
            raw_reference: row.raw_reference,
        })
        .collect();
    // `recent_context_events` returns oldest-first; the strip reads newest
    // first.
    recent_events.reverse();

    let state = sources.session.snapshot();
    // The page ranks the same candidates `resolve_continuation` does, minus
    // the two reads that need the vault/DB (`open_task`, `wake_next_step`)
    // — those are wake-time work, and the page is a live view, not a wake.
    let ranked = match continuation::rank(
        &ContinuationInputs::from_session(&state),
        chrono::Utc::now(),
        &sources.continuation,
    ) {
        ContinuationOutcome::Recommend(c) => vec![c],
        ContinuationOutcome::Ask(list) => list,
        ContinuationOutcome::Nothing => Vec::new(),
    };
    let continuation = ranked
        .into_iter()
        .map(|c| ContinuationCandidateView {
            kind: c.kind.token().to_string(),
            label: c.kind.label().to_string(),
            text: c.text,
            confidence: c.confidence,
        })
        .collect();

    ContextPageSnapshot {
        projects,
        rules,
        pins,
        recent_events,
        toggles: ObservationTogglesView::from(&sources.toggles.snapshot()),
        continuation,
    }
}

/// Builds the [`ContextEngineSnapshot`] published into `state.json` every
/// tick (Task A8, spec §7). The runtime process has no `HealthRegistry`
/// (see `docs/self-healing.md`) — this snapshot IS its health surface:
/// each context-engine component's shared health handle is read and
/// folded into a uniform [`ComponentHealthSummary`]. Handles that are
/// `None` (pause_all — no watcher ever spawned) report as
/// disabled-with-reason, which is a healthy state (spec §7).
#[allow(clippy::too_many_arguments)]
fn build_context_engine_snapshot(
    cadence: &CadenceControl,
    hub: &LiveContextHub,
    context_watch: Option<&SharedContextWatchHealth>,
    git_watch: Option<&SharedGitWatchHealth>,
    file_watch: Option<&SharedFileWatchHealth>,
    process_watch: Option<&SharedProcessWatchHealth>,
    event_writer: &continuum_core::memory::events::EventWriterHandle,
    triage_busy: &TriageBusyHandle,
    triage_enabled: bool,
    screen_capture_enabled: bool,
    context_poll_interval: Duration,
) -> ContextEngineSnapshot {
    let now = chrono::Utc::now();

    let not_running = || ComponentHealthSummary {
        healthy: true,
        enabled: false,
        should_restart: false,
        detail: Some("not running (pause_all set in [privacy.toggles])".to_string()),
    };

    let context_watcher = context_watch
        .map(|handle| {
            let health = handle.read().clone();
            ComponentHealthSummary {
                healthy: health.is_healthy(now, context_poll_interval),
                enabled: health.enabled,
                should_restart: health.should_restart(now, context_poll_interval),
                detail: health.disabled_reason.clone().or_else(|| {
                    Some(format!(
                        "polls={} last_poll_at={}",
                        health.polls,
                        health
                            .last_poll_at
                            .map(|ts| ts.to_rfc3339())
                            .unwrap_or_else(|| "never".to_string()),
                    ))
                }),
            }
        })
        .unwrap_or_else(not_running);

    let live_context = {
        let health = hub.health();
        let interval = Duration::from_millis(cadence.capture_interval_ms().max(50));
        let should_restart = health.should_restart(now, screen_capture_enabled, interval);
        ComponentHealthSummary {
            healthy: !should_restart,
            enabled: screen_capture_enabled,
            should_restart,
            detail: Some(format!(
                "captures={} vision_updates={} dropped={} failures={} last_capture_at={}",
                health.capture_events,
                health.vision_updates,
                health.dropped_capture_events,
                health.capture_failures,
                health
                    .last_capture_at
                    .map(|ts| ts.to_rfc3339())
                    .unwrap_or_else(|| "never".to_string()),
            )),
        }
    };

    let git_watcher = git_watch
        .map(|handle| {
            let health = handle.read().clone();
            ComponentHealthSummary {
                // Spec §4.4: disabled-with-reason and probe failures are
                // healthy states; a restart can never fix a missing git.
                healthy: true,
                enabled: health.enabled,
                should_restart: false,
                detail: health.disabled_reason.clone().or_else(|| {
                    Some(format!(
                        "probes={} consecutive_failures={} events={} last_probe_at={}",
                        health.probes,
                        health.consecutive_failures,
                        health.events_emitted,
                        health
                            .last_probe_at
                            .map(|ts| ts.to_rfc3339())
                            .unwrap_or_else(|| "never".to_string()),
                    ))
                }),
            }
        })
        .unwrap_or_else(not_running);

    let file_watcher = file_watch
        .map(|handle| {
            let health = handle.read().clone();
            ComponentHealthSummary {
                // Spec §4.5: per-root unavailability rearms on backoff
                // and stays healthy; only notify channel death is a
                // genuine break (and the ONLY restart state).
                healthy: !health.channel_dead,
                enabled: health.enabled,
                should_restart: health.channel_dead,
                detail: health.disabled_reason.clone().or_else(|| {
                    Some(format!(
                        "roots_active={} roots_unavailable={} events={}",
                        health.roots_active,
                        health.roots_unavailable.len(),
                        health.events_emitted,
                    ))
                }),
            }
        })
        .unwrap_or_else(not_running);

    let process_watcher = process_watch
        .map(|handle| {
            let health = handle.read().clone();
            ComponentHealthSummary {
                healthy: !health.enabled || health.last_error.is_none(),
                enabled: health.enabled,
                should_restart: false,
                detail: health
                    .disabled_reason
                    .clone()
                    .or_else(|| health.last_error.clone())
                    .or_else(|| {
                        Some(format!(
                            "polls={} active={} events={} last_poll_at={}",
                            health.polls,
                            health.active_processes,
                            health.events_emitted,
                            health
                                .last_poll_at
                                .map(|ts| ts.to_rfc3339())
                                .unwrap_or_else(|| "never".to_string()),
                        ))
                    }),
            }
        })
        .unwrap_or_else(not_running);

    let events_writer = ComponentHealthSummary {
        healthy: event_writer.is_healthy(),
        enabled: true,
        should_restart: event_writer.should_restart(),
        detail: Some(format!(
            "queue_depth={} rows_written={} last_flush_at={}",
            event_writer.queue_depth(),
            event_writer.rows_written(),
            event_writer
                .last_flush_at()
                .map(|ts| ts.to_rfc3339())
                .unwrap_or_else(|| "never".to_string()),
        )),
    };

    // C1: the coalescer's busy flag is claimed by the frame arm and
    // released by the result arm. If an evaluation ever fails to report
    // (the drop guard is the first line of defence), every later frame is
    // parked silently — so the busy-since clock is a health surface: an
    // evaluation "in flight" for a minute means triage is wedged and a
    // restart is the only cure.
    let triage = {
        let stuck = triage_busy.is_stuck(now, chrono::Duration::seconds(TRIAGE_STUCK_SECS));
        ComponentHealthSummary {
            healthy: !stuck,
            enabled: triage_enabled,
            should_restart: stuck,
            detail: Some(match triage_busy.busy_since() {
                Some(since) if stuck => format!(
                    "evaluation stuck since {} ({}s)",
                    since.to_rfc3339(),
                    (now - since).num_seconds()
                ),
                Some(since) => format!("evaluating since {}", since.to_rfc3339()),
                None => "idle".to_string(),
            }),
        }
    };

    ContextEngineSnapshot {
        idle: cadence.is_idle(),
        context_watcher: Some(context_watcher),
        live_context: Some(live_context),
        git_watcher: Some(git_watcher),
        file_watcher: Some(file_watcher),
        process_watcher: Some(process_watcher),
        events_writer: Some(events_writer),
        triage: Some(triage),
    }
}

/// Shared dependencies of [`handle_triage_decision`] (Task B2, spec §4.7).
///
/// Decision consumption used to live inline in `main`'s frame arm with all
/// of these in lexical scope; with triage off the main loop the same code
/// runs from two select arms, so the dependencies are bundled once (all
/// cheap clones — `Arc` handles, small configs, flags) into this struct
/// built right before the loop.
struct DecisionCtx {
    /// Task B3 (spec §4.7): classification consumption — events, vault
    /// candidates, `triage_decision` column. Lives here because both
    /// consumption sites go through [`handle_triage_decision`].
    classification: ClassificationConsumer,
    /// Task B7 (spec §4.9): the shared recent-frames ring — the wake's
    /// "just before" fallback and the maintenance ticker's history.
    frames: FrameRing,
    /// Task B7 (spec §4.9): packager sources (deduped events + budget).
    wake_sources: WakeSources,
    orchestrator_busy: Arc<std::sync::atomic::AtomicBool>,
    idle_controller: Arc<std::sync::Mutex<IdleController>>,
    cadence: CadenceControl,
    live_context: LiveContextHub,
    speech: Option<Arc<SpeechController>>,
    feedback: FeedbackPlayer,
    followup_until: Arc<std::sync::Mutex<Option<Instant>>>,
    followup_secs: u64,
    ambient_mute_enabled: bool,
    orch_config: OrchestratorConfig,
    semantic: Arc<SemanticStore>,
    episodic: Arc<Mutex<EpisodicStore>>,
    vault: Arc<continuum_memory::Vault>,
    curator_cfg: CuratorConfig,
    runtime_state: Arc<std::sync::Mutex<continuum_core::runtime_publish::RuntimeSnapshot>>,
    skill_loader: SkillLoader,
    skill_token_budget: usize,
    dev_dir: PathBuf,
    /// Task B5 (spec §4.8): live session state, snapshotted at wake time
    /// for the skills `MatchContext`.
    session_state: SessionStateHub,
    /// `[session_state].confidence_floor` — below it, an inferred task is
    /// treated as unknown rather than fed to consumers.
    session_confidence_floor: f32,
    /// Task C5 (spec §4.13): the action audit log — every wake writes a
    /// `wake_start` and a `wake_result` line.
    audit: AuditLog,
    /// Moshi tier-split bridge: interrupt before an orchestrator wake and
    /// resume on every exit path, including panic/cancellation.
    #[cfg(feature = "moshi")]
    moshi_frontend: Option<std::sync::Arc<continuum_core::voice::moshi::MoshiFrontend>>,
    shutdown_rx: watch::Receiver<bool>,
}

/// One off-loop triage evaluation result, sent from the spawned evaluation
/// task back into `main`'s select loop (Task B2, spec §4.7).
///
/// `outcome` is `None` when the evaluation task died without producing one
/// (panic, or the task being dropped). That message exists purely so the
/// coalescer's busy flag is still released: without it a single panicking
/// evaluation would park every later frame forever, silently (C1).
struct TriageEvalResult {
    /// The frame the evaluation was for — always present, so the result
    /// arm can clean up its supersede bookkeeping either way.
    frame_id: uuid::Uuid,
    outcome: Option<TriageEvalOutcome>,
}

/// A successful triage evaluation. Carries the originating frame and its
/// resolver output so decision consumption sees exactly what the pre-B2
/// inline path saw.
struct TriageEvalOutcome {
    frame: PerceptionFrame,
    current_project: Option<CurrentProject>,
    output: TriageOutput,
    latency_ms: u64,
}

/// Spawns one triage evaluation as a tokio task (Task B2, spec §4.7 —
/// mirrors the do_wake spawn pattern). The result comes back over `tx`
/// into the main loop's triage arm; a send failure means the loop is
/// shutting down and is deliberately ignored.
///
/// Task B5 (spec §4.7/§4.8): the `memory_summary` argument — hardwired to
/// `""` since the triage layer existed — is now the session-state render,
/// char-capped at [`session_state::MEMORY_SUMMARY_MAX_CHARS`] (600) to
/// respect the prompt's token budget. It is rendered inside the spawned
/// task so it reflects the state at evaluation time, not submit time. The
/// local triage model is allowed to see `local_only` content (spec §4.1),
/// so this render is deliberately NOT the `cloud_view`.
fn spawn_triage_eval(
    triage_layer: TriageLayer,
    frame: PerceptionFrame,
    current_project: Option<CurrentProject>,
    session_state: SessionStateHub,
    confidence_floor: f32,
    tx: mpsc::UnboundedSender<TriageEvalResult>,
) {
    tokio::spawn(async move {
        let frame_id = frame.id;
        // C1: the coalescer's busy flag is released ONLY by the result
        // arm, i.e. only if this task reports back. A panic inside
        // `evaluate` (or a cancelled task) would otherwise latch the flag
        // and park every later frame forever, with nothing logged. The
        // guard turns any abnormal exit into an empty result, which the
        // result arm treats as "release and move on".
        let guard = {
            let tx = tx.clone();
            FallbackGuard::new(move || {
                tracing::error!(
                    layer = "triage",
                    component = "continuum",
                    frame_id = %frame_id,
                    "Triage evaluation task ended without a result; releasing the coalescer"
                );
                let _ = tx.send(TriageEvalResult {
                    frame_id,
                    outcome: None,
                });
            })
        };
        let start = Instant::now();
        let memory_summary = session_state.snapshot().render_memory_summary(
            Utc::now(),
            session_state::MEMORY_SUMMARY_MAX_CHARS,
            confidence_floor,
        );
        let output = triage_layer.evaluate(&frame, &memory_summary).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        guard.disarm();
        let _ = tx.send(TriageEvalResult {
            frame_id,
            outcome: Some(TriageEvalOutcome {
                frame,
                current_project,
                output,
                latency_ms,
            }),
        });
    });
}

/// Releases the coalescer for the evaluation that just reported and starts
/// the parked frame, if any (Task B2, spec §4.7).
///
/// Called from BOTH exits of the triage-result arm — the normal one and
/// the C1 "evaluation died" one — because forgetting it on either path is
/// exactly the latch this fix is about. Parked frames a voice wake
/// superseded while they waited are skipped rather than evaluated (no
/// point burning a GPU pass on a result we would drop).
fn chain_next_triage_eval(
    coalescer: &TriageCoalescer<(PerceptionFrame, Option<CurrentProject>)>,
    voice_superseded: &mut Vec<uuid::Uuid>,
    triage: Option<&TriageLayer>,
    session_state: &SessionStateHub,
    confidence_floor: f32,
    tx: &mpsc::UnboundedSender<TriageEvalResult>,
) {
    while let Some((next_frame, next_project)) = coalescer.complete() {
        if let Some(pos) = voice_superseded.iter().position(|id| *id == next_frame.id) {
            voice_superseded.remove(pos);
            tracing::debug!(
                layer = "triage",
                component = "continuum",
                frame_id = %next_frame.id,
                "Skipping parked frame — superseded by a voice/forced wake"
            );
            continue;
        }
        if let Some(triage_layer) = triage {
            spawn_triage_eval(
                triage_layer.clone(),
                next_frame,
                next_project,
                session_state.clone(),
                confidence_floor,
                tx.clone(),
            );
        }
        break;
    }
}

/// Consumes one effective triage-class decision — from the voice/forced
/// path in the frame arm or an off-loop triage result in the triage arm
/// (Task B2, spec §4.7). Body moved from the pre-B2 inline `match` in
/// `main`'s frame arm, unchanged in order and semantics; only the
/// dependency plumbing goes through [`DecisionCtx`] now.
///
/// `classification` is the same output's classification block (Task B3):
/// `Some` on the off-loop triage path, `None` on the voice/forced/skip
/// paths, which produce a decision without ever calling the model. It is
/// consumed first, before the decision is acted on, so an observation is
/// recorded even when the decision itself is dropped downstream (e.g. a
/// wake suppressed because the orchestrator is busy).
fn handle_triage_decision(
    decision: &TriageDecision,
    frame: &PerceptionFrame,
    current_project: Option<&CurrentProject>,
    classification: Option<&Classification>,
    ctx: &DecisionCtx,
) {
    // Task B3 (spec §4.7 consumption): events + vault candidate +
    // `triage_decision` column. Never blocks — the vault/raw-log writes
    // are spawned inside.
    ctx.classification
        .consume(frame, decision, classification, current_project);

    match decision {
        TriageDecision::WakeOrchestrator {
            reason,
            suggested_skill,
        } => {
            // If we're already inside a wake (orchestrator still
            // streaming from a previous trigger), don't stack — log
            // and skip. The user's latest utterance still landed in
            // the raw log; they can ask again.
            if !try_claim_busy(&ctx.orchestrator_busy) {
                tracing::warn!(
                    layer = "orchestrator",
                    component = "continuum",
                    reason = %reason,
                    "Orchestrator already busy — skipping new wake"
                );
            } else {
                // Idle restore trigger (Task A8, spec §4.11): ANY do_wake
                // entry exits idle; the cadence nudge forces one fresh
                // capture+vision pass for the wake.
                idle_wake_restore(&ctx.idle_controller, &ctx.cadence, &ctx.live_context);
                // History excludes the trigger frame itself. Pre-B2 this
                // was `recent_frames[..len-1]` (the trigger was always
                // the newest frame); with triage off the loop the trigger
                // may no longer be newest, so filter by id — frames newer
                // than the trigger stay in, which only adds context.
                // Task B7: the filter moved onto the shared ring, which
                // snapshot-clones under a short guard.
                let history = ctx.frames.snapshot_excluding(frame.id);
                let wake_speech_opt = if ctx.ambient_mute_enabled && frame.context.in_call {
                    tracing::info!(
                        layer = "voice",
                        component = "continuum",
                        "Quiet mode active during call; orchestrator response will not be spoken"
                    );
                    None
                } else {
                    ctx.speech.clone()
                };

                #[cfg(feature = "moshi")]
                if let Some(frontend) = ctx.moshi_frontend.as_ref() {
                    frontend.interrupt();
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
                if let Ok(mut s) = ctx.runtime_state.lock() {
                    s.wake_count += 1;
                    s.voice_mode = Some("thinking".to_string());
                }
                let busy_flag = ctx.orchestrator_busy.clone();
                let followup_shared = ctx.followup_until.clone();
                let orch_cfg_clone = ctx.orch_config.clone();
                let semantic_clone = ctx.semantic.clone();
                let episodic_clone = ctx.episodic.clone();
                let vault_clone = ctx.vault.clone();
                let curator_cfg_clone = ctx.curator_cfg.clone();
                let feedback_clone = ctx.feedback.clone();
                let frame_clone = frame.clone();
                let reason_clone = reason.clone();
                let followup_secs = ctx.followup_secs;
                let runtime_state_clone = ctx.runtime_state.clone();
                let skill_loader_clone = ctx.skill_loader.clone();
                let suggested_skill_clone = suggested_skill.clone();
                let skill_budget = ctx.skill_token_budget;
                let dev_dir_clone = ctx.dev_dir.clone();
                let wake_project = current_project.cloned();
                // Task B5 (spec §4.8): snapshot session state at wake
                // time — the skills matcher reads its inferred task and
                // (when the resolver has none) its project.
                let wake_session = Some(ctx.session_state.snapshot());
                let wake_confidence_floor = ctx.session_confidence_floor;
                // Task B7 (spec §4.9): the packager's own sources.
                let wake_sources = ctx.wake_sources.clone();
                let wake_audit = ctx.audit.clone();
                let mut wake_shutdown = ctx.shutdown_rx.clone();
                #[cfg(feature = "moshi")]
                let moshi_frontend_clone = ctx.moshi_frontend.clone();

                tokio::spawn(async move {
                    // I4: releasing `orchestrator_busy` must survive an
                    // abnormal exit. `do_wake` parses subprocess output,
                    // touches the vault and drives TTS; a panic anywhere
                    // in there used to skip the store below and latch the
                    // flag, after which Continuum never woke again — with
                    // nothing logged. The guard runs on every exit path,
                    // including an unwind, so it replaces the tail store
                    // entirely rather than duplicating it.
                    let _busy_guard = FallbackGuard::new(move || {
                        busy_flag.store(false, std::sync::atomic::Ordering::Release);
                        if let Ok(mut s) = runtime_state_clone.lock() {
                            s.voice_mode = Some("idle".to_string());
                        }
                        #[cfg(feature = "moshi")]
                        if let Some(frontend) = moshi_frontend_clone.as_ref() {
                            frontend.resume();
                            tracing::info!(
                                layer = "voice",
                                component = "moshi",
                                "Moshi resumed after orchestrator turn"
                            );
                        }
                    });
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
                            wake_project.as_ref(),
                            wake_session.as_ref(),
                            wake_confidence_floor,
                            &wake_sources,
                            &wake_audit,
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
                                    wake_project.as_ref(),
                                    wake_session.as_ref(),
                                    wake_confidence_floor,
                                    &wake_sources,
                                    &wake_audit,
                                )
                                .await
                            }
                        }
                    };

                    match result {
                        Ok(()) => {
                            if followup_secs > 0 {
                                if let Ok(mut slot) = followup_shared.lock() {
                                    *slot =
                                        Some(Instant::now() + Duration::from_secs(followup_secs));
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

                    // `_busy_guard` clears the busy flag and the voice mode
                    // here, on the way out of this scope.
                });
            }
        }
        TriageDecision::Whisper { text } => {
            // Don't step on the orchestrator's speech — if a wake
            // is already streaming through TTS, a concurrent
            // whisper would either queue behind it (adding delay)
            // or race with it (interrupt noise). Silently drop.
            if ctx
                .orchestrator_busy
                .load(std::sync::atomic::Ordering::Acquire)
            {
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
                if ctx.ambient_mute_enabled && frame.context.in_call {
                    println!("[quiet mode: would say via TTS: {text}]");
                } else if let Some(sc) = ctx.speech.as_ref() {
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
/// [`PerceptionFrame`]: skips (debug log) until the shared
/// [`FrameRing`] has observed at least one real frame from the main loop.
/// Also skips when the curator
/// is disabled, when the vault has no pending decisions, or when
/// [`try_claim_busy`] can't claim the orchestrator — shared with the
/// `WakeOrchestrator` arm precisely to avoid a double-wake race between
/// the two (see that function's doc comment).
///
/// Task B7 (spec §4.9): the `&[]` history is gone — the ticker shares the
/// frame loop's [`FrameRing`], so a maintenance wake carries the same
/// "just before" context a triage wake does.
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
    recent_frames: FrameRing,
    wake_sources: WakeSources,
    followup_until: Arc<std::sync::Mutex<Option<Instant>>>,
    followup_secs: u64,
    ambient_mute_enabled: bool,
    orchestrator_busy: Arc<std::sync::atomic::AtomicBool>,
    runtime_state: Arc<std::sync::Mutex<continuum_core::runtime_publish::RuntimeSnapshot>>,
    project_handle: CurrentProjectHandle,
    idle_controller: Arc<std::sync::Mutex<IdleController>>,
    cadence: CadenceControl,
    live_context: LiveContextHub,
    session_state: SessionStateHub,
    confidence_floor: f32,
    audit: AuditLog,
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

            // Task B7 (spec §4.9): the shared ring replaces the old
            // single-frame slot — the trigger is the newest frame and the
            // history is everything else the loop has seen.
            let Some(frame) = recent_frames.latest() else {
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

            // I4 (same shape as the WakeOrchestrator arm): the claim above
            // is released by a drop guard, so a panic inside `do_wake`
            // cannot latch `orchestrator_busy` and block every later wake.
            let _busy_guard = {
                let busy = orchestrator_busy.clone();
                let state = runtime_state.clone();
                FallbackGuard::new(move || {
                    busy.store(false, std::sync::atomic::Ordering::Release);
                    if let Ok(mut s) = state.lock() {
                        s.voice_mode = Some("idle".to_string());
                    }
                })
            };

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

            // Task A8 (spec §4.11): ANY do_wake entry is an idle restore
            // trigger — a maintenance wake during pause exits idle and
            // the cadence nudge forces one fresh capture+vision pass so
            // the wake doesn't reason over a stale frame. The controller
            // may re-enter idle on the next frame (user still away).
            idle_wake_restore(&idle_controller, &cadence, &live_context);

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
            // Task A4: read the resolver's current project at fire time
            // (the shared handle stays fresh via the frame loop).
            let wake_project = project_handle.read().clone();
            let history = recent_frames.snapshot_excluding(frame.id);
            let result = do_wake(
                &frame,
                &history,
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
                wake_project.as_ref(),
                Some(&session_state.snapshot()),
                confidence_floor,
                &wake_sources,
                &audit,
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

            // `_busy_guard` releases the claim and resets the voice mode
            // as this iteration's scope ends.
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
    session_state: &SessionStateHub,
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
            // Task B5 (spec §4.8): `last_user_command` + ts. This is the
            // only text-carrying user-command path *inside* the runtime —
            // the hotkey and the dashboard's TalkNow intent carry no words
            // (they arm listening; the transcript arrives here). The
            // desktop chat is a separate process; fixwave 3b (I9) gave it
            // the `record_user_command` context intent, drained by the
            // same 250 ms tick as every other Context-page intent.
            session_state.note_user_command(&text, "voice", Utc::now());
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
#[allow(clippy::too_many_arguments)]
fn compose_wake_config(
    base: &OrchestratorConfig,
    skill_loader: &SkillLoader,
    reason: &str,
    frame: &PerceptionFrame,
    suggested_skill: Option<&str>,
    token_budget: usize,
    dev_dir: &std::path::Path,
    current_project: Option<&CurrentProject>,
    session: Option<&SessionState>,
    confidence_floor: f32,
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
        // Task B5 (spec §4.8): the hardwired `None` dies — skills now
        // match against the inferred session-state task, but only when it
        // clears `confidence_floor` (a low-confidence guess must not
        // silently steer which skill loads).
        task: session
            .and_then(|s| s.task_if_confident(confidence_floor))
            .map(str::to_string),
        // Task A4: skills match against the resolver's current project
        // (name — human-readable, what skill triggers cite). Task B5: when
        // the resolver has no current project, session state's own
        // (possibly rehydrated) project id is the fallback.
        project: session_state::match_context_project(
            current_project.map(|p| p.name.as_str()),
            session,
        ),
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
                return Arc::new(StubVisionModel);
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
