//! Default health-check registrations for the Kairo runtime.
//!
//! These probes intentionally don't block on heavy work — they mostly
//! check the state-store flags set by the headless `kairo` binary (when
//! it's running) or verify on-disk model artefacts are present. That
//! keeps the dashboard usable even when the main runtime isn't active,
//! and the Health tab displays `Unknown` for subsystems whose owner
//! process hasn't published yet.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use kairo_core::config::KairoConfig;
use kairo_core::health::{HealthCheck, HealthRegistry, HealthResult};
use kairo_core::runtime::KairoRuntime;
use kairo_core::state::StateHandle;

pub fn register_default(registry: &HealthRegistry, runtime: &KairoRuntime) {
    let state = Arc::new(RwLock::new(runtime.state.clone()));
    let cfg = Arc::new(runtime.config_snapshot());
    let dev_dir = runtime.dev_dir();

    registry.register(VisionCheck {
        state: state.clone(),
        cfg: cfg.clone(),
    });
    registry.register(TriageCheck {
        state: state.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(OrchestratorCheck {
        state: state.clone(),
    });
    registry.register(VoiceTtsCheck {
        state: state.clone(),
        cfg: cfg.clone(),
    });
    registry.register(VoiceSttCheck {
        state: state.clone(),
        cfg: cfg.clone(),
    });
    registry.register(MemoryCheck {
        state: state.clone(),
        dev_dir: dev_dir.clone(),
    });
    registry.register(McpCheck {});
    registry.register(ContextCheck {
        state: state.clone(),
    });
}

async fn snap(state: &Arc<RwLock<StateHandle>>) -> kairo_core::state::KairoState {
    state.read().await.snapshot().await
}

struct VisionCheck {
    state: Arc<RwLock<StateHandle>>,
    cfg: Arc<KairoConfig>,
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
        let snap = snap(&self.state).await;
        let model_path = PathBuf::from(&self.cfg.vision.model_path);
        if !model_path.exists() {
            return HealthResult::error("vision model missing on disk", 1);
        }
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
        let snap = snap(&self.state).await;
        if snap.system.orchestrator_ready {
            HealthResult::healthy(1)
        } else if let Some(ts) = snap.orchestrator.last_wake_ts {
            let age = chrono::Utc::now().signed_duration_since(ts).num_seconds();
            if age < 60 * 60 {
                HealthResult::healthy(1)
            } else {
                HealthResult::degrading(
                    "orchestrator has not been woken in the last hour",
                    1,
                )
            }
        } else {
            HealthResult::degrading("orchestrator never woken", 1)
        }
    }
}

struct VoiceTtsCheck {
    state: Arc<RwLock<StateHandle>>,
    cfg: Arc<KairoConfig>,
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
        // the binary on disk is the best boot-time signal available.
        let candidates = [
            "target/release/kairo-mcp.exe",
            "target/release/kairo-mcp",
            "target/debug/kairo-mcp.exe",
            "target/debug/kairo-mcp",
        ];
        if candidates.iter().any(|p| PathBuf::from(p).exists()) {
            HealthResult::healthy(1)
        } else {
            HealthResult::degrading("kairo-mcp binary not built yet", 1)
        }
    }
}

struct ContextCheck {
    state: Arc<RwLock<StateHandle>>,
}

#[async_trait]
impl HealthCheck for ContextCheck {
    fn name(&self) -> &str {
        "context_watcher"
    }
    async fn probe(&self) -> HealthResult {
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
