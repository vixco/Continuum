//! Default health-check registrations for the Continuum runtime.
//!
//! These probes intentionally don't block on heavy work — they mostly
//! check the state-store flags set by the headless `continuum` binary (when
//! it's running) or verify on-disk model artefacts are present. That
//! keeps the dashboard usable even when the main runtime isn't active,
//! and the Health tab displays `Unknown` for subsystems whose owner
//! process hasn't published yet.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use sysinfo::System;
use tokio::sync::RwLock;

use continuum_core::config::ContinuumConfig;
use continuum_core::hardware::{self, HardwareSpecs, ResolvedResourcePlan};
use continuum_core::health::{HealthCheck, HealthRegistry, HealthResult};
use continuum_core::runtime::ContinuumRuntime;
use continuum_core::state::StateHandle;

use crate::memory::MemoryState;

type ConfigProvider = Arc<dyn Fn() -> ContinuumConfig + Send + Sync>;

/// Root containing the packaged runtime assets. Tauri puts those assets under
/// `Contents/Resources` on macOS; portable and Windows installs keep them next
/// to the desktop executable.
fn install_dir() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))?;
    let macos_resources = exe_dir.parent().map(|contents| contents.join("Resources"));
    if let Some(resources) = macos_resources.filter(|path| path.is_dir()) {
        return Some(resources);
    }
    Some(exe_dir)
}

/// True if `continuum.exe` has touched `~/.continuum-dev/state.json` recently.
/// Used to avoid screaming "Degrading" across every model/voice/orchestrator
/// probe when the runtime simply isn't running (the common case for dashboard-only
/// installs). `pub(crate)` so `commands::get_runtime_status` and `chat.rs`'s
/// "Background runtime: running/not running" system-prompt line share this
/// one freshness check instead of each keeping their own copy.
pub(crate) fn runtime_alive(dev_dir: &Path) -> bool {
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

pub fn register_default(registry: &HealthRegistry, runtime: &ContinuumRuntime) {
    let state = Arc::new(RwLock::new(runtime.state.clone()));
    let cfg: ConfigProvider = {
        let runtime = runtime.clone();
        Arc::new(move || runtime.config_snapshot())
    };
    let startup_cfg = runtime.config_snapshot();
    let dev_dir = runtime.dev_dir();

    registry.register(RuntimeCheck {
        dev_dir: dev_dir.clone(),
    });
    registry.register(VisionCheck {
        state: state.clone(),
        cfg: cfg.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(LiveContextCheck {
        cfg: cfg.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(TriageCheck {
        state: state.clone(),
        cfg: cfg.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(OrchestratorCheck {
        state: state.clone(),
        cfg: cfg.clone(),
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
    registry.register(SystemResourceCheck::new(&startup_cfg));
    registry.register(ChatProvidersCheck {
        dev_dir: dev_dir.clone(),
    });
}

/// Registers the Memory tab's vault health probe. Kept separate from
/// [`register_default`] (rather than folding `Arc<MemoryState>` into that
/// function's signature) because `MemoryState` is constructed later in
/// `main.rs`, after `register_default` already needs to have run so the
/// other probes are live as early as possible.
pub fn register_memory(registry: &HealthRegistry, memory_state: Arc<MemoryState>) {
    registry.register(MemoryVaultCheck { memory_state });
}

async fn snap(state: &Arc<RwLock<StateHandle>>) -> continuum_core::state::ContinuumState {
    state.read().await.snapshot().await
}

/// Heartbeat: is the headless `continuum.exe` runtime currently alive?
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
            "The desktop starts the runtime automatically. Inspect the startup error in the title bar and verify the configured model directory in Settings."
                .into(),
        )
    }
    async fn probe(&self) -> HealthResult {
        if runtime_alive(&self.dev_dir) {
            HealthResult::healthy(1)
        } else {
            HealthResult::unknown(
                "runtime process is not publishing state — automatic startup is pending or failed",
                1,
            )
        }
    }
}

struct VisionCheck {
    state: Arc<RwLock<StateHandle>>,
    cfg: ConfigProvider,
    dev_dir: PathBuf,
}

struct LiveContextCheck {
    cfg: ConfigProvider,
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for LiveContextCheck {
    fn name(&self) -> &str {
        "live_context"
    }
    fn log_path(&self) -> Option<String> {
        Some("~/.continuum-dev/logs/continuum.log".into())
    }
    fn recovery_note(&self) -> Option<String> {
        Some(
            "Restart the vision/context components; reduce capture cadence or increase screen.buffer_capacity if drops persist."
                .into(),
        )
    }
    async fn probe(&self) -> HealthResult {
        let cfg = (self.cfg)();
        if !cfg.screen.enabled {
            return HealthResult::healthy(1);
        }
        if !runtime_alive(&self.dev_dir) {
            return HealthResult::unknown("runtime offline", 1);
        }
        let path = self.dev_dir.join("live-context.json");
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) => return HealthResult::degrading(error.to_string(), 1),
        };
        let state: continuum_core::senses::live_context::LiveWorldState =
            match serde_json::from_str(&contents) {
                Ok(state) => state,
                Err(error) => return HealthResult::error(error.to_string(), 1),
            };
        // Task A8: live-context.json is content-versioned (spec §4.11) —
        // its timestamps legitimately go stale on a quiet/idle screen
        // because unchanged ticks skip the write. The runtime evaluates
        // its own capture-stall check every 2 s at the CURRENT cadence
        // (idle mode relaxes it) and publishes it in state.json; prefer
        // that verdict and only fall back to the file-based check for
        // pre-A8 runtimes that don't publish `context_engine`.
        let runtime_stalled = std::fs::read_to_string(self.dev_dir.join("state.json"))
            .ok()
            .and_then(|contents| {
                serde_json::from_str::<continuum_core::runtime_publish::RuntimeSnapshot>(&contents)
                    .ok()
            })
            .and_then(|snap| snap.context_engine)
            .and_then(|engine| engine.live_context)
            .map(|live| live.should_restart);
        let stalled = runtime_stalled.unwrap_or_else(|| {
            let interval = std::time::Duration::from_millis(cfg.screen.capture_interval_ms.max(50));
            state
                .health
                .should_restart(chrono::Utc::now(), true, interval)
        });
        if stalled {
            return HealthResult::error("multi-monitor capture stalled", 1);
        }
        if state.monitors.is_empty() {
            return HealthResult::degrading("no monitor captures published yet", 1);
        }
        let total = state.health.capture_events.max(1);
        if state.health.dropped_capture_events.saturating_mul(10) > total {
            return HealthResult::degrading(
                format!(
                    "capture buffer dropped {} of {} event(s)",
                    state.health.dropped_capture_events, total
                ),
                1,
            );
        }
        if state.health.capture_deadline_misses.saturating_mul(10) > total {
            return HealthResult::degrading(
                format!(
                    "capture missed the configured cadence {} of {} event(s)",
                    state.health.capture_deadline_misses, total
                ),
                1,
            );
        }
        HealthResult::healthy(1)
    }
}

#[async_trait]
impl HealthCheck for VisionCheck {
    fn name(&self) -> &str {
        "vision"
    }
    fn log_path(&self) -> Option<String> {
        Some("~/.continuum-dev/logs/continuum.log".into())
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Re-run scripts/download-models.ps1 to reinstall SmolVLM.".into())
    }
    async fn probe(&self) -> HealthResult {
        let cfg = (self.cfg)();
        let model_path = PathBuf::from(&cfg.vision.model_path);
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
    cfg: ConfigProvider,
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for TriageCheck {
    fn name(&self) -> &str {
        "triage"
    }
    fn log_path(&self) -> Option<String> {
        Some("~/.continuum-dev/logs/continuum.log".into())
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Re-download the triage GGUF via scripts/download-models.ps1.".into())
    }
    async fn probe(&self) -> HealthResult {
        let cfg = (self.cfg)();
        let model_path = PathBuf::from(&cfg.triage.model_path);
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
    cfg: ConfigProvider,
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for OrchestratorCheck {
    fn name(&self) -> &str {
        "orchestrator"
    }
    fn recovery_note(&self) -> Option<String> {
        let agent = (self.cfg)().orchestrator.agent;
        Some(format!(
            "Install the selected {agent} agent CLI, or choose an available runtime in Brain."
        ))
    }
    async fn probe(&self) -> HealthResult {
        if !runtime_alive(&self.dev_dir) {
            return HealthResult::unknown("runtime offline", 1);
        }
        let agent = (self.cfg)().orchestrator.agent;
        let available = tokio::process::Command::new(&agent)
            .arg("--version")
            .output()
            .await
            .is_ok_and(|output| output.status.success());
        if !available {
            return HealthResult::error(format!("selected {agent} agent CLI is unavailable"), 1);
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
    cfg: ConfigProvider,
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
        let cfg = (self.cfg)();
        if !cfg.tts.enabled {
            return HealthResult::healthy(1);
        }
        let Some(primary) = cfg.tts.voices.get(&cfg.tts.primary) else {
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
    cfg: ConfigProvider,
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
        let cfg = (self.cfg)();
        if !cfg.audio.enabled {
            return HealthResult::healthy(1);
        }
        if !PathBuf::from(&cfg.audio.whisper_model_path).exists() {
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

/// Health probe for the Memory tab's vault (Task 10). Unlike the other
/// probes here, this one talks to `MemoryState` directly rather than
/// reading `state.json` — the vault has its own lifecycle (lazy open,
/// per-file quarantine) independent of whether the headless `continuum`
/// runtime is running, so it stays meaningful even when `runtime_alive`
/// would report offline.
struct MemoryVaultCheck {
    memory_state: Arc<MemoryState>,
}

#[async_trait]
impl HealthCheck for MemoryVaultCheck {
    fn name(&self) -> &str {
        "memory_vault"
    }
    fn recovery_note(&self) -> Option<String> {
        Some(
            "Check quarantined notes' YAML frontmatter (see the Memory tab), or delete \
             `.continuum/index.db` under the vault directory to force a full rebuild."
                .into(),
        )
    }
    async fn probe(&self) -> HealthResult {
        let vault = match self.memory_state.vault().await {
            Ok(v) => v,
            Err(e) => return HealthResult::error(e, 1),
        };
        match vault.info().await {
            Ok(info) if info.quarantined.is_empty() => HealthResult::healthy(1),
            Ok(info) => HealthResult::degrading(
                format!(
                    "{} note(s) quarantined due to parse errors",
                    info.quarantined.len()
                ),
                1,
            ),
            Err(e) => HealthResult::error(e.user_message(), 1),
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
        Some("Rebuild continuum-mcp: `cargo build --release -p continuum-mcp`".into())
    }
    async fn probe(&self) -> HealthResult {
        // continuum-mcp is spawned on demand by the orchestrator. Presence of
        // the binary on disk is the best boot-time signal available. Check
        // both packaged installs (next to continuum-desktop.exe) and local dev
        // layouts (target/release/... from a repo cwd).
        let mut candidates: Vec<PathBuf> = Vec::new();
        candidates.extend(crate::commands::bundled_binary_candidates(
            "continuum-mcp.exe",
        ));
        candidates.extend(crate::commands::bundled_binary_candidates("continuum-mcp"));
        candidates.extend(
            [
                "target/release/continuum-mcp.exe",
                "target/release/continuum-mcp",
                "target/debug/continuum-mcp.exe",
                "target/debug/continuum-mcp",
            ]
            .iter()
            .map(PathBuf::from),
        );
        if candidates.iter().any(|p| p.exists()) {
            HealthResult::healthy(1)
        } else {
            HealthResult::degrading("continuum-mcp binary not found", 1)
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
        Some("~/.continuum-dev/logs/continuum.log".into())
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Check workers/*.json under the Continuum data dir; restart continuum runtime.".into())
    }
    async fn probe(&self) -> HealthResult {
        let snaps = match continuum_core::workers::intent::list_snapshots(&self.dev_dir) {
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
                    continuum_core::workers::WorkerStatus::Failed
                        | continuum_core::workers::WorkerStatus::TimedOut
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
    cfg: ConfigProvider,
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
        let cfg = (self.cfg)();
        if !cfg.skills.enabled {
            return HealthResult::healthy(1);
        }
        let configured = std::path::PathBuf::from(&cfg.skills.dir);
        let root = if configured.is_absolute() && configured.exists() {
            configured
        } else {
            // Try in order: cwd-relative, install-dir-relative (next to the
            // exe — covers packaged installs), dev-dir-relative.
            let rel = &cfg.skills.dir;
            std::env::current_dir()
                .ok()
                .map(|cwd| cwd.join(rel))
                .filter(|p| p.exists())
                .or_else(|| install_dir().map(|d| d.join(rel)).filter(|p| p.exists()))
                .unwrap_or_else(|| self.dev_dir.join(rel))
        };
        let loader = continuum_core::skills::SkillLoader::new(&root);
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

/// Health probe for the Chat tab's provider connections (Task 10). Reads
/// `providers.json` directly rather than through `ChatState` — this probe
/// runs from the health-poll loop, not from a Tauri command, so it has no
/// `tauri::State` to borrow from.
struct ChatProvidersCheck {
    dev_dir: PathBuf,
}

#[async_trait]
impl HealthCheck for ChatProvidersCheck {
    fn name(&self) -> &str {
        "chat_providers"
    }
    fn recovery_note(&self) -> Option<String> {
        Some("Re-test or remove the failing provider in Settings → Integrations.".into())
    }
    async fn probe(&self) -> HealthResult {
        let providers = crate::providers::ProviderStore::new(self.dev_dir.clone()).load();
        match providers.iter().find(|p| p.last_test_ok == Some(false)) {
            Some(bad) => HealthResult::degrading(
                format!(
                    "provider '{}' failed its last connection test",
                    bad.display_name
                ),
                1,
            ),
            None => HealthResult::healthy(1),
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

/// System-resource health probe (CLAUDE.md non-negotiable #5).
///
/// Samples host CPU + RAM every poll cycle (30 s by default). Reports
/// `Degrading` when CPU usage stays above 90 % across the last two samples
/// (~60 s sustained) and `Error` when RAM utilisation exceeds 95 %. The
/// detected `HardwareSpecs` and resolved `ResolvedResourcePlan` are folded
/// into the message so the repair agent can reason about model-load
/// failures (e.g. triage OOM on a 4-core laptop → reduce `cpu_core_fraction`).
///
/// This sits *outside* the four cognitive layers — it only reads system
/// state, never feeds perception frames upward.
struct SystemResourceCheck {
    specs: HardwareSpecs,
    plan: ResolvedResourcePlan,
    /// sysinfo handle kept across probes so CPU usage reflects the delta
    /// since the previous refresh. `System` is `Send`; wrap in a `Mutex`
    /// so the probe is `Sync`.
    sys: Mutex<System>,
    /// Rolling CPU% samples (newest at the back). 4 entries = ~120 s window.
    recent_cpu: Mutex<VecDeque<f32>>,
}

impl SystemResourceCheck {
    fn new(cfg: &ContinuumConfig) -> Self {
        let specs = hardware::probe_hardware();
        let plan = hardware::resolve_resource_policy(&specs, &cfg.resources);
        let mut sys = System::new();
        // Prime the first CPU reading so the next probe has a real delta.
        sys.refresh_cpu_all();
        sys.refresh_memory();
        Self {
            specs,
            plan,
            sys: Mutex::new(sys),
            recent_cpu: Mutex::new(VecDeque::with_capacity(4)),
        }
    }
}

#[async_trait]
impl HealthCheck for SystemResourceCheck {
    fn name(&self) -> &str {
        "system_resources"
    }
    fn log_path(&self) -> Option<String> {
        Some("~/.continuum-dev/logs/continuum.log".into())
    }
    fn recovery_note(&self) -> Option<String> {
        Some(
            "Lower [resources].cpu_core_fraction or workers_max_concurrent in config.toml, \
             then restart the Continuum runtime."
                .into(),
        )
    }
    async fn probe(&self) -> HealthResult {
        // CPU usage is computed from the delta since the last refresh, so
        // we read first, then refresh for next time.
        let (cpu_pct, ram_used_bytes, ram_total_bytes) = {
            let mut sys = self.sys.lock();
            let cpus = sys.cpus();
            let cpu_pct = if cpus.is_empty() {
                0.0
            } else {
                let sum: f32 = cpus.iter().map(|c| c.cpu_usage()).sum();
                sum / cpus.len() as f32
            };
            let ram_used = sys.used_memory();
            let ram_total = sys.total_memory();
            sys.refresh_cpu_all();
            sys.refresh_memory();
            (cpu_pct, ram_used, ram_total)
        };

        // Rolling window of CPU samples (cap 4 → ~120 s at a 30 s cadence).
        {
            let mut recent = self.recent_cpu.lock();
            recent.push_back(cpu_pct);
            while recent.len() > 4 {
                recent.pop_front();
            }
        }

        let ram_total_mb = ram_total_bytes / 1024 / 1024;
        let ram_used_mb = ram_used_bytes / 1024 / 1024;
        let ram_fraction = if ram_total_bytes > 0 {
            ram_used_bytes as f32 / ram_total_bytes as f32
        } else {
            0.0
        };
        let ram_pct = ram_fraction * 100.0;

        let plan_summary = format!(
            "cpu={cpu_pct:.0}% ram={ram_used_mb}/{ram_total_mb}MB ({ram_pct:.0}%) | \
             specs: {} cores / {ram_total_mb}MB / cuda={cuda} vram={vram_mb}MB battery={batt} | \
             plan: triage_threads={tt} gpu_layers={gl} vision={vis} workers={w}",
            self.specs.logical_cores,
            cuda = self.specs.has_cuda,
            vram_mb = self.specs.vram_mb.unwrap_or(0),
            batt = self.specs.on_battery,
            tt = self.plan.triage_threads,
            gl = self.plan.triage_gpu_layers,
            vis = if self.plan.vision_enabled {
                "on"
            } else {
                "off"
            },
            w = self.plan.workers_max_concurrent,
        );

        // Classify via the pure helper so the threshold logic is unit-tested
        // in `continuum_core::hardware::tests`.
        let recent_vec: Vec<f32> = self.recent_cpu.lock().iter().copied().collect();
        match hardware::classify_system_load(&recent_vec, ram_fraction) {
            hardware::LoadStatus::Critical => HealthResult::error(
                format!("RAM nearly exhausted ({ram_pct:.0}%). {plan_summary}"),
                1,
            ),
            hardware::LoadStatus::Degrading => {
                HealthResult::degrading(format!("Sustained high CPU/RAM load. {plan_summary}"), 1)
            }
            hardware::LoadStatus::Ok => HealthResult::healthy_with(plan_summary, 1),
        }
    }
}
