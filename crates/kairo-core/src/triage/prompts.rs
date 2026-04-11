//! # Triage prompt management
//!
//! Loads and formats the triage system prompt from `prompts/triage-system.md`.
//! Injects runtime context (user name, current frame, memory summary) into
//! the prompt template before each triage call.

use crate::senses::types::PerceptionFrame;

/// JSON Schema for the triage decision.
///
/// Used with `llama_cpp_2::json_schema_to_grammar()` at init time to produce
/// a GBNF grammar that is guaranteed compatible with the linked llama.cpp
/// version. This replaces a hand-written GBNF that triggered assertion
/// failures in llama-grammar.cpp.
const TRIAGE_JSON_SCHEMA: &str = r#"{
  "oneOf": [
    {
      "type": "object",
      "properties": {
        "decision": { "const": "ignore" }
      },
      "required": ["decision"],
      "additionalProperties": false
    },
    {
      "type": "object",
      "properties": {
        "decision": { "const": "remember" },
        "summary": { "type": "string" }
      },
      "required": ["decision", "summary"],
      "additionalProperties": false
    },
    {
      "type": "object",
      "properties": {
        "decision": { "const": "whisper" },
        "text": { "type": "string" }
      },
      "required": ["decision", "text"],
      "additionalProperties": false
    },
    {
      "type": "object",
      "properties": {
        "decision": { "const": "execute_simple" },
        "action": { "type": "string" }
      },
      "required": ["decision", "action"],
      "additionalProperties": false
    },
    {
      "type": "object",
      "properties": {
        "decision": { "const": "wake_orchestrator" },
        "reason": { "type": "string" }
      },
      "required": ["decision", "reason"],
      "additionalProperties": false
    }
  ]
}"#;

/// Build the GBNF grammar string from the JSON schema at runtime.
///
/// This is called once at `TriageLayer::new()` time, not per-evaluation.
/// Falls back to the hand-written grammar file if schema conversion fails.
pub fn build_triage_grammar() -> String {
    match llama_cpp_2::json_schema_to_grammar(TRIAGE_JSON_SCHEMA) {
        Ok(grammar) => {
            tracing::debug!(
                layer = "triage",
                component = "grammar",
                grammar_len = grammar.len(),
                "Generated GBNF from JSON schema"
            );
            grammar
        }
        Err(e) => {
            tracing::warn!(
                layer = "triage",
                component = "grammar",
                error = %e,
                "json_schema_to_grammar failed, falling back to hand-written GBNF"
            );
            include_str!("../../../../prompts/triage-grammar.gbnf").to_string()
        }
    }
}

/// The triage system prompt, kept deliberately short to minimize prompt
/// processing time. At ~250 tokens this is 4x shorter than the original.
const SYSTEM_PROMPT: &str = include_str!("../../../../prompts/triage-system.md");

/// Build the triage prompt in Qwen 3 ChatML format.
///
/// The ChatML wrapper is required so `/no_think` suppresses thinking tokens.
/// The frame is serialized as compact JSON in the user turn to save tokens.
pub fn build_triage_prompt(frame: &PerceptionFrame, memory_summary: &str) -> String {
    let frame_json = serde_json::to_string(frame).unwrap_or_else(|_| "{}".to_string());

    let memory = if memory_summary.is_empty() {
        "No recent memory."
    } else {
        memory_summary
    };

    format!(
        "<|im_start|>system\n{SYSTEM_PROMPT}<|im_end|>\n\
         <|im_start|>user\n/no_think\n\
         Frame: {frame_json}\n\
         Memory: {memory}<|im_end|>\n\
         <|im_start|>assistant\n"
    )
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
    fn test_grammar_builds_successfully() {
        let grammar = build_triage_grammar();
        assert!(!grammar.is_empty());
        assert!(grammar.contains("root"));
    }

    #[test]
    fn test_system_prompt_is_compact() {
        assert!(SYSTEM_PROMPT.contains("triage"));
        assert!(SYSTEM_PROMPT.contains("ignore"));
        // System prompt should be under 1200 bytes (~300 tokens)
        assert!(
            SYSTEM_PROMPT.len() < 1200,
            "System prompt too long: {} bytes",
            SYSTEM_PROMPT.len()
        );
    }

    #[test]
    fn test_build_prompt_is_chatml() {
        let prompt = build_triage_prompt(&sample_frame(), "");
        assert!(prompt.starts_with("<|im_start|>system\n"));
        assert!(prompt.contains("<|im_end|>"));
        assert!(prompt.contains("<|im_start|>user\n/no_think\n"));
        assert!(prompt.ends_with("<|im_start|>assistant\n"));
    }

    #[test]
    fn test_build_prompt_contains_frame_data() {
        let prompt = build_triage_prompt(&sample_frame(), "");
        assert!(prompt.contains("Code.exe"));
        assert!(prompt.contains("main.rs - kairo-ai"));
    }

    #[test]
    fn test_build_prompt_with_memory() {
        let prompt = build_triage_prompt(&sample_frame(), "User was debugging a bug in layer.rs");
        assert!(prompt.contains("debugging a bug"));
    }

    #[test]
    fn test_build_prompt_empty_memory_gets_placeholder() {
        let prompt = build_triage_prompt(&sample_frame(), "");
        assert!(prompt.contains("No recent memory."));
    }
}
