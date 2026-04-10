//! # Text-to-speech
//!
//! TTS options:
//! - **Piper** (default) — local, fast, free, Dutch and English voices
//! - **Kokoro TTS** — local, better quality, English only
//! - **ElevenLabs streaming** — best quality, cloud, requires API key
//!
//! Supports streaming: first tokens from the orchestrator are spoken while
//! the rest is still generating. Interrupt handling cuts playback within
//! 50ms of detected user speech.
