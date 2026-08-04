//! # Memory retrieval
//!
//! Combines episodic (vector search) and semantic (fact lookup) memory into
//! a single context package for the orchestrator wake message.
//!
//! Runs before every orchestrator wake-up. Target latency: under 200ms.

use anyhow::Result;
use chrono::{DateTime, Utc};
use continuum_memory::{NodeStatus, NodeSummary, Sensitivity, Vault};
use tracing::{debug, warn};

use super::episodic::{EpisodicEvent, EpisodicStore};
use super::semantic::{Fact, SemanticStore};
use crate::config::CuratorConfig;
use crate::senses::types::PerceptionFrame;

/// Minimum age a pending vault candidate must have before it's surfaced in
/// wake context or counted by the daily maintenance ticker (see
/// [`filter_pending`]) — gives the curator a moment after writing a
/// candidate before nudging for review of it.
const PENDING_MIN_AGE_MINUTES: i64 = 30;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The assembled memory context passed to the wake context builder.
///
/// Contains everything the orchestrator needs to understand the current
/// situation: what just happened, what happened before, and what Continuum
/// knows about the user.
#[derive(Debug, Clone)]
pub struct MemoryContext {
    /// Recent episodic events similar to the current situation (from LanceDB).
    pub similar_events: Vec<EpisodicEvent>,
    /// Relevant semantic facts about the user and their context.
    pub relevant_facts: Vec<Fact>,
    /// Confirmed, non-sensitive vault notes relevant to the current frame
    /// (Plan B curator). Empty unless a caller fills it via
    /// [`retrieve_vault_context`].
    pub vault_notes: Vec<NodeSummary>,
    /// Candidate vault notes that have sat unresolved long enough to nudge
    /// the orchestrator to review them. Empty unless a caller fills it via
    /// [`retrieve_vault_context`].
    pub pending_decisions: Vec<NodeSummary>,
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
    let similar_events: Vec<EpisodicEvent> = similar_events.into_iter().map(|r| r.event).collect();

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
        // Vault retrieval runs separately via `retrieve_vault_context` — it
        // needs the `Vault` handle, which this function doesn't take.
        vault_notes: Vec::new(),
        pending_decisions: Vec::new(),
    })
}

/// Retrieves long-term vault context for a wake: confirmed, non-sensitive
/// notes relevant to the current frame, plus candidate notes that have sat
/// unresolved long enough that the orchestrator should be nudged to review
/// them.
///
/// Every internal failure (vault search error, `pending()` error, an
/// unparseable `created` timestamp on a candidate) is caught, logged, and
/// treated as "no data" for that half of the result — a wake must never die
/// because the vault is having trouble.
pub async fn retrieve_vault_context(
    vault: &Vault,
    frame: &PerceptionFrame,
    curator_cfg: &CuratorConfig,
) -> (Vec<NodeSummary>, Vec<NodeSummary>) {
    let query = build_query_from_frame(frame);

    let mut notes = match vault.search(&query, 24).await {
        Ok(results) => results,
        Err(e) => {
            warn!(
                layer = "memory",
                component = "retrieval",
                error = %e.user_message(),
                "vault search failed during wake retrieval; continuing without vault notes"
            );
            Vec::new()
        }
    };

    notes.retain(|n| {
        n.status == NodeStatus::Confirmed
            && (n.sensitivity != Sensitivity::Sensitive || curator_cfg.include_sensitive_in_context)
    });
    notes.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    notes.truncate(curator_cfg.wake_vault_notes_max as usize);

    let pending_all = match vault.pending().await {
        Ok(results) => results,
        Err(e) => {
            warn!(
                layer = "memory",
                component = "retrieval",
                error = %e.user_message(),
                "vault pending() failed during wake retrieval; continuing without pending decisions"
            );
            Vec::new()
        }
    };

    let mut pending = filter_pending(pending_all, curator_cfg, Utc::now());
    pending.truncate(curator_cfg.claude_batch as usize);

    debug!(
        layer = "memory",
        component = "retrieval",
        vault_note_count = notes.len(),
        pending_count = pending.len(),
        "Vault retrieval complete"
    );

    (notes, pending)
}

/// Filters raw `Vault::pending()` results down to candidates actually
/// worth surfacing to a human/orchestrator: non-sensitive (unless
/// `cfg.include_sensitive_in_context` opts in) and at least
/// `PENDING_MIN_AGE_MINUTES` old, so a candidate isn't nudged for review
/// the instant the curator writes it.
///
/// I4 fix: shared by [`retrieve_vault_context`] (which additionally
/// truncates to `cfg.claude_batch` for the wake message) and
/// `bin/continuum.rs`'s daily memory-maintenance ticker, which previously
/// gated on the raw, unfiltered `vault.pending()` list — a vault that only
/// ever had sensitive-and-excluded or too-fresh candidates would still
/// report "pending decisions" and could fire a daily wake that then found
/// nothing to show, forever, every day. Gating the ticker on this same
/// filtered list instead means "no-op wake" and "nothing shown in the wake
/// message" can no longer disagree.
pub fn filter_pending(
    items: Vec<NodeSummary>,
    cfg: &CuratorConfig,
    now: DateTime<Utc>,
) -> Vec<NodeSummary> {
    let cutoff = now - chrono::Duration::minutes(PENDING_MIN_AGE_MINUTES);
    items
        .into_iter()
        .filter(|n| {
            // A hand-edited vault is schema-legal for a Sensitive
            // candidate, and it must not leak into the wake context unless
            // the operator opted in.
            (n.sensitivity != Sensitivity::Sensitive || cfg.include_sensitive_in_context)
                && match DateTime::parse_from_rfc3339(&n.created) {
                    Ok(created) => created.with_timezone(&Utc) < cutoff,
                    Err(e) => {
                        warn!(
                            layer = "memory",
                            component = "retrieval",
                            id = %n.id,
                            raw_created = %n.created,
                            error = %e,
                            "pending vault note has unparseable created timestamp; excluding from wake context"
                        );
                        false
                    }
                }
        })
        .collect()
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

/// Tries to infer a bare project hint (e.g. `"continuum"`) from the current
/// frame context.
///
/// Looks at the foreground window title and process to guess which project
/// the user is working on. Used both for semantic-fact prefix lookups (via
/// [`infer_project_prefix`]) and, unformatted, as the curator's
/// `ActivitySignal::project_hint`.
pub fn infer_project_hint(frame: &PerceptionFrame) -> Option<String> {
    let title = &frame.context.foreground_window_title;

    // Common patterns: "filename - ProjectName - Editor"
    // or file paths containing project names.
    if title.contains("continuum") {
        Some("continuum".to_string())
    } else if title.contains("simcharts") || title.contains("SimCharts") {
        Some("simcharts".to_string())
    } else {
        // Could be extended with a lookup table from semantic memory,
        // but for now just return None for unknown projects.
        None
    }
}

/// Formats [`infer_project_hint`] as a `"project.<hint>."` semantic-fact key
/// prefix. Thin legacy wrapper — kept private since nothing outside this
/// module needs the formatted form.
fn infer_project_prefix(frame: &PerceptionFrame) -> Option<String> {
    infer_project_hint(frame).map(|hint| format!("project.{hint}."))
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
            "mod.rs - continuum-ai - VS Code",
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
    fn test_infer_project_prefix_continuum() {
        let frame = test_frame("editor", None, "mod.rs - continuum-ai - VS Code");
        assert_eq!(
            infer_project_prefix(&frame),
            Some("project.continuum.".to_string())
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

    #[test]
    fn test_infer_project_hint_continuum() {
        let frame = test_frame("editor", None, "mod.rs - continuum-ai - VS Code");
        assert_eq!(infer_project_hint(&frame), Some("continuum".to_string()));
    }

    #[test]
    fn test_infer_project_hint_simcharts() {
        let frame = test_frame("editor", None, "ProcedureLayer.tsx - SimCharts");
        assert_eq!(infer_project_hint(&frame), Some("simcharts".to_string()));
    }

    #[test]
    fn test_infer_project_hint_unknown() {
        let frame = test_frame("editor", None, "random-project - VS Code");
        assert_eq!(infer_project_hint(&frame), None);
    }

    // -----------------------------------------------------------------
    // retrieve_vault_context (tempdir vault; exercises real search/pending)
    // -----------------------------------------------------------------

    use continuum_memory::{NodeType, NoteDraft, Source};

    fn note_draft(
        title: &str,
        body: &str,
        status: NodeStatus,
        sensitivity: Sensitivity,
    ) -> NoteDraft {
        NoteDraft {
            node_type: NodeType::Fact,
            title: title.to_string(),
            body: body.to_string(),
            project: None,
            status,
            confidence: 0.9,
            importance: 0.9,
            source: Source::Observed,
            source_ref: None,
            sensitivity,
            relations: vec![],
            tags: vec![],
        }
    }

    /// `test_frame` (shared with the module's other tests) hardcodes
    /// `foreground_process_name` to `"Code.exe"`, which `build_query_from_frame`
    /// always appends as `"App: Code.exe"` — an extra FTS AND-term that has
    /// nothing to do with what this test is checking (status/sensitivity
    /// filtering). Use an empty process name so the query is exactly the
    /// description, keeping the fixture notes' bodies the only thing that
    /// needs to match it.
    fn vault_search_frame(description: &str) -> PerceptionFrame {
        PerceptionFrame {
            id: uuid::Uuid::new_v4(),
            ts: Utc::now(),
            screen: ScreenObservation {
                description: description.to_string(),
                foreground_app: String::new(),
                has_error_visible: false,
                confidence: 0.9,
                screenshot_path: None,
                ts: Utc::now(),
            },
            audio: None,
            context: ContextObservation {
                foreground_window_title: String::new(),
                foreground_process_name: String::new(),
                idle_seconds: 0,
                in_call: false,
                ts: Utc::now(),
            },
            salience_hint: 0.7,
        }
    }

    #[tokio::test]
    async fn retrieve_vault_context_filters_status_and_sensitivity() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        vault
            .create(note_draft(
                "Zorbatron setup",
                "Zorbatron dashboard runs on port 4200 in dev",
                NodeStatus::Confirmed,
                Sensitivity::Internal,
            ))
            .await
            .unwrap();
        vault
            .create(note_draft(
                "Zorbatron secret",
                "Zorbatron dashboard API key info",
                NodeStatus::Confirmed,
                Sensitivity::Sensitive,
            ))
            .await
            .unwrap();
        vault
            .create(note_draft(
                "Zorbatron draft idea",
                "Zorbatron dashboard maybe needs a cache",
                NodeStatus::Candidate,
                Sensitivity::Internal,
            ))
            .await
            .unwrap();

        let frame = vault_search_frame("Zorbatron dashboard");

        // Default config: sensitive notes excluded.
        let cfg = CuratorConfig::default();
        let (notes, _pending) = retrieve_vault_context(&vault, &frame, &cfg).await;
        assert_eq!(
            notes.len(),
            1,
            "only the confirmed, non-sensitive note should surface"
        );
        assert_eq!(notes[0].title, "Zorbatron setup");

        // include_sensitive_in_context = true surfaces the sensitive note too.
        let cfg_sensitive = CuratorConfig {
            include_sensitive_in_context: true,
            ..Default::default()
        };
        let (notes, _pending) = retrieve_vault_context(&vault, &frame, &cfg_sensitive).await;
        assert_eq!(notes.len(), 2);
        assert!(notes.iter().any(|n| n.title == "Zorbatron secret"));
    }

    #[tokio::test]
    async fn retrieve_vault_context_pending_only_older_than_30_minutes() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        let fresh = vault
            .create(note_draft(
                "Fresh candidate",
                "just noticed",
                NodeStatus::Candidate,
                Sensitivity::Internal,
            ))
            .await
            .unwrap();
        let old = vault
            .create(note_draft(
                "Old candidate",
                "sat around a while",
                NodeStatus::Candidate,
                Sensitivity::Internal,
            ))
            .await
            .unwrap();

        // Backdate `old`'s created timestamp: load, edit frontmatter.created,
        // save. `save` re-stamps `updated`, but the pending-age filter reads
        // `created`, so that's irrelevant here.
        let mut old_note = vault.get(&old.frontmatter.id).await.unwrap();
        old_note.frontmatter.created = Utc::now() - chrono::Duration::hours(1);
        vault.save(&old_note).await.unwrap();

        let frame = test_frame("irrelevant context", None, "x");
        let cfg = CuratorConfig::default();
        let (_notes, pending) = retrieve_vault_context(&vault, &frame, &cfg).await;

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, old.frontmatter.id);
        assert_ne!(pending[0].id, fresh.frontmatter.id);
    }

    #[tokio::test]
    async fn retrieve_vault_context_pending_sensitivity_gated() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        let sensitive = vault
            .create(note_draft(
                "Sensitive candidate",
                "salary negotiation notes",
                NodeStatus::Candidate,
                Sensitivity::Sensitive,
            ))
            .await
            .unwrap();

        // Backdate past the 30-minute pending-age cutoff, same as the
        // age-only test above.
        let mut note = vault.get(&sensitive.frontmatter.id).await.unwrap();
        note.frontmatter.created = Utc::now() - chrono::Duration::hours(1);
        vault.save(&note).await.unwrap();

        let frame = test_frame("irrelevant context", None, "x");

        // Default config: sensitive candidate excluded even though it's old
        // enough to otherwise qualify.
        let cfg = CuratorConfig::default();
        let (_notes, pending) = retrieve_vault_context(&vault, &frame, &cfg).await;
        assert!(
            pending.is_empty(),
            "sensitive candidate must not leak into pending decisions by default"
        );

        // include_sensitive_in_context = true surfaces it.
        let cfg_sensitive = CuratorConfig {
            include_sensitive_in_context: true,
            ..Default::default()
        };
        let (_notes, pending) = retrieve_vault_context(&vault, &frame, &cfg_sensitive).await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, sensitive.frontmatter.id);
    }

    // -----------------------------------------------------------------
    // filter_pending (I4 fix) — pure unit tests, no vault needed
    // -----------------------------------------------------------------

    fn mk_pending_summary(
        id: &str,
        created: DateTime<Utc>,
        sensitivity: Sensitivity,
    ) -> NodeSummary {
        NodeSummary {
            id: id.to_string(),
            slug: id.to_string(),
            title: id.to_string(),
            node_type: NodeType::Fact,
            status: NodeStatus::Candidate,
            project: None,
            confidence: 0.5,
            importance: 0.5,
            source: Source::Observed,
            sensitivity,
            created: created.to_rfc3339(),
            updated: created.to_rfc3339(),
            tags: vec![],
            snippet: None,
        }
    }

    #[test]
    fn filter_pending_keeps_only_old_enough_non_sensitive_items() {
        let now = Utc::now();
        let items = vec![
            mk_pending_summary(
                "fresh",
                now - chrono::Duration::minutes(5),
                Sensitivity::Internal,
            ),
            mk_pending_summary(
                "old",
                now - chrono::Duration::hours(1),
                Sensitivity::Internal,
            ),
            mk_pending_summary(
                "old_sensitive",
                now - chrono::Duration::hours(1),
                Sensitivity::Sensitive,
            ),
        ];

        let cfg = CuratorConfig::default();
        let filtered = filter_pending(items.clone(), &cfg, now);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "old");

        // Shared behavior with retrieve_vault_context's own sensitivity
        // gate: include_sensitive_in_context surfaces the sensitive one too
        // (still subject to the same age cutoff).
        let cfg_sensitive = CuratorConfig {
            include_sensitive_in_context: true,
            ..Default::default()
        };
        let filtered_sensitive = filter_pending(items, &cfg_sensitive, now);
        assert_eq!(filtered_sensitive.len(), 2);
        assert!(filtered_sensitive.iter().any(|n| n.id == "old_sensitive"));
    }

    #[test]
    fn filter_pending_excludes_unparseable_created_timestamp() {
        let now = Utc::now();
        let mut bad = mk_pending_summary(
            "bad_ts",
            now - chrono::Duration::hours(1),
            Sensitivity::Internal,
        );
        bad.created = "not a timestamp".to_string();

        let cfg = CuratorConfig::default();
        let filtered = filter_pending(vec![bad], &cfg, now);
        assert!(filtered.is_empty());
    }
}
