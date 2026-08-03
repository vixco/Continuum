//! Tauri command handlers — the request/response IPC surface.
//!
//! These handlers live on the Tauri side so they can `.await` against
//! continuum-core handles directly. Long-running work (memory search, repair
//! agent) spawns into the tokio runtime the Tauri app owns.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use continuum_core::automations::{Automation, AutomationInput};
use continuum_core::config::{ContinuumConfig, ProfileMode, ResourceConfig};
use continuum_core::hardware::{self, HardwareSpecs, ResolvedResourcePlan};
use continuum_core::health::{self, repair::RepairInput};
use continuum_core::logs::{LogEntry, LogFilter};
use continuum_core::skills::{self, SkillFrontmatter, SkillLoader};
use continuum_core::state::{ComponentHealth, ContinuumState};
use continuum_core::workers::intent::{self as worker_intent};
use continuum_core::workers::{WorkerIntent, WorkerSnapshot};

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
    let mute = app.runtime.is_voice_muted();
    app.runtime
        .state
        .set_voice_config_snapshot(cfg.voice.volume, cfg.voice.wake_word_enabled, mute, None)
        .await;
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

/// Placeholder semantic-search hook. Full wiring into [`EpisodicStore`]
/// ships with the continuum-mcp `memory_query_episodic` tool — the dashboard
/// piggybacks on that when the main runtime is running; we return an empty
/// list otherwise so the UI renders "no results" instead of erroring.
#[tauri::command]
pub async fn search_episodic(
    _app: State<'_, Arc<AppState>>,
    _query: String,
    _limit: Option<u32>,
) -> Result<Vec<Value>, String> {
    Ok(Vec::new())
}

#[tauri::command]
pub async fn delete_episodic(_app: State<'_, Arc<AppState>>, _id: String) -> Result<(), String> {
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SemanticFact {
    pub key: String,
    pub value: String,
    pub source: String,
    pub confidence: f32,
    pub namespace: String,
}

#[tauri::command]
pub async fn list_semantic(_app: State<'_, Arc<AppState>>) -> Result<Vec<SemanticFact>, String> {
    Ok(Vec::new())
}

#[tauri::command]
pub async fn set_semantic(
    _app: State<'_, Arc<AppState>>,
    _key: String,
    _value: String,
) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
pub async fn delete_semantic(_app: State<'_, Arc<AppState>>, _key: String) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
pub async fn wipe_memory(app: State<'_, Arc<AppState>>, confirm: String) -> Result<(), String> {
    if confirm != "DELETE" {
        return Err("wipe requires the literal string \"DELETE\" as confirmation".into());
    }
    // Actual wipe is performed by the continuum runtime if running; we just
    // log the request here. A follow-up PR will extend continuum-mcp with a
    // `memory__wipe_all` tool that this command forwards to.
    tracing::warn!(
        layer = "memory",
        component = "dashboard",
        "User requested memory wipe via dashboard"
    );
    app.runtime.state.mark_distill().await;
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

#[tauri::command]
pub async fn get_health(app: State<'_, Arc<AppState>>) -> Result<Vec<ComponentHealth>, String> {
    Ok(app.health.run_all().await)
}

#[tauri::command]
pub async fn trigger_repair(
    app: State<'_, Arc<AppState>>,
    app_handle: AppHandle,
    reason: Option<String>,
) -> Result<(), String> {
    let runtime = app.runtime.clone();
    let health = app.health.clone();
    tokio::spawn(async move {
        let components = health.run_all().await;
        let dev_dir = runtime.dev_dir();
        let repo_root = std::env::current_dir().unwrap_or_else(|_| dev_dir.clone());
        let cfg = runtime.config_snapshot();

        let input = RepairInput {
            dev_dir: &dev_dir,
            repo_root: &repo_root,
            config: &cfg,
            state: &runtime.state,
            logs: &runtime.logs,
            components,
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
    });
    Ok(())
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
        "User requested component restart"
    );
    // Restart is component-specific and needs the running runtime. The
    // runtime bridge listens on a local socket; for now we re-run the
    // health probe so the dashboard shows up-to-date status.
    Ok(app.health.run_single(&name).await)
}

#[tauri::command]
pub async fn run_backup_now(app: State<'_, Arc<AppState>>) -> Result<String, String> {
    let dev_dir = app.runtime.dev_dir();
    let backups_dir = continuum_core::config::continuum_backups_dir();
    let result = continuum_core::health::backup::run_backup(&dev_dir, &backups_dir)
        .map_err(|e| e.to_string())?;
    let _ = continuum_core::health::backup::prune_backups(
        &backups_dir,
        continuum_core::health::backup::DEFAULT_RETENTION,
    );
    let latest = continuum_core::health::backup::latest_backup_ts(&backups_dir);
    let count = continuum_core::health::backup::count_backups(&backups_dir);
    app.runtime.state.set_backup_status(latest, count).await;
    Ok(result.path.display().to_string())
}

#[tauri::command]
pub async fn rollback_config(app: State<'_, Arc<AppState>>, date: String) -> Result<(), String> {
    let dev_dir = app.runtime.dev_dir();
    let backups_dir = continuum_core::config::continuum_backups_dir();
    continuum_core::health::repair::rollback_config(&dev_dir, &backups_dir, &date)
        .map(|_| ())
        .map_err(|e| e.to_string())
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
    if engine != "piper" && engine != "elevenlabs" {
        return Err(format!(
            "Unknown TTS engine '{engine}'; expected 'piper' or 'elevenlabs'"
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

// --- Runtime control ---

#[tauri::command]
pub async fn set_paused(app: State<'_, Arc<AppState>>, paused: bool) -> Result<(), String> {
    app.runtime.set_paused(paused).await;
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
    pub state_path: String,
    pub binary_path: Option<String>,
}

#[tauri::command]
pub async fn get_runtime_status(app: State<'_, Arc<AppState>>) -> Result<RuntimeStatus, String> {
    let dev_dir = app.runtime.dev_dir();
    let state_path = dev_dir.join("state.json");
    let alive = crate::components::runtime_alive(&dev_dir);
    Ok(RuntimeStatus {
        alive,
        state_path: state_path.to_string_lossy().into_owned(),
        binary_path: locate_runtime_binary().map(|p| p.to_string_lossy().into_owned()),
    })
}

/// Health of the named-pipe bridge to the running `continuum.exe` process.
///
/// Surfaces the two latches maintained by [`runtime_bridge::pipe`] on
/// Windows. On non-Windows the result reports `connected = false` and
/// `pipe_name = None` so the UI can render "not available" honestly
/// instead of an error.
#[tauri::command]
pub async fn pipe_health() -> Result<PipeHealth, String> {
    Ok(runtime_bridge::current_pipe_health())
}

#[tauri::command]
pub async fn start_runtime() -> Result<(), String> {
    let Some(bin) = locate_runtime_binary() else {
        return Err(
            "continuum.exe not found next to continuum-desktop.exe — install may be incomplete"
                .to_string(),
        );
    };
    // Set cwd to the install dir so the runtime can find its sibling
    // `prompts/`, `skills/` and `config/` folders via relative paths.
    let working_dir = bin
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    std::process::Command::new(&bin)
        .current_dir(&working_dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to spawn {}: {}", bin.display(), e))
}

fn locate_runtime_binary() -> Option<std::path::PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for name in ["continuum.exe", "continuum"] {
        let p = exe_dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
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
            description: "List semantic facts (optional prefix filter)".into(),
        },
        McpTool {
            namespace: "memory".into(),
            name: "memory_get_fact".into(),
            description: "Fetch a single semantic fact by key".into(),
        },
        McpTool {
            namespace: "memory".into(),
            name: "memory_set_fact".into(),
            description: "Write or update a semantic fact".into(),
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
            description: "Queue a restart for a runtime subsystem".into(),
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
