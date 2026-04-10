//! Stub audio watcher for builds without the `audio` feature.
//!
//! Provides the same public API as the full implementation so that the rest
//! of kairo-core compiles without whisper-rs / cpal / rubato.

use crate::config::AudioConfig;
use crate::senses::types::AudioObservation;

/// Stub audio watcher when the `audio` feature is not compiled.
///
/// Logs a warning at creation and parks until shutdown. No microphone
/// access, no transcription. Rebuild with `--features audio` and LLVM
/// installed to enable the full audio pipeline.
pub struct AudioWatcher {
    #[allow(dead_code)]
    config: AudioConfig,
}

impl AudioWatcher {
    /// Creates a stub audio watcher (no-op).
    pub fn new(config: AudioConfig) -> Self {
        tracing::warn!(
            layer = "senses",
            component = "audio",
            "Audio watcher compiled without `audio` feature — mic capture and \
             transcription are disabled. Rebuild with `--features audio` to enable."
        );
        Self { config }
    }

    /// Parks until shutdown (no audio processing).
    pub async fn run(
        &self,
        _tx: tokio::sync::mpsc::Sender<AudioObservation>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        tracing::info!(
            layer = "senses",
            component = "audio",
            "Stub audio watcher parking until shutdown"
        );
        let _ = shutdown.changed().await;
    }

    /// Always returns `true` — the stub is "healthy" by definition.
    pub fn is_healthy(&self) -> bool {
        true
    }

    /// Always returns `false` — no restart needed for a stub.
    pub fn should_restart(&self) -> bool {
        false
    }
}
