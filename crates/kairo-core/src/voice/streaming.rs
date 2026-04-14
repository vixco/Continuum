//! # Streaming speech controller
//!
//! Binds a [`TtsEngine`] to a [`PlaybackStream`] so the orchestrator's
//! `TextDelta` events can be spoken while Opus is still generating. The
//! controller buffers tokens until a sentence terminator arrives, then
//! hands the complete sentence to a dedicated synthesis worker thread.
//!
//! The split is **sentence-level**, not token-level: Piper cannot
//! synthesise a single word faster than the prompt-processing overhead
//! (≈ 100 ms), so per-token synthesis would only add jitter. Per-sentence
//! synthesis gives coherent prosody, a stable first-audio latency, and
//! naturally chunked playback that aligns with how humans pause between
//! clauses.
//!
//! The synthesis worker runs in a dedicated OS thread (not a tokio task)
//! because [`piper_rs::Piper::create`] is CPU-bound and calling it from a
//! tokio worker would block the async runtime. The controller pushes jobs
//! onto an mpsc channel; the worker drains serially and pushes PCM into
//! the [`PlaybackStream`].
//!
//! [`TtsEngine`]: crate::voice::tts::TtsEngine
//! [`PlaybackStream`]: crate::voice::playback::PlaybackStream

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::voice::playback::PlaybackStream;
use crate::voice::tts::TtsEngine;

/// A job for the synthesis worker thread.
enum SpeechJob {
    /// Synthesise this sentence and push it to playback.
    Speak {
        text: String,
        language: Option<String>,
        generation: u64,
    },
    /// Shut down the worker thread. Sent on `Drop`.
    Shutdown,
}

/// Controls streaming speech output for the orchestrator and triage layers.
///
/// Cloneable via `Arc` — holds the engine, playback stream, and sentence
/// buffer, and forwards synthesis requests to a single worker thread.
pub struct SpeechController {
    tx: mpsc::Sender<SpeechJob>,
    pending: Arc<AtomicUsize>,
    generation: Arc<AtomicU64>,
    buffer: Mutex<String>,
    playback: Arc<PlaybackStream>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl SpeechController {
    /// Wire an engine and a playback stream into a streaming controller.
    ///
    /// Spawns the synthesis worker thread immediately; it lives until the
    /// controller is dropped.
    pub fn new(engine: Arc<dyn TtsEngine>, playback: Arc<PlaybackStream>) -> Self {
        let (tx, rx) = mpsc::channel::<SpeechJob>();
        let pending = Arc::new(AtomicUsize::new(0));
        let generation = Arc::new(AtomicU64::new(0));

        let worker_engine = engine;
        let worker_playback = playback.clone();
        let worker_pending = pending.clone();
        let worker_generation = generation.clone();
        let worker = thread::Builder::new()
            .name("kairo-tts-worker".into())
            .spawn(move || {
                worker_loop(
                    worker_engine,
                    worker_playback,
                    worker_pending,
                    worker_generation,
                    rx,
                )
            })
            .expect("failed to spawn TTS worker thread");

        Self {
            tx,
            pending,
            generation,
            buffer: Mutex::new(String::new()),
            playback,
            worker: Mutex::new(Some(worker)),
        }
    }

    /// Append a streaming text fragment (e.g. an Opus `TextDelta`). If the
    /// fragment completes a sentence, the buffered sentence is enqueued
    /// for synthesis immediately.
    ///
    /// Safe to call from any thread. Non-blocking — synthesis happens
    /// on the worker thread.
    pub fn push_delta(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut buf = match self.buffer.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        buf.push_str(text);

        // Drain as many complete sentences as currently in the buffer.
        while let Some(cut) = find_sentence_end(&buf) {
            let sentence: String = buf.drain(..cut).collect();
            drop(buf);
            let trimmed = sentence.trim();
            if !trimmed.is_empty() {
                self.enqueue(trimmed, None);
            }
            buf = match self.buffer.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }

    /// Flush any pending text in the buffer as a final utterance. Call this
    /// when the orchestrator stream ends without a final sentence terminator
    /// (e.g. a response that trails off without a period).
    pub fn flush(&self) {
        let remaining: String = {
            let mut buf = match self.buffer.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::take(&mut *buf)
        };
        let trimmed = remaining.trim();
        if !trimmed.is_empty() {
            self.enqueue(trimmed, None);
        }
    }

    /// Speak `text` as a single utterance, bypassing the delta buffer.
    /// Used for triage `whisper` decisions and any other code path that
    /// already has a complete sentence.
    pub fn say(&self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        self.enqueue(trimmed, None);
    }

    /// Speak `text` with a language hint for voice routing.
    pub fn say_with_language(&self, text: &str, language: Option<&str>) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        self.enqueue(trimmed, language);
    }

    /// Interrupt speech output. Clears buffered text and queued playback, and
    /// invalidates queued synthesis jobs so stale utterances are skipped.
    pub fn interrupt(&self) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.clear();
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.playback.clear();
        tracing::info!(
            layer = "voice",
            component = "streaming",
            "Speech playback interrupted"
        );
    }

    /// Block until every queued utterance has been synthesised and played.
    ///
    /// Polls at 20 ms; adequate for "wait for Kairo to finish talking"
    /// use cases, not for anything requiring tighter sync.
    pub fn wait_idle(&self) {
        while self.pending.load(Ordering::Acquire) > 0 || self.playback.is_active() {
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Number of utterances queued but not yet fully played. Includes the
    /// currently-synthesising one.
    pub fn pending_count(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    /// Returns true while synthesis jobs are queued or playback is active.
    pub fn is_speaking(&self) -> bool {
        self.pending_count() > 0 || self.playback.is_active()
    }

    fn enqueue(&self, sentence: &str, language: Option<&str>) {
        self.pending.fetch_add(1, Ordering::AcqRel);
        let job = SpeechJob::Speak {
            text: sentence.to_string(),
            language: language.map(str::to_string),
            generation: self.generation.load(Ordering::Acquire),
        };
        if self.tx.send(job).is_err() {
            // Worker died — we must not leave `pending` inflated.
            self.pending.fetch_sub(1, Ordering::AcqRel);
            tracing::warn!(
                layer = "voice",
                component = "streaming",
                "TTS worker channel closed, dropping utterance"
            );
        }
    }
}

impl Drop for SpeechController {
    fn drop(&mut self) {
        let _ = self.tx.send(SpeechJob::Shutdown);
        if let Ok(mut slot) = self.worker.lock() {
            if let Some(handle) = slot.take() {
                let _ = handle.join();
            }
        }
    }
}

/// Worker thread main loop. Drains synthesis jobs serially, pushing PCM
/// into the playback stream. Exits on `Shutdown` or channel close.
fn worker_loop(
    engine: Arc<dyn TtsEngine>,
    playback: Arc<PlaybackStream>,
    pending: Arc<AtomicUsize>,
    generation: Arc<AtomicU64>,
    rx: mpsc::Receiver<SpeechJob>,
) {
    tracing::debug!(
        layer = "voice",
        component = "streaming",
        engine = engine.name(),
        "TTS worker started"
    );

    while let Ok(job) = rx.recv() {
        match job {
            SpeechJob::Speak {
                text,
                language,
                generation: job_generation,
            } => {
                if job_generation != generation.load(Ordering::Acquire) {
                    pending.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                match engine.synthesize_for_language(&text, language.as_deref()) {
                    Ok(audio) => {
                        if job_generation == generation.load(Ordering::Acquire) {
                            playback.push_mono(&audio.samples, audio.sample_rate);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            layer = "voice",
                            component = "streaming",
                            error = %e,
                            text_preview = %truncate_for_log(&text),
                            "Synthesis failed, dropping utterance"
                        );
                    }
                }
                pending.fetch_sub(1, Ordering::AcqRel);
            }
            SpeechJob::Shutdown => break,
        }
    }

    tracing::debug!(
        layer = "voice",
        component = "streaming",
        "TTS worker exiting"
    );
}

/// Find the byte position one past the end of the first complete sentence
/// in `s`, or `None` if no terminator has arrived yet.
///
/// Terminators: `.`, `!`, `?`, `:`, `;` *followed by* whitespace or end of
/// string, or `\n\n` (paragraph break). This deliberately skips `.` inside
/// numbers (`3.14`) and adjacent to word characters (`e.g`), so decimals
/// and tight abbreviations stay in one utterance — at the cost of letting
/// some edge cases like `"e.g. see"` break early, which is acceptable.
pub(crate) fn find_sentence_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        let b = bytes[i];
        // Paragraph break.
        if b == b'\n' && bytes.get(i + 1) == Some(&b'\n') {
            return Some(i + 2);
        }
        // Sentence terminator.
        if matches!(b, b'.' | b'!' | b'?' | b':' | b';') {
            // Skip repeated terminators ("...", "?!", "!!"): only split on
            // the last in the run.
            let next = bytes.get(i + 1).copied();
            let is_terminator = |c: u8| matches!(c, b'.' | b'!' | b'?' | b':' | b';');
            if let Some(c) = next {
                if is_terminator(c) {
                    continue;
                }
                if c.is_ascii_whitespace() {
                    return Some(i + 1);
                }
            } else {
                return Some(i + 1);
            }
        }
    }
    None
}

fn truncate_for_log(s: &str) -> String {
    if s.len() <= 60 {
        s.to_string()
    } else {
        format!("{}...", &s[..57])
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentence_end_returns_none_for_incomplete() {
        assert_eq!(find_sentence_end("Hello there"), None);
        assert_eq!(find_sentence_end(""), None);
        assert_eq!(find_sentence_end("almost. "), Some(7));
    }

    #[test]
    fn sentence_end_at_end_of_string() {
        assert_eq!(find_sentence_end("Hello there."), Some(12));
        assert_eq!(find_sentence_end("Really?"), Some(7));
        assert_eq!(find_sentence_end("Wow!"), Some(4));
    }

    #[test]
    fn sentence_end_before_whitespace_splits_one_sentence() {
        assert_eq!(find_sentence_end("Hello. World."), Some(6));
    }

    #[test]
    fn sentence_end_skips_decimals() {
        // '.' inside "3.14" has a digit after it, so no split there.
        assert_eq!(find_sentence_end("Pi is 3.14 exactly"), None);
        // But a period after the number followed by space → split.
        assert_eq!(find_sentence_end("Pi is 3.14. More."), Some(11));
    }

    #[test]
    fn sentence_end_skips_trailing_dots_in_run() {
        // "..." is treated as one terminator run; only the final dot counts.
        assert_eq!(find_sentence_end("Wait..."), Some(7));
        assert_eq!(find_sentence_end("Wait... more"), Some(7));
    }

    #[test]
    fn sentence_end_paragraph_break() {
        assert_eq!(find_sentence_end("first para\n\nsecond"), Some(12));
    }

    #[test]
    fn sentence_end_single_newline_does_not_split() {
        assert_eq!(find_sentence_end("line one\nline two"), None);
    }

    #[test]
    fn sentence_end_multiple_terminators_runs_together() {
        // "?!" counted as one run.
        assert_eq!(find_sentence_end("What?!"), Some(6));
        // "?! " splits at the '!'.
        assert_eq!(find_sentence_end("What?! Ok"), Some(6));
    }

    #[test]
    fn sentence_end_colon_in_address_still_splits() {
        // We accept that "https://" etc. would split — orchestrator
        // rarely speaks URLs; if it matters, wrap them.
        assert_eq!(find_sentence_end("See: next line"), Some(4));
    }

    #[test]
    fn truncate_for_log_short() {
        assert_eq!(truncate_for_log("hi"), "hi");
    }

    #[test]
    fn truncate_for_log_long() {
        let s = "x".repeat(200);
        assert_eq!(truncate_for_log(&s).len(), 60);
        assert!(truncate_for_log(&s).ends_with("..."));
    }

    // Controller tests that touch a real cpal device don't belong in
    // `cargo test` (CI has no audio stack). The sentence splitter has full
    // coverage above; controller wiring is verified by the `kairo` binary
    // smoke test and in manual runs.
}
