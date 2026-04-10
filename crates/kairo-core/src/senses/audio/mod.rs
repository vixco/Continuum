//! # Audio watcher
//!
//! Continuously captures microphone audio with voice activity detection (VAD).
//! Only when speech is detected does the audio get sent through whisper.cpp
//! for transcription.
//!
//! Part of Layer 1 (Senses) in the Kairo cognitive architecture.

mod full;
pub use full::AudioWatcher;
