//! # Event stream processing
//!
//! Re-exports and utilities for working with the orchestrator event stream.
//! The primary stream processing logic lives in [`super::spawn::process_event_stream`].
//!
//! This module provides helper functions for consumers that want to handle
//! orchestrator events (e.g. printing to terminal, feeding TTS queue).

use super::spawn::OrchestratorEvent;

/// Formats an orchestrator event for terminal display.
///
/// Returns `Some(text)` for events that should be shown to the user,
/// `None` for internal events.
pub fn format_for_terminal(event: &OrchestratorEvent) -> Option<String> {
    match event {
        OrchestratorEvent::TextDelta(text) => Some(text.clone()),
        OrchestratorEvent::ResponseComplete {
            cost_usd,
            duration_ms,
            ..
        } => {
            let mut parts = Vec::new();
            if let Some(cost) = cost_usd {
                parts.push(format!("${cost:.4}"));
            }
            if let Some(dur) = duration_ms {
                parts.push(format!("{dur}ms"));
            }
            if parts.is_empty() {
                None
            } else {
                Some(format!(" [{}]", parts.join(", ")))
            }
        }
        OrchestratorEvent::Error(msg) => Some(format!("[ERROR: {msg}]")),
        OrchestratorEvent::SessionReady { .. } => None,
    }
}
