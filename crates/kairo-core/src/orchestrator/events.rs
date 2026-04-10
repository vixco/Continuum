//! # Claude Code event types
//!
//! Strongly-typed representations of the JSON events that Claude Code emits
//! on stdout when running in `--output-format stream-json` mode.
//!
//! Key event types:
//! - `system` — init events with session id, tools, model info
//! - `assistant` — messages with content blocks (text, tool_use)
//! - `user` — messages containing tool_result blocks
//! - `stream_event` — content_block_delta events for live streaming text
//! - `result` — the final event with cost, duration, and session metadata
//!
//! Uses `#[serde(tag = "type")]` for top-level dispatch and handles unknown
//! variants gracefully to stay forward-compatible with CLI changes.
