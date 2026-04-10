//! # Triage prompt management
//!
//! Loads and formats the triage system prompt from `prompts/triage-system.md`.
//! Injects runtime context (user name, current frame, memory summary) into
//! the prompt template before each triage call.

use crate::senses::types::PerceptionFrame;

/// The GBNF grammar for triage decisions, loaded from the grammar file.
///
/// This is compiled into the binary to avoid runtime file reads.
pub const TRIAGE_GRAMMAR: &str = include_str!("../../../../prompts/triage-grammar.gbnf");

/// The triage system prompt template.
///
/// Placeholders: `{user}`, `{PERCEPTION_FRAME}`, `{MEMORY_SUMMARY}`.
const PROMPT_TEMPLATE: &str = include_str!("../../../../prompts/triage-system.md");

/// Build the full triage prompt for a single evaluation call.
///
/// Substitutes the perception frame JSON and memory summary into the
/// template. The user name defaults to "the user" until Phase 3 adds
/// user profile support.
pub fn build_triage_prompt(frame: &PerceptionFrame, memory_summary: &str) -> String {
    let frame_json = serde_json::to_string_pretty(frame).unwrap_or_else(|_| "{}".to_string());

    let memory = if memory_summary.is_empty() {
        "No recent memory available."
    } else {
        memory_summary
    };

    PROMPT_TEMPLATE
        .replace("{user}", "the user")
        .replace("{PERCEPTION_FRAME}", &frame_json)
        .replace("{MEMORY_SUMMARY}", memory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senses::types::{ContextObservation, PerceptionFrame, ScreenObservation};
    use chrono::Utc;
    use uuid::Uuid;

    fn sample_frame() -> PerceptionFrame {
        PerceptionFrame {
            id: Uuid::nil(),
            ts: Utc::now(),
            screen: ScreenObservation {
                description: "VS Code editing main.rs".to_string(),
                foreground_app: "Code.exe".to_string(),
                has_error_visible: false,
                confidence: 0.8,
                screenshot_path: None,
                ts: Utc::now(),
            },
            audio: None,
            context: ContextObservation {
                foreground_window_title: "main.rs - kairo-ai".to_string(),
                foreground_process_name: "Code.exe".to_string(),
                idle_seconds: 5,
                in_call: false,
                ts: Utc::now(),
            },
            salience_hint: 0.25,
        }
    }

    #[test]
    fn test_grammar_is_nonempty() {
        assert!(TRIAGE_GRAMMAR.contains("root"));
        assert!(TRIAGE_GRAMMAR.contains("decision-body"));
    }

    #[test]
    fn test_prompt_template_is_nonempty() {
        assert!(PROMPT_TEMPLATE.contains("{PERCEPTION_FRAME}"));
        assert!(PROMPT_TEMPLATE.contains("{MEMORY_SUMMARY}"));
    }

    #[test]
    fn test_build_prompt_substitutes_frame() {
        let prompt = build_triage_prompt(&sample_frame(), "");
        assert!(prompt.contains("Code.exe"));
        assert!(prompt.contains("main.rs - kairo-ai"));
        assert!(!prompt.contains("{PERCEPTION_FRAME}"));
        assert!(!prompt.contains("{MEMORY_SUMMARY}"));
    }

    #[test]
    fn test_build_prompt_with_memory() {
        let prompt = build_triage_prompt(&sample_frame(), "User was debugging a bug in layer.rs");
        assert!(prompt.contains("debugging a bug"));
    }

    #[test]
    fn test_build_prompt_empty_memory_gets_placeholder() {
        let prompt = build_triage_prompt(&sample_frame(), "");
        assert!(prompt.contains("No recent memory available"));
    }
}
