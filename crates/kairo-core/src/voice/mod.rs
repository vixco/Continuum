//! # Voice pipeline
//!
//! Bidirectional voice: wake word detection → streaming STT → triage fast path
//! → orchestrator slow path → streaming TTS → interrupt handling.
//!
//! - [`wake`] — Porcupine wake word detection
//! - [`stt`] — Speech-to-text via whisper.cpp with streaming
//! - [`tts`] — Text-to-speech via Piper (local) or ElevenLabs (optional cloud)

pub mod stt;
pub mod tts;
pub mod wake;
