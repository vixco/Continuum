//! # Memory retrieval
//!
//! Combines episodic (vector search) and semantic (fact lookup) memory into
//! a single context package for the orchestrator wake message.
//!
//! Runs before every orchestrator wake-up. Target latency: under 200ms.

use anyhow::Result;
use tracing::debug;

use super::episodic::{EpisodicEvent, EpisodicStore};
use super::semantic::{Fact, SemanticStore};
use crate::senses::types::PerceptionFrame;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The assembled memory context passed to the wake context builder.
///
/// Contains everything the orchestrator needs to understand the current
/// situation: what just happened, what happened before, and what Kairo
/// knows about the user.
#[derive(Debug, Clone)]
pub struct MemoryContext {
    /// Recent episodic events similar to the current situation (from LanceDB).
    pub similar_events: Vec<EpisodicEvent>,
    /// Relevant semantic facts about the user and their context.
    pub relevant_facts: Vec<Fact>,
}

// ---------------------------------------------------------------------------
// Retrieval
// ---------------------------------------------------------------------------

/// Retrieves context from both memory stores for a given trigger frame.
///
/// Steps:
/// 1. Builds a natural language query from the trigger frame
/// 2. Searches episodic memory for similar past events
/// 3. Looks up relevant semantic facts by key prefix
///
/// Target: complete in under 200ms.
pub async fn retrieve_context(
    trigger_frame: &PerceptionFrame,
    episodic: &mut EpisodicStore,
    semantic: &SemanticStore,
) -> Result<MemoryContext> {
    // Build a query string from the trigger frame.
    let query = build_query_from_frame(trigger_frame);

    debug!(
        layer = "memory",
        component = "retrieval",
        query = %query,
        "Retrieving memory context"
    );

    // Search episodic memory for similar events (top 5).
    let similar_events = episodic.search_similar(&query, 5).await?;
    let similar_events: Vec<EpisodicEvent> = similar_events
        .into_iter()
        .map(|r| r.event)
        .collect();

    // Look up relevant semantic facts.
    // Strategy: get user facts + project facts related to current app context.
    let mut relevant_facts = Vec::new();

    // Always include core user facts.
    let user_facts = semantic.query_facts_by_prefix("user.").await?;
    relevant_facts.extend(user_facts);

    // Include project facts if we can identify the project from the frame.
    if let Some(project_prefix) = infer_project_prefix(trigger_frame) {
        let project_facts = semantic.query_facts_by_prefix(&project_prefix).await?;
        relevant_facts.extend(project_facts);
    }

    // Limit total facts to keep context compact.
    relevant_facts.truncate(10);

    debug!(
        layer = "memory",
        component = "retrieval",
        similar_count = similar_events.len(),
        fact_count = relevant_facts.len(),
        "Memory retrieval complete"
    );

    Ok(MemoryContext {
        similar_events,
        relevant_facts,
    })
}

/// Builds a natural language query string from a perception frame.
///
/// Combines screen description, audio transcript, and context into a
/// single sentence that works well as a vector search query.
fn build_query_from_frame(frame: &PerceptionFrame) -> String {
    let mut parts = Vec::new();

    // Screen description is always available.
    if !frame.screen.description.is_empty() {
        parts.push(frame.screen.description.clone());
    }

    // Audio transcript if present.
    if let Some(ref audio) = frame.audio {
        if !audio.transcript.is_empty() {
            parts.push(format!("User said: {}", audio.transcript));
        }
    }

    // Window context.
    if !frame.context.foreground_process_name.is_empty() {
        parts.push(format!("App: {}", frame.context.foreground_process_name));
    }

    if parts.is_empty() {
        "general context".to_string()
    } else {
        parts.join(". ")
    }
}

/// Tries to infer a project prefix from the current frame context.
///
/// Looks at the foreground window title and process to guess which project
/// the user is working on.
fn infer_project_prefix(frame: &PerceptionFrame) -> Option<String> {
    let title = &frame.context.foreground_window_title;

    // Common patterns: "filename - ProjectName - Editor"
    // or file paths containing project names.
    if title.contains("kairo") {
        Some("project.kairo.".to_string())
    } else if title.contains("simcharts") || title.contains("SimCharts") {
        Some("project.simcharts.".to_string())
    } else {
        // Could be extended with a lookup table from semantic memory,
        // but for now just return None for unknown projects.
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senses::types::*;
    use chrono::Utc;

    fn test_frame(description: &str, audio: Option<&str>, window_title: &str) -> PerceptionFrame {
        PerceptionFrame {
            id: uuid::Uuid::new_v4(),
            ts: Utc::now(),
            screen: ScreenObservation {
                description: description.to_string(),
                foreground_app: "Code.exe".to_string(),
                has_error_visible: false,
                confidence: 0.9,
                screenshot_path: None,
                ts: Utc::now(),
            },
            audio: audio.map(|t| AudioObservation {
                transcript: t.to_string(),
                language: "nl".to_string(),
                duration_ms: 2000,
                confidence: 0.85,
                ts: Utc::now(),
            }),
            context: ContextObservation {
                foreground_window_title: window_title.to_string(),
                foreground_process_name: "Code.exe".to_string(),
                idle_seconds: 0,
                in_call: false,
                ts: Utc::now(),
            },
            salience_hint: 0.7,
        }
    }

    #[test]
    fn test_build_query_from_frame_full() {
        let frame = test_frame(
            "VS Code showing error in terminal",
            Some("waarom werkt dit niet"),
            "mod.rs - kairo-ai - VS Code",
        );

        let query = build_query_from_frame(&frame);
        assert!(query.contains("VS Code showing error"));
        assert!(query.contains("User said: waarom werkt dit niet"));
        assert!(query.contains("App: Code.exe"));
    }

    #[test]
    fn test_build_query_from_frame_no_audio() {
        let frame = test_frame("Browser on Stack Overflow", None, "rust - Stack Overflow");

        let query = build_query_from_frame(&frame);
        assert!(query.contains("Browser on Stack Overflow"));
        assert!(!query.contains("User said"));
    }

    #[test]
    fn test_infer_project_prefix_kairo() {
        let frame = test_frame("editor", None, "mod.rs - kairo-ai - VS Code");
        assert_eq!(
            infer_project_prefix(&frame),
            Some("project.kairo.".to_string())
        );
    }

    #[test]
    fn test_infer_project_prefix_simcharts() {
        let frame = test_frame("editor", None, "ProcedureLayer.tsx - SimCharts");
        assert_eq!(
            infer_project_prefix(&frame),
            Some("project.simcharts.".to_string())
        );
    }

    #[test]
    fn test_infer_project_prefix_unknown() {
        let frame = test_frame("editor", None, "random-project - VS Code");
        assert_eq!(infer_project_prefix(&frame), None);
    }
}
