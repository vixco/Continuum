//! Default health-check registrations for the Kairo runtime.
//!
//! These probes intentionally don't block on heavy work — they mostly
//! check the state-store flags set by the headless `kairo` binary (when
//! it's running) or verify on-disk model artefacts are present. That
//! keeps the dashboard usable even when the main runtime isn't active,
//! and the Health tab displays `Unknown` for subsystems whose owner
//! process hasn't published yet.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use kairo_core::config::KairoConfig;
use kairo_core::health::{HealthCheck, HealthRegistry, HealthResult};
use kairo_core::runtime::KairoRuntime;
use kairo_core::state::StateHandle;

/// Directory where kairo-desktop.exe was started from. Used by checks that
/// need to find sibling files (kairo-mcp.exe, prompts/, skills/) that the
/// installer places next to the binaries.
fn install_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
}

/// True if `kairo.exe` has touched `~/.kairo-dev/state.json` recently.
/// Used to avoid screaming "Degrading" across every model/voice/orchestrator
/// probe when the runtime simply isn't running (the common case for dashboard-only
/// installs).
fn runtime_alive(dev_dir: &Path) -> bool {
    let state_path = dev_dir.join("state.json");
    let Ok(meta) = std::fs::metadata(&state_path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    std::time::SystemTime::now()
        .duration_since(modified)
        .map(|age| age.as_secs() < 10)
        .unwrap_or(false)
}

pub fn register_default(registry: &HealthRegistry, runtime: &KairoRuntime) {
    let state = Arc::new(RwLock::new(runtime.state.clone()));
    let cfg = Arc::new(runtime.config_snapshot());
    let dev_dir = runtime.dev_dir();

    registry.register(RuntimeCheck {
        dev_dir: dev_dir.clone(),
    });
    registry.register(VisionCheck {
        state: state.clone(),
        cfg: cfg.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(TriageCheck {
        state: state.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(OrchestratorCheck {
        state: state.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(VoiceTtsCheck {
        state: state.clone(),
        cfg: cfg.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(VoiceSttCheck {
        state: state.clone(),
        cfg: cfg.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(MemoryCheck {
        state: state.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(McpCheck {});
    registry.register(ContextCheck {
        state: state.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(WorkersCheck {
        dev_dir: dev_dir.clone(),
    });
    registry.register(SkillsCheck {
        cfg: cfg.clone(),
        dev_dir: dev_dir.clone(),
    });
}

async fn snap(state: &Arc<RwLock<StateHandle>>) -> kairo_core::state::KairoState {
    state.read().await.snapshot().await
}

/// Heartbeat: is the headless `kairo.exe` runtime currently alive?
/// Everything else downgrades to Unknown when this is offline, because
/// the state snapshot the dashboard reads is stale / default.
struct RuntimeCheck {
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for RuntimeCheck {
    fn name(&self) -> &str {
        "runtime"
    }
    fn recovery_note(&self) -> Option<String> {
        Some(
            "Start the Kairo runtime: launch `kairo.exe` (it lives next to kairo-desktop.exe)."
                .into(),
        )
    }
    async fn probe(&self) -> HealthResult {
        if runtime_alive(&self.dev_dir) {
            HealthResult::healthy(1)
        } else {
            HealthResult::unknown(
                "runtime process is not publishing state — start kairo.exe",
                1,
            )
        }
    }
}

struct VisionCheck {
    state: Arc<RwLock<StateHandle>>,
    cfg: Arc<KairoConfig>,
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for VisionCheck {
    fn name(&self) -> &str {
        "vision"
    }
    fn log_path(&self) -> Option<String> {
        Some("~/.kairo-dev/logs/kairo.log".into())
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Re-run scripts/download-models.ps1 to reinstall SmolVLM.".into())
    }
    async fn probe(&self) -> HealthResult {
        let model_path = PathBuf::from(&self.cfg.vision.model_path);
        if !model_path.exists() {
            return HealthResult::error("vision model missing on disk", 1);
        }
        if !runtime_alive(&self.dev_dir) {
            return HealthResult::unknown("runtime offline", 1);
        }
        let snap = snap(&self.state).await;
        if snap.system.vision_model_loaded {
            HealthResult::healthy(1)
        } else {
            HealthResult::degrading("vision model not loaded yet", 1)
        }
    }
}

struct TriageCheck {
    state: Arc<RwLock<StateHandle>>,
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for TriageCheck {
    fn name(&self) -> &str {
        "triage"
    }
    fn log_path(&self) -> Option<String> {
        Some("~/.kairo-dev/logs/kairo.log".into())
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Re-download the triage GGUF via scripts/download-models.ps1.".into())
    }
    async fn probe(&self) -> HealthResult {
        let model_path = self
            .dev_dir
            .join("models")
            .join("triage")
            .join("qwen3-8b-q4_k_m.gguf");
        if !model_path.exists() {
            return HealthResult::error("triage GGUF missing on disk", 1);
        }
        if !runtime_alive(&self.dev_dir) {
            return HealthResult::unknown("runtime offline", 1);
        }
        let snap = snap(&self.state).await;
        if snap.system.triage_model_loaded {
            HealthResult::healthy(1)
        } else {
            HealthResult::degrading("triage model not yet loaded", 1)
        }
    }
}

struct OrchestratorCheck {
    state: Arc<RwLock<StateHandle>>,
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for OrchestratorCheck {
    fn name(&self) -> &str {
        "orchestrator"
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Install the Claude Code CLI (`claude --version`) or re-run setup.".into())
    }
    async fn probe(&self) -> HealthResult {
        if !runtime_alive(&self.dev_dir) {
            return HealthResult::unknown("runtime offline", 1);
        }
        let snap = snap(&self.state).await;
        if snap.system.orchestrator_ready {
            HealthResult::healthy(1)
        } else if let Some(ts) = snap.orchestrator.last_wake_ts {
            let age = chrono::Utc::now().signed_duration_since(ts).num_seconds();
            if age < 60 * 60 {
                HealthResult::healthy(1)
            } else {
                HealthResult::degrading("orchestrator has not been woken in the last hour", 1)
            }
        } else {
            HealthResult::degrading("orchestrator never woken", 1)
        }
    }
}

struct VoiceTtsCheck {
    state: Arc<RwLock<StateHandle>>,
    cfg: Arc<KairoConfig>,
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for VoiceTtsCheck {
    fn name(&self) -> &str {
        "tts"
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Verify Piper voice files exist, rerun scripts/download-models.ps1.".into())
    }
    async fn probe(&self) -> HealthResult {
        if !self.cfg.tts.enabled {
            return HealthResult::healthy(1);
        }
        let Some(primary) = self.cfg.tts.voices.get(&self.cfg.tts.primary) else {
            return HealthResult::error("TTS primary voice key missing from config", 1);
        };
        if !PathBuf::from(&primary.model_path).exists() {
            return HealthResult::error("Piper model file missing", 1);
        }
        if !runtime_alive(&self.dev_dir) {
            return HealthResult::unknown("runtime offline", 1);
        }
        let snap = snap(&self.state).await;
        if snap.system.tts_loaded {
            HealthResult::healthy(1)
        } else {
            HealthResult::degrading("TTS engine not loaded yet", 1)
        }
    }
}

struct VoiceSttCheck {
    state: Arc<RwLock<StateHandle>>,
    cfg: Arc<KairoConfig>,
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for VoiceSttCheck {
    fn name(&self) -> &str {
        "stt"
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Verify whisper model at audio.whisper_model_path.".into())
    }
    async fn probe(&self) -> HealthResult {
        if !self.cfg.audio.enabled {
            return HealthResult::healthy(1);
        }
        if !PathBuf::from(&self.cfg.audio.whisper_model_path).exists() {
            return HealthResult::error("whisper model file missing", 1);
        }
        if !runtime_alive(&self.dev_dir) {
            return HealthResult::unknown("runtime offline", 1);
        }
        let snap = snap(&self.state).await;
        if snap.system.stt_loaded {
            HealthResult::healthy(1)
        } else {
            HealthResult::degrading("STT not yet active", 1)
        }
    }
}

struct MemoryCheck {
    state: Arc<RwLock<StateHandle>>,
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for MemoryCheck {
    fn name(&self) -> &str {
        "memory"
    }
    async fn probe(&self) -> HealthResult {
        let raw = self.dev_dir.join("raw_log.sqlite");
        if !raw.exists() {
            return HealthResult::degrading("raw log DB not yet created", 1);
        }
        if !runtime_alive(&self.dev_dir) {
            return HealthResult::unknown("runtime offline", 1);
        }
        let snap = snap(&self.state).await;
        if snap.memory.raw_log_rows == 0 && snap.memory.episodic_count == 0 {
            HealthResult::degrading("memory has no rows yet", 1)
        } else {
            HealthResult::healthy(1)
        }
    }
}

struct McpCheck;

#[async_trait]
impl HealthCheck for McpCheck {
    fn name(&self) -> &str {
        "mcp"
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Rebuild kairo-mcp: `cargo build --release -p kairo-mcp`".into())
    }
    async fn probe(&self) -> HealthResult {
        // kairo-mcp is spawned on demand by the orchestrator. Presence of
        // the binary on disk is the best boot-time signal available. Check
        // both packaged installs (next to kairo-desktop.exe) and local dev
        // layouts (target/release/... from a repo cwd).
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(dir) = install_dir() {
            candidates.push(dir.join("kairo-mcp.exe"));
            candidates.push(dir.join("kairo-mcp"));
        }
        candidates.extend(
            [
                "target/release/kairo-mcp.exe",
                "target/release/kairo-mcp",
                "target/debug/kairo-mcp.exe",
                "target/debug/kairo-mcp",
            ]
            .iter()
            .map(PathBuf::from),
        );
        if candidates.iter().any(|p| p.exists()) {
            HealthResult::healthy(1)
        } else {
            HealthResult::degrading("kairo-mcp binary not found", 1)
        }
    }
}

struct WorkersCheck {
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for WorkersCheck {
    fn name(&self) -> &str {
        "workers"
    }
    fn log_path(&self) -> Option<String> {
        Some("~/.kairo-dev/logs/kairo.log".into())
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Check workers/*.json under the Kairo data dir; restart kairo runtime.".into())
    }
    async fn probe(&self) -> HealthResult {
        let snaps = match kairo_core::workers::intent::list_snapshots(&self.dev_dir) {
            Ok(s) => s,
            Err(e) => return HealthResult::error(e.to_string(), 1),
        };
        let now = chrono::Utc::now();
        let recent_window = chrono::Duration::minutes(10);
        let recent_failed = snaps
            .iter()
            .filter(|s| {
                matches!(
                    s.status,
                    kairo_core::workers::WorkerStatus::Failed
                        | kairo_core::workers::WorkerStatus::TimedOut
                )
            })
            .filter(|s| {
                s.finished_at
                    .map(|ts| now.signed_duration_since(ts) < recent_window)
                    .unwrap_or(false)
            })
            .count();
        if recent_failed >= 3 {
            HealthResult::error(
                format!("{recent_failed} workers failed in the last 10 minutes"),
                1,
            )
        } else if recent_failed > 0 {
            HealthResult::degrading(
                format!("{recent_failed} worker failure(s) in the last 10 minutes"),
                1,
            )
        } else {
            HealthResult::healthy(1)
        }
    }
}

struct SkillsCheck {
    cfg: Arc<KairoConfig>,
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for SkillsCheck {
    fn name(&self) -> &str {
        "skills"
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Fix or remove any SKILL.md that failed to parse. See logs.".into())
    }
    async fn probe(&self) -> HealthResult {
        if !self.cfg.skills.enabled {
            return HealthResult::healthy(1);
        }
        let configured = std::path::PathBuf::from(&self.cfg.skills.dir);
        let root = if configured.is_absolute() && configured.exists() {
            configured
        } else {
            // Try in order: cwd-relative, install-dir-relative (next to the
            // exe — covers packaged installs), dev-dir-relative.
            let rel = &self.cfg.skills.dir;
            std::env::current_dir()
                .ok()
                .map(|cwd| cwd.join(rel))
                .filter(|p| p.exists())
                .or_else(|| install_dir().map(|d| d.join(rel)).filter(|p| p.exists()))
                .unwrap_or_else(|| self.dev_dir.join(rel))
        };
        let loader = kairo_core::skills::SkillLoader::new(&root);
        if let Err(e) = loader.reload() {
            return HealthResult::error(e.to_string(), 1);
        }
        let errors = loader.errors();
        if !errors.is_empty() {
            return HealthResult::error(
                format!("{} skill file(s) failed to parse", errors.len()),
                1,
            );
        }
        let count = loader.list().len();
        if count == 0 {
            HealthResult::degrading("no skills loaded", 1)
        } else {
            HealthResult::healthy(1)
        }
    }
}

struct ContextCheck {
    state: Arc<RwLock<StateHandle>>,
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for ContextCheck {
    fn name(&self) -> &str {
        "context_watcher"
    }
    async fn probe(&self) -> HealthResult {
        if !runtime_alive(&self.dev_dir) {
            return HealthResult::unknown("runtime offline", 1);
        }
        let snap = snap(&self.state).await;
        if let Some(ts) = snap.perception.last_frame_ts {
            let age = chrono::Utc::now().signed_duration_since(ts).num_seconds();
            if age < 15 {
                HealthResult::healthy(1)
            } else if age < 60 {
                HealthResult::degrading("no perception frames in 15s", 1)
            } else {
                HealthResult::error("perception stalled", 1)
            }
        } else {
            HealthResult::degrading("no perception frames yet", 1)
        }
    }
}
