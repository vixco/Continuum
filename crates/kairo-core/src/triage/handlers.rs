//! # Triage decision handlers
//!
//! Each triage decision variant has a corresponding handler. These execute
//! the appropriate action based on the triage layer's output.
//!
//! Phase 2 handlers are minimal — real implementations come in later phases:
//! - Whisper: prints placeholder (real TTS is Phase 5)
//! - WakeOrchestrator: logs loudly (real orchestrator wake is Phase 3)
//! - ExecuteSimple: allowlisted actions only (expand in Phase 4)

use anyhow::{bail, Result};
use tracing::{debug, info, warn};

use crate::triage::TriageDecision;

/// Allowed actions for the `execute_simple` decision variant.
const ALLOWED_ACTIONS: &[&str] = &["launch_app", "show_notification", "toggle_mute"];

/// Handle a triage decision, executing the appropriate action.
///
/// Returns `Ok(())` for all decisions. Errors only occur for invalid
/// `execute_simple` actions.
pub fn handle_decision(decision: &TriageDecision) -> Result<()> {
    match decision {
        TriageDecision::Ignore => handle_ignore(),
        TriageDecision::Remember { summary } => handle_remember(summary),
        TriageDecision::Whisper { text } => handle_whisper(text),
        TriageDecision::ExecuteSimple { action } => handle_execute_simple(action),
        TriageDecision::WakeOrchestrator { reason } => handle_wake_orchestrator(reason),
    }
}

/// Ignore: drop the frame silently.
fn handle_ignore() -> Result<()> {
    debug!(
        layer = "triage",
        component = "handler",
        decision = "ignore",
        "Frame ignored"
    );
    Ok(())
}

/// Remember: flag the frame for episodic memory distillation.
///
/// In Phase 2, this writes a log entry. The actual memory system is Phase 3.
fn handle_remember(summary: &str) -> Result<()> {
    info!(
        layer = "triage",
        component = "handler",
        decision = "remember",
        summary = %summary,
        "Frame flagged for memory"
    );
    Ok(())
}

/// Whisper: speak a short sentence via local TTS.
///
/// Phase 2: prints a placeholder. Real TTS integration is Phase 5.
fn handle_whisper(text: &str) -> Result<()> {
    info!(
        layer = "triage",
        component = "handler",
        decision = "whisper",
        text = %text,
        "TTS placeholder"
    );
    println!("[would say via TTS: {text}]");
    Ok(())
}

/// Execute a simple pre-approved action.
///
/// Allowed actions: `launch_app`, `show_notification`, `toggle_mute`.
/// Action format: `action_name:parameter` (e.g., `launch_app:notepad`).
fn handle_execute_simple(action: &str) -> Result<()> {
    let action_name = action.split(':').next().unwrap_or(action);

    if !ALLOWED_ACTIONS.contains(&action_name) {
        warn!(
            layer = "triage",
            component = "handler",
            decision = "execute_simple",
            action = %action,
            allowed = ?ALLOWED_ACTIONS,
            "Rejected unknown action"
        );
        bail!(
            "Action '{}' is not in the allowlist. Allowed: {:?}",
            action_name,
            ALLOWED_ACTIONS
        );
    }

    info!(
        layer = "triage",
        component = "handler",
        decision = "execute_simple",
        action = %action,
        "Executing simple action"
    );

    // Phase 2: log only. Real execution is Phase 4 (MCP tools).
    match action_name {
        "launch_app" => {
            let param = action.strip_prefix("launch_app:").unwrap_or("unknown");
            println!("[would launch app: {param}]");
        }
        "show_notification" => {
            let param = action
                .strip_prefix("show_notification:")
                .unwrap_or("notification");
            println!("[would show notification: {param}]");
        }
        "toggle_mute" => {
            println!("[would toggle mute]");
        }
        _ => unreachable!("Validated above"),
    }

    Ok(())
}

/// Wake the orchestrator for genuine reasoning.
///
/// Phase 2: logs loudly and prints a placeholder. Real orchestrator
/// spawning is Phase 3.
fn handle_wake_orchestrator(reason: &str) -> Result<()> {
    warn!(
        layer = "triage",
        component = "handler",
        decision = "wake_orchestrator",
        reason = %reason,
        "WOULD WAKE ORCHESTRATOR"
    );
    println!("[WOULD WAKE ORCHESTRATOR: {reason}]");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handle_ignore() {
        assert!(handle_decision(&TriageDecision::Ignore).is_ok());
    }

    #[test]
    fn test_handle_remember() {
        let d = TriageDecision::Remember {
            summary: "user opened config file".to_string(),
        };
        assert!(handle_decision(&d).is_ok());
    }

    #[test]
    fn test_handle_whisper() {
        let d = TriageDecision::Whisper {
            text: "meeting in 5 minutes".to_string(),
        };
        assert!(handle_decision(&d).is_ok());
    }

    #[test]
    fn test_handle_execute_simple_launch_app() {
        let d = TriageDecision::ExecuteSimple {
            action: "launch_app:notepad".to_string(),
        };
        assert!(handle_decision(&d).is_ok());
    }

    #[test]
    fn test_handle_execute_simple_show_notification() {
        let d = TriageDecision::ExecuteSimple {
            action: "show_notification:Build complete".to_string(),
        };
        assert!(handle_decision(&d).is_ok());
    }

    #[test]
    fn test_handle_execute_simple_toggle_mute() {
        let d = TriageDecision::ExecuteSimple {
            action: "toggle_mute".to_string(),
        };
        assert!(handle_decision(&d).is_ok());
    }

    #[test]
    fn test_handle_execute_simple_rejected_unknown() {
        let d = TriageDecision::ExecuteSimple {
            action: "delete_everything:now".to_string(),
        };
        let result = handle_decision(&d);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not in the allowlist"));
    }

    #[test]
    fn test_handle_wake_orchestrator() {
        let d = TriageDecision::WakeOrchestrator {
            reason: "user asked a complex question".to_string(),
        };
        assert!(handle_decision(&d).is_ok());
    }

    #[test]
    fn test_all_allowed_actions_work() {
        for action in ALLOWED_ACTIONS {
            let d = TriageDecision::ExecuteSimple {
                action: action.to_string(),
            };
            assert!(
                handle_decision(&d).is_ok(),
                "Action '{action}' should be allowed"
            );
        }
    }
}
