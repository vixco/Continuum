//! # Embeddable Continuum runtime
//!
//! The desktop app needs the same four-layer pipeline the `continuum` binary
//! runs, but with hooks for the dashboard: a shared state store, a log
//! buffer, and a way to pause / resume and trigger repair.
//!
//! [`ContinuumRuntime`] owns the [`StateHandle`], [`LogBuffer`],
//! [`AutomationStore`], config handle, and lifecycle flags. The actual
//! sensor/triage/orchestrator loop is spawned by the desktop bootstrap
//! and publishes into these handles; keeping the loop outside the runtime
//! struct means we don't drag llama-cpp into the public type surface of
//! consumers that only want dashboard data.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::RwLock;
use tokio::sync::watch;

use crate::automations::AutomationStore;
use crate::config::{continuum_dev_dir, load_config, ContinuumConfig};
use crate::logs::{LogBuffer, DEFAULT_BUFFER_CAP};
use crate::state::{StateHandle, VoiceMode};

/// Runtime handles the dashboard backend holds onto.
#[derive(Clone)]
pub struct ContinuumRuntime {
    pub state: StateHandle,
    pub logs: LogBuffer,
    pub automations: AutomationStore,
    inner: Arc<RwLock<ContinuumRuntimeInner>>,
    shutdown_tx: watch::Sender<bool>,
}

struct ContinuumRuntimeInner {
    config: ContinuumConfig,
    config_path: PathBuf,
    dev_dir: PathBuf,
    paused: bool,
    voice_muted: bool,
}

impl ContinuumRuntime {
    /// Initialise the dashboard-facing handles. Config is loaded from
    /// `~/.continuum-dev/config.toml` (created on demand).
    pub fn init() -> Result<Self> {
        let dev_dir = continuum_dev_dir();
        std::fs::create_dir_all(&dev_dir).ok();
        let config_path = dev_dir.join("config.toml");
        let config = load_config(&config_path)?;

        let automations_path = dev_dir.join("automations.json");
        let automations = AutomationStore::open(automations_path)?;

        let (shutdown_tx, _shutdown_rx) = watch::channel(false);

        let state =
            StateHandle::new_with_voice_config(config.voice.volume, config.voice.wake_word_enabled);

        Ok(Self {
            state,
            logs: LogBuffer::new(DEFAULT_BUFFER_CAP),
            automations,
            inner: Arc::new(RwLock::new(ContinuumRuntimeInner {
                config,
                config_path,
                dev_dir,
                paused: false,
                voice_muted: false,
            })),
            shutdown_tx,
        })
    }

    pub fn config_snapshot(&self) -> ContinuumConfig {
        self.inner.read().config.clone()
    }

    pub fn dev_dir(&self) -> PathBuf {
        self.inner.read().dev_dir.clone()
    }

    pub fn config_path(&self) -> PathBuf {
        self.inner.read().config_path.clone()
    }

    /// Update config, persist, and return the new snapshot. The caller is
    /// responsible for invalidating anything that cached config values
    /// (e.g. the TTS engine reloads on voice volume changes via state
    /// push).
    pub fn update_config<F>(&self, updater: F) -> Result<ContinuumConfig>
    where
        F: FnOnce(&mut ContinuumConfig),
    {
        let (snapshot, path) = {
            let mut guard = self.inner.write();
            updater(&mut guard.config);
            (guard.config.clone(), guard.config_path.clone())
        };
        persist_config(&path, &snapshot)?;
        Ok(snapshot)
    }

    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub async fn set_paused(&self, paused: bool) {
        self.inner.write().paused = paused;
        self.state.set_paused(paused).await;
    }

    pub fn is_paused(&self) -> bool {
        self.inner.read().paused
    }

    pub async fn set_voice_muted(&self, muted: bool) {
        self.inner.write().voice_muted = muted;
        self.state
            .set_voice_mode(if muted {
                VoiceMode::Muted
            } else {
                VoiceMode::Idle
            })
            .await;
    }

    pub fn is_voice_muted(&self) -> bool {
        self.inner.read().voice_muted
    }
}

fn persist_config(path: &Path, cfg: &ContinuumConfig) -> Result<()> {
    let toml_str = toml::to_string_pretty(cfg)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, toml_str)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_runtime_without_panic() {
        let rt = ContinuumRuntime::init();
        // Should at least not panic. The config path lives under the real
        // home dir, which exists in CI.
        assert!(rt.is_ok());
    }
}
