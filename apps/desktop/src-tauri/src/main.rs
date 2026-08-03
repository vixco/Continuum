//! # Continuum Desktop
//!
//! Tauri 2 backend for the Continuum dashboard. Hosts the continuum-core runtime
//! handles (state store, log buffer, automations, repair agent) and
//! exposes them to the Next.js frontend via:
//!
//! - `#[tauri::command]` handlers for request/response IPC (see [`commands`])
//! - `AppHandle::emit(topic, payload)` for live-updating streams (see [`events`])
//!
//! The heavy runtime loop (senses + triage + orchestrator) is not booted by
//! the dashboard process itself in this phase — llama-cpp-sys-2 does not
//! play nicely with Tauri's default debug build on Windows. Instead the
//! dashboard reads state the separate `continuum` binary publishes into the
//! shared dev dir, plus whatever state updates arrive via the runtime
//! bridge. See CLAUDE.md "Build workflow" and docs/dashboard.md.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chat;
mod chat_store;
mod commands;
mod components;
mod events;
mod onboarding;
mod providers;
mod runtime_bridge;
mod tray;

use std::sync::Arc;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use continuum_core::logs::BufferLayer;
use continuum_core::runtime::ContinuumRuntime;

/// Shared app state held by Tauri.
pub struct AppState {
    pub runtime: ContinuumRuntime,
    pub health: continuum_core::health::HealthRegistry,
}

fn main() {
    // Build a Tokio runtime and enter its context for the rest of `main`.
    // Tauri's Builder::setup() callback is NOT guaranteed to run inside a
    // Tokio context (despite Tauri using tokio internally via
    // tauri::async_runtime), so calls like health::spawn_poller — which
    // do `tokio::spawn` — panic with "no reactor running" unless we hold
    // an EnterGuard on this thread. The guard lives until the end of
    // main(), which spans the entire Tauri run() blocking call.
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let _tokio_guard = tokio_rt.enter();

    let runtime = ContinuumRuntime::init().expect("initialise Continuum runtime");
    let log_layer = BufferLayer::new(runtime.logs.clone());
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,continuum_core=debug,continuum_desktop=debug"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_target(false),
        )
        .with(log_layer)
        .try_init();

    let health = continuum_core::health::HealthRegistry::new();
    components::register_default(&health, &runtime);

    let dev_dir = runtime.dev_dir();
    let backups_dir = continuum_core::config::continuum_backups_dir();

    // Background pollers — the EnterGuard above means tokio::spawn works here.
    continuum_core::health::spawn_poller(
        health.clone(),
        runtime.state.clone(),
        30,
        runtime.shutdown_receiver(),
    );

    continuum_core::health::backup::spawn_nightly(
        dev_dir.clone(),
        backups_dir.clone(),
        continuum_core::health::backup::DEFAULT_BACKUP_HOUR,
        continuum_core::health::backup::DEFAULT_RETENTION,
        runtime.shutdown_receiver(),
        {
            let state = runtime.state.clone();
            let bd = backups_dir.clone();
            move |_res| {
                let state = state.clone();
                let bd = bd.clone();
                tokio::spawn(async move {
                    let latest = continuum_core::health::backup::latest_backup_ts(&bd);
                    let count = continuum_core::health::backup::count_backups(&bd);
                    state.set_backup_status(latest, count).await;
                });
            }
        },
    );

    // Prime backup status once at boot.
    {
        let state = runtime.state.clone();
        let bd = backups_dir.clone();
        tokio::spawn(async move {
            let latest = continuum_core::health::backup::latest_backup_ts(&bd);
            let count = continuum_core::health::backup::count_backups(&bd);
            state.set_backup_status(latest, count).await;
        });
    }

    let app_state = Arc::new(AppState {
        runtime: runtime.clone(),
        health: health.clone(),
    });
    let runtime_for_tauri = runtime.clone();

    let chat_state = Arc::new(providers::ChatState {
        providers: std::sync::Mutex::new(providers::ProviderStore::new(dev_dir.clone())),
        secrets: Box::new(providers::KeyringSecretStore),
        inflight: std::sync::Mutex::new(std::collections::HashMap::new()),
        conv_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(app_state)
        .manage(chat_state)
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::get_config,
            commands::get_resource_profile,
            commands::update_resource_profile,
            commands::update_voice_volume,
            commands::update_voice_flag,
            commands::update_screen_interval,
            commands::update_triage_threshold,
            commands::get_logs,
            commands::get_memory_summary,
            commands::search_episodic,
            commands::delete_episodic,
            commands::list_semantic,
            commands::set_semantic,
            commands::delete_semantic,
            commands::wipe_memory,
            commands::list_automations,
            commands::create_automation,
            commands::update_automation,
            commands::delete_automation,
            commands::toggle_automation,
            commands::get_health,
            commands::trigger_repair,
            commands::restart_component,
            commands::run_backup_now,
            commands::rollback_config,
            commands::set_paused,
            commands::set_voice_muted,
            commands::talk_now,
            commands::update_wake_sensitivity,
            commands::update_tts_length_scale,
            commands::update_tts_engine,
            commands::update_tts_primary_voice,
            commands::quit_app,
            commands::list_skills,
            commands::save_skill,
            commands::delete_skill,
            commands::toggle_skill,
            commands::install_skill_from_url,
            commands::list_workers,
            commands::get_worker,
            commands::cancel_worker,
            commands::dismiss_worker,
            commands::get_runtime_status,
            commands::pipe_health,
            commands::start_runtime,
            commands::list_mcp_tools,
            providers::catalog_list,
            providers::providers_list,
            providers::provider_add,
            providers::provider_test,
            providers::provider_refresh_models,
            providers::provider_remove,
            providers::provider_set_default_model,
            chat::chat_list_conversations,
            chat::chat_get_conversation,
            chat::chat_create_conversation,
            chat::chat_delete_conversation,
            chat::chat_rename_conversation,
            chat::chat_set_conversation_model,
            chat::chat_send_message,
            chat::chat_cancel,
            onboarding::check_claude_cli,
            onboarding::check_claude_auth,
            onboarding::list_audio_input_devices,
            onboarding::list_audio_output_devices,
            onboarding::download_model,
            onboarding::run_diagnostics,
            onboarding::is_onboarding_complete,
            onboarding::complete_onboarding,
            onboarding::reset_onboarding,
            onboarding::list_ai_clis,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            events::bridge_state(runtime_for_tauri.state.clone(), handle.clone());
            events::bridge_logs(runtime_for_tauri.logs.clone(), handle.clone());
            runtime_bridge::spawn_ipc_listener(runtime_for_tauri.clone(), handle.clone());
            tray::init(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
