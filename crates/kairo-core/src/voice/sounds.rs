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
//! | Cue       | Shape                           | Role                               |
//! |-----------|---------------------------------|------------------------------------|
//! | `Wake`    | 880 Hz → 1320 Hz ramp, 180 ms   | Wake word detected                 |
//! | `Listen`  | 1200 Hz tick, 80 ms             | Active listening started           |
//! | `Done`    | 660 Hz double-click, 180 ms     | Voice session ended cleanly        |
//! | `Error`   | 220 Hz → 165 Hz double-beep     | Something failed; keep non-alarming|
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
    /// Wake word just fired — Kairo is now paying attention.
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
            FeedbackCue::Wake => ramp_tone(880.0, 1320.0, 0.18),
            FeedbackCue::Listen => sine_tone(1200.0, 0.08),
            FeedbackCue::Done => double_click(660.0, 0.06, 0.05),
            FeedbackCue::Error => double_beep(220.0, 165.0, 0.10, 0.04),
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

/// Generate a plain sine tone at `freq` Hz for `duration_secs` seconds.
/// Applies a short linear attack/release to avoid click artefacts.
fn sine_tone(freq: f32, duration_secs: f32) -> Vec<f32> {
    let n = seconds_to_samples(duration_secs);
    let mut out = Vec::with_capacity(n);
    let fade = (n / 10).max(32);
    for i in 0..n {
        let t = i as f32 / CUE_SAMPLE_RATE as f32;
        let envelope = if i < fade {
            i as f32 / fade as f32
        } else if i >= n.saturating_sub(fade) {
            (n - i) as f32 / fade as f32
        } else {
            1.0
        };
        out.push(0.3 * envelope * (TAU * freq * t).sin());
    }
    out
}

/// Linear-ramp the frequency from `start_freq` to `end_freq` over the
/// duration. Useful for a "brighten up" cue on wake.
fn ramp_tone(start_freq: f32, end_freq: f32, duration_secs: f32) -> Vec<f32> {
    let n = seconds_to_samples(duration_secs);
    let mut out = Vec::with_capacity(n);
    let fade = (n / 10).max(32);
    let mut phase: f32 = 0.0;
    for i in 0..n {
        let alpha = i as f32 / n as f32;
        let freq = start_freq + (end_freq - start_freq) * alpha;
        phase += TAU * freq / CUE_SAMPLE_RATE as f32;
        let envelope = if i < fade {
            i as f32 / fade as f32
        } else if i >= n.saturating_sub(fade) {
            (n - i) as f32 / fade as f32
        } else {
            1.0
        };
        out.push(0.3 * envelope * phase.sin());
    }
    out
}

/// Two short sine tones at the same frequency separated by a gap of silence.
fn double_click(freq: f32, tone_secs: f32, gap_secs: f32) -> Vec<f32> {
    let tone = sine_tone(freq, tone_secs);
    let gap = vec![0.0; seconds_to_samples(gap_secs)];
    let mut out = Vec::with_capacity(tone.len() * 2 + gap.len());
    out.extend_from_slice(&tone);
    out.extend_from_slice(&gap);
    out.extend_from_slice(&tone);
    out
}

/// Two descending tones: first at `freq1`, then at `freq2` after a short
/// gap. Used for the error cue — deliberately low and non-alarming.
fn double_beep(freq1: f32, freq2: f32, tone_secs: f32, gap_secs: f32) -> Vec<f32> {
    let t1 = sine_tone(freq1, tone_secs);
    let t2 = sine_tone(freq2, tone_secs);
    let gap = vec![0.0; seconds_to_samples(gap_secs)];
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
        assert!(samples[0].abs() < 0.05, "first sample should be near zero");
        assert!(
            samples.last().copied().unwrap_or(1.0).abs() < 0.05,
            "last sample should be near zero"
        );
    }

    #[test]
    fn all_cues_bounded_by_safe_amplitude() {
        for cue in [
            FeedbackCue::Wake,
            FeedbackCue::Listen,
            FeedbackCue::Done,
            FeedbackCue::Error,
        ] {
            for s in cue.render() {
                assert!(s.abs() <= 0.31, "{} cue over nominal 0.3 amplitude", cue.name());
            }
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
    fn double_click_contains_gap_of_silence() {
        let samples = double_click(1000.0, 0.05, 0.04);
        let tone_len = seconds_to_samples(0.05);
        let gap_start = tone_len;
        let gap_end = tone_len + seconds_to_samples(0.04);
        for s in &samples[gap_start..gap_end] {
            assert_eq!(*s, 0.0);
        }
    }
}
