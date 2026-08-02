//! # Feedback sounds
//!
//! Short procedurally-generated audio cues for voice state transitions.
//! Each cue is a few hundred milliseconds of windowed sine tones rendered at
//! 22050 Hz mono so it can flow through the same [`PlaybackStream`] resample
//! path as Piper utterances.
//!
//! Generating them at runtime means we avoid bundling `.wav` files in the
//! repo, keeps the crate portable, and lets the user tune frequencies or
//! disable the cues entirely via `config.voice.feedback_sounds`.
//!
//! | Cue       | Shape                                  | Role                               |
//! |-----------|----------------------------------------|------------------------------------|
//! | `Wake`    | C5 → G5 ascending chord, 260 ms        | Wake word detected                 |
//! | `Listen`  | 660 Hz soft tick, 50 ms                | Active listening started           |
//! | `Done`    | E5 → C5 descending, 280 ms             | Voice session ended cleanly        |
//! | `Error`   | 220 Hz → 165 Hz double-beep, 240 ms    | Something failed; keep non-alarming|
//!
//! All cues use a cosine (raised-cosine) envelope and sit at a low base
//! amplitude (0.12 vs the old 0.3) so they don't fatigue the user over
//! a long session — the old pure-sine 0.3-amplitude pings were piercing.
//!
//! [`PlaybackStream`]: crate::voice::playback::PlaybackStream

use std::f32::consts::TAU;
use std::sync::Arc;

use crate::voice::playback::PlaybackStream;

/// Sample rate used for all generated cues. Matches Piper medium voices,
/// which lets the playback stream reuse the same resample path without a
/// special-case.
pub const CUE_SAMPLE_RATE: u32 = 22_050;

/// The set of transitions worth announcing to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackCue {
    /// Wake word just fired — Continuum is now paying attention.
    Wake,
    /// Active listening began (hotkey or post-wake transition).
    Listen,
    /// Voice session completed cleanly — nothing else pending.
    Done,
    /// Something broke — keep it non-alarming, not a klaxon.
    Error,
}

impl FeedbackCue {
    /// Render this cue to mono `f32` PCM at [`CUE_SAMPLE_RATE`].
    pub fn render(self) -> Vec<f32> {
        match self {
            // Two ascending notes — C5 then G5 (perfect fifth), slight
            // overlap for a bell-like blend. Gentle attack, slower
            // release.
            FeedbackCue::Wake => chime(523.25, 783.99, 0.13, 0.15, 0.04),
            // Barely-there tick at a round, dull 660 Hz. Much lower
            // amplitude than the old 1200 Hz ping.
            FeedbackCue::Listen => soft_tone(660.0, 0.05, 0.08),
            // Descending E5 → C5 (minor third down), friendlier than
            // double-click.
            FeedbackCue::Done => two_note(659.25, 523.25, 0.13, 0.15, 0.03),
            // Existing double-beep, but gentler amplitude.
            FeedbackCue::Error => double_beep(220.0, 165.0, 0.10, 0.04, 0.10),
        }
    }

    /// Identifier used for logging.
    pub fn name(self) -> &'static str {
        match self {
            FeedbackCue::Wake => "wake",
            FeedbackCue::Listen => "listen",
            FeedbackCue::Done => "done",
            FeedbackCue::Error => "error",
        }
    }
}

/// Thin helper that owns the enabled flag and (optionally) the
/// [`PlaybackStream`] so callers don't have to wire both through.
/// Clone-friendly via `Arc`.
#[derive(Clone)]
pub struct FeedbackPlayer {
    playback: Option<Arc<PlaybackStream>>,
    enabled: bool,
}

impl FeedbackPlayer {
    /// Wrap a playback stream with an enable flag. `enabled = false` makes
    /// [`FeedbackPlayer::play`] a no-op regardless of the stream.
    pub fn new(playback: Arc<PlaybackStream>, enabled: bool) -> Self {
        Self {
            playback: Some(playback),
            enabled,
        }
    }

    /// A FeedbackPlayer that never plays anything. Useful when the audio
    /// stack isn't available (headless tests, `--no-tts`, etc.) and you
    /// still want a value to pass through.
    pub fn disabled() -> Self {
        Self {
            playback: None,
            enabled: false,
        }
    }

    /// Play a cue. No-op when disabled or no playback stream is attached.
    pub fn play(&self, cue: FeedbackCue) {
        if !self.enabled {
            return;
        }
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let samples = cue.render();
        tracing::debug!(
            layer = "voice",
            component = "feedback",
            cue = cue.name(),
            samples = samples.len(),
            "Queued feedback cue"
        );
        playback.push_mono(&samples, CUE_SAMPLE_RATE);
    }
}

// ---------------------------------------------------------------------------
// Waveform primitives
// ---------------------------------------------------------------------------

fn seconds_to_samples(secs: f32) -> usize {
    (secs * CUE_SAMPLE_RATE as f32).round() as usize
}

/// Raised-cosine (Hann) envelope: 0.0 at the edges, 1.0 at the middle,
/// smoothly curving. Sounds softer than a linear fade — the derivative
/// is continuous at the boundaries, so there's no audible "click".
fn hann(i: usize, n: usize) -> f32 {
    if n <= 1 {
        return 0.0;
    }
    0.5 * (1.0 - (TAU * i as f32 / (n - 1) as f32).cos())
}

/// Generate a soft sine tone at `freq` Hz for `duration_secs`, using a
/// Hann envelope and a configurable peak amplitude.
fn soft_tone(freq: f32, duration_secs: f32, amplitude: f32) -> Vec<f32> {
    let n = seconds_to_samples(duration_secs);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / CUE_SAMPLE_RATE as f32;
        out.push(amplitude * hann(i, n) * (TAU * freq * t).sin());
    }
    out
}

/// Two-tone "chime": freq1 plays for `tone_secs` seconds with a Hann
/// envelope, freq2 plays immediately after with the same shape. If
/// `overlap_secs` > 0, the second tone's attack overlaps the first
/// tone's release for a bell-like blend instead of a hard gap.
fn chime(freq1: f32, freq2: f32, tone_secs: f32, amplitude: f32, overlap_secs: f32) -> Vec<f32> {
    let t1 = soft_tone(freq1, tone_secs, amplitude);
    let t2 = soft_tone(freq2, tone_secs, amplitude);
    let overlap = seconds_to_samples(overlap_secs).min(t1.len()).min(t2.len());
    let total = t1.len() + t2.len() - overlap;
    let mut out = vec![0.0_f32; total];
    for (i, &s) in t1.iter().enumerate() {
        out[i] += s;
    }
    let offset = t1.len() - overlap;
    for (i, &s) in t2.iter().enumerate() {
        out[offset + i] += s;
    }
    out
}

/// Two tones in sequence (no overlap), first at `freq1` then at `freq2`.
/// Descending pair for the Done cue.
fn two_note(freq1: f32, freq2: f32, tone_secs: f32, amplitude: f32, gap_secs: f32) -> Vec<f32> {
    let t1 = soft_tone(freq1, tone_secs, amplitude);
    let t2 = soft_tone(freq2, tone_secs, amplitude);
    let gap = vec![0.0_f32; seconds_to_samples(gap_secs)];
    let mut out = Vec::with_capacity(t1.len() + gap.len() + t2.len());
    out.extend_from_slice(&t1);
    out.extend_from_slice(&gap);
    out.extend_from_slice(&t2);
    out
}

/// Two descending tones with a configurable amplitude. Kept separate
/// from `two_note` so the error cue can use its own amplitude profile.
fn double_beep(freq1: f32, freq2: f32, tone_secs: f32, gap_secs: f32, amplitude: f32) -> Vec<f32> {
    let t1 = soft_tone(freq1, tone_secs, amplitude);
    let t2 = soft_tone(freq2, tone_secs, amplitude);
    let gap = vec![0.0_f32; seconds_to_samples(gap_secs)];
    let mut out = Vec::with_capacity(t1.len() + gap.len() + t2.len());
    out.extend_from_slice(&t1);
    out.extend_from_slice(&gap);
    out.extend_from_slice(&t2);
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_cue_renders_nonempty_audio() {
        let samples = FeedbackCue::Wake.render();
        assert!(!samples.is_empty());
        assert!(samples.iter().any(|&s| s.abs() > 0.01));
    }

    #[test]
    fn listen_cue_is_shortest() {
        let wake = FeedbackCue::Wake.render().len();
        let listen = FeedbackCue::Listen.render().len();
        let done = FeedbackCue::Done.render().len();
        let error = FeedbackCue::Error.render().len();
        assert!(listen < wake);
        assert!(listen < done);
        assert!(listen < error);
    }

    #[test]
    fn cues_have_soft_edges() {
        let samples = FeedbackCue::Wake.render();
        assert!(
            samples[0].abs() < 0.01,
            "first sample should be at Hann zero"
        );
        assert!(
            samples.last().copied().unwrap_or(1.0).abs() < 0.01,
            "last sample should be at Hann zero"
        );
    }

    #[test]
    fn all_cues_below_soft_amplitude_ceiling() {
        // Raised to 0.31 in the old design. The new design keeps every
        // cue under 0.18 so they sit clearly below speech volume
        // (typical Piper output peaks near 0.5–0.8).
        for cue in [
            FeedbackCue::Wake,
            FeedbackCue::Listen,
            FeedbackCue::Done,
            FeedbackCue::Error,
        ] {
            let peak = cue.render().iter().map(|s| s.abs()).fold(0.0_f32, f32::max);
            assert!(peak <= 0.18, "{} cue peaks at {peak}", cue.name());
        }
    }

    #[test]
    fn names_are_unique() {
        let names = [
            FeedbackCue::Wake.name(),
            FeedbackCue::Listen.name(),
            FeedbackCue::Done.name(),
            FeedbackCue::Error.name(),
        ];
        let unique: std::collections::HashSet<_> = names.into_iter().collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn two_note_contains_gap_of_silence() {
        let samples = two_note(660.0, 440.0, 0.05, 0.1, 0.04);
        let tone_len = seconds_to_samples(0.05);
        let gap_start = tone_len;
        let gap_end = tone_len + seconds_to_samples(0.04);
        for s in &samples[gap_start..gap_end] {
            assert_eq!(*s, 0.0);
        }
    }

    #[test]
    fn hann_envelope_is_zero_at_edges_one_at_middle() {
        let n = 1000;
        assert!(hann(0, n) < 1e-6);
        assert!(hann(n - 1, n) < 1e-6);
        let mid = hann(n / 2, n);
        assert!((mid - 1.0).abs() < 0.01, "Hann peak at middle: {mid}");
    }

    #[test]
    fn chime_length_equals_sum_minus_overlap() {
        let c = chime(440.0, 660.0, 0.1, 0.1, 0.03);
        let tone = seconds_to_samples(0.1);
        let overlap = seconds_to_samples(0.03);
        assert_eq!(c.len(), tone * 2 - overlap);
    }
}
