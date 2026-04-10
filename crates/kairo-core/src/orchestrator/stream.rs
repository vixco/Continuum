//! # Event stream processing
//!
//! Reads newline-delimited JSON events from a Claude Code process's stdout
//! and dispatches them to the appropriate handlers. Pipes `text_delta` events
//! to the TTS queue as they arrive for low-latency voice output.
