//! # Speech-to-text
//!
//! Whisper streaming mode transcription. Runs after wake word detection
//! and produces partial transcripts every ~300ms. Semantic endpoint
//! detection via the triage LLM determines when the user is done speaking.
