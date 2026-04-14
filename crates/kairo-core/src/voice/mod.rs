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
//!   TTS engine, so Kairo starts speaking before Opus finishes generating.
//! - [`stt`] — post-wake voice sessions with local heuristic endpoint
//!   detection.
//! - [`wake`] — transcript-based wake phrase detection on top of the
//!   continuous whisper stream.
//! - [`sounds`] — procedurally-generated feedback cues (wake chime, listen
//!   click, error beep).
//! - [`hotkey`] — global keyboard shortcut for push-to-talk / toggle
//!   listening (Windows only).
//! - [`health`] — component-level health checks used by the repair agent.
//!
//! ## Architectural placement
//!
//! The voice modules are a Layer 1/Layer 3 bridge, not a new layer:
//! output (TTS) consumes the orchestrator's text stream, and input
//! (STT, wake word) feeds the existing perception + triage pipeline.
//! Nothing here speaks to the MCP server directly.

pub mod health;
#[cfg(windows)]
pub mod hotkey;
pub mod playback;
pub mod sounds;
pub mod streaming;
pub mod stt;
pub mod tts;
pub mod wake;
