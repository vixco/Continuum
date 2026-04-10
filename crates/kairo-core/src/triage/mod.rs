//! # Layer 2 — Triage
//!
//! The triage layer is a small local LLM (3–4B parameters) that reads every
//! salient perception frame and decides what to do. It is the gatekeeper that
//! decides whether to spend money on Opus or not.
//!
//! Default model: Qwen 3 4B (Q4_K_M quantization).
//!
//! Decisions:
//! - `ignore` — nothing worth doing, discard the frame
//! - `remember` — worth remembering but no action needed
//! - `whisper` — say a short sentence via local TTS (no orchestrator)
//! - `execute_simple` — perform a pre-approved simple action
//! - `wake_orchestrator` — wake Claude Opus for genuine reasoning

pub mod handlers;
pub mod llm;
pub mod prompts;

use serde::{Deserialize, Serialize};

/// The five triage decision variants.
///
/// The triage LLM outputs one of these as JSON on every evaluation. The
/// grammar constraint ensures the output is always valid JSON matching
/// one of these variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum TriageDecision {
    /// Nothing worth doing, discard the frame.
    Ignore,
    /// Worth remembering but no action needed.
    Remember {
        /// Brief summary of what to remember (max 200 chars).
        summary: String,
    },
    /// Say a short sentence aloud via local TTS.
    Whisper {
        /// The text to speak (max 200 chars).
        text: String,
    },
    /// Perform a simple pre-approved action.
    ExecuteSimple {
        /// The action to execute (e.g., "launch_app:notepad").
        action: String,
    },
    /// The situation needs Claude Opus to think about it.
    WakeOrchestrator {
        /// Why the orchestrator should wake up.
        reason: String,
    },
}

impl TriageDecision {
    /// Returns the decision variant name as a static string for logging.
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Remember { .. } => "remember",
            Self::Whisper { .. } => "whisper",
            Self::ExecuteSimple { .. } => "execute_simple",
            Self::WakeOrchestrator { .. } => "wake_orchestrator",
        }
    }

    /// Truncate string fields to 200 characters, logging a warning if any
    /// field was over the limit. Grammar mode cannot enforce length bounds,
    /// so we apply this in post-processing.
    pub fn truncated(self) -> Self {
        fn trunc(s: String) -> String {
            if s.len() > 200 {
                tracing::warn!(
                    layer = "triage",
                    component = "decision",
                    original_len = s.len(),
                    "Truncating triage decision field to 200 chars"
                );
                let mut t = s;
                t.truncate(200);
                t
            } else {
                s
            }
        }

        match self {
            Self::Ignore => Self::Ignore,
            Self::Remember { summary } => Self::Remember {
                summary: trunc(summary),
            },
            Self::Whisper { text } => Self::Whisper {
                text: trunc(text),
            },
            Self::ExecuteSimple { action } => Self::ExecuteSimple {
                action: trunc(action),
            },
            Self::WakeOrchestrator { reason } => Self::WakeOrchestrator {
                reason: trunc(reason),
            },
        }
    }

    /// Parse a JSON string into a TriageDecision.
    ///
    /// Strips markdown code fences and any text before the first `{` to
    /// handle models that wrap JSON in prose or thinking tokens.
    /// Returns `None` if the JSON is malformed or doesn't match any variant.
    pub fn from_json(raw: &str) -> Option<Self> {
        let cleaned = extract_json_object(raw);
        serde_json::from_str(cleaned).ok()
    }
}

impl std::fmt::Display for TriageDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ignore => write!(f, "ignore"),
            Self::Remember { summary } => write!(f, "remember: {summary}"),
            Self::Whisper { text } => write!(f, "whisper: {text}"),
            Self::ExecuteSimple { action } => write!(f, "execute_simple: {action}"),
            Self::WakeOrchestrator { reason } => write!(f, "wake_orchestrator: {reason}"),
        }
    }
}

/// Extract the last complete JSON object from raw model output.
///
/// Handles common model output patterns:
/// - Markdown code fences: `` ```json\n{...}\n``` ``
/// - `<think>...</think>` blocks before the JSON (Qwen 3 thinking mode)
/// - Trailing prose after the JSON
///
/// Strategy: find the LAST `{` that starts a valid brace-balanced object.
/// This skips any JSON-like content inside thinking blocks.
fn extract_json_object(raw: &str) -> &str {
    let s = raw.trim();

    // If there's a </think> tag, only look at text after it.
    let search_area = if let Some(pos) = s.rfind("</think>") {
        &s[pos + 8..]
    } else {
        s
    };
    let search_area = search_area.trim();

    // Find the last '{' ... '}' pair that is brace-balanced.
    let start = match search_area.rfind('{') {
        Some(i) => i,
        None => return search_area,
    };

    // From that '{', find its matching '}' by counting braces.
    let mut depth = 0i32;
    let mut end = start;
    let mut in_string = false;
    let mut escape_next = false;
    for (i, ch) in search_area[start..].char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    end = start + i;
                    break;
                }
            }
            _ => {}
        }
    }

    if depth == 0 && end > start {
        &search_area[start..=end]
    } else {
        search_area
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_from_code_fence() {
        let raw = "```json\n{\"decision\":\"ignore\"}\n```";
        assert_eq!(extract_json_object(raw), r#"{"decision":"ignore"}"#);
    }

    #[test]
    fn test_extract_json_with_thinking_prefix() {
        let raw = "Let me analyze this frame.\n{\"decision\":\"ignore\"}";
        assert_eq!(extract_json_object(raw), r#"{"decision":"ignore"}"#);
    }

    #[test]
    fn test_extract_json_after_think_tags() {
        let raw = "<think>\nThis is idle.\n</think>\n\n{\"decision\":\"ignore\"}";
        assert_eq!(extract_json_object(raw), r#"{"decision":"ignore"}"#);
    }

    #[test]
    fn test_extract_json_think_with_json_inside() {
        let raw = "<think>\n{\"inner\":true}\n</think>\n{\"decision\":\"remember\",\"summary\":\"test\"}";
        assert_eq!(
            extract_json_object(raw),
            r#"{"decision":"remember","summary":"test"}"#
        );
    }

    #[test]
    fn test_extract_json_clean() {
        let raw = r#"{"decision":"remember","summary":"test"}"#;
        assert_eq!(extract_json_object(raw), raw);
    }

    #[test]
    fn test_parse_ignore() {
        let d = TriageDecision::from_json(r#"{"decision":"ignore"}"#).unwrap();
        assert_eq!(d, TriageDecision::Ignore);
    }

    #[test]
    fn test_parse_remember() {
        let d = TriageDecision::from_json(
            r#"{"decision":"remember","summary":"user opened VS Code"}"#,
        )
        .unwrap();
        assert!(matches!(d, TriageDecision::Remember { .. }));
        if let TriageDecision::Remember { summary } = d {
            assert_eq!(summary, "user opened VS Code");
        }
    }

    #[test]
    fn test_parse_whisper() {
        let d =
            TriageDecision::from_json(r#"{"decision":"whisper","text":"meeting in 5 minutes"}"#)
                .unwrap();
        assert!(matches!(d, TriageDecision::Whisper { .. }));
    }

    #[test]
    fn test_parse_execute_simple() {
        let d = TriageDecision::from_json(
            r#"{"decision":"execute_simple","action":"launch_app:notepad"}"#,
        )
        .unwrap();
        assert!(matches!(d, TriageDecision::ExecuteSimple { .. }));
    }

    #[test]
    fn test_parse_wake_orchestrator() {
        let d = TriageDecision::from_json(
            r#"{"decision":"wake_orchestrator","reason":"user asked a complex question"}"#,
        )
        .unwrap();
        assert!(matches!(d, TriageDecision::WakeOrchestrator { .. }));
    }

    #[test]
    fn test_parse_malformed_json() {
        assert!(TriageDecision::from_json("not json at all").is_none());
    }

    #[test]
    fn test_parse_empty_string() {
        assert!(TriageDecision::from_json("").is_none());
    }

    #[test]
    fn test_parse_wrong_decision_value() {
        assert!(TriageDecision::from_json(r#"{"decision":"explode"}"#).is_none());
    }

    #[test]
    fn test_parse_missing_required_field() {
        // remember requires summary
        assert!(TriageDecision::from_json(r#"{"decision":"remember"}"#).is_none());
    }

    #[test]
    fn test_parse_extra_keys_accepted() {
        // serde by default ignores extra keys
        let d = TriageDecision::from_json(
            r#"{"decision":"ignore","extra":"field","another":42}"#,
        )
        .unwrap();
        assert_eq!(d, TriageDecision::Ignore);
    }

    #[test]
    fn test_parse_unicode_in_summary() {
        let d = TriageDecision::from_json(
            r#"{"decision":"remember","summary":"gebruiker opende het bestand \u00e9\u00e8n.rs"}"#,
        )
        .unwrap();
        if let TriageDecision::Remember { summary } = d {
            assert!(summary.contains("gebruiker"));
        }
    }

    #[test]
    fn test_parse_empty_summary_accepted() {
        let d = TriageDecision::from_json(r#"{"decision":"remember","summary":""}"#).unwrap();
        assert!(matches!(d, TriageDecision::Remember { summary } if summary.is_empty()));
    }

    #[test]
    fn test_truncate_long_summary() {
        let long = "x".repeat(300);
        let d = TriageDecision::Remember {
            summary: long.clone(),
        }
        .truncated();
        if let TriageDecision::Remember { summary } = d {
            assert_eq!(summary.len(), 200);
        }
    }

    #[test]
    fn test_truncate_short_summary_unchanged() {
        let d = TriageDecision::Remember {
            summary: "short".to_string(),
        }
        .truncated();
        if let TriageDecision::Remember { summary } = d {
            assert_eq!(summary, "short");
        }
    }

    #[test]
    fn test_variant_name() {
        assert_eq!(TriageDecision::Ignore.variant_name(), "ignore");
        assert_eq!(
            TriageDecision::WakeOrchestrator {
                reason: "test".to_string()
            }
            .variant_name(),
            "wake_orchestrator"
        );
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", TriageDecision::Ignore), "ignore");
        assert_eq!(
            format!(
                "{}",
                TriageDecision::Remember {
                    summary: "opened file".to_string()
                }
            ),
            "remember: opened file"
        );
    }

    #[test]
    fn test_roundtrip_serialize_deserialize() {
        let original = TriageDecision::WakeOrchestrator {
            reason: "error detected".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: TriageDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }
}
