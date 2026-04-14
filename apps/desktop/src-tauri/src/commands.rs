//! Tauri command handlers — the request/response IPC surface.
//!
//! These handlers live on the Tauri side so they can `.await` against
//! kairo-core handles directly. Long-running work (memory search, repair
//! agent) spawns into the tokio runtime the Tauri app owns.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use kairo_core::automations::{Automation, AutomationInput};
use kairo_core::config::KairoConfig;
use kairo_core::health::{self, repair::RepairInput};
use kairo_core::logs::{LogEntry, LogFilter};
use kairo_core::skills::{self, SkillFrontmatter, SkillLoader};
use kairo_core::state::{ComponentHealth, KairoState};
use kairo_core::workers::intent::{self as worker_intent};
use kairo_core::workers::{WorkerIntent, WorkerSnapshot};

use crate::AppState;

/// Full state snapshot. The dashboard calls this once on mount and then
/// listens to `kairo:state` events for updates.
#[tauri::command]
pub async fn get_state(app: State<'_, Arc<AppState>>) -> Result<KairoState, String> {
    Ok(app.runtime.state.snapshot().await)
}

#[tauri::command]
pub async fn get_config(app: State<'_, Arc<AppState>>) -> Result<KairoConfig, String> {
    Ok(app.runtime.config_snapshot())
}

#[tauri::command]
pub async fn update_voice_volume(
    app: State<'_, Arc<AppState>>,
    volume: f32,
) -> Result<KairoConfig, String> {
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
) -> Result<KairoConfig, String> {
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
) -> Result<KairoConfig, String> {
    app.runtime
        .update_config(|c| c.screen.interval_secs = seconds.clamp(1, 30))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_triage_threshold(
    app: State<'_, Arc<AppState>>,
    threshold: f32,
) -> Result<KairoConfig, String> {
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
/// ships with the kairo-mcp `memory_query_episodic` tool — the dashboard
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
    // Actual wipe is performed by the kairo runtime if running; we just
    // log the request here. A follow-up PR will extend kairo-mcp with a
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
        let cb = move |ev: kairo_core::health::repair::RepairEvent| {
            let _ = emit_handle.emit("kairo:repair", ev);
        };

        if let Err(e) = health::repair::run_repair(input, cb).await {
            let _ = app_handle.emit(
                "kairo:repair",
                kairo_core::health::repair::RepairEvent::Error {
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
    let backups_dir = dev_dir
        .parent()
        .map(|p| p.join(".kairo-backups"))
        .unwrap_or_else(|| dev_dir.join(".kairo-backups"));
    let result = kairo_core::health::backup::run_backup(&dev_dir, &backups_dir)
        .map_err(|e| e.to_string())?;
    let _ = kairo_core::health::backup::prune_backups(
        &backups_dir,
        kairo_core::health::backup::DEFAULT_RETENTION,
    );
    let latest = kairo_core::health::backup::latest_backup_ts(&backups_dir);
    let count = kairo_core::health::backup::count_backups(&backups_dir);
    app.runtime.state.set_backup_status(latest, count).await;
    Ok(result.path.display().to_string())
}

#[tauri::command]
pub async fn rollback_config(app: State<'_, Arc<AppState>>, date: String) -> Result<(), String> {
    let dev_dir = app.runtime.dev_dir();
    let backups_dir = dev_dir
        .parent()
        .map(|p| p.join(".kairo-backups"))
        .unwrap_or_else(|| dev_dir.join(".kairo-backups"));
    kairo_core::health::repair::rollback_config(&dev_dir, &backups_dir, &date)
        .map(|_| ())
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
    let root = skills_root(&app);
    skills::delete_skill(&root, &name).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn toggle_skill(
    app: State<'_, Arc<AppState>>,
    name: String,
    enabled: bool,
) -> Result<KairoConfig, String> {
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
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("URL must not be empty".into());
    }
    if !trimmed.starts_with("https://") && !trimmed.starts_with("git@") {
        return Err("Only https:// or git@ URLs are allowed".into());
    }
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let clone_target = tmp.path().join("skill");
    let status = std::process::Command::new("git")
        .arg("clone")
        .arg("--depth")
        .arg("1")
        .arg(trimmed)
        .arg(&clone_target)
        .status()
        .map_err(|e| format!("failed to invoke git: {e}"))?;
    if !status.success() {
        return Err("git clone exited with a non-zero status".into());
    }
    let skill_md = clone_target.join("SKILL.md");
    if !skill_md.exists() {
        return Err("cloned repo does not contain a SKILL.md at its root".into());
    }
    let parsed = skills::parse_skill_file(&skill_md).map_err(|e| e.to_string())?;
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
