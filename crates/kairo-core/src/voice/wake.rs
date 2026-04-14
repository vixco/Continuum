//! # Wake word detection
//!
//! The production path is intentionally local-only. Kairo can use a native
//! wake-word backend later, but the core runtime already has continuous
//! Whisper transcripts, so Phase 5 gates voice commands with a transcript
//! wake phrase by default.

/// Result of a wake phrase detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeDetection {
    /// The normalized keyword that matched.
    pub keyword: String,
    /// Text after the wake phrase in the same transcript, if any.
    pub utterance_after_wake: String,
}

/// Local wake phrase detector over STT transcripts.
#[derive(Debug, Clone)]
pub struct TranscriptWakeDetector {
    keyword: String,
    normalized_keyword: String,
}

impl TranscriptWakeDetector {
    /// Creates a new transcript detector for a configurable keyword.
    pub fn new(keyword: impl Into<String>) -> Self {
        let keyword = keyword.into();
        let normalized_keyword = normalize_text(&keyword);
        Self {
            keyword,
            normalized_keyword,
        }
    }

    /// Returns a detection if `transcript` contains the configured wake phrase.
    pub fn detect(&self, transcript: &str) -> Option<WakeDetection> {
        if self.normalized_keyword.is_empty() {
            return None;
        }

        let normalized = normalize_text(transcript);
        let idx = normalized.find(&self.normalized_keyword)?;
        let after_start = idx + self.normalized_keyword.len();
        let after = normalized[after_start..].trim().to_string();
        Some(WakeDetection {
            keyword: self.keyword.clone(),
            utterance_after_wake: after,
        })
    }

    /// Configured keyword.
    pub fn keyword(&self) -> &str {
        &self.keyword
    }

    /// Returns true if this detector appears usable.
    pub fn is_healthy(&self) -> bool {
        !self.normalized_keyword.is_empty()
    }

    /// Returns true if a restart can plausibly fix detector state.
    pub fn should_restart(&self) -> bool {
        false
    }
}

/// Normalize transcript text for phrase matching.
pub fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = true;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            out.push(ch);
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_keyword_with_punctuation() {
        let detector = TranscriptWakeDetector::new("Hey Kairo");
        let got = detector.detect("Hey, Kairo, wat staat er op mijn planning?");
        assert!(got.is_some());
        assert_eq!(
            got.unwrap().utterance_after_wake,
            "wat staat er op mijn planning"
        );
    }

    #[test]
    fn ignores_non_matching_text() {
        let detector = TranscriptWakeDetector::new("hey kairo");
        assert!(detector.detect("ik praat tegen iemand anders").is_none());
    }

    #[test]
    fn normalize_collapses_symbols_and_spaces() {
        assert_eq!(normalize_text("  Hey,  KAIRO! "), "hey kairo");
    }
}
