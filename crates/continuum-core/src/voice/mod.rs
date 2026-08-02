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
//!
//! ## Architectural placement
//!
//! The voice modules are a Layer 1/Layer 3 bridge, not a new layer:
//! output (TTS) consumes the orchestrator's text stream, and input
//! (STT, wake word) feeds the existing perception + triage pipeline.
//! Nothing here speaks to the MCP server directly.

pub mod intent;

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
