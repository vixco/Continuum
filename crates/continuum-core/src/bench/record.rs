//! # Record mode (context engine spec §9, Task C6)
//!
//! The on-disk format `continuum-perception --record <path>` writes and the
//! replay harness reads: newline-delimited JSON, one line per artifact.
//!
//! ```json
//! {"t_ms":0,"kind":"frame","data":{ …PerceptionFrame… }}
//! {"t_ms":11500,"kind":"event","data":{ …ContextEvent… }}
//! ```
//!
//! `t_ms` is **relative** to the recording's start, so a recording (or the
//! committed synthetic fixture) replays identically whenever it is run;
//! the absolute `ts` inside `data` is kept for provenance but the replay
//! rebases it from `t_ms` (see [`crate::bench::replay`]).
//!
//! # Real recordings are LOCAL-ONLY — never commit one
//!
//! A recording is post-privacy: scrubbers have run, `never_observe`
//! windows appear only as the `[excluded]` sentinel, `local_only` rows
//! carry their sensitivity tag. That makes it safe to *keep*, not safe to
//! *publish*. Its content is exactly what the privacy work protects —
//! window titles, captions, transcripts, project paths, commit subjects
//! from the user's real day. Recordings belong outside the repository
//! (`~/.continuum-dev/recordings/` is the suggested home) and must never
//! be committed, attached to an issue, or handed to a cloud model. The
//! only JSONL under version control is the hand-authored synthetic
//! fixture in `crates/continuum-core/benches/data/`
//! ([`crate::bench::fixture`]), which describes a scripted narrative and
//! contains no real observation at all.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::memory::events::ContextEvent;
use crate::senses::types::PerceptionFrame;

/// The payload of one recorded line: `kind` selects the variant, `data`
/// carries it. Boxed because a [`PerceptionFrame`] is much larger than a
/// [`ContextEvent`] and clippy is right about the size difference.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum RecordData {
    /// A post-privacy perception frame, exactly as the frame builder emitted it.
    Frame(Box<PerceptionFrame>),
    /// A collector event, exactly as it was handed to the events channel.
    Event(Box<ContextEvent>),
}

/// One line of a recording / of the fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordLine {
    /// Milliseconds since the recording started. Monotonically
    /// non-decreasing within a file.
    pub t_ms: i64,
    /// The recorded artifact.
    #[serde(flatten)]
    pub data: RecordData,
}

impl RecordLine {
    /// A frame line at `t_ms`.
    pub fn frame(t_ms: i64, frame: PerceptionFrame) -> Self {
        Self {
            t_ms,
            data: RecordData::Frame(Box::new(frame)),
        }
    }

    /// An event line at `t_ms`.
    pub fn event(t_ms: i64, event: ContextEvent) -> Self {
        Self {
            t_ms,
            data: RecordData::Event(Box::new(event)),
        }
    }

    /// The frame this line carries, if it is a frame line.
    pub fn as_frame(&self) -> Option<&PerceptionFrame> {
        match &self.data {
            RecordData::Frame(frame) => Some(frame),
            RecordData::Event(_) => None,
        }
    }

    /// The event this line carries, if it is an event line.
    pub fn as_event(&self) -> Option<&ContextEvent> {
        match &self.data {
            RecordData::Event(event) => Some(event),
            RecordData::Frame(_) => None,
        }
    }

    /// Stable token for summaries and tests.
    pub fn kind(&self) -> &'static str {
        match &self.data {
            RecordData::Frame(_) => "frame",
            RecordData::Event(_) => "event",
        }
    }
}

/// Serializes recorded artifacts to a JSONL file.
///
/// Every method is **best effort and infallible from the caller's point of
/// view**: a recording is a debugging aid, and a full disk must never take
/// down the perception loop. Failures are logged once per line and the
/// counters in [`Recorder::written`] / [`Recorder::failed`] tell the
/// operator what happened.
///
/// The writer flushes after every line: a recording is usually ended with
/// Ctrl+C, and a buffered tail that never lands is worse than the syscall.
#[derive(Debug)]
pub struct Recorder {
    file: Mutex<std::fs::File>,
    base: DateTime<Utc>,
    written: AtomicU64,
    failed: AtomicU64,
}

impl Recorder {
    /// Creates (or truncates) the recording at `path`, timestamping
    /// everything relative to `base`.
    pub fn create(path: &Path, base: DateTime<Utc>) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = std::fs::File::create(path)?;
        Ok(Self {
            file: Mutex::new(file),
            base,
            written: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        })
    }

    /// Relative offset of `ts` in milliseconds, clamped at zero — a frame
    /// stamped fractionally before the recorder was created (the builder
    /// assembled it a moment earlier) records as `0` rather than negative.
    fn offset_ms(&self, ts: DateTime<Utc>) -> i64 {
        ts.signed_duration_since(self.base)
            .num_milliseconds()
            .max(0)
    }

    /// Records one post-privacy perception frame.
    pub fn record_frame(&self, frame: &PerceptionFrame) {
        let line = RecordLine::frame(self.offset_ms(frame.ts), frame.clone());
        self.write_line(&line);
    }

    /// Records one collector event.
    pub fn record_event(&self, event: &ContextEvent) {
        let line = RecordLine::event(self.offset_ms(event.ts), event.clone());
        self.write_line(&line);
    }

    fn write_line(&self, line: &RecordLine) {
        let encoded = match serde_json::to_string(line) {
            Ok(encoded) => encoded,
            Err(error) => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    layer = "senses",
                    component = "recorder",
                    error = %error,
                    "Failed to serialize a recorded line; skipping it"
                );
                return;
            }
        };
        let mut file = self.file.lock();
        match writeln!(file, "{encoded}").and_then(|()| file.flush()) {
            Ok(()) => {
                self.written.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    layer = "senses",
                    component = "recorder",
                    error = %error,
                    "Failed to write a recorded line; recording may be incomplete"
                );
            }
        }
    }

    /// Lines successfully written so far.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Lines that could not be written.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
}

/// Parses a recording / fixture from JSONL text.
///
/// Blank lines and `//` comment lines are skipped (the committed fixture
/// carries a provenance header); every other line must parse, so a
/// truncated recording is reported rather than silently short.
pub fn parse_jsonl(text: &str) -> anyhow::Result<Vec<RecordLine>> {
    let mut out = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        let line: RecordLine = serde_json::from_str(trimmed)
            .map_err(|error| anyhow::anyhow!("line {}: {error}", index + 1))?;
        out.push(line);
    }
    Ok(out)
}

/// Renders lines back to JSONL, one object per line, with a `//` header.
pub fn to_jsonl(header: &[&str], lines: &[RecordLine]) -> anyhow::Result<String> {
    let mut out = String::new();
    for comment in header {
        out.push_str("// ");
        out.push_str(comment);
        out.push('\n');
    }
    for line in lines {
        out.push_str(&serde_json::to_string(line)?);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::fixture;

    #[test]
    fn record_line_wire_shape_is_t_ms_kind_data() {
        let lines = fixture::synthetic_narrative();
        let frame = lines
            .iter()
            .find(|l| l.as_frame().is_some())
            .expect("fixture has frames");
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(frame).unwrap()).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 3, "exactly t_ms + kind + data: {obj:?}");
        assert!(obj["t_ms"].is_i64());
        assert_eq!(obj["kind"], "frame");
        assert!(obj["data"]["id"].is_string());

        let event = lines
            .iter()
            .find(|l| l.as_event().is_some())
            .expect("fixture has events");
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(event).unwrap()).unwrap();
        assert_eq!(json["kind"], "event");
        assert!(json["data"]["event_type"].is_string());
    }

    #[test]
    fn jsonl_round_trips_through_parse_and_render() {
        let lines = fixture::synthetic_narrative();
        let text = to_jsonl(&["header"], &lines).unwrap();
        assert!(text.starts_with("// header\n"));
        let back = parse_jsonl(&text).unwrap();
        assert_eq!(back.len(), lines.len());
        for (a, b) in lines.iter().zip(back.iter()) {
            assert_eq!(a.t_ms, b.t_ms);
            assert_eq!(a.kind(), b.kind());
        }
    }

    #[test]
    fn parse_reports_the_offending_line_number() {
        let text = "{\"t_ms\":0,\"kind\":\"event\",\"data\":{}}\nnot json\n";
        let error = parse_jsonl(text).unwrap_err().to_string();
        assert!(error.starts_with("line 1:"), "{error}");
    }

    #[tokio::test]
    async fn recorder_writes_relative_timestamps_and_replays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.jsonl");
        let lines = fixture::synthetic_narrative();
        let base = fixture::FIXTURE_BASE.parse::<DateTime<Utc>>().unwrap();
        let recorder = Recorder::create(&path, base).unwrap();

        // Feed the fixture through the recorder as if it were live: the
        // absolute ts a live artifact would carry is base + t_ms.
        for line in lines.iter().take(12) {
            let ts = base + chrono::Duration::milliseconds(line.t_ms);
            match &line.data {
                RecordData::Frame(frame) => {
                    let mut frame = (**frame).clone();
                    frame.ts = ts;
                    recorder.record_frame(&frame);
                }
                RecordData::Event(event) => {
                    let mut event = (**event).clone();
                    event.ts = ts;
                    recorder.record_event(&event);
                }
            }
        }
        assert_eq!(recorder.written(), 12);
        assert_eq!(recorder.failed(), 0);

        let back = parse_jsonl(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.len(), 12);
        for (a, b) in lines.iter().take(12).zip(back.iter()) {
            assert_eq!(a.t_ms, b.t_ms, "relative offsets survive the round trip");
            assert_eq!(a.kind(), b.kind());
        }
    }
}
