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

mod agent_os_bootstrap;
mod chat;
mod chat_store;
mod chat_tools;
mod commands;
mod components;
mod events;
mod github;
mod memory;
mod onboarding;
#[cfg_attr(test, allow(dead_code))]
mod permissions;
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
    /// Tracks the desktop-owned automatic runtime launch across UI polls.
    pub(crate) runtime_startup: commands::RuntimeStartupState,
    /// Serializes repair runs so two UI invocations cannot race mutations.
    pub(crate) repair_gate: Arc<tokio::sync::Mutex<()>>,
    /// One-time live preview required before a repair can start.
    pub(crate) pending_repair: tokio::sync::Mutex<Option<commands::RepairPreview>>,
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
    let backup_retention = runtime.config_snapshot().health.backup_retention.max(1);

    // Memory vault: opened lazily by the first command/health-probe call
    // (see `memory::MemoryState::vault`), not here.
    let cfg = runtime.config_snapshot();
    let vault_dir = cfg.memory.vault.resolve_vault_dir(&dev_dir);
    let memory_state = Arc::new(
        memory::MemoryState::new(vault_dir, dev_dir.join("semantic.sqlite"), dev_dir.clone())
            .with_opts(continuum_memory::VaultOptions {
                watcher_debounce_ms: cfg.memory.vault.watcher_debounce_ms,
                graph_max_nodes: cfg.memory.vault.graph_max_nodes,
            }),
    );
    components::register_memory(&health, memory_state.clone());

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
        backup_retention,
        runtime.shutdown_receiver(),
        {
            let state = runtime.state.clone();
            let bd = backups_dir.clone();
            move |_res| {
                let state = state.clone();
                let bd = bd.clone();
                tokio::spawn(async move {
                    match continuum_core::health::backup::backup_status(&bd) {
                        Ok((latest, count)) => state.set_backup_status(latest, count).await,
                        Err(error) => tracing::warn!(
                            layer = "health",
                            component = "backup",
                            error = %error,
                            "Could not refresh backup status"
                        ),
                    }
                });
            }
        },
    );

    // Prime backup status once at boot.
    {
        let state = runtime.state.clone();
        let bd = backups_dir.clone();
        tokio::spawn(async move {
            match continuum_core::health::backup::backup_status(&bd) {
                Ok((latest, count)) => state.set_backup_status(latest, count).await,
                Err(error) => tracing::warn!(
                    layer = "health",
                    component = "backup",
                    error = %error,
                    "Could not read backup status at startup"
                ),
            }
        });
    }

    let app_state = Arc::new(AppState {
        runtime: runtime.clone(),
        health: health.clone(),
        runtime_startup: commands::RuntimeStartupState::new(),
        repair_gate: Arc::new(tokio::sync::Mutex::new(())),
        pending_repair: tokio::sync::Mutex::new(None),
    });

    match agent_os_bootstrap::ensure_registered(&app_state) {
        Ok(agent_os_bootstrap::BootstrapOutcome::Registered { binary }) => tracing::info!(
            layer = "desktop",
            component = "agent_os_bootstrap",
            binary = %binary.display(),
            "Registered the bundled Agent OS MCP server"
        ),
        Ok(agent_os_bootstrap::BootstrapOutcome::AlreadyCurrent { binary }) => tracing::debug!(
            layer = "desktop",
            component = "agent_os_bootstrap",
            binary = %binary.display(),
            "Bundled Agent OS MCP registration is current"
        ),
        Ok(agent_os_bootstrap::BootstrapOutcome::Missing { searched }) => {
            if cfg!(debug_assertions) {
                tracing::debug!(
                    layer = "desktop",
                    component = "agent_os_bootstrap",
                    ?searched,
                    "Bundled Agent OS binary is not present in this development build"
                );
            } else {
                tracing::error!(
                    layer = "desktop",
                    component = "agent_os_bootstrap",
                    ?searched,
                    "Packaged desktop is missing the Agent OS binary; computer use and Composio will be unavailable"
                );
            }
        }
        Err(error) => tracing::error!(
            layer = "desktop",
            component = "agent_os_bootstrap",
            error = %error,
            "Could not repair the bundled Agent OS MCP registration"
        ),
    }

    let runtime_for_tauri = runtime.clone();

    let chat_state = Arc::new(providers::ChatState {
        providers: std::sync::Mutex::new(providers::ProviderStore::new(dev_dir.clone())),
        secrets: Box::new(providers::KeyringSecretStore),
        inflight: std::sync::Mutex::new(std::collections::HashMap::new()),
        conv_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(app_state.clone())
        .manage(chat_state)
        .manage(memory_state.clone())
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::get_config,
            commands::get_resource_profile,
            commands::update_resource_profile,
            commands::update_voice_volume,
            commands::update_voice_flag,
            commands::update_screen_interval,
            commands::update_live_context_config,
            commands::update_triage_threshold,
            commands::get_logs,
            commands::get_memory_summary,
            commands::wipe_memory,
            commands::list_automations,
            commands::create_automation,
            commands::update_automation,
            commands::delete_automation,
            commands::toggle_automation,
            commands::get_health,
            commands::preview_repair,
            commands::trigger_repair,
            commands::restart_component,
            commands::run_backup_now,
            commands::rollback_config,
            commands::set_paused,
            commands::get_observation_pause,
            commands::pause_observation,
            commands::resume_observation,
            commands::set_voice_muted,
            commands::talk_now,
            commands::context_write_intent,
            commands::update_wake_sensitivity,
            commands::update_tts_length_scale,
            commands::update_tts_engine,
            commands::update_tts_primary_voice,
            commands::update_kokoros_voice,
            commands::update_kokoros_speed,
            commands::update_voice_frontend_mode,
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
            commands::list_mcp_tools,
            commands::list_permission_requests,
            commands::list_permission_grants,
            commands::approve_permission_request,
            commands::deny_permission_request,
            commands::revoke_permission_grant,
            commands::list_installed_mcp_servers,
            commands::install_mcp_server,
            permissions::list_tool_permissions,
            permissions::set_tool_permission,
            providers::catalog_list,
            providers::providers_list,
            providers::provider_add,
            providers::provider_test,
            providers::provider_refresh_models,
            providers::provider_remove,
            providers::provider_set_default_model,
            github::github_status,
            github::github_connect,
            github::github_disconnect,
            chat::chat_list_conversations,
            chat::chat_get_conversation,
            chat::chat_create_conversation,
            chat::chat_delete_conversation,
            chat::chat_rename_conversation,
            chat::chat_set_conversation_model,
            chat::chat_send_message,
            chat::chat_cancel,
            memory::memory_graph,
            memory::memory_search,
            memory::memory_get_note,
            memory::memory_create_note,
            memory::memory_save_note,
            memory::memory_delete_note,
            memory::memory_resolve_candidate,
            memory::memory_pending,
            memory::memory_events,
            memory::memory_vault_info,
            memory::memory_migrate_legacy,
            memory::memory_rebuild_index,
            memory::memory_open_vault,
            onboarding::check_claude_cli,
            onboarding::check_claude_auth,
            onboarding::list_audio_input_devices,
            onboarding::list_audio_output_devices,
            onboarding::download_model,
            onboarding::get_models_directory,
            onboarding::update_models_directory,
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
            memory::spawn_watcher_bridge(handle.clone(), memory_state.clone());
            tray::init(app)?;
            if onboarding::is_complete(&app_state) {
                commands::spawn_automatic_runtime_start(app_state.clone(), handle);
            }
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
