//! # Audio watcher
//!
//! Continuously captures microphone audio with voice activity detection (VAD).
//! Only when speech is detected does the audio get sent through whisper.cpp
//! for transcription.
//!
//! **Compile-time feature gate:** The full audio pipeline (cpal + whisper-rs +
//! rubato) requires the `audio` Cargo feature, which in turn requires LLVM/
//! libclang installed for the whisper-rs bindgen build step. When the `audio`
//! feature is disabled, a stub [`AudioWatcher`] is provided that parks until
//! shutdown.
//!
//! Enable with: `cargo build -p kairo-core --features audio`

#[cfg(feature = "audio")]
mod full;
#[cfg(feature = "audio")]
pub use full::AudioWatcher;

#[cfg(not(feature = "audio"))]
mod stub;
#[cfg(not(feature = "audio"))]
pub use stub::AudioWatcher;
