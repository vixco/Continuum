//! # Structured log capture
//!
//! A `tracing::Layer` that copies every emitted event into an in-memory
//! ring buffer (default 10 000 entries) and also fans each event out to a
//! tokio broadcast channel so the dashboard Logs tab can stream live.
//!
//! Events carry structured fields. We extract the two fields the four-layer
//! architecture standardises on (`layer`, `component`) so the dashboard can
//! filter without parsing the `message` field.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// Default ring buffer size.
pub const DEFAULT_BUFFER_CAP: usize = 10_000;

/// Broadcast channel capacity for the Logs tab live stream.
const LIVE_CHANNEL_CAPACITY: usize = 1024;

/// One captured log event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: u64,
    pub ts: DateTime<Utc>,
    pub level: String,
    pub layer: Option<String>,
    pub component: Option<String>,
    pub target: String,
    pub message: String,
    /// Remaining structured fields as key=value strings, for the Logs tab detail view.
    pub fields: Vec<(String, String)>,
}

/// Filter parameters for the `get_logs` Tauri command.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogFilter {
    pub level: Option<String>,
    pub layer: Option<String>,
    pub component: Option<String>,
    pub text: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
}

/// Shared handle to the log buffer + broadcast channel.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<LogBufferInner>>,
    live_tx: broadcast::Sender<LogEntry>,
    capacity: usize,
}

struct LogBufferInner {
    entries: VecDeque<LogEntry>,
    next_id: u64,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(LIVE_CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(LogBufferInner {
                entries: VecDeque::with_capacity(capacity),
                next_id: 1,
            })),
            live_tx: tx,
            capacity,
        }
    }

    /// Subscribe to new log entries as they arrive.
    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.live_tx.subscribe()
    }

    /// Query the buffer with a filter.
    pub fn query(&self, filter: &LogFilter) -> Vec<LogEntry> {
        let guard = self.inner.lock();
        let limit = filter.limit.unwrap_or(500);
        guard
            .entries
            .iter()
            .rev()
            .filter(|e| matches_filter(e, filter))
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn total_entries(&self) -> usize {
        self.inner.lock().entries.len()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn push(&self, entry: LogEntry) {
        {
            let mut guard = self.inner.lock();
            if guard.entries.len() >= self.capacity {
                guard.entries.pop_front();
            }
            guard.entries.push_back(entry.clone());
        }
        let _ = self.live_tx.send(entry);
    }
}

fn matches_filter(entry: &LogEntry, filter: &LogFilter) -> bool {
    if let Some(ref level) = filter.level {
        if !level.eq_ignore_ascii_case(&entry.level) {
            return false;
        }
    }
    if let Some(ref layer) = filter.layer {
        match &entry.layer {
            Some(l) if l.eq_ignore_ascii_case(layer) => {}
            _ => return false,
        }
    }
    if let Some(ref comp) = filter.component {
        match &entry.component {
            Some(c) if c.eq_ignore_ascii_case(comp) => {}
            _ => return false,
        }
    }
    if let Some(ref text) = filter.text {
        let needle = text.to_ascii_lowercase();
        if !entry.message.to_ascii_lowercase().contains(&needle)
            && !entry.target.to_ascii_lowercase().contains(&needle)
            && !entry
                .fields
                .iter()
                .any(|(_, v)| v.to_ascii_lowercase().contains(&needle))
        {
            return false;
        }
    }
    if let Some(since) = filter.since {
        if entry.ts < since {
            return false;
        }
    }
    true
}

/// A `tracing::Layer` that forwards events into a [`LogBuffer`].
pub struct BufferLayer {
    buffer: LogBuffer,
}

impl BufferLayer {
    pub fn new(buffer: LogBuffer) -> Self {
        Self { buffer }
    }
}

impl<S> Layer<S> for BufferLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        let id = {
            let mut guard = self.buffer.inner.lock();
            let id = guard.next_id;
            guard.next_id += 1;
            id
        };

        let entry = LogEntry {
            id,
            ts: Utc::now(),
            level: level_name(*meta.level()).to_string(),
            layer: visitor.layer,
            component: visitor.component,
            target: meta.target().to_string(),
            message: visitor.message.unwrap_or_default(),
            fields: visitor.other,
        };
        self.buffer.push(entry);
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    }
}

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    layer: Option<String>,
    component: Option<String>,
    other: Vec<(String, String)>,
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_named(field.name(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_named(field.name(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_named(field.name(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_named(field.name(), value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record_named(field.name(), format!("{value}"));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_named(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let mut s = String::new();
        let _ = write!(s, "{value:?}");
        self.record_named(field.name(), s);
    }
}

impl EventVisitor {
    fn record_named(&mut self, name: &str, value: String) {
        match name {
            "message" => self.message = Some(value),
            "layer" => self.layer = Some(value),
            "component" => self.component = Some(value),
            _ => self.other.push((name.to_string(), value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::info;
    use tracing_subscriber::layer::SubscriberExt;

    fn with_buffer<F: FnOnce()>(f: F) -> LogBuffer {
        let buf = LogBuffer::new(128);
        let sub = tracing_subscriber::registry().with(BufferLayer::new(buf.clone()));
        tracing::subscriber::with_default(sub, f);
        buf
    }

    #[test]
    fn captures_info_events_with_structured_fields() {
        let buf = with_buffer(|| {
            info!(
                layer = "triage",
                component = "llm",
                latency_ms = 420u64,
                "decision"
            );
        });
        let entries = buf.query(&LogFilter::default());
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.message, "decision");
        assert_eq!(e.layer.as_deref(), Some("triage"));
        assert_eq!(e.component.as_deref(), Some("llm"));
        assert_eq!(e.level, "info");
        assert!(e
            .fields
            .iter()
            .any(|(k, v)| k == "latency_ms" && v == "420"));
    }

    #[test]
    fn ring_buffer_drops_oldest() {
        let buf = with_buffer(|| {
            for i in 0..200 {
                info!(layer = "test", "n={i}");
            }
        });
        assert_eq!(buf.total_entries(), 128);
    }

    #[test]
    fn filter_by_level() {
        let buf = with_buffer(|| {
            tracing::warn!(layer = "t", "warn event");
            tracing::info!(layer = "t", "info event");
        });
        let f = LogFilter {
            level: Some("warn".into()),
            ..LogFilter::default()
        };
        let out = buf.query(&f);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].level, "warn");
    }

    #[test]
    fn filter_by_text_hits_message_and_fields() {
        let buf = with_buffer(|| {
            info!(layer = "x", reason = "breakfast", "nothing");
            info!(layer = "x", "brunch happened");
        });
        let f = LogFilter {
            text: Some("break".into()),
            ..LogFilter::default()
        };
        assert_eq!(buf.query(&f).len(), 1);
        let f2 = LogFilter {
            text: Some("brunch".into()),
            ..LogFilter::default()
        };
        assert_eq!(buf.query(&f2).len(), 1);
    }
}
