//! # Audio watcher
//!
//! Continuously captures microphone audio with voice activity detection (VAD).
//! Only when speech is detected does the audio get sent through whisper.cpp
//! for transcription. This avoids wasting cycles transcribing silence.
//!
//! Produces [`AudioObservation`] structs with transcript, detected language,
//! duration, and confidence score.
