//! # Wake context builder
//!
//! Produces the user message sent to Opus on each wake. Combines the current
//! perception frame, recent history, memory retrieval results, and wake reason
//! into a structured, compact message.
//!
//! Target: under 600 tokens total for the user message.

use chrono::{DateTime, Utc};

use crate::memory::retrieval::MemoryContext;
use crate::senses::types::PerceptionFrame;

/// Maximum number of history frames to include.
const MAX_HISTORY_FRAMES: usize = 5;

/// Maximum number of similar events from episodic memory to include.
const MAX_SIMILAR_EVENTS: usize = 3;

/// Maximum number of semantic facts to include.
const MAX_FACTS: usize = 8;

/// Builds the complete user message for the orchestrator.
///
/// The message has these sections:
/// - Current moment (the trigger frame)
/// - Just before (compressed history)
/// - Relevant memories (from episodic store)
/// - What you know about the user (semantic facts)
/// - Why you were woken (triage reason)
///
/// Target: under 600 tokens.
pub fn build_wake_message(
    trigger_frame: &PerceptionFrame,
    history_frames: &[PerceptionFrame],
    memory_context: &MemoryContext,
    wake_reason: &str,
) -> String {
    let mut msg = String::with_capacity(2048);

    // Section 1: Current moment.
    msg.push_str("## Current moment\n");
    msg.push_str(&format_frame(trigger_frame));
    msg.push('\n');

    // Section 2: Just before (compressed history).
    if !history_frames.is_empty() {
        msg.push_str("\n## Just before\n");
        let frames_to_show = &history_frames[..history_frames.len().min(MAX_HISTORY_FRAMES)];
        for frame in frames_to_show.iter().rev() {
            msg.push_str(&format!("- {}\n", format_frame_oneline(frame, trigger_frame.ts)));
        }
    }

    // Section 3: Relevant memories.
    let events = &memory_context.similar_events;
    if !events.is_empty() {
        msg.push_str("\n## Relevant memories\n");
        for event in events.iter().take(MAX_SIMILAR_EVENTS) {
            msg.push_str(&format!("- [{}] {}\n", format_relative_time(event.ts), event.summary));
        }
    }

    // Section 4: What you know about the user.
    let facts = &memory_context.relevant_facts;
    if !facts.is_empty() {
        msg.push_str("\n## What you know about the user\n");
        for fact in facts.iter().take(MAX_FACTS) {
            msg.push_str(&format!("- {}: {}\n", format_fact_key(&fact.key), &fact.value));
        }
    }

    // Section 5: Why you were woken.
    msg.push_str("\n## Why you were woken\n");
    msg.push_str(wake_reason);
    msg.push('\n');

    msg
}

/// Formats a perception frame as a multi-line description for the "Current moment" section.
fn format_frame(frame: &PerceptionFrame) -> String {
    let mut lines = Vec::new();

    // Screen.
    lines.push(format!("Screen: {}", frame.screen.description));

    // Audio.
    if let Some(ref audio) = frame.audio {
        if !audio.transcript.is_empty() {
            let lang_hint = if audio.language != "en" {
                format!(" ({})", audio.language)
            } else {
                String::new()
            };
            lines.push(format!("Audio: \"{}\"{}", audio.transcript, lang_hint));
        }
    }

    // Context.
    let ctx = &frame.context;
    if !ctx.foreground_process_name.is_empty() {
        lines.push(format!("App: {}", ctx.foreground_process_name));
    }

    lines.join("\n")
}

/// Formats a frame as a single compressed line for the "Just before" section.
fn format_frame_oneline(frame: &PerceptionFrame, reference_time: DateTime<Utc>) -> String {
    let ago = (reference_time - frame.ts).num_seconds().max(0);

    let mut parts = Vec::new();
    parts.push(format!("[{}s ago]", ago));

    // Short screen description (truncate if long).
    let desc = if frame.screen.description.len() > 60 {
        format!("{}...", &frame.screen.description[..57])
    } else {
        frame.screen.description.clone()
    };
    parts.push(desc);

    // Audio if present (very brief).
    if let Some(ref audio) = frame.audio {
        if !audio.transcript.is_empty() {
            let short_transcript = if audio.transcript.len() > 30 {
                format!("\"{}...\"", &audio.transcript[..27])
            } else {
                format!("\"{}\"", audio.transcript)
            };
            parts.push(short_transcript);
        }
    }

    parts.join(" ")
}

/// Formats a relative time as a human-readable string.
fn format_relative_time(ts: DateTime<Utc>) -> String {
    let ago = (Utc::now() - ts).num_seconds().max(0);

    if ago < 60 {
        format!("{ago}s ago")
    } else if ago < 3600 {
        format!("{}m ago", ago / 60)
    } else if ago < 86400 {
        format!("{}h ago", ago / 3600)
    } else {
        format!("{}d ago", ago / 86400)
    }
}

/// Formats a dotted fact key into a human-readable label.
///
/// `user.name` → `Name`, `project.kairo.stack` → `kairo stack`
fn format_fact_key(key: &str) -> String {
    let parts: Vec<&str> = key.split('.').collect();
    match parts.as_slice() {
        ["user", field] => capitalize(field),
        ["project", name, field] => format!("{name} {field}"),
        ["routine", field] => format!("routine: {field}"),
        ["contact", name, field] => format!("{name} {field}"),
        _ => key.to_string(),
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::episodic::{EpisodicEvent, EventKind};
    use crate::memory::semantic::{Fact, FactSource};
    use crate::senses::types::*;
    use chrono::Duration;
    use uuid::Uuid;

    fn test_frame(desc: &str, secs_ago: i64) -> PerceptionFrame {
        PerceptionFrame {
            id: Uuid::new_v4(),
            ts: Utc::now() - Duration::seconds(secs_ago),
            screen: ScreenObservation {
                description: desc.to_string(),
                foreground_app: "Code.exe".to_string(),
                has_error_visible: false,
                confidence: 0.9,
                screenshot_path: None,
                ts: Utc::now() - Duration::seconds(secs_ago),
            },
            audio: None,
            context: ContextObservation {
                foreground_window_title: "test - VS Code".to_string(),
                foreground_process_name: "Code.exe".to_string(),
                idle_seconds: 0,
                in_call: false,
                ts: Utc::now() - Duration::seconds(secs_ago),
            },
            salience_hint: 0.5,
        }
    }

    fn test_memory_context() -> MemoryContext {
        MemoryContext {
            similar_events: vec![
                EpisodicEvent {
                    id: Uuid::new_v4().to_string(),
                    ts: Utc::now() - Duration::hours(2),
                    kind: EventKind::Remember,
                    summary: "User was debugging triage JSON parsing".to_string(),
                    importance: 0.8,
                    tags: vec!["debugging".to_string()],
                    source_frame_id: None,
                },
            ],
            relevant_facts: vec![
                Fact {
                    key: "user.name".to_string(),
                    value: "\"Toshan\"".to_string(),
                    confidence: 1.0,
                    source: FactSource::UserStated,
                    source_frame_id: None,
                    updated_at: Utc::now(),
                },
                Fact {
                    key: "user.language".to_string(),
                    value: "\"Dutch\"".to_string(),
                    confidence: 1.0,
                    source: FactSource::UserStated,
                    source_frame_id: None,
                    updated_at: Utc::now(),
                },
            ],
        }
    }

    #[test]
    fn test_build_wake_message_has_all_sections() {
        let trigger = test_frame("VS Code showing error in terminal", 0);
        let history = vec![
            test_frame("VS Code, same file, typing", 3),
            test_frame("VS Code, same file, no changes", 6),
        ];
        let memory = test_memory_context();

        let msg = build_wake_message(
            &trigger,
            &history,
            &memory,
            "User appears stuck on test failures",
        );

        assert!(msg.contains("## Current moment"));
        assert!(msg.contains("## Just before"));
        assert!(msg.contains("## Relevant memories"));
        assert!(msg.contains("## What you know about the user"));
        assert!(msg.contains("## Why you were woken"));
        assert!(msg.contains("VS Code showing error in terminal"));
        assert!(msg.contains("User appears stuck on test failures"));
        assert!(msg.contains("Toshan"));
    }

    #[test]
    fn test_build_wake_message_is_compact() {
        let trigger = test_frame("VS Code showing error in terminal", 0);
        let history = vec![
            test_frame("VS Code, same file, typing", 3),
            test_frame("VS Code, same file", 6),
            test_frame("VS Code, switched files", 9),
            test_frame("Browser on Stack Overflow", 12),
            test_frame("Browser on docs.rs", 15),
        ];
        let memory = test_memory_context();

        let msg = build_wake_message(&trigger, &history, &memory, "User asked a question");

        // Rough token estimate: ~4 chars per token, target under 600 tokens = 2400 chars.
        // This is a generous bound — real messages will be shorter.
        assert!(
            msg.len() < 3000,
            "Wake message too long: {} chars (target < 3000)",
            msg.len()
        );
    }

    #[test]
    fn test_format_fact_key() {
        assert_eq!(format_fact_key("user.name"), "Name");
        assert_eq!(format_fact_key("project.kairo.stack"), "kairo stack");
        assert_eq!(format_fact_key("routine.morning_start"), "routine: morning_start");
    }

    #[test]
    fn test_format_relative_time() {
        let now = Utc::now();
        assert!(format_relative_time(now - Duration::seconds(30)).contains("s ago"));
        assert!(format_relative_time(now - Duration::minutes(5)).contains("m ago"));
        assert!(format_relative_time(now - Duration::hours(3)).contains("h ago"));
        assert!(format_relative_time(now - Duration::days(2)).contains("d ago"));
    }

    #[test]
    fn test_empty_history_and_memory() {
        let trigger = test_frame("idle desktop", 0);
        let memory = MemoryContext {
            similar_events: vec![],
            relevant_facts: vec![],
        };

        let msg = build_wake_message(&trigger, &[], &memory, "Test wake");

        assert!(msg.contains("## Current moment"));
        assert!(msg.contains("## Why you were woken"));
        // Should NOT include empty sections.
        assert!(!msg.contains("## Just before"));
        assert!(!msg.contains("## Relevant memories"));
        assert!(!msg.contains("## What you know about the user"));
    }
}
