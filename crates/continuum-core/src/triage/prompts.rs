//! # Triage prompt management
//!
//! Loads and formats the triage system prompt from `prompts/triage-system.md`.
//! Injects runtime context (user name, current frame, memory summary) into
//! the prompt template before each triage call.
//!
//! ## Slim-frame invariant (context engine spec §4.7 / §4.10)
//!
//! The user turn **must never** serialize [`PerceptionFrame`] directly.
//! Every field the frame gains would otherwise silently inflate the triage
//! prompt, and the triage token budget (`prompt_tokens < context_size −
//! max_tokens`, asserted by `continuum-triage-bench`) is already tight.
//! The prompt therefore renders a hand-maintained projection —
//! [`TriagePromptFrame`] — that contains exactly the fields the system
//! prompt reasons about, in the same shape as its few-shot examples:
//!
//! ```text
//! {"context":{...},"audio":{"transcript":"..."},"screen":{"description":"...",
//!  "has_error_visible":false},"salience_hint":0.0}
//! ```
//!
//! Deliberately excluded: `screen.world_compact` (the ~1.4 kB compact
//! world-state blob — the packager's input, §4.10), focus-switch details
//! (§4.2 — only their scalar salience contribution survives), frame/observation
//! ids and timestamps, screenshot paths, and the window/process enrichment
//! fields (`pid`, `exe_path`, `active_since_secs`, `monitor_id`). Adding a
//! field here is a deliberate act that costs prompt tokens; measure it in the
//! bench before doing so.

use serde::Serialize;

use crate::senses::types::PerceptionFrame;

/// The GBNF grammar for triage decisions, loaded from the grammar file.
///
/// This is a flat grammar with no separate char/escape sub-rules and no
/// bounded repetition operators (`{N}`) — both of which caused
/// GGML_ASSERT crashes in llama-grammar.cpp with the previous grammar.
pub const TRIAGE_GRAMMAR: &str = include_str!("../../../../prompts/triage-grammar.gbnf");

/// The triage system prompt, kept deliberately short to minimize prompt
/// processing time. At ~250 tokens this is 4x shorter than the original.
const SYSTEM_PROMPT: &str = include_str!("../../../../prompts/triage-system.md");

/// The slim frame projection sent to the triage model.
///
/// See the module docs for the invariant: this is the *only* thing the
/// user turn serializes, so [`PerceptionFrame`] can grow without inflating
/// the triage prompt. Field order matches the system prompt's few-shot
/// examples (`context`, `audio`, `screen`, `salience_hint`).
#[derive(Debug, Serialize)]
pub struct TriagePromptFrame<'a> {
    /// Reliable Windows-API facts — signal-trust tier 1 in the system prompt.
    pub context: TriagePromptContext<'a>,
    /// Speech, when the frame carries a transcript. Serialized as `null`
    /// otherwise — the system prompt's few-shot examples show `"audio":null`,
    /// so the key stays present rather than being skipped.
    pub audio: Option<TriagePromptAudio<'a>>,
    /// The vision caption plus the error flag — signal-trust tier 3.
    pub screen: TriagePromptScreen<'a>,
    /// Rule-based salience score from the frame builder.
    pub salience_hint: f32,
}

/// Context fields the triage rules actually read.
#[derive(Debug, Serialize)]
pub struct TriagePromptContext<'a> {
    /// Foreground window title.
    pub foreground_window_title: &'a str,
    /// Foreground process name.
    pub foreground_process_name: &'a str,
    /// Seconds since the last user input (drives the idle ignore rule).
    pub idle_seconds: u64,
    /// Whether the user appears to be in a call (drives the ignore rule).
    pub in_call: bool,
}

/// Audio fields the triage rules actually read.
#[derive(Debug, Serialize)]
pub struct TriagePromptAudio<'a> {
    /// The transcript. Language/duration/confidence are omitted: the rules
    /// key off the text itself and the system prompt already answers in
    /// English regardless of the spoken language.
    pub transcript: &'a str,
}

/// Screen fields the triage rules actually read.
#[derive(Debug, Serialize)]
pub struct TriagePromptScreen<'a> {
    /// The one-sentence vision caption (spec §4.10) — never the compact
    /// world-state blob.
    pub description: &'a str,
    /// Whether an error dialog/stack trace was detected.
    pub has_error_visible: bool,
}

impl<'a> TriagePromptFrame<'a> {
    /// Projects a [`PerceptionFrame`] onto the slim triage view.
    pub fn from_frame(frame: &'a PerceptionFrame) -> Self {
        Self {
            context: TriagePromptContext {
                foreground_window_title: &frame.context.foreground_window_title,
                foreground_process_name: &frame.context.foreground_process_name,
                idle_seconds: frame.context.idle_seconds,
                in_call: frame.context.in_call,
            },
            audio: frame
                .audio
                .as_ref()
                .filter(|audio| !audio.transcript.is_empty())
                .map(|audio| TriagePromptAudio {
                    transcript: &audio.transcript,
                }),
            screen: TriagePromptScreen {
                description: &frame.screen.description,
                has_error_visible: frame.screen.has_error_visible,
            },
            salience_hint: frame.salience_hint,
        }
    }
}

/// Build the triage prompt in Qwen 3 ChatML format.
///
/// The ChatML wrapper is required so `/no_think` suppresses thinking tokens.
/// The frame is projected onto [`TriagePromptFrame`] and serialized as
/// compact JSON in the user turn to save tokens (see the module docs — never
/// serialize the frame itself).
pub fn build_triage_prompt(frame: &PerceptionFrame, memory_summary: &str) -> String {
    let frame_json = serde_json::to_string(&TriagePromptFrame::from_frame(frame))
        .unwrap_or_else(|_| "{}".to_string());

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
                world_compact: None,
                foreground_app: "Code.exe".to_string(),
                has_error_visible: false,
                confidence: 0.8,
                screenshot_path: None,
                ts: Utc::now(),
            },
            audio: None,
            context: ContextObservation {
                foreground_window_title: "main.rs - continuum-ai".to_string(),
                foreground_process_name: "Code.exe".to_string(),
                idle_seconds: 5,
                in_call: false,
                ts: Utc::now(),
                ..Default::default()
            },
            salience_hint: 0.25,
        }
    }

    #[test]
    fn test_grammar_is_valid() {
        assert!(!TRIAGE_GRAMMAR.is_empty());
        assert!(TRIAGE_GRAMMAR.contains("root"));
        assert!(TRIAGE_GRAMMAR.contains("ignore"));
        assert!(TRIAGE_GRAMMAR.contains("wake"));
    }

    #[test]
    fn test_system_prompt_is_compact() {
        assert!(SYSTEM_PROMPT.contains("triage"));
        assert!(SYSTEM_PROMPT.contains("ignore"));
        // Byte cap 9500 ≈ 2715 tokens at the ~3.5 bytes/token Qwen
        // heuristic. Token-budget arithmetic (spec §4.7, context_size
        // 4096, max_tokens 256): 2715 (system) + ~170 (frame JSON)
        // + ~170 (memory_summary, char-capped 600 in B5) + ~30 (ChatML
        // wrapper) ≈ 3085 < 3840 = 4096 − 256. Grew in Phase 2 (signal
        // reliability hierarchy), Phase 8 (suggested_skill hints), and
        // Plan B Task B1 (the §4.7 classification block + examples).
        assert!(
            SYSTEM_PROMPT.len() < 9500,
            "System prompt too long: {} bytes",
            SYSTEM_PROMPT.len()
        );
    }

    #[test]
    fn test_system_prompt_documents_classification() {
        // Task B1 (spec §4.7): the classification block is part of the
        // single top-level output object.
        assert!(SYSTEM_PROMPT.contains("\"classification\""));
        assert!(SYSTEM_PROMPT.contains("should_store"));
        assert!(SYSTEM_PROMPT.contains("event_type"));
        // All eight classification variants are named for the model.
        for variant in [
            "error",
            "success",
            "decision",
            "preference",
            "task_progress",
            "communication",
            "routine",
            "other",
        ] {
            assert!(
                SYSTEM_PROMPT.contains(variant),
                "classification variant {variant} missing from system prompt"
            );
        }
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
        assert!(prompt.contains("main.rs - continuum-ai"));
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

    // --- Slim-frame projection (spec §4.7 token budget / §4.10) ---

    #[test]
    fn slim_projection_has_exactly_the_whitelisted_keys() {
        // Guards the module invariant: a new PerceptionFrame field must not
        // reach the prompt by accident. Update this list only together with
        // a measured bench run.
        let frame = sample_frame();
        let value =
            serde_json::to_value(TriagePromptFrame::from_frame(&frame)).expect("projection");
        // serde_json::Value keys are sorted (no preserve_order feature) —
        // wire order is asserted separately on the serialized string.
        let top: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(top, vec!["audio", "context", "salience_hint", "screen"]);

        let context: Vec<&str> = value["context"]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            context,
            vec![
                "foreground_process_name",
                "foreground_window_title",
                "idle_seconds",
                "in_call"
            ]
        );

        let screen: Vec<&str> = value["screen"]
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(screen, vec!["description", "has_error_visible"]);
    }

    #[test]
    fn prompt_excludes_world_compact_blob() {
        // The whole point of Task B4: the ~1.4 kB compact world-state blob
        // is packager input, never triage input.
        let mut frame = sample_frame();
        frame.screen.world_compact = Some(
            "live-context/v2 seq=9 generated=2026-08-05T12:00:00Z\n\
             [monitor:display-1] Primary primary event=3 capture=4 change=0.900 \
             privacy=Visible vision=\"a code editor\""
                .to_string(),
        );
        let prompt = build_triage_prompt(&frame, "");
        assert!(!prompt.contains("world_compact"));
        assert!(!prompt.contains("live-context/v2"));
        assert!(!prompt.contains("[monitor:display-1]"));
        // …while the caption itself is still there.
        assert!(prompt.contains("VS Code editing main.rs"));
    }

    #[test]
    fn prompt_excludes_enrichment_and_switch_details() {
        // Focus-switch details (spec §4.2) survive only as the scalar
        // salience contribution; the window/process enrichment fields
        // (§4.2) are raw-log/packager material, not prompt material.
        let mut frame = sample_frame();
        frame.context.pid = Some(4242);
        frame.context.exe_path = Some("~\\bin\\Code.exe".to_string());
        frame.context.active_since_secs = 91;
        frame.context.monitor_id = Some("display-7".to_string());
        frame.screen.screenshot_path = Some("C:\\shots\\frame.jpg".to_string());
        let prompt = build_triage_prompt(&frame, "");
        for absent in [
            "4242",
            "exe_path",
            "active_since_secs",
            "display-7",
            "screenshot_path",
            "frame.jpg",
        ] {
            assert!(
                !prompt.contains(absent),
                "slim frame leaked {absent} into the triage prompt"
            );
        }
        // salience_hint (the switch signal's only survivor) is present.
        assert!(prompt.contains("\"salience_hint\":0.25"));
    }

    #[test]
    fn slim_projection_matches_system_prompt_example_shape() {
        // The user turn must look like the few-shot examples, or the model
        // is being taught one shape and shown another.
        let mut frame = sample_frame();
        frame.salience_hint = 0.0;
        let json =
            serde_json::to_string(&TriagePromptFrame::from_frame(&frame)).expect("projection");
        assert!(json.starts_with("{\"context\":{\"foreground_window_title\":"));
        assert!(json.contains("\"audio\":null"));
        assert!(json.contains("\"screen\":{\"description\":"));
        assert!(json.ends_with("\"salience_hint\":0.0}"));

        frame.audio = Some(crate::senses::types::AudioObservation {
            transcript: "hey continuum".to_string(),
            language: "en".to_string(),
            duration_ms: 1200,
            confidence: 0.9,
            ts: Utc::now(),
        });
        let json =
            serde_json::to_string(&TriagePromptFrame::from_frame(&frame)).expect("projection");
        assert!(json.contains("\"audio\":{\"transcript\":\"hey continuum\"}"));
        // Duration/confidence/language never reach the model.
        assert!(!json.contains("duration_ms"));
        assert!(!json.contains("1200"));
    }

    #[test]
    fn slim_projection_drops_empty_transcript() {
        let mut frame = sample_frame();
        frame.audio = Some(crate::senses::types::AudioObservation {
            transcript: String::new(),
            language: "en".to_string(),
            duration_ms: 0,
            confidence: 0.0,
            ts: Utc::now(),
        });
        let json =
            serde_json::to_string(&TriagePromptFrame::from_frame(&frame)).expect("projection");
        assert!(json.contains("\"audio\":null"));
    }

    #[test]
    fn prompt_fits_the_triage_token_budget() {
        // Mirror of the bench gate (spec §4.7) on a worst-case-ish frame,
        // so a prompt regression fails `cargo test` and not only a GPU run.
        // 3.5 bytes/token is the documented Qwen-family proxy; the budget
        // is the shipped default context_size − max_tokens.
        let mut frame = sample_frame();
        frame.context.foreground_window_title = "x".repeat(300);
        frame.screen.description = "y".repeat(400);
        frame.screen.world_compact = Some("z".repeat(1_400));
        frame.audio = Some(crate::senses::types::AudioObservation {
            transcript: "w".repeat(400),
            language: "en".to_string(),
            duration_ms: 4000,
            confidence: 0.9,
            ts: Utc::now(),
        });
        let memory = "m".repeat(600);
        let prompt = build_triage_prompt(&frame, &memory);
        let est_tokens = (prompt.len() as f64 / 3.5).ceil() as u32;
        let defaults = crate::config::TriageSection::default();
        let budget = defaults.context_size.saturating_sub(defaults.max_tokens);
        assert!(
            est_tokens < budget,
            "triage prompt does not fit: {} bytes ≈ {est_tokens} tokens >= budget {budget}",
            prompt.len()
        );
    }
}
