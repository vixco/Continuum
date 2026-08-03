//! # Wake word detection
//!
//! The production path is intentionally local-only. Continuum can use a native
//! wake-word backend later, but the core runtime already has continuous
//! Whisper transcripts, so Phase 5 gates voice commands with a transcript
//! wake phrase by default.
//!
//! ## Homophone handling
//!
//! Whisper-small almost always transcribed "Kairo" — Continuum's name
//! before its rename — as "Cairo" (the Egyptian capital, a real English
//! word) because "Kairo" wasn't in its vocabulary. To keep the wake gate
//! from false-rejecting real utterances on a keyword like that, the
//! detector expands the configured keyword into a small set of phonetic
//! variants before matching. Currently just K→C, which covers that case
//! (and any future keyword with the same K/C ambiguity); a future
//! fuzzy-matcher can replace this if needed.

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
    variants: Vec<String>,
}

impl TranscriptWakeDetector {
    /// Creates a new transcript detector for a configurable keyword.
    /// The keyword is automatically expanded to include common whisper
    /// homophone variants (e.g. `continuum` → also matches `cairo`).
    pub fn new(keyword: impl Into<String>) -> Self {
        let keyword = keyword.into();
        let normalized = normalize_text(&keyword);
        let variants = expand_variants(&normalized);
        Self { keyword, variants }
    }

    /// Returns a detection if `transcript` contains any wake-phrase variant.
    pub fn detect(&self, transcript: &str) -> Option<WakeDetection> {
        let normalized = normalize_text(transcript);
        // Pick the variant that matches earliest in the transcript so that
        // "utterance_after_wake" reflects everything the user said after
        // the wake phrase, not after some overlapping sub-phrase.
        let mut best: Option<(usize, &String)> = None;
        for v in &self.variants {
            if v.is_empty() {
                continue;
            }
            if let Some(idx) = normalized.find(v.as_str()) {
                if best.map(|(best_idx, _)| idx < best_idx).unwrap_or(true) {
                    best = Some((idx, v));
                }
            }
        }
        let (idx, matched) = best?;
        let after_start = idx + matched.len();
        let after = normalized[after_start..].trim().to_string();
        Some(WakeDetection {
            keyword: self.keyword.clone(),
            utterance_after_wake: after,
        })
    }

    /// Configured keyword (original, pre-normalization).
    pub fn keyword(&self) -> &str {
        &self.keyword
    }

    /// Normalized variants the detector actually matches against. Exposed
    /// for logging and tests.
    pub fn variants(&self) -> &[String] {
        &self.variants
    }

    /// Returns true if this detector appears usable.
    pub fn is_healthy(&self) -> bool {
        self.variants.iter().any(|v| !v.is_empty())
    }

    /// Returns true if a restart can plausibly fix detector state.
    pub fn should_restart(&self) -> bool {
        false
    }
}

/// Expand a normalized keyword into phonetic variants whisper may produce.
///
/// Rule set (kept deliberately small):
///  - Every `k` → `c` (handles "kairo"→"cairo", "kyle"→"cyle")
///  - A leading `hey ` → `ei ` (observed Whisper transcription)
///  - Adds the unchanged keyword first so primary spelling wins ties.
///
/// Future: drop in a fuzzy matcher (Jaro-Winkler) with a confidence
/// threshold if more variants are needed.
pub fn expand_variants(normalized_keyword: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(4);
    if normalized_keyword.is_empty() {
        return out;
    }
    out.push(normalized_keyword.to_string());
    let k_to_c = normalized_keyword.replace('k', "c");
    if k_to_c != normalized_keyword {
        out.push(k_to_c);
    }
    if let Some(rest) = normalized_keyword.strip_prefix("hey ") {
        let ei_variant = format!("ei {rest}");
        out.push(ei_variant.clone());
        let ei_k_to_c = ei_variant.replace('k', "c");
        if ei_k_to_c != ei_variant {
            out.push(ei_k_to_c);
        }
    }
    out
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
        let detector = TranscriptWakeDetector::new("Hey Continuum");
        let got = detector.detect("Hey, Continuum, wat staat er op mijn planning?");
        assert!(got.is_some());
        assert_eq!(
            got.unwrap().utterance_after_wake,
            "wat staat er op mijn planning"
        );
    }

    #[test]
    fn ignores_non_matching_text() {
        let detector = TranscriptWakeDetector::new("hey continuum");
        assert!(detector.detect("ik praat tegen iemand anders").is_none());
    }

    #[test]
    fn normalize_collapses_symbols_and_spaces() {
        assert_eq!(normalize_text("  Hey,  CONTINUUM! "), "hey continuum");
    }

    #[test]
    fn matches_whisper_k_to_c_homophone() {
        // Whisper-small transcribes "Hey Kairo" as "Ei, Cairo!" (PT) or
        // "Hey Cairo!" (EN). Both should still fire the wake detector.
        let detector = TranscriptWakeDetector::new("hey kairo");
        assert!(detector.detect("Hey Cairo, what's the time?").is_some());
        assert!(detector.detect("Ei, Cairo!").is_some());
    }

    #[test]
    fn utterance_after_wake_captures_remaining_command() {
        let detector = TranscriptWakeDetector::new("hey kairo");
        let got = detector.detect("hey cairo open the build log").unwrap();
        assert_eq!(got.utterance_after_wake, "open the build log");
    }

    #[test]
    fn earliest_variant_wins_on_multiple_matches() {
        // If both variants appear, we want the first-occurring one so
        // `utterance_after_wake` is the longest possible tail.
        let detector = TranscriptWakeDetector::new("hey continuum");
        let got = detector
            .detect("hey cairo, hey continuum, check this")
            .unwrap();
        // "hey cairo" appears first; after = "hey continuum check this"
        assert!(got.utterance_after_wake.contains("check this"));
    }

    #[test]
    fn expand_variants_produces_k_and_c_versions() {
        let v = expand_variants("hey kairo");
        assert_eq!(
            v,
            vec![
                "hey kairo".to_string(),
                "hey cairo".to_string(),
                "ei kairo".to_string(),
                "ei cairo".to_string(),
            ]
        );
    }

    #[test]
    fn expand_variants_keeps_hey_to_ei_without_k() {
        let v = expand_variants("hey jarvis");
        assert_eq!(v, vec!["hey jarvis".to_string(), "ei jarvis".to_string()]);
    }

    #[test]
    fn expand_variants_empty_input() {
        assert!(expand_variants("").is_empty());
    }

    #[test]
    fn detector_with_empty_keyword_is_unhealthy() {
        let detector = TranscriptWakeDetector::new("");
        assert!(!detector.is_healthy());
        assert!(detector.detect("anything").is_none());
    }

    #[test]
    fn variants_accessor_exposes_normalized_set() {
        let detector = TranscriptWakeDetector::new("Hey Kairo!");
        let vs = detector.variants();
        assert!(vs.contains(&"hey kairo".to_string()));
        assert!(vs.contains(&"hey cairo".to_string()));
    }
}
