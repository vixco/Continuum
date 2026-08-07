//! # Voice pipeline
//!
//! Bidirectional voice stack on local-first infrastructure:
//!
//! - [`tts`] — text-to-speech via Piper (local neural VITS), wrapped as a
//!   [`tts::TtsEngine`] trait so alternate backends (ElevenLabs) slot in.
//! - [`playback`] — cpal output stream with a sample queue, resampling
//!   and channel expansion so Piper's mono 22050 Hz output plays through
//!   the user's default audio device. Master volume is applied in the cpal
//!   callback.
//! - [`streaming`] — [`streaming::SpeechController`] accumulates
//!   orchestrator `TextDelta` tokens into sentences and hands them to the
//!   TTS engine, so Continuum starts speaking before Opus finishes generating.
//! - [`stt`] — post-wake voice sessions with local heuristic endpoint
//!   detection.
//! - [`wake`] — transcript-based wake phrase detection on top of the
//!   continuous whisper stream.
//! - [`sounds`] — procedurally-generated feedback cues (wake chime, listen
//!   click, error beep).
//! - [`hotkey`] — global keyboard shortcut for push-to-talk / toggle
//!   listening (Windows only).
//! - [`intent`] — file-based push-to-talk intents from the dashboard process.
//!   Pure serde/std; available without the `runtime` feature so the Tauri
//!   crate can write intents.
//! - [`health`] — component-level health checks used by the repair agent.
//! - [`frontend`] — [`frontend::VoiceFrontend`] trait abstracting the
//!   realtime voice path: the existing `PipelineFrontend` (wake→STT→
//!   triage→orchestrator→TTS) and, behind the `moshi` cargo feature, the
//!   full-duplex [`moshi::MoshiFrontend`] S2S subprocess.
//! - [`moshi`] — Kyutai Moshi speech-to-speech front-end (feature = "moshi").
//!
//! ## Architectural placement
//!
//! The voice modules are a Layer 1/Layer 3 bridge, not a new layer:
//! output (TTS) consumes the orchestrator's text stream, and input
//! (STT, wake word) feeds the existing perception + triage pipeline.
//! Nothing here speaks to the MCP server directly.

pub mod intent;

#[cfg(feature = "runtime")]
pub mod frontend;
#[cfg(feature = "moshi")]
pub mod moshi;

#[cfg(feature = "runtime")]
pub mod health;
#[cfg(all(feature = "runtime", windows))]
pub mod hotkey;
#[cfg(feature = "runtime")]
pub mod playback;
#[cfg(feature = "runtime")]
pub mod sounds;
#[cfg(feature = "runtime")]
pub mod streaming;
#[cfg(feature = "runtime")]
pub mod stt;
#[cfg(feature = "runtime")]
pub mod tts;
#[cfg(feature = "runtime")]
pub mod wake;

/// Polls an optional hotkey channel inside a `tokio::select!` arm.
///
/// Lives here rather than in [`hotkey`] because the hotkey listener itself
/// is Windows-only while the runtime's select loop is not.
///
/// Three behaviours, all deliberate:
///
/// - **Disabled** (`rx` is `None`, e.g. registration failed): the future
///   pends forever, so the select arm is simply inert.
/// - **Press**: yields `Some(())`.
/// - **Channel closed** (M3): the listener thread died. Before this,
///   `recv()` returning `None` made the arm's `Some(())` pattern fail and
///   the branch re-poll a dead channel on every loop iteration, forever,
///   with nothing logged. Now the channel is dropped (`*rx = None`), the
///   failure is logged at error level so the repair agent sees it, and the
///   arm goes inert like the disabled case.
pub async fn recv_hotkey(rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<()>>) -> Option<()> {
    let closed = match rx.as_mut() {
        Some(channel) => channel.recv().await.is_none(),
        // Disabled: never resolves.
        None => std::future::pending::<bool>().await,
    };
    if !closed {
        return Some(());
    }
    tracing::error!(
        layer = "voice",
        component = "hotkey",
        "Hotkey listener channel closed — push-to-talk hotkey is disabled until restart"
    );
    *rx = None;
    // Pend rather than returning: a `None` return would leave the caller's
    // select arm re-polling us on every iteration for the rest of the
    // process's life.
    std::future::pending().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn recv_hotkey_yields_presses() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut rx = Some(rx);
        tx.send(()).unwrap();
        assert_eq!(recv_hotkey(&mut rx).await, Some(()));
        assert!(rx.is_some(), "a live channel stays installed");
    }

    #[tokio::test]
    async fn recv_hotkey_pends_forever_when_disabled() {
        let mut rx: Option<mpsc::UnboundedReceiver<()>> = None;
        let timed_out = tokio::time::timeout(Duration::from_millis(20), recv_hotkey(&mut rx))
            .await
            .is_err();
        assert!(timed_out, "a disabled hotkey must never resolve");
    }

    /// M3: a dead listener disables itself once instead of leaving the
    /// select arm spinning on a closed channel.
    #[tokio::test]
    async fn recv_hotkey_drops_a_closed_channel() {
        let (tx, rx) = mpsc::unbounded_channel::<()>();
        let mut rx = Some(rx);
        drop(tx);
        let timed_out = tokio::time::timeout(Duration::from_millis(20), recv_hotkey(&mut rx))
            .await
            .is_err();
        assert!(timed_out, "must pend, not resolve, on a closed channel");
        assert!(rx.is_none(), "the dead channel must be dropped");

        // And it stays inert afterwards.
        let timed_out_again = tokio::time::timeout(Duration::from_millis(20), recv_hotkey(&mut rx))
            .await
            .is_err();
        assert!(timed_out_again);
    }
}
