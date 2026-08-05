//! # Voice front-end abstraction
//!
//! [`VoiceFrontend`] abstracts the realtime voice path so the main
//! perception loop can switch between the existing segment-granular pipeline
//! and a full-duplex speech-to-speech front-end (Moshi) by config, without
//! the loop code branching on a string everywhere.
//!
//! ## Implementations
//!
//! - [`PipelineFrontend`] — a thin shell over the existing wake → whisper
//!   STT → triage → orchestrator → TTS loop. That loop is driven directly
//!   from `bin/continuum.rs`, so most trait methods are no-ops here; the
//!   trait exists so Moshi and the pipeline share one interface for mode
//!   reporting, dashboard status, and barge-in.
//! - `MoshiFrontend` (feature = `moshi`, in [`super::moshi`]) — Kyutai Moshi
//!   S2S run as a `moshi-backend.exe` subprocess over a local WebSocket.
//!
//! ## Object safety
//!
//! The trait is object-safe so the main loop can hold a
//! `Arc<dyn VoiceFrontend>` and swap implementations at boot based on
//! `voice.frontend.mode`.

use anyhow::Result;

/// Realtime voice front-end.
///
/// All methods are synchronous and non-blocking from the caller's view;
/// Moshi performs its subprocess/WebSocket I/O on a background tokio task
/// and communicates state through atomics + channels.
pub trait VoiceFrontend: Send + Sync {
    /// Which front-end this is — `"pipeline"` or `"moshi"`. Used for
    /// dashboard status and config validation.
    fn mode(&self) -> &'static str;

    /// Start the front-end. For Moshi this spawns the subprocess and opens
    /// the WebSocket. For the pipeline this is a no-op (the loop owns its
    /// own capture). Idempotent: calling `start()` when already active is
    /// safe and returns `Ok(())`.
    fn start(&self) -> Result<()>;

    /// Stop the front-end and release resources (kill subprocess, close
    /// WebSocket). Safe to call when not active.
    fn stop(&self);

    /// Whether the front-end is currently active and (for Moshi) its
    /// WebSocket is connected.
    fn is_active(&self) -> bool;

    /// Whether the backend is loaded. For Moshi this is `true` only once the
    /// subprocess has been spawned and the WebSocket handshake completed;
    /// for the pipeline it is `true` once `start()` succeeded. Distinct from
    /// `is_active` so a paused-but-loaded Moshi can still report `loaded`.
    fn loaded(&self) -> bool;

    /// Interrupt ongoing output (barge-in). For Moshi: signal the backend to
    /// stop emitting audio and clear the playback queue. For the pipeline:
    /// delegates to the existing `SpeechController` barge-in path, so this
    /// method is only called on the Moshi impl in practice.
    fn interrupt(&self);

    /// Feed captured microphone PCM (16 kHz mono `f32`, range [-1, 1]) into
    /// the front-end. Moshi uses this as its continuous full-duplex input
    /// stream. The pipeline impl is a no-op — it owns its own whisper
    /// capture path and never receives audio through this method.
    fn feed_pcm(&self, _samples: &[f32]) {}
}

/// No-op shell over the existing voice pipeline.
///
/// The real pipeline logic lives in `bin/continuum.rs` (wake detection,
/// whisper sessions, triage gate, orchestrator wake, TTS streaming). This
/// struct exists so the main loop can treat both front-ends uniformly:
/// `mode()` reports `"pipeline"`, and the other methods are inert because
/// the loop drives the pipeline directly.
///
/// Created with [`PipelineFrontend::new`].
#[derive(Debug, Default)]
pub struct PipelineFrontend {
    active: std::sync::atomic::AtomicBool,
}

impl Clone for PipelineFrontend {
    fn clone(&self) -> Self {
        Self {
            active: std::sync::atomic::AtomicBool::new(
                self.active.load(std::sync::atomic::Ordering::Relaxed),
            ),
        }
    }
}

impl PipelineFrontend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl VoiceFrontend for PipelineFrontend {
    fn mode(&self) -> &'static str {
        "pipeline"
    }

    fn start(&self) -> Result<()> {
        self.active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn stop(&self) {
        self.active
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn loaded(&self) -> bool {
        // The pipeline is always "loaded" when active — there is no external
        // subprocess whose readiness we have to wait on.
        self.is_active()
    }

    fn interrupt(&self) {
        // No-op: the pipeline's barge-in is handled inline in the main loop
        // via the SpeechController. This method is only meaningfully called
        // on the Moshi impl.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_frontend_lifecycle() {
        let fe = PipelineFrontend::new();
        assert_eq!(fe.mode(), "pipeline");
        assert!(!fe.is_active());
        assert!(!fe.loaded());
        fe.start().unwrap();
        assert!(fe.is_active());
        assert!(fe.loaded());
        fe.interrupt(); // no-op, must not panic
        fe.feed_pcm(&[0.0; 16]); // no-op, must not panic
        fe.stop();
        assert!(!fe.is_active());
    }
}
