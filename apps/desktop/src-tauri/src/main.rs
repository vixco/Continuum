//! # Kairo Desktop
//!
//! Tauri 2 backend for the Kairo dashboard. Hosts the kairo-core runtime
//! handles (state store, log buffer, automations, repair agent) and
//! exposes them to the Next.js frontend via:
//!
//! - `#[tauri::command]` handlers for request/response IPC (see [`commands`])
//! - `AppHandle::emit(topic, payload)` for live-updating streams (see [`events`])
//!
//! The heavy runtime loop (senses + triage + orchestrator) is not booted by
//! the dashboard process itself in this phase — llama-cpp-sys-2 does not
//! play nicely with Tauri's default debug build on Windows. Instead the
//! dashboard reads state the separate `kairo` binary publishes into the
//! shared dev dir, plus whatever state updates arrive via the runtime
//! bridge. See CLAUDE.md "Build workflow" and docs/dashboard.md.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod components;
mod events;
mod runtime_bridge;
mod tray;

use std::sync::Arc;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use kairo_core::logs::BufferLayer;
use kairo_core::runtime::KairoRuntime;

/// Shared app state held by Tauri.
pub struct AppState {
    pub runtime: KairoRuntime,
    pub health: kairo_core::health::HealthRegistry,
}

fn main() {
    let runtime = KairoRuntime::init().expect("initialise Kairo runtime");
    let log_layer = BufferLayer::new(runtime.logs.clone());
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,kairo_core=debug,kairo_desktop=debug"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().compact().with_target(false))
        .with(log_layer)
        .try_init();

    let health = kairo_core::health::HealthRegistry::new();
    components::register_default(&health, &runtime);

    kairo_core::health::spawn_poller(
        health.clone(),
        runtime.state.clone(),
        30,
        runtime.shutdown_receiver(),
    );

    let dev_dir = runtime.dev_dir();
    let backups_dir = dev_dir
        .parent()
        .map(|p| p.join(".kairo-backups"))
        .unwrap_or_else(|| dev_dir.join(".kairo-backups"));

    kairo_core::health::backup::spawn_nightly(
        dev_dir.clone(),
        backups_dir.clone(),
        kairo_core::health::backup::DEFAULT_BACKUP_HOUR,
        kairo_core::health::backup::DEFAULT_RETENTION,
        runtime.shutdown_receiver(),
        {
            let state = runtime.state.clone();
            let backups_dir = backups_dir.clone();
            move |_res| {
                let state = state.clone();
                let backups_dir = backups_dir.clone();
                tokio::spawn(async move {
                    let latest = kairo_core::health::backup::latest_backup_ts(&backups_dir);
                    let count = kairo_core::health::backup::count_backups(&backups_dir);
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
            let latest = kairo_core::health::backup::latest_backup_ts(&bd);
            let count = kairo_core::health::backup::count_backups(&bd);
            state.set_backup_status(latest, count).await;
        });
    }

    let app_state = Arc::new(AppState {
        runtime: runtime.clone(),
        health: health.clone(),
    });
    let runtime_for_tauri = runtime.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::get_config,
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
            commands::quit_app,
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
