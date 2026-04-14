//! # Speech-to-text
//!
//! Whisper transcription runs continuously through the senses audio watcher.
//! After wake-word detection, this module tracks a short voice session and
//! uses local semantic endpoint heuristics to decide when the user is done.

use std::time::{Duration, Instant};

/// Tracks one post-wake voice command until it is complete.
#[derive(Debug, Clone)]
pub struct VoiceSession {
    started_at: Instant,
    last_update: Instant,
    text: String,
    language: String,
}

impl VoiceSession {
    /// Starts a voice session with an optional initial utterance.
    pub fn new(initial_text: &str, language: &str) -> Self {
        let now = Instant::now();
        Self {
            started_at: now,
            last_update: now,
            text: initial_text.trim().to_string(),
            language: normalize_language(language),
        }
    }

    /// Appends a new transcript fragment.
    pub fn push_transcript(&mut self, text: &str, language: &str) {
        let text = text.trim();
        if !text.is_empty() {
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            self.text.push_str(text);
        }
        let language = normalize_language(language);
        if language != "unknown" && language != "auto" {
            self.language = language;
        }
        self.last_update = Instant::now();
    }

    /// Returns the accumulated utterance.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the best current language hint.
    pub fn language(&self) -> &str {
        &self.language
    }

    /// Returns true when enough text has arrived and the utterance appears done.
    pub fn is_endpoint(&self, endpoint_silence: Duration, min_chars: usize) -> bool {
        self.text.trim().chars().count() >= min_chars
            && (self.last_update.elapsed() >= endpoint_silence
                || looks_complete(&self.text))
    }

    /// Returns true when this session has exceeded its configured maximum age.
    pub fn timed_out(&self, timeout: Duration) -> bool {
        self.started_at.elapsed() >= timeout
    }
}

/// Outcome from the local semantic endpoint detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointDecision {
    /// Continue listening for more transcript fragments.
    Continue,
    /// The command appears complete.
    Complete,
    /// The session exceeded its maximum age.
    TimedOut,
}

/// Local semantic endpoint detector for post-wake voice commands.
///
/// This deliberately stays local and deterministic. It uses transcript shape
/// (question/imperative starts, punctuation, short stop/cancel commands) plus
/// silence and timeout gates. A future triage-LLM endpoint classifier can slot
/// in behind the same decision boundary if needed, but Phase 5 does not require
/// another model invocation on the hot path.
#[derive(Debug, Clone)]
pub struct SemanticEndpointDetector {
    endpoint_silence: Duration,
    timeout: Duration,
    min_chars: usize,
}

impl SemanticEndpointDetector {
    /// Creates a detector from configured thresholds.
    pub fn new(endpoint_silence: Duration, timeout: Duration, min_chars: usize) -> Self {
        Self {
            endpoint_silence,
            timeout,
            min_chars,
        }
    }

    /// Classifies the current voice session.
    pub fn decide(&self, session: &VoiceSession) -> EndpointDecision {
        if session.timed_out(self.timeout) {
            return EndpointDecision::TimedOut;
        }
        if session.is_endpoint(self.endpoint_silence, self.min_chars) {
            return EndpointDecision::Complete;
        }
        EndpointDecision::Continue
    }
}

/// Returns true if a transcript looks like a complete utterance.
pub fn looks_complete(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    if matches!(trimmed.as_bytes().last(), Some(b'.' | b'?' | b'!')) {
        return true;
    }

    let normalized = trimmed.to_lowercase();
    let words = normalized.split_whitespace().count();
    if words <= 2 {
        return matches!(
            normalized.as_str(),
            "stop" | "cancel" | "thanks" | "bedankt" | "stil" | "mute"
        );
    }

    normalized.starts_with("what ")
        || normalized.starts_with("when ")
        || normalized.starts_with("why ")
        || normalized.starts_with("how ")
        || normalized.starts_with("waar ")
        || normalized.starts_with("wat ")
        || normalized.starts_with("wanneer ")
        || normalized.starts_with("hoe ")
        || normalized.starts_with("kun je ")
        || normalized.starts_with("kan je ")
        || normalized.starts_with("can you ")
}

/// Normalizes Whisper language labels to short BCP-47-ish keys used in config.
pub fn normalize_language(language: &str) -> String {
    let language = language.trim().to_lowercase();
    if language.is_empty() {
        return "unknown".to_string();
    }
    match language.as_str() {
        "dutch" | "nederlands" | "nl_nl" => "nl".to_string(),
        "english" | "en_us" | "en_gb" => "en".to_string(),
        "auto" | "unknown" => language,
        _ => language
            .split(['-', '_'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_complete_questions() {
        assert!(looks_complete("wat staat er op mijn planning"));
        assert!(looks_complete("can you open the build log"));
        assert!(looks_complete("hello?"));
        assert!(!looks_complete("maybe the"));
    }

    #[test]
    fn normalizes_languages() {
        assert_eq!(normalize_language("Dutch"), "nl");
        assert_eq!(normalize_language("en-US"), "en");
        assert_eq!(normalize_language("auto"), "auto");
    }

    #[test]
    fn voice_session_accumulates_text() {
        let mut session = VoiceSession::new("open", "en-US");
        session.push_transcript("the logs", "auto");
        assert_eq!(session.text(), "open the logs");
        assert_eq!(session.language(), "en");
    }

    #[test]
    fn endpoint_detector_completes_semantic_question() {
        let session = VoiceSession::new("wat staat er op mijn planning", "nl");
        let detector = SemanticEndpointDetector::new(
            Duration::from_secs(10),
            Duration::from_secs(30),
            3,
        );
        assert_eq!(detector.decide(&session), EndpointDecision::Complete);
    }

    #[test]
    fn endpoint_detector_continues_incomplete_fragment() {
        let session = VoiceSession::new("maybe the", "en");
        let detector = SemanticEndpointDetector::new(
            Duration::from_secs(10),
            Duration::from_secs(30),
            3,
        );
        assert_eq!(detector.decide(&session), EndpointDecision::Continue);
    }
}
