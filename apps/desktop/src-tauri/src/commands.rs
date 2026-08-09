//! Tauri command handlers — the request/response IPC surface.
//!
//! These handlers live on the Tauri side so they can `.await` against
//! continuum-core handles directly. Long-running work (memory search, repair
//! agent) spawns into the tokio runtime the Tauri app owns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State, Window};

use continuum_core::automations::{Automation, AutomationInput};
use continuum_core::config::{ContinuumConfig, ProfileMode, ResourceConfig};
use continuum_core::hardware::{self, HardwareSpecs, ResolvedResourcePlan};
use continuum_core::health::{self, repair::RepairInput};
use continuum_core::logs::{LogEntry, LogFilter};
use continuum_core::permissions::{
    GrantScope, PermissionGateway, PermissionGrant, PermissionRequest,
};
use continuum_core::privacy_pause::{self, ObservationPausePreset, ObservationPauseStatus};
use continuum_core::skills::{self, SkillFrontmatter, SkillLoader};
use continuum_core::state::{ComponentHealth, ComponentStatus, ContinuumState};
use continuum_core::workers::intent::{self as worker_intent};
use continuum_core::workers::{WorkerIntent, WorkerSnapshot};

use crate::memory::MemoryState;
use crate::runtime_bridge::{self, PipeHealth};
use crate::AppState;

/// Full state snapshot. The dashboard calls this once on mount and then
/// listens to `continuum:state` events for updates.
#[tauri::command]
pub async fn get_state(app: State<'_, Arc<AppState>>) -> Result<ContinuumState, String> {
    Ok(app.runtime.state.snapshot().await)
}

#[tauri::command]
pub async fn get_config(app: State<'_, Arc<AppState>>) -> Result<ContinuumConfig, String> {
    Ok(app.runtime.config_snapshot())
}

/// Detected hardware + resolved resource plan + current `[resources]` config.
/// Read-only view for the dashboard Resource panel.
#[derive(Debug, Serialize)]
pub struct ResourceProfile {
    pub specs: HardwareSpecs,
    pub plan: ResolvedResourcePlan,
    pub config: ResourceConfig,
    /// True when the live runtime has published a resource plan to state.json
    /// (i.e. `continuum.exe` is running and applied its boot-time plan). When
    /// false, the displayed plan is a fresh resolve against the current config
    /// and would only take effect after a runtime restart.
    pub applied: bool,
}

#[tauri::command]
pub async fn get_resource_profile(
    app: State<'_, Arc<AppState>>,
) -> Result<ResourceProfile, String> {
    let cfg = app.runtime.config_snapshot();
    let specs = hardware::probe_hardware();
    let plan = hardware::resolve_resource_policy(&specs, &cfg.resources);
    // "applied" = the running runtime has published a resource plan to
    // state.json AND it matches the plan we'd resolve from the current
    // config. When false, the displayed plan only takes effect after a
    // runtime restart (no hot-reload channel).
    let state_path = app.runtime.dev_dir().join("state.json");
    let published: Option<ResolvedResourcePlan> = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| {
            serde_json::from_str::<continuum_core::runtime_publish::RuntimeSnapshot>(&s).ok()
        })
        .and_then(|snap| snap.resource_plan);
    let applied = published.map(|p| p == plan).unwrap_or(false);
    Ok(ResourceProfile {
        specs,
        plan,
        config: cfg.resources,
        applied,
    })
}

/// Editable subset of `[resources]`. Every field is optional — only the
/// supplied fields are changed. Setting `profile` to `Custom` is implied
/// whenever an individual knob is set alongside a non-Custom profile, since
/// presets would override the knob at resolve time.
#[derive(Debug, Deserialize, Default)]
pub struct ResourceProfileUpdate {
    pub profile: Option<ProfileMode>,
    pub cpu_core_fraction: Option<f32>,
    pub gpu_enabled: Option<Option<bool>>,
    pub vision_enabled: Option<Option<bool>>,
    pub workers_max_concurrent: Option<Option<u32>>,
    pub screen_interval_secs: Option<Option<u64>>,
    pub context_interval_secs: Option<Option<u64>>,
    pub battery_throttle: Option<bool>,
}

/// Result of a profile update. `restart_required` is always true — the
/// resource plan is resolved once at boot and there's no hot-reload channel
/// (the runtime reads config at startup). The dashboard shows a banner.
#[derive(Debug, Serialize)]
pub struct ResourceProfileUpdateResult {
    pub config: ResourceConfig,
    pub plan: ResolvedResourcePlan,
    pub restart_required: bool,
}

#[tauri::command]
pub async fn update_resource_profile(
    app: State<'_, Arc<AppState>>,
    update: ResourceProfileUpdate,
) -> Result<ResourceProfileUpdateResult, String> {
    // Apply the partial update on top of the current [resources] section.
    let cfg = app
        .runtime
        .update_config(|c| {
            let r = &mut c.resources;
            if let Some(profile) = update.profile {
                r.profile = profile;
            }
            // Setting an individual knob while on a preset → flip to Custom so
            // the knob is actually honoured (presets re-derive most knobs).
            let mut touched_custom = false;
            if let Some(frac) = update.cpu_core_fraction {
                r.cpu_core_fraction = frac;
                touched_custom = true;
            }
            if let Some(gpu) = update.gpu_enabled {
                r.gpu_enabled = gpu;
                touched_custom = true;
            }
            if let Some(vis) = update.vision_enabled {
                r.vision_enabled = vis;
                touched_custom = true;
            }
            if let Some(w) = update.workers_max_concurrent {
                r.workers_max_concurrent = w;
                touched_custom = true;
            }
            if let Some(s) = update.screen_interval_secs {
                r.screen_interval_secs = s;
                touched_custom = true;
            }
            if let Some(s) = update.context_interval_secs {
                r.context_interval_secs = s;
                touched_custom = true;
            }
            if let Some(b) = update.battery_throttle {
                r.battery_throttle = b;
                touched_custom = true;
            }
            if touched_custom && r.profile != ProfileMode::Custom {
                r.profile = ProfileMode::Custom;
            }
        })
        .map_err(|e| e.to_string())?;

    // Validate the merged config; update_config already persisted, so a
    // validation failure here means we should roll back. For simplicity we
    // surface the error — the caller can re-send a valid value.
    if let Err(e) = cfg.resources.validate() {
        return Err(format!("invalid resource config: {e}"));
    }

    let specs = hardware::probe_hardware();
    let plan = hardware::resolve_resource_policy(&specs, &cfg.resources);

    Ok(ResourceProfileUpdateResult {
        config: cfg.resources,
        plan,
        restart_required: true,
    })
}

#[tauri::command]
pub async fn update_voice_volume(
    app: State<'_, Arc<AppState>>,
    volume: f32,
) -> Result<ContinuumConfig, String> {
    let cfg = app
        .runtime
        .update_config(|c| c.voice.volume = volume.clamp(0.0, 1.0))
        .map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
pub async fn update_voice_flag(
    app: State<'_, Arc<AppState>>,
    flag: String,
    value: bool,
) -> Result<ContinuumConfig, String> {
    let cfg = app
        .runtime
        .update_config(|c| match flag.as_str() {
            "enabled" => c.voice.enabled = value,
            "wake_word_enabled" => c.voice.wake_word_enabled = value,
            "barge_in_enabled" => c.voice.barge_in_enabled = value,
            "ambient_mute_enabled" => c.voice.ambient_mute_enabled = value,
            "feedback_sounds" => c.voice.feedback_sounds = value,
            "language_detection_enabled" => c.voice.language_detection_enabled = value,
            "tts_enabled" => c.tts.enabled = value,
            _ => {}
        })
        .map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
pub async fn update_screen_interval(
    app: State<'_, Arc<AppState>>,
    seconds: u64,
) -> Result<ContinuumConfig, String> {
    app.runtime
        .update_config(|c| c.screen.interval_secs = seconds.clamp(1, 30))
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize, Default)]
pub struct LiveContextConfigUpdate {
    pub enabled: Option<bool>,
    pub capture_interval_ms: Option<u64>,
    pub all_monitors: Option<bool>,
    pub save_screenshots: Option<bool>,
}

/// Persist the visible consent/performance boundary for continuous context.
/// The headless runtime applies the change on restart.
#[tauri::command]
pub async fn update_live_context_config(
    app: State<'_, Arc<AppState>>,
    update: LiveContextConfigUpdate,
) -> Result<ContinuumConfig, String> {
    app.runtime
        .update_config(|config| {
            if let Some(enabled) = update.enabled {
                config.screen.enabled = enabled;
            }
            if let Some(interval_ms) = update.capture_interval_ms {
                config.screen.capture_interval_ms = interval_ms.clamp(50, 5_000);
            }
            if let Some(all_monitors) = update.all_monitors {
                config.screen.all_monitors = all_monitors;
            }
            if let Some(save_screenshots) = update.save_screenshots {
                config.screen.save_screenshots = save_screenshots;
            }
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn update_triage_threshold(
    app: State<'_, Arc<AppState>>,
    threshold: f32,
) -> Result<ContinuumConfig, String> {
    app.runtime
        .update_config(|c| c.frame.salience_threshold = threshold.clamp(0.0, 1.0))
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    pub level: Option<String>,
    pub layer: Option<String>,
    pub component: Option<String>,
    pub text: Option<String>,
    pub limit: Option<usize>,
}

#[tauri::command]
pub async fn get_logs(
    app: State<'_, Arc<AppState>>,
    query: Option<LogQuery>,
) -> Result<Vec<LogEntry>, String> {
    let q = query.unwrap_or(LogQuery {
        level: None,
        layer: None,
        component: None,
        text: None,
        limit: Some(500),
    });
    let filter = LogFilter {
        level: q.level,
        layer: q.layer,
        component: q.component,
        text: q.text,
        limit: q.limit,
        ..LogFilter::default()
    };
    Ok(app.runtime.logs.query(&filter))
}

// --- Memory ---

#[derive(Debug, Serialize)]
pub struct MemorySummary {
    pub raw_log_rows: u64,
    pub episodic_count: u64,
    pub semantic_count: u64,
}

#[tauri::command]
pub async fn get_memory_summary(app: State<'_, Arc<AppState>>) -> Result<MemorySummary, String> {
    let snap = app.runtime.state.snapshot().await;
    Ok(MemorySummary {
        raw_log_rows: snap.memory.raw_log_rows,
        episodic_count: snap.memory.episodic_count,
        semantic_count: snap.memory.semantic_count,
    })
}

/// Atomically writes the wipe-request contract file
/// (`curator::run::process_wipe_request` in continuum-core, Task 7) into
/// `dev_dir`: `<dev_dir>/wipe-request.json` =
/// `{"requested_at": "<rfc3339>", "scopes": ["raw_log", "episodic", "events"]}`.
/// Written to a `.tmp` sibling first and renamed into place so a reader
/// (the runtime's boot drain or daily hygiene tick) never observes a
/// partially-written file — `std::fs::rename` is atomic on the same
/// filesystem, which `dev_dir` always is here (both paths are under the
/// same `dev_dir`).
fn write_wipe_request_file(dev_dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dev_dir)
        .map_err(|e| format!("Could not create {}: {e}", dev_dir.display()))?;

    let payload = serde_json::json!({
        "requested_at": chrono::Utc::now().to_rfc3339(),
        "scopes": ["raw_log", "episodic", "events"],
    });
    let final_path = dev_dir.join("wipe-request.json");
    let tmp_path = dev_dir.join("wipe-request.json.tmp");

    std::fs::write(&tmp_path, payload.to_string())
        .map_err(|e| format!("Could not write {}: {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &final_path)
        .map_err(|e| format!("Could not finalize {}: {e}", final_path.display()))?;

    Ok(())
}

/// Requests a wipe of derived memory data (raw log rows, episodic
/// memories, and the vault's timeline events) that the headless runtime
/// maintains. This never touches the memory vault's markdown notes — the
/// vault is the user-owned, Obsidian-compatible source of truth and is
/// only ever edited through the vault commands (`memory_*`, see
/// `memory.rs`) or by the user directly on disk.
///
/// The dashboard process cannot reach into the separate `continuum`
/// runtime process to wipe `RawLog`/`EpisodicStore` directly, so this
/// records the request as `<dev_dir>/wipe-request.json`
/// (`write_wipe_request_file`) for the runtime to drain — at its next boot
/// or the next daily hygiene tick (`curator::run::process_wipe_request`).
/// What this command *can* do immediately, since it holds a vault handle:
/// it clears the vault's own timeline events (`prune_events(0)`) and
/// rebuilds the derived index, rather than leaving that piece waiting on
/// the runtime too.
#[tauri::command]
pub async fn wipe_memory(
    memory: State<'_, Arc<MemoryState>>,
    confirm: String,
) -> Result<(), String> {
    wipe_memory_inner(&memory, &confirm).await
}

pub(crate) async fn wipe_memory_inner(state: &MemoryState, confirm: &str) -> Result<(), String> {
    if confirm != "DELETE" {
        return Err("wipe requires the literal string \"DELETE\" as confirmation".into());
    }

    tracing::warn!(
        layer = "memory",
        component = "dashboard",
        "User requested memory wipe via dashboard"
    );

    write_wipe_request_file(state.dev_dir())?;

    let vault = state.vault().await?;
    vault.prune_events(0).await.map_err(|e| e.user_message())?;
    vault.rebuild_index().await.map_err(|e| e.user_message())?;

    Ok(())
}

// --- Automations ---

#[tauri::command]
pub async fn list_automations(app: State<'_, Arc<AppState>>) -> Result<Vec<Automation>, String> {
    Ok(app.runtime.automations.list())
}

#[tauri::command]
pub async fn create_automation(
    app: State<'_, Arc<AppState>>,
    input: AutomationInput,
) -> Result<Automation, String> {
    app.runtime
        .automations
        .create(input)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_automation(
    app: State<'_, Arc<AppState>>,
    id: String,
    input: AutomationInput,
) -> Result<Automation, String> {
    app.runtime
        .automations
        .update(&id, input)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_automation(app: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    app.runtime
        .automations
        .delete(&id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn toggle_automation(
    app: State<'_, Arc<AppState>>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    app.runtime
        .automations
        .set_enabled(&id, enabled)
        .map_err(|e| e.to_string())
}

// --- Health + repair ---

const SAFE_TEST_TARGETS: &[&str] = &[
    "vision",
    "triage",
    "orchestrator",
    "tts",
    "stt",
    "memory",
    "mcp",
    "context_watcher",
];
const SAFE_DIRECT_TARGETS: &[&str] = &["runtime"];

#[derive(Debug, Clone, Serialize)]
pub struct RepairPreviewIssue {
    pub component: String,
    pub status: ComponentStatus,
    pub detail: String,
    pub proposed_action: String,
    pub actionable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepairPreview {
    pub id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub issues: Vec<RepairPreviewIssue>,
    pub backup_required: bool,
    pub allowed_actions: Vec<String>,
}

fn ensure_main_window(window: &Window) -> Result<(), String> {
    if window.label() != "main" {
        return Err("Health repair commands are authorized only for the main window".into());
    }
    Ok(())
}

fn is_preview_repair_issue(component: &ComponentHealth) -> bool {
    matches!(
        component.status,
        ComponentStatus::Error | ComponentStatus::Degrading
    ) || (component.name == "runtime" && component.status == ComponentStatus::Unknown)
}

#[tauri::command]
pub async fn preview_repair(
    app: State<'_, Arc<AppState>>,
    window: Window,
) -> Result<RepairPreview, String> {
    ensure_main_window(&window)?;
    if app.runtime.state.snapshot().await.health.repair_running {
        return Err("a repair is already running".into());
    }
    let _preview_gate = app
        .repair_gate
        .clone()
        .try_lock_owned()
        .map_err(|_| "a repair is already running or being previewed".to_string())?;
    let mut pending = app.pending_repair.lock().await;
    *pending = None;
    let components = app.health.run_all().await;
    app.runtime.state.set_components(components.clone()).await;
    let issues = components
        .into_iter()
        .filter(is_preview_repair_issue)
        .map(|component| {
            let actionable = SAFE_DIRECT_TARGETS.contains(&component.name.as_str());
            RepairPreviewIssue {
                detail: component
                    .last_error
                    .clone()
                    .or(component.recovery_note.clone())
                    .unwrap_or_else(|| "live health probe reported a non-healthy state".into()),
                proposed_action: if actionable {
                    "create a verified backup, start the offline runtime once, then wait for a live heartbeat".into()
                } else {
                    "diagnose and escalate; no automatic mutation is allowlisted".into()
                },
                component: component.name,
                status: component.status,
                actionable,
            }
        })
        .collect::<Vec<_>>();
    let created_at = chrono::Utc::now();
    let ttl = app
        .runtime
        .config_snapshot()
        .health
        .repair_session_ttl_secs
        .clamp(30, 15 * 60);
    let preview = RepairPreview {
        id: uuid::Uuid::new_v4().to_string(),
        created_at,
        expires_at: created_at + chrono::Duration::seconds(ttl as i64),
        issues,
        backup_required: true,
        allowed_actions: vec![
            "start an offline runtime after a verified backup".into(),
            "test previewed components".into(),
            "report explicit manual next steps".into(),
        ],
    };
    if let Err(error) = continuum_core::health::repair::append_repair_audit(
        &app.runtime.dev_dir(),
        "repair_preview_created",
        serde_json::json!({
            "preview_id": preview.id,
            "expires_at": preview.expires_at,
            "issues": preview.issues.iter().map(|issue| &issue.component).collect::<Vec<_>>(),
        }),
    ) {
        return Err(error.to_string());
    }
    *pending = Some(preview.clone());
    Ok(preview)
}

#[tauri::command]
pub async fn get_health(app: State<'_, Arc<AppState>>) -> Result<Vec<ComponentHealth>, String> {
    let components = app.health.run_all().await;
    app.runtime.state.set_components(components.clone()).await;
    Ok(components)
}

#[tauri::command]
pub async fn trigger_repair(
    app: State<'_, Arc<AppState>>,
    app_handle: AppHandle,
    window: Window,
    preview_id: String,
    reason: Option<String>,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    uuid::Uuid::parse_str(&preview_id).map_err(|_| "invalid repair preview id".to_string())?;
    if reason.as_ref().map(|value| value.len()).unwrap_or(0) > 1_000 {
        return Err("repair reason is limited to 1000 characters".into());
    }
    let gate = app
        .repair_gate
        .clone()
        .try_lock_owned()
        .map_err(|_| "a repair is already running".to_string())?;
    let preview = app
        .pending_repair
        .lock()
        .await
        .take()
        .ok_or_else(|| "repair preview missing or already used; preview again".to_string())?;
    if preview.id != preview_id {
        return Err("repair preview does not match the authorized plan".into());
    }
    if preview.expires_at <= chrono::Utc::now() {
        return Err("repair preview expired; preview the live issues again".into());
    }
    let live_components = app.health.run_all().await;
    let previewed = preview
        .issues
        .iter()
        .map(|issue| issue.component.clone())
        .collect::<std::collections::HashSet<_>>();
    let live_issues = live_components
        .iter()
        .filter(|component| previewed.contains(&component.name))
        .filter(|component| is_preview_repair_issue(component))
        .cloned()
        .collect::<Vec<_>>();
    let allowed_components = live_issues
        .iter()
        .filter(|component| SAFE_TEST_TARGETS.contains(&component.name.as_str()))
        .map(|component| component.name.clone())
        .collect::<Vec<_>>();
    if live_issues.is_empty() {
        return Err("none of the previewed live issues still requires repair".into());
    }
    let start_runtime = live_issues
        .iter()
        .any(|component| component.name == "runtime");
    let needs_agent = live_issues
        .iter()
        .any(|component| component.name != "runtime");
    let runtime = app.runtime.clone();
    let health = app.health.clone();
    runtime.state.set_repair_running(true).await;
    tokio::spawn(async move {
        let _gate = gate;
        let components = live_components;
        let dev_dir = runtime.dev_dir();
        let backups_dir = continuum_core::config::continuum_backups_dir();
        let repo_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| dev_dir.clone());
        let cfg = runtime.config_snapshot();

        if start_runtime {
            if !needs_agent {
                let _ = app_handle.emit(
                    "continuum:repair",
                    continuum_core::health::repair::RepairEvent::Started {
                        ts: chrono::Utc::now(),
                    },
                );
            }
            let action = "start_runtime".to_string();
            let outcome = guarded_start_runtime(
                &dev_dir,
                &backups_dir,
                cfg.health.backup_retention.max(1),
                cfg.health.runtime_start_timeout_secs.clamp(10, 5 * 60),
                &app_handle,
            )
            .await;
            let direct_action_success = outcome.is_ok();
            let mut detail = outcome.unwrap_or_else(|error| error);
            if let Err(error) = continuum_core::health::repair::append_repair_audit(
                &dev_dir,
                "runtime_start_result",
                serde_json::json!({
                    "success": direct_action_success,
                    "detail": &detail,
                }),
            ) {
                detail.push_str(&format!("; warning: result audit failed: {error}"));
            }
            let _ = app_handle.emit(
                "continuum:repair",
                continuum_core::health::repair::RepairEvent::ActionResult {
                    action,
                    success: direct_action_success,
                    detail,
                },
            );
            if !needs_agent {
                let _ = app_handle.emit(
                    "continuum:repair",
                    continuum_core::health::repair::RepairEvent::Finished {
                        ts: chrono::Utc::now(),
                        success: direct_action_success,
                        cost_usd: None,
                    },
                );
            }
        }

        if needs_agent {
            let input = RepairInput {
                dev_dir: &dev_dir,
                backups_dir: &backups_dir,
                repo_root: &repo_root,
                config: &cfg,
                state: &runtime.state,
                logs: &runtime.logs,
                components,
                allowed_components,
                user_reason: reason,
            };

            let emit_handle = app_handle.clone();
            let cb = move |ev: continuum_core::health::repair::RepairEvent| {
                let _ = emit_handle.emit("continuum:repair", ev);
            };

            if let Err(e) = health::repair::run_repair(input, cb).await {
                let _ = app_handle.emit(
                    "continuum:repair",
                    continuum_core::health::repair::RepairEvent::Error {
                        message: e.to_string(),
                    },
                );
            }
            // `run_repair` owns its standalone state lifecycle. Keep the
            // enclosing UI action marked busy through the authoritative
            // follow-up probes below.
            runtime.state.set_repair_running(true).await;
        }
        let verified_components = health.run_all().await;
        runtime
            .state
            .set_components(verified_components.clone())
            .await;
        let unresolved = verified_components
            .into_iter()
            .filter(|component| {
                matches!(
                    component.status,
                    ComponentStatus::Error | ComponentStatus::Degrading
                ) || (previewed.contains(&component.name)
                    && component.status != ComponentStatus::Healthy)
            })
            .collect::<Vec<_>>();
        let _ = app_handle.emit(
            "continuum:repair",
            continuum_core::health::repair::RepairEvent::Verification {
                checked_at: chrono::Utc::now(),
                unresolved: unresolved.clone(),
            },
        );
        let _ = continuum_core::health::repair::append_repair_audit(
            &dev_dir,
            "repair_verified",
            serde_json::json!({
                "unresolved": unresolved.iter().map(|item| &item.name).collect::<Vec<_>>(),
            }),
        );
        runtime.state.set_repair_running(false).await;
        match continuum_core::health::backup::backup_status(&backups_dir) {
            Ok((latest, count)) => runtime.state.set_backup_status(latest, count).await,
            Err(error) => {
                let _ = app_handle.emit(
                    "continuum:repair",
                    continuum_core::health::repair::RepairEvent::Error {
                        message: format!(
                            "repair finished but backup status refresh failed: {error}"
                        ),
                    },
                );
            }
        }
    });
    Ok(())
}

async fn guarded_start_runtime(
    dev_dir: &std::path::Path,
    backups_dir: &std::path::Path,
    retention: u32,
    timeout_secs: u64,
    app_handle: &AppHandle,
) -> Result<String, String> {
    if crate::components::runtime_alive(dev_dir) {
        return Ok("runtime is already publishing a live heartbeat; no action taken".into());
    }
    let bin = locate_runtime_binary().ok_or_else(|| {
        "continuum runtime binary was not found next to the desktop executable".to_string()
    })?;
    if runtime_process_exists(&bin) {
        return Err(
            "a Continuum runtime process already exists but is not publishing a fresh heartbeat; refusing to start a duplicate"
                .into(),
        );
    }

    let backup = continuum_core::health::backup::run_backup(dev_dir, backups_dir)
        .map_err(|error| format!("pre-start backup failed; runtime was not started: {error}"))?;
    continuum_core::health::backup::prune_backups(backups_dir, retention)
        .map_err(|error| format!("backup retention failed; runtime was not started: {error}"))?;
    continuum_core::health::backup::verify_backup(&backup.path).map_err(|error| {
        format!("verified backup was not retained; runtime was not started: {error}")
    })?;
    let _ = app_handle.emit(
        "continuum:repair",
        continuum_core::health::repair::RepairEvent::BackupCreated {
            path: backup.path.display().to_string(),
            bytes: backup.bytes,
            verified: true,
        },
    );
    continuum_core::health::repair::append_repair_audit(
        dev_dir,
        "runtime_start_requested",
        serde_json::json!({
            "backup": backup.path,
            "backup_bytes": backup.bytes,
            "backup_verified": true,
        }),
    )
    .map_err(|error| format!("repair audit failed; runtime was not started: {error}"))?;

    if crate::components::runtime_alive(dev_dir) {
        return Ok(
            "runtime began publishing a live heartbeat during backup; no process was started"
                .into(),
        );
    }
    if runtime_process_exists(&bin) {
        return Err(
            "a Continuum runtime process appeared during backup but has no fresh heartbeat; refusing to start a duplicate"
                .into(),
        );
    }
    let working_dir = runtime_working_dir(&bin);
    let mut child = runtime_command(&bin, &working_dir)
        .spawn()
        .map_err(|error| format!("failed to start {}: {error}", bin.display()))?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if crate::components::runtime_alive(dev_dir) {
            let detail = format!(
                "runtime started (pid {}) and published a fresh heartbeat",
                child.id()
            );
            if let Err(error) = continuum_core::health::repair::append_repair_audit(
                dev_dir,
                "runtime_started",
                serde_json::json!({ "pid": child.id(), "verified": "fresh_state_heartbeat" }),
            ) {
                return Ok(format!("{detail}; warning: success audit failed: {error}"));
            }
            return Ok(detail);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "runtime exited before publishing a heartbeat (status {status})"
                ));
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "failed to inspect runtime process; it was stopped: {error}"
                ));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let detail = format!(
                "runtime did not publish a heartbeat within {timeout_secs} seconds and was stopped"
            );
            let _ = continuum_core::health::repair::append_repair_audit(
                dev_dir,
                "runtime_start_failed",
                serde_json::json!({ "reason": "heartbeat_timeout", "timeout_secs": timeout_secs }),
            );
            return Err(detail);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn runtime_process_exists(bin: &std::path::Path) -> bool {
    let expected = bin.canonicalize().unwrap_or_else(|_| bin.to_path_buf());
    let mut system = sysinfo::System::new_all();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    system.processes().values().any(|process| {
        process.exe().is_some_and(|path| {
            path.canonicalize().unwrap_or_else(|_| path.to_path_buf()) == expected
        })
    })
}

fn runtime_command(bin: &std::path::Path, working_dir: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(bin);
    command.current_dir(working_dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: the runtime is a background companion process.
        command.creation_flags(0x0800_0000);
    }
    command
}

#[tauri::command]
pub async fn restart_component(
    app: State<'_, Arc<AppState>>,
    name: String,
) -> Result<Option<ComponentHealth>, String> {
    tracing::info!(
        layer = "health",
        component = "dashboard",
        target = %name,
        "User requested component re-probe"
    );
    // Kept under the published Tauri command name for compatibility. This is
    // deliberately a re-probe and must not be presented as a restart.
    Ok(app.health.run_single(&name).await)
}

#[tauri::command]
pub async fn run_backup_now(app: State<'_, Arc<AppState>>) -> Result<String, String> {
    let dev_dir = app.runtime.dev_dir();
    let backups_dir = continuum_core::config::continuum_backups_dir();
    let result = continuum_core::health::backup::run_backup(&dev_dir, &backups_dir)
        .map_err(|e| e.to_string())?;
    continuum_core::health::backup::prune_backups(
        &backups_dir,
        app.runtime.config_snapshot().health.backup_retention.max(1),
    )
    .map_err(|e| e.to_string())?;
    continuum_core::health::backup::verify_backup(&result.path).map_err(|e| e.to_string())?;
    let (latest, count) =
        continuum_core::health::backup::backup_status(&backups_dir).map_err(|e| e.to_string())?;
    app.runtime.state.set_backup_status(latest, count).await;
    Ok(result.path.display().to_string())
}

#[tauri::command]
pub async fn rollback_config(
    app: State<'_, Arc<AppState>>,
    window: Window,
    date: String,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    let dev_dir = app.runtime.dev_dir();
    let backups_dir = continuum_core::config::continuum_backups_dir();
    continuum_core::health::repair::rollback_config(&dev_dir, &backups_dir, &date)
        .map_err(|e| e.to_string())?;
    let (latest, count) =
        continuum_core::health::backup::backup_status(&backups_dir).map_err(|e| e.to_string())?;
    app.runtime.state.set_backup_status(latest, count).await;
    Ok(())
}

// --- Voice: push-to-talk ---

/// Write a `TalkNow` voice intent for the daemon to pick up. Equivalent to
/// pressing the configured global hotkey, but works from the dashboard
/// without requiring the user to leave the app.
#[tauri::command]
pub async fn talk_now(app: State<'_, Arc<AppState>>) -> Result<(), String> {
    let dev_dir = app.runtime.dev_dir();
    continuum_core::voice::intent::write_intent(
        &dev_dir,
        &continuum_core::voice::intent::VoiceIntent::talk_now(),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

// --- Context page (Task C5, spec §4.13) ---

/// Writes one Context-page intent file for the runtime to drain.
///
/// The dashboard links `continuum-core` without the `runtime` feature, so
/// it cannot open the raw-log database, the vault index or the episodic
/// store — every Context-page action (confirm a project, correct the
/// project/goal/task, pin, forget, delete a range, flip an honest toggle)
/// travels to the runtime as a file under
/// `<dev_dir>/context-intents/`, the same pattern
/// [`continuum_core::voice::intent`] uses for push-to-talk.
///
/// Fire-and-forget by design: this returns as soon as the file is on disk.
/// The runtime drains it on its next 250 ms tick and republishes
/// `state.json`, which is what actually updates the page. There is
/// deliberately **no** TTL on the file — a correction made while the
/// runtime is stopped applies at its next boot.
#[tauri::command]
pub async fn context_write_intent(
    app: State<'_, Arc<AppState>>,
    intent: continuum_core::context::intents::ContextAction,
) -> Result<(), String> {
    let dev_dir = app.runtime.dev_dir();
    let envelope = continuum_core::context::intents::ContextIntent::new(intent);
    tracing::info!(
        layer = "context",
        component = "dashboard",
        intent = envelope.action.kind_label(),
        intent_id = %envelope.id,
        "Context page intent queued for the runtime"
    );
    continuum_core::context::intents::write_intent(&dev_dir, &envelope)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// --- Voice: settings that persist to config (require daemon restart) ---

#[tauri::command]
pub async fn update_wake_sensitivity(
    app: State<'_, Arc<AppState>>,
    value: f32,
) -> Result<ContinuumConfig, String> {
    app.runtime
        .update_config(|c| c.voice.wake_sensitivity = value.clamp(0.0, 1.0))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_tts_length_scale(
    app: State<'_, Arc<AppState>>,
    value: f32,
) -> Result<ContinuumConfig, String> {
    app.runtime
        .update_config(|c| c.tts.length_scale = Some(value.clamp(0.5, 2.0)))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_tts_engine(
    app: State<'_, Arc<AppState>>,
    engine: String,
) -> Result<ContinuumConfig, String> {
    if engine != "piper" && engine != "kokoros" && engine != "elevenlabs" {
        return Err(format!(
            "Unknown TTS engine '{engine}'; expected 'piper', 'kokoros', or 'elevenlabs'"
        ));
    }
    app.runtime
        .update_config(|c| c.tts.engine = engine.clone())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_tts_primary_voice(
    app: State<'_, Arc<AppState>>,
    voice: String,
) -> Result<ContinuumConfig, String> {
    if voice.trim().is_empty() {
        return Err("Voice name cannot be empty".to_string());
    }
    app.runtime
        .update_config(|c| c.tts.primary = voice.clone())
        .map_err(|e| e.to_string())
}

/// Set the Kokoros voice style (e.g. `af_sky` or `af_sarah.4+af_nicole.6`).
/// Only meaningful when `tts.engine = "kokoros"`.
#[tauri::command]
pub async fn update_kokoros_voice(
    app: State<'_, Arc<AppState>>,
    voice: String,
) -> Result<ContinuumConfig, String> {
    if voice.trim().is_empty() {
        return Err("Kokoros voice name cannot be empty".to_string());
    }
    app.runtime
        .update_config(|c| c.tts.kokoros.voice_name = voice.clone())
        .map_err(|e| e.to_string())
}

/// Set the Kokoros speech-rate multiplier (1.0 = native).
#[tauri::command]
pub async fn update_kokoros_speed(
    app: State<'_, Arc<AppState>>,
    value: f32,
) -> Result<ContinuumConfig, String> {
    app.runtime
        .update_config(|c| c.tts.kokoros.speed = value.clamp(0.5, 2.0))
        .map_err(|e| e.to_string())
}

/// Select the realtime voice front-end. `"pipeline"` (default, whisper→
/// triage→orchestrator→TTS) or `"moshi"` (Kyutai Moshi S2S subprocess;
/// requires the `moshi` cargo feature + a CUDA-capable GPU + a built
/// `moshi-backend.exe`). Takes effect on the next daemon start.
#[tauri::command]
pub async fn update_voice_frontend_mode(
    app: State<'_, Arc<AppState>>,
    mode: String,
) -> Result<ContinuumConfig, String> {
    if mode != "pipeline" && mode != "moshi" {
        return Err(format!(
            "Unknown voice front-end mode '{mode}'; expected 'pipeline' or 'moshi'"
        ));
    }
    app.runtime
        .update_config(|c| c.voice.frontend.mode = mode.clone())
        .map_err(|e| e.to_string())
}

// --- Runtime control ---

fn queue_pause_all(dev_dir: &std::path::Path, paused: bool) -> Result<(), String> {
    let action = continuum_core::context::intents::ContextAction::SetToggle {
        name: continuum_core::context::intents::ToggleName::PauseAll,
        value: paused,
    };
    let intent = continuum_core::context::intents::ContextIntent::new(action);
    continuum_core::context::intents::write_intent(dev_dir, &intent)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Returns the durable privacy-pause state. Corrupt records fail closed.
#[tauri::command]
pub async fn get_observation_pause(
    app: State<'_, Arc<AppState>>,
) -> Result<ObservationPauseStatus, String> {
    match privacy_pause::read_status(&app.runtime.dev_dir(), chrono::Utc::now()) {
        Ok(mut status) => {
            if !status.paused && app.runtime.state.snapshot().await.system.paused {
                status.paused = true;
                status.until = None;
            }
            Ok(status)
        }
        Err(error) => {
            tracing::error!(
                layer = "privacy",
                component = "observation_pause",
                error = %error,
                "Privacy pause record could not be read; reporting paused"
            );
            Ok(ObservationPauseStatus {
                paused: true,
                until: None,
            })
        }
    }
}

/// Pauses all observation for a trusted, bounded preset or indefinitely.
#[tauri::command]
pub async fn pause_observation(
    app: State<'_, Arc<AppState>>,
    preset: ObservationPausePreset,
) -> Result<ObservationPauseStatus, String> {
    let dev_dir = app.runtime.dev_dir();
    let status = privacy_pause::pause(&dev_dir, preset, chrono::Utc::now())
        .map_err(|error| error.to_string())?;
    queue_pause_all(&dev_dir, true)?;
    app.runtime.set_paused(true).await;
    tracing::info!(
        layer = "privacy",
        component = "observation_pause",
        until = ?status.until,
        "All observation pause requested"
    );
    Ok(status)
}

/// Resumes every individually enabled observation source.
#[tauri::command]
pub async fn resume_observation(
    app: State<'_, Arc<AppState>>,
) -> Result<ObservationPauseStatus, String> {
    let dev_dir = app.runtime.dev_dir();
    let status = privacy_pause::resume(&dev_dir).map_err(|error| error.to_string())?;
    queue_pause_all(&dev_dir, false)?;
    app.runtime.set_paused(false).await;
    tracing::info!(
        layer = "privacy",
        component = "observation_pause",
        "All observation resume requested"
    );
    Ok(status)
}

#[tauri::command]
pub async fn set_paused(app: State<'_, Arc<AppState>>, paused: bool) -> Result<(), String> {
    if paused {
        pause_observation(app, ObservationPausePreset::Indefinite).await?;
    } else {
        resume_observation(app).await?;
    }
    Ok(())
}

#[tauri::command]
pub async fn set_voice_muted(app: State<'_, Arc<AppState>>, muted: bool) -> Result<(), String> {
    app.runtime.set_voice_muted(muted).await;
    Ok(())
}

#[tauri::command]
pub async fn quit_app(app_handle: AppHandle) -> Result<(), String> {
    app_handle.exit(0);
    Ok(())
}

// --- Phase 8: Skills ---

#[derive(Debug, Serialize, Deserialize)]
pub struct SkillView {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub source: Option<String>,
    pub manual_only: bool,
    pub enabled: bool,
    pub body: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct SaveSkillInput {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub body: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub manual_only: bool,
}

fn skills_root(app: &AppState) -> std::path::PathBuf {
    let cfg = app.runtime.config_snapshot();
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
    app.runtime.dev_dir().join(&cfg.skills.dir)
}

/// Reject skill names the JS layer could craft to escape `skills_root`.
///
/// Allowed: non-empty ASCII-ish identifier with `-`/`_` separators. No path
/// separators, no traversal segments, no absolute paths, no NUL bytes.
/// Runs on every `save_skill` / `delete_skill` / `install_skill_from_url`.
fn validate_skill_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("skill name must not be empty".into());
    }
    if trimmed.len() > 120 {
        return Err("skill name is too long (max 120 chars)".into());
    }
    if trimmed != name {
        return Err("skill name must not have leading or trailing whitespace".into());
    }
    if name == "." || name == ".." {
        return Err("skill name cannot be '.' or '..'".into());
    }
    for ch in name.chars() {
        let ok = ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ' ');
        if !ok {
            return Err(format!(
                "skill name contains an illegal character: {:?}",
                ch
            ));
        }
    }
    if name.contains("..") {
        return Err("skill name may not contain '..'".into());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("skill name may not contain path separators".into());
    }
    Ok(())
}

/// Allowlist of hosts we accept for `install_skill_from_url` so a compromised
/// or crafted URL can't have the Tauri process clone from an arbitrary
/// server. Keep tight — add hosts on request rather than pre-emptively.
const SKILL_CLONE_HOST_ALLOWLIST: &[&str] = &[
    "github.com",
    "gitlab.com",
    "bitbucket.org",
    "codeberg.org",
    "git.sr.ht",
];

/// Validate and normalise a skill-install URL. Returns the URL to pass to
/// `git clone` on success; an error message otherwise.
fn validate_clone_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("URL must not be empty".into());
    }

    // SSH-style `git@host:org/repo(.git)` — pull the host out by hand, the
    // rest gets handed to git verbatim.
    if let Some(rest) = trimmed.strip_prefix("git@") {
        let (host, _path) = rest
            .split_once(':')
            .ok_or_else(|| "malformed git@ URL — expected git@host:path".to_string())?;
        if !SKILL_CLONE_HOST_ALLOWLIST.contains(&host) {
            return Err(format!(
                "host '{host}' is not in the skill-install allowlist"
            ));
        }
        return Ok(trimmed.to_string());
    }

    // Everything else must parse as HTTPS.
    let parsed = url::Url::parse(trimmed).map_err(|e| format!("invalid URL: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("only https:// or git@ URLs are allowed".into());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    if !SKILL_CLONE_HOST_ALLOWLIST.contains(&host) {
        return Err(format!(
            "host '{host}' is not in the skill-install allowlist"
        ));
    }
    Ok(parsed.to_string())
}

#[tauri::command]
pub async fn list_skills(app: State<'_, Arc<AppState>>) -> Result<Vec<SkillView>, String> {
    let root = skills_root(&app);
    let loader = SkillLoader::new(&root);
    let cfg = app.runtime.config_snapshot();
    loader.set_disabled(cfg.skills.disabled.clone());
    loader.reload().map_err(|e| e.to_string())?;
    let out: Vec<SkillView> = loader
        .list()
        .into_iter()
        .map(|s| SkillView {
            name: s.frontmatter.name,
            description: s.frontmatter.description,
            triggers: s.frontmatter.triggers,
            source: s.frontmatter.source,
            manual_only: s.frontmatter.manual_only,
            enabled: s.enabled,
            body: s.body,
            path: s.path.display().to_string(),
        })
        .collect();
    Ok(out)
}

#[tauri::command]
pub async fn save_skill(
    app: State<'_, Arc<AppState>>,
    input: SaveSkillInput,
) -> Result<SkillView, String> {
    validate_skill_name(&input.name)?;
    let root = skills_root(&app);
    let fm = SkillFrontmatter {
        name: input.name,
        description: input.description,
        triggers: input.triggers,
        source: input.source,
        manual_only: input.manual_only,
    };
    let skill = skills::save_skill(&root, fm, &input.body).map_err(|e| e.to_string())?;
    Ok(SkillView {
        name: skill.frontmatter.name,
        description: skill.frontmatter.description,
        triggers: skill.frontmatter.triggers,
        source: skill.frontmatter.source,
        manual_only: skill.frontmatter.manual_only,
        enabled: skill.enabled,
        body: skill.body,
        path: skill.path.display().to_string(),
    })
}

#[tauri::command]
pub async fn delete_skill(app: State<'_, Arc<AppState>>, name: String) -> Result<(), String> {
    validate_skill_name(&name)?;
    let root = skills_root(&app);
    skills::delete_skill(&root, &name).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn toggle_skill(
    app: State<'_, Arc<AppState>>,
    name: String,
    enabled: bool,
) -> Result<ContinuumConfig, String> {
    app.runtime
        .update_config(|c| {
            let already = c.skills.disabled.iter().any(|d| d == &name);
            if enabled && already {
                c.skills.disabled.retain(|d| d != &name);
            } else if !enabled && !already {
                c.skills.disabled.push(name.clone());
            }
        })
        .map_err(|e| e.to_string())
}

/// Rudimentary git-URL installer: runs `git clone --depth 1 <url>` into
/// a temp dir, validates the SKILL.md, and copies the directory into the
/// skills root. Requires `git` on PATH.
#[tauri::command]
pub async fn install_skill_from_url(
    app: State<'_, Arc<AppState>>,
    url: String,
) -> Result<SkillView, String> {
    let clone_url = validate_clone_url(&url)?;
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let clone_target = tmp.path().join("skill");
    let status = tokio::process::Command::new("git")
        .arg("clone")
        .arg("--depth")
        .arg("1")
        .arg("--")
        .arg(&clone_url)
        .arg(&clone_target)
        .status()
        .await
        .map_err(|e| format!("failed to invoke git: {e}"))?;
    if !status.success() {
        return Err("git clone exited with a non-zero status".into());
    }
    let skill_md = clone_target.join("SKILL.md");
    if !skill_md.exists() {
        return Err("cloned repo does not contain a SKILL.md at its root".into());
    }
    let parsed = skills::parse_skill_file(&skill_md).map_err(|e| e.to_string())?;
    validate_skill_name(&parsed.frontmatter.name)?;
    let fm = SkillFrontmatter {
        source: Some("third-party".into()),
        ..parsed.frontmatter
    };
    let root = skills_root(&app);
    let skill = skills::save_skill(&root, fm, &parsed.body).map_err(|e| e.to_string())?;
    Ok(SkillView {
        name: skill.frontmatter.name,
        description: skill.frontmatter.description,
        triggers: skill.frontmatter.triggers,
        source: skill.frontmatter.source,
        manual_only: skill.frontmatter.manual_only,
        enabled: skill.enabled,
        body: skill.body,
        path: skill.path.display().to_string(),
    })
}

// --- Phase 8: Workers ---

#[tauri::command]
pub async fn list_workers(
    app: State<'_, Arc<AppState>>,
    limit: Option<u32>,
) -> Result<Vec<WorkerSnapshot>, String> {
    let dev = app.runtime.dev_dir();
    let mut items = worker_intent::list_snapshots(&dev).map_err(|e| e.to_string())?;
    if let Some(l) = limit {
        items.truncate(l as usize);
    }
    Ok(items)
}

#[tauri::command]
pub async fn get_worker(
    app: State<'_, Arc<AppState>>,
    id: String,
) -> Result<Option<WorkerSnapshot>, String> {
    let dev = app.runtime.dev_dir();
    worker_intent::read_snapshot(&dev, &id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn cancel_worker(app: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    let dev = app.runtime.dev_dir();
    worker_intent::write_intent(&dev, &WorkerIntent::Cancel { id })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn dismiss_worker(app: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    let dev = app.runtime.dev_dir();
    worker_intent::delete_snapshot(&dev, &id).map_err(|e| e.to_string())
}

/// State of the headless `continuum.exe` runtime from the dashboard's point of view.
#[derive(Serialize)]
pub struct RuntimeStatus {
    pub alive: bool,
    pub starting: bool,
    pub error: Option<String>,
    pub state_path: String,
    pub binary_path: Option<String>,
}

/// Shared state for the desktop-owned automatic runtime launch.
pub struct RuntimeStartupState {
    starting: AtomicBool,
    error: Mutex<Option<String>>,
    gate: tokio::sync::Mutex<()>,
}

impl RuntimeStartupState {
    /// Create an idle startup tracker.
    pub fn new() -> Self {
        Self {
            starting: AtomicBool::new(false),
            error: Mutex::new(None),
            gate: tokio::sync::Mutex::new(()),
        }
    }

    fn begin(&self) {
        self.starting.store(true, Ordering::Release);
        if let Ok(mut error) = self.error.lock() {
            *error = None;
        }
    }

    fn finish(&self, result: &Result<String, String>) {
        self.starting.store(false, Ordering::Release);
        if let Ok(mut error) = self.error.lock() {
            *error = result.as_ref().err().cloned();
        }
    }

    fn snapshot(&self) -> (bool, Option<String>) {
        let starting = self.starting.load(Ordering::Acquire);
        let error = self.error.lock().ok().and_then(|error| error.clone());
        (starting, error)
    }
}

impl Default for RuntimeStartupState {
    fn default() -> Self {
        Self::new()
    }
}

#[tauri::command]
pub async fn get_runtime_status(app: State<'_, Arc<AppState>>) -> Result<RuntimeStatus, String> {
    let dev_dir = app.runtime.dev_dir();
    let state_path = dev_dir.join("state.json");
    let alive = crate::components::runtime_alive(&dev_dir);
    let (starting, error) = app.runtime_startup.snapshot();
    Ok(RuntimeStatus {
        alive,
        starting: !alive && starting,
        error: if alive { None } else { error },
        state_path: state_path.to_string_lossy().into_owned(),
        binary_path: locate_runtime_binary().map(|p| p.to_string_lossy().into_owned()),
    })
}

/// Health of the low-latency runtime bridge to the running Continuum process.
///
/// Surfaces the two latches maintained by [`runtime_bridge::pipe`] on
/// Windows currently uses a named pipe. macOS uses the portable `state.json`
/// bridge, so the result reports `connected = false` and `pipe_name = None`
/// without treating the running runtime as unhealthy.
#[tauri::command]
pub async fn pipe_health() -> Result<PipeHealth, String> {
    Ok(runtime_bridge::current_pipe_health())
}

/// Start the packaged runtime in the background and keep one authoritative
/// startup state until a fresh heartbeat proves readiness.
pub(crate) fn spawn_automatic_runtime_start(app: Arc<AppState>, app_handle: AppHandle) {
    app.runtime_startup.begin();
    tokio::spawn(async move {
        let _guard = app.runtime_startup.gate.lock().await;
        let cfg = app.runtime.config_snapshot();
        let result = guarded_start_runtime(
            &app.runtime.dev_dir(),
            &continuum_core::config::continuum_backups_dir(),
            cfg.health.backup_retention.max(1),
            cfg.health.runtime_start_timeout_secs.clamp(10, 5 * 60),
            &app_handle,
        )
        .await;

        match &result {
            Ok(detail) => tracing::info!(
                layer = "desktop",
                component = "runtime_startup",
                %detail,
                "Automatic runtime startup completed"
            ),
            Err(error) => tracing::error!(
                layer = "desktop",
                component = "runtime_startup",
                %error,
                "Automatic runtime startup failed"
            ),
        }
        app.runtime_startup.finish(&result);
        let _ = app_handle.emit(
            "continuum:runtime_startup",
            serde_json::json!({
                "starting": false,
                "error": result.as_ref().err(),
            }),
        );
    });
}

pub(crate) fn bundled_binary_candidates(name: &str) -> Vec<std::path::PathBuf> {
    let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
    else {
        return Vec::new();
    };

    let mut candidates = vec![
        exe_dir.join(name),
        exe_dir.join("resources").join("bin").join(name),
    ];
    // Tauri puts resources at `Continuum.app/Contents/Resources` on macOS.
    // The desktop executable itself lives in `Contents/MacOS`.
    if let Some(contents_dir) = exe_dir.parent() {
        candidates.push(contents_dir.join("Resources").join("bin").join(name));
    }
    candidates
}

fn locate_runtime_binary() -> Option<std::path::PathBuf> {
    ["continuum.exe", "continuum"].into_iter().find_map(|name| {
        bundled_binary_candidates(name)
            .into_iter()
            .find(|candidate| candidate.exists())
    })
}

fn runtime_working_dir(binary: &std::path::Path) -> std::path::PathBuf {
    let binary_dir = binary.parent().unwrap_or_else(|| std::path::Path::new("."));
    // Packaged Tauri resources keep binaries in `Resources/bin` and runtime
    // assets in its parent. Development and portable installs keep them all
    // together, so preserve the binary directory there.
    if binary_dir.file_name().is_some_and(|name| name == "bin") {
        if let Some(resources_dir) = binary_dir.parent() {
            if resources_dir.join("config").is_dir()
                && resources_dir.join("prompts").is_dir()
                && resources_dir.join("skills").is_dir()
            {
                return resources_dir.to_path_buf();
            }
        }
    }
    binary_dir.to_path_buf()
}

// --- MCP tool registry (static manifest) ---

/// One entry in the static MCP-tool manifest the dashboard renders.
///
/// `continuum-mcp` runs as a separate process and exposes its tool list over
/// the MCP protocol. Spawning it on every dashboard open just to ask "what
/// tools do you have?" is wasteful — the tool surface is a published
/// contract (see `AGENTS.md` "Never break the public API of `continuum-mcp`"),
/// so we mirror the registered `#[tool]` functions in
/// `crates/continuum-mcp/src/server.rs` as a static manifest. When the
/// tool set changes, update both sides in the same commit.
#[derive(Debug, Clone, Serialize)]
pub struct McpTool {
    pub namespace: String,
    pub name: String,
    pub description: String,
}

/// Return the static manifest of MCP tools the orchestrator can call. Grouped
/// by namespace so the dashboard can render the existing "MCP tools" card
/// without any further client-side aggregation.
#[tauri::command]
pub async fn list_mcp_tools() -> Result<Vec<McpTool>, String> {
    Ok(mcp_tool_manifest())
}

fn permission_gateway(app: &AppState) -> PermissionGateway {
    PermissionGateway::new(
        app.runtime.dev_dir(),
        "desktop",
        include_str!("../../../../config/default-permissions.toml"),
    )
}

/// Return permission requests waiting for a user decision.
#[tauri::command]
pub async fn list_permission_requests(
    app: State<'_, Arc<AppState>>,
) -> Result<Vec<PermissionRequest>, String> {
    Ok(permission_gateway(&app).list_requests())
}

/// Return active permission grants so the user can revoke them.
#[tauri::command]
pub async fn list_permission_grants(
    app: State<'_, Arc<AppState>>,
) -> Result<Vec<PermissionGrant>, String> {
    Ok(permission_gateway(&app).list_grants())
}

/// Approve a pending permission request.
#[tauri::command]
pub async fn approve_permission_request(
    app: State<'_, Arc<AppState>>,
    request_id: String,
    scope: GrantScope,
) -> Result<PermissionGrant, String> {
    permission_gateway(&app)
        .approve(&request_id, scope, 8 * 60 * 60)
        .map_err(|error| error.to_string())
}

/// Deny a pending permission request.
#[tauri::command]
pub async fn deny_permission_request(
    app: State<'_, Arc<AppState>>,
    request_id: String,
) -> Result<(), String> {
    permission_gateway(&app)
        .deny_request(&request_id)
        .map_err(|error| error.to_string())
}

/// Revoke an active permission grant.
#[tauri::command]
pub async fn revoke_permission_grant(
    app: State<'_, Arc<AppState>>,
    grant_id: String,
) -> Result<(), String> {
    permission_gateway(&app)
        .revoke(&grant_id)
        .map_err(|error| error.to_string())
}

/// Input accepted by the local MCP-server registration flow.
#[derive(Debug, Deserialize)]
pub struct InstallMcpServerInput {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Return user-managed MCP servers that will be attached to the next agent run.
#[tauri::command]
pub async fn list_installed_mcp_servers(
    app: State<'_, Arc<AppState>>,
) -> Result<Vec<continuum_core::mcp_registry::McpServerRegistration>, String> {
    continuum_core::mcp_registry::list_servers(&app.runtime.dev_dir()).map_err(|error| {
        tracing::error!(
            layer = "orchestrator",
            component = "mcp_registry",
            error = %error,
            "Failed to list installed MCP servers"
        );
        error.to_string()
    })
}

/// Register an already-installed local stdio MCP server after validating that
/// its executable is available. No package manager or shell is invoked.
#[tauri::command]
pub async fn install_mcp_server(
    app: State<'_, Arc<AppState>>,
    input: InstallMcpServerInput,
) -> Result<continuum_core::mcp_registry::McpServerRegistration, String> {
    let InstallMcpServerInput {
        name,
        command,
        args,
    } = input;
    let result =
        continuum_core::mcp_registry::install_server(&app.runtime.dev_dir(), &name, &command, args);
    match result {
        Ok(server) => {
            tracing::info!(
                layer = "orchestrator",
                component = "mcp_registry",
                server = %server.name,
                command = %server.command,
                "Installed local MCP server registration"
            );
            Ok(server)
        }
        Err(error) => {
            tracing::warn!(
                layer = "orchestrator",
                component = "mcp_registry",
                server = %name,
                error = %error,
                "MCP server registration failed"
            );
            Err(error.to_string())
        }
    }
}

/// Single source of truth for the dashboard's MCP tool list. Keep in lockstep
/// with the `#[tool]` functions in `crates/continuum-mcp/src/server.rs`.
/// Wrapped in a function because `McpTool` owns `String`s, which can't be
/// built inside a `const` expression on stable Rust.
fn mcp_tool_manifest() -> Vec<McpTool> {
    vec![
        // --- memory ---
        McpTool {
            namespace: "memory".into(),
            name: "memory_query_episodic".into(),
            description: "Vector search over past events".into(),
        },
        McpTool {
            namespace: "memory".into(),
            name: "memory_list_facts".into(),
            description: "List facts (vault-first, legacy fallback)".into(),
        },
        McpTool {
            namespace: "memory".into(),
            name: "memory_get_fact".into(),
            description: "Fetch a fact by key (vault-first, legacy fallback)".into(),
        },
        McpTool {
            namespace: "memory".into(),
            name: "memory_set_fact".into(),
            description: "Write or update a fact (stored in the memory vault)".into(),
        },
        McpTool {
            namespace: "memory".into(),
            name: "memory_vault_search".into(),
            description: "Full-text search over memory vault notes".into(),
        },
        McpTool {
            namespace: "memory".into(),
            name: "memory_vault_get".into(),
            description: "Fetch a vault note by id (with backlinks)".into(),
        },
        McpTool {
            namespace: "memory".into(),
            name: "memory_vault_save".into(),
            description: "Create or update a confirmed vault note".into(),
        },
        McpTool {
            namespace: "memory".into(),
            name: "memory_vault_resolve".into(),
            description: "Resolve a candidate note (confirm/reject/supersede)".into(),
        },
        McpTool {
            namespace: "memory".into(),
            name: "memory_wipe_all".into(),
            description: "Queue a derived-memory wipe request (vault markdown untouched)".into(),
        },
        // --- system ---
        McpTool {
            namespace: "system".into(),
            name: "system_current_time".into(),
            description: "Local wall-clock time + timezone".into(),
        },
        McpTool {
            namespace: "system".into(),
            name: "system_active_window".into(),
            description: "Foreground window title + process name".into(),
        },
        McpTool {
            namespace: "system".into(),
            name: "system_live_context".into(),
            description: "Shared local monitor/window/activity/project world-state".into(),
        },
        McpTool {
            namespace: "system".into(),
            name: "system_clipboard_get".into(),
            description: "Read current clipboard text".into(),
        },
        McpTool {
            namespace: "system".into(),
            name: "system_notification".into(),
            description: "Show a Windows toast".into(),
        },
        // --- fs (read-only) ---
        McpTool {
            namespace: "fs".into(),
            name: "fs_read_file".into(),
            description: "Read up to 100 KB of a UTF-8 text file".into(),
        },
        McpTool {
            namespace: "fs".into(),
            name: "fs_list_dir".into(),
            description: "List up to 500 entries of a directory".into(),
        },
        McpTool {
            namespace: "fs".into(),
            name: "fs_create_file".into(),
            description: "Atomically create a new allowlisted UTF-8 file".into(),
        },
        McpTool {
            namespace: "fs".into(),
            name: "fs_apply_patch".into(),
            description: "Apply an exact-text patch and preserve the original".into(),
        },
        McpTool {
            namespace: "fs".into(),
            name: "fs_move".into(),
            description: "Move without overwriting the destination".into(),
        },
        McpTool {
            namespace: "fs".into(),
            name: "fs_delete_to_trash".into(),
            description: "Move a file or directory into recovery storage".into(),
        },
        // --- git ---
        McpTool {
            namespace: "git".into(),
            name: "git_checkpoint".into(),
            description: "Checkpoint tracked and safe untracked repository state".into(),
        },
        McpTool {
            namespace: "git".into(),
            name: "git_diff".into(),
            description: "Show bounded status and unified diff".into(),
        },
        McpTool {
            namespace: "git".into(),
            name: "git_checkpoint_list".into(),
            description: "List durable Continuum checkpoint refs".into(),
        },
        McpTool {
            namespace: "git".into(),
            name: "git_rollback".into(),
            description: "Recoverably restore a confirmed checkpoint".into(),
        },
        // --- terminal ---
        McpTool {
            namespace: "terminal".into(),
            name: "terminal_run".into(),
            description: "Run a restricted confirmed program + args invocation".into(),
        },
        McpTool {
            namespace: "terminal".into(),
            name: "terminal_verify".into(),
            description: "Run a verifier and persist bounded evidence".into(),
        },
        // --- native IDE bridge ---
        McpTool {
            namespace: "ide".into(),
            name: "ide_status".into(),
            description: "Check configured native editor availability".into(),
        },
        McpTool {
            namespace: "ide".into(),
            name: "ide_open_file".into(),
            description: "Open an allowlisted file at a source location".into(),
        },
        McpTool {
            namespace: "ide".into(),
            name: "ide_open_diff".into(),
            description: "Show two allowlisted files in the native IDE diff view".into(),
        },
        // --- opt-in browser DOM bridge ---
        McpTool {
            namespace: "browser".into(),
            name: "browser_status".into(),
            description: "Check loopback Chromium bridge status".into(),
        },
        McpTool {
            namespace: "browser".into(),
            name: "browser_list_tabs".into(),
            description: "List explicitly allowed Chromium tabs".into(),
        },
        McpTool {
            namespace: "browser".into(),
            name: "browser_dom_snapshot".into(),
            description: "Read bounded visible DOM text and form structure".into(),
        },
        McpTool {
            namespace: "browser".into(),
            name: "browser_navigate".into(),
            description: "Navigate a tab to an allowed host after confirmation".into(),
        },
        McpTool {
            namespace: "browser".into(),
            name: "browser_click".into(),
            description: "Click a DOM element after confirmation".into(),
        },
        McpTool {
            namespace: "browser".into(),
            name: "browser_fill".into(),
            description: "Fill a non-password field after confirmation".into(),
        },
        McpTool {
            namespace: "windows_ui".into(),
            name: "windows_ui_focused_element".into(),
            description: "Inspect focused accessibility metadata".into(),
        },
        McpTool {
            namespace: "windows_ui".into(),
            name: "windows_ui_invoke_focused".into(),
            description: "Invoke the focused semantic control after confirmation".into(),
        },
        McpTool {
            namespace: "windows_ui".into(),
            name: "windows_ui_set_focused_value".into(),
            description: "Fill the focused non-password control after confirmation".into(),
        },
        McpTool {
            namespace: "tasks".into(),
            name: "task_plan_write".into(),
            description: "Create or update a durable task plan".into(),
        },
        McpTool {
            namespace: "tasks".into(),
            name: "task_plan_get".into(),
            description: "Read a durable task plan".into(),
        },
        McpTool {
            namespace: "tasks".into(),
            name: "task_plan_list".into(),
            description: "List durable task plans".into(),
        },
        McpTool {
            namespace: "evidence".into(),
            name: "evidence_record".into(),
            description: "Persist bounded agent evidence".into(),
        },
        McpTool {
            namespace: "evidence".into(),
            name: "evidence_list".into(),
            description: "List durable agent evidence".into(),
        },
        // --- github (optional connection) ---
        McpTool {
            namespace: "github".into(),
            name: "github_status".into(),
            description: "Check secure official GitHub CLI auth status".into(),
        },
        McpTool {
            namespace: "github".into(),
            name: "github_me".into(),
            description: "Read the connected GitHub user profile".into(),
        },
        McpTool {
            namespace: "github".into(),
            name: "github_list_repos".into(),
            description: "List repositories visible to the connected account".into(),
        },
        McpTool {
            namespace: "github".into(),
            name: "github_get_repo".into(),
            description: "Read repository metadata".into(),
        },
        McpTool {
            namespace: "github".into(),
            name: "github_list_issues".into(),
            description: "List repository issues and pull requests".into(),
        },
        McpTool {
            namespace: "github".into(),
            name: "github_get_file".into(),
            description: "Read a UTF-8 repository file or directory listing".into(),
        },
        McpTool {
            namespace: "github".into(),
            name: "github_create_issue".into(),
            description: "Create a bounded issue after explicit confirmation".into(),
        },
        McpTool {
            namespace: "github".into(),
            name: "github_comment_issue".into(),
            description: "Comment after explicit confirmation".into(),
        },
        McpTool {
            namespace: "github".into(),
            name: "github_create_pull_request".into(),
            description: "Open a pull request after explicit confirmation".into(),
        },
        // --- web ---
        McpTool {
            namespace: "web".into(),
            name: "web_fetch".into(),
            description: "HTTP GET, 50 KB cap, public IPs only, no redirects".into(),
        },
        // --- repair (repair-agent only) ---
        McpTool {
            namespace: "repair".into(),
            name: "repair_restart_component".into(),
            description: "Compatibility tool; denied unless an execution path is authorized".into(),
        },
        McpTool {
            namespace: "repair".into(),
            name: "repair_reinstall_model".into(),
            description: "Re-download a model for a component".into(),
        },
        McpTool {
            namespace: "repair".into(),
            name: "repair_rollback_config".into(),
            description: "Rollback config.toml from a dated backup".into(),
        },
        McpTool {
            namespace: "repair".into(),
            name: "repair_test_component".into(),
            description: "Quick file-presence sanity check".into(),
        },
        McpTool {
            namespace: "repair".into(),
            name: "repair_escalate".into(),
            description: "Surface a manual-intervention banner to the dashboard".into(),
        },
        // --- workers ---
        McpTool {
            namespace: "workers".into(),
            name: "workers_spawn_worker".into(),
            description: "Queue a new Claude Code worker".into(),
        },
        McpTool {
            namespace: "workers".into(),
            name: "workers_worker_status".into(),
            description: "Poll a worker's snapshot".into(),
        },
        McpTool {
            namespace: "workers".into(),
            name: "workers_worker_cancel".into(),
            description: "Stop a running or queued worker".into(),
        },
        McpTool {
            namespace: "workers".into(),
            name: "workers_worker_wait".into(),
            description: "Block until a worker reaches a terminal state".into(),
        },
        McpTool {
            namespace: "workers".into(),
            name: "workers_worker_list".into(),
            description: "List recent worker snapshots".into(),
        },
    ]
}

#[cfg(test)]
mod health_repair_tests {
    use super::*;

    fn component(name: &str, status: ComponentStatus) -> ComponentHealth {
        ComponentHealth {
            name: name.into(),
            status,
            last_check_ts: None,
            last_error: None,
            error_count_24h: 0,
            avg_response_ms: None,
            log_path: None,
            recovery_note: None,
        }
    }

    #[test]
    fn preview_includes_only_actionable_runtime_unknown_and_real_failures() {
        assert!(is_preview_repair_issue(&component(
            "runtime",
            ComponentStatus::Unknown
        )));
        assert!(!is_preview_repair_issue(&component(
            "vision",
            ComponentStatus::Unknown
        )));
        assert!(is_preview_repair_issue(&component(
            "vision",
            ComponentStatus::Error
        )));
        assert!(is_preview_repair_issue(&component(
            "memory",
            ComponentStatus::Degrading
        )));
        assert!(!is_preview_repair_issue(&component(
            "mcp",
            ComponentStatus::Healthy
        )));
    }

    #[test]
    fn only_offline_runtime_has_a_direct_mutating_action() {
        assert_eq!(SAFE_DIRECT_TARGETS, &["runtime"]);
        assert!(!SAFE_DIRECT_TARGETS.contains(&"vision"));
    }

    async fn state() -> (tempfile::TempDir, MemoryState) {
        let tmp = tempfile::tempdir().unwrap();
        let s = MemoryState::new(
            tmp.path().join("vault"),
            tmp.path().join("semantic.sqlite"),
            tmp.path().join("dev"),
        );
        s.vault().await.unwrap(); // force init
        (tmp, s)
    }

    /// Task 7's real derived-data wipe path: `wipe_memory` must write the
    /// `wipe-request.json` contract file into `dev_dir` (for the headless
    /// runtime's boot drain / daily hygiene tick to pick up — it owns
    /// `RawLog`/`EpisodicStore`, which this dashboard process cannot touch
    /// directly) *and* clear the piece it can reach immediately: the
    /// vault's own timeline events, via `prune_events(0)`.
    #[tokio::test]
    async fn wipe_memory_writes_request_and_clears_events() {
        let (tmp, s) = state().await;
        let vault = s.vault().await.unwrap();
        vault
            .append_event(continuum_memory::NewEvent {
                ts: None,
                kind: "distilled".to_string(),
                text: "an event".to_string(),
                project: None,
                node_id: None,
                reference: None,
                local_only: false,
            })
            .await
            .unwrap();
        assert_eq!(
            vault
                .events(&continuum_memory::EventRange::default())
                .await
                .unwrap()
                .len(),
            1
        );

        wipe_memory_inner(&s, "DELETE").await.unwrap();

        let request_path = tmp.path().join("dev").join("wipe-request.json");
        assert!(request_path.exists());
        let raw = std::fs::read_to_string(&request_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            parsed["scopes"],
            serde_json::json!(["raw_log", "episodic", "events"])
        );
        assert!(parsed["requested_at"].is_string());

        // The piece this command can reach directly (vault events) is
        // cleared right away, in-app — no need to wait on the runtime.
        assert_eq!(
            vault
                .events(&continuum_memory::EventRange::default())
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn wipe_memory_rejects_wrong_confirmation() {
        let (tmp, s) = state().await;
        let err = wipe_memory_inner(&s, "not delete").await.unwrap_err();
        assert!(err.contains("DELETE"));
        // No request file, no side effects, on a rejected confirmation.
        assert!(!tmp.path().join("dev").join("wipe-request.json").exists());
    }
}
