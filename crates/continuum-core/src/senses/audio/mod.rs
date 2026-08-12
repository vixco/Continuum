//! # Audio watcher
//!
//! Continuously captures microphone audio with voice activity detection (VAD).
//! Only when speech is detected does the audio get sent through whisper.cpp
//! for transcription.
//!
//! Part of Layer 1 (Senses) in the Continuum cognitive architecture.

mod full;
pub mod picker;

pub use full::{AudioLevelHandle, AudioWatcher};
pub use picker::{
    clear_config as clear_audio_config, pick_interactive, save_config as save_audio_config,
    verify_saved, DevicePick,
};
