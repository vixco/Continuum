//! # Wake context builder — the runtime-full package profile (spec §4.9)
//!
//! Produces the user message sent to Opus on each wake. Since Task B7 this
//! is a thin, runtime-side **assembler** on top of the ungated
//! [`ContextPackage`](crate::context::package::ContextPackage): this module
//! converts runtime types (perception frames, [`MemoryContext`], deduped
//! `context_events` rows, [`SessionState`]) into the packager's small owned
//! section structs, and the packager owns order, cloud gate, caps and the
//! budget drop ladder.
//!
//! The other two profiles (spec §4.9 matrix) assemble the *same* struct
//! from their own sources: `context_package` (MCP, Task C4) from published
//! files + read-only SQLite, and the desktop chat prompt (Task B8) from the
//! vault + state.json.
//!
//! Order contract (asserted below and in `context::package`): pending
//! memory decisions are the last section before "Why you were woken".
//!
//! Budget: `[context_package] token_budget`, default **1000** tokens for
//! the wake profile (spec §6).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use crate::context::package::{
    ContextPackage, CurrentMoment, EventLine, FactLine, MemoryLine, NextStep, PackageBudget,
    PendingDecisionLine, SectionCaps, SessionSection, ToolsSection, VaultNoteLine,
};
use crate::context::session_state::SessionState;
use crate::memory::events::{event_enum_token, EventSensitivity, EventSource, EventType};
use crate::memory::raw_log::ContextEventRow;
use crate::memory::retrieval::MemoryContext;
use crate::senses::context::REDACTED_TITLE;
use crate::senses::privacy::EXCLUDED_PROCESS;
use crate::senses::types::PerceptionFrame;

/// Maximum number of history frames to include when the packager has to
/// fall back to raw frames (no deduped events available).
const MAX_HISTORY_FRAMES: usize = 5;

/// Characters of a history frame's caption kept in a one-line entry.
const HISTORY_CAPTION_CHARS: usize = 60;

/// Characters of a history frame's audio transcript kept in a one-line entry.
const HISTORY_AUDIO_CHARS: usize = 30;

// ---------------------------------------------------------------------------
// Shared perception-frame ring (spec §4.9: "recent_frames ring")
// ---------------------------------------------------------------------------

/// Default capacity of the shared recent-frames ring.
pub const FRAME_RING_CAP: usize = 10;

/// The frame loop's rolling recent-frames buffer, shared with every wake
/// path (spec §4.9).
///
/// Before Task B7 this was a loop-local `Vec` and the maintenance-wake
/// ticker had to pass `&[]` as history. It is now an
/// `Arc<std::sync::Mutex<VecDeque<..>>>` so both wake entry points see the
/// same frames.
///
/// **Locking discipline (non-negotiable):** every method takes the guard,
/// does O(1)/O(n) synchronous work and drops it before returning. The guard
/// is *never* held across an `.await` — that is why this is a
/// `std::sync::Mutex` and not a `tokio::sync::Mutex`, and why
/// [`FrameRing::snapshot`] clones instead of returning a borrow. A poisoned
/// mutex is recovered with `into_inner` rather than propagated: losing the
/// wake history is never worth failing a wake.
#[derive(Debug, Clone)]
pub struct FrameRing {
    inner: Arc<Mutex<VecDeque<PerceptionFrame>>>,
    cap: usize,
}

impl Default for FrameRing {
    fn default() -> Self {
        Self::new(FRAME_RING_CAP)
    }
}

impl FrameRing {
    /// Creates an empty ring bounded at `cap` frames (minimum 1).
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(cap))),
            cap,
        }
    }

    /// Appends a frame, evicting the oldest once the ring is full.
    pub fn push(&self, frame: PerceptionFrame) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.push_back(frame);
        while guard.len() > self.cap {
            guard.pop_front();
        }
    }

    /// Clones the ring's contents, oldest first.
    pub fn snapshot(&self) -> Vec<PerceptionFrame> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.iter().cloned().collect()
    }

    /// Clones the newest frame, if any.
    pub fn latest(&self) -> Option<PerceptionFrame> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.back().cloned()
    }

    /// Clones every frame except the one with `exclude_id` — the wake
    /// history (the trigger frame itself is rendered as "current moment").
    ///
    /// Frames *newer* than the trigger stay in: with triage off the main
    /// loop (Task B2) the trigger is not necessarily the newest frame, and
    /// extra context is additive.
    pub fn snapshot_excluding(&self, exclude_id: uuid::Uuid) -> Vec<PerceptionFrame> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .iter()
            .filter(|f| f.id != exclude_id)
            .cloned()
            .collect()
    }

    /// Number of frames currently held.
    pub fn len(&self) -> usize {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.len()
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// Assembler inputs
// ---------------------------------------------------------------------------

/// Everything the runtime-full profile can put in a package.
///
/// Every field beyond the first four is optional/empty-able on purpose:
/// the assembler follows the **never-fail-the-wake** pattern, so a source
/// that errors is logged and skipped by the caller, which simply leaves
/// that field empty here.
pub struct WakeContextInputs<'a> {
    /// The frame that triggered the wake.
    pub trigger_frame: &'a PerceptionFrame,
    /// Recent frames (fallback "just before" when no events are available).
    pub history_frames: &'a [PerceptionFrame],
    /// Episodic/semantic/vault retrieval output.
    pub memory_context: &'a MemoryContext,
    /// Triage's wake reason.
    pub wake_reason: &'a str,
    /// Session-state snapshot at wake time (spec §4.8).
    pub session: Option<&'a SessionState>,
    /// `[session_state].confidence_floor` — goal/task below it render as
    /// unknown (i.e. not at all).
    pub session_confidence_floor: f32,
    /// Deduped `context_events` rows in the packager's window, oldest
    /// first (Task B6 seam: `RawLog::recent_context_events`).
    pub events: &'a [ContextEventRow],
    /// Tools + permission mode, summarized from the composed wake config.
    pub tools: Option<ToolsSection>,
    /// Continuation resolver suggestion (Task B8; `None` until it lands).
    pub recommended_next_step: Option<NextStep>,
    /// Clock, injected so the assembler is testable.
    pub now: DateTime<Utc>,
    /// Per-section caps (applied again by the renderer; passed here so the
    /// event split does not materialize more lines than needed).
    pub caps: SectionCaps,
}

// ---------------------------------------------------------------------------
// Assembler
// ---------------------------------------------------------------------------

/// Assembles the runtime-full [`ContextPackage`] (spec §4.9 wake profile).
///
/// Pure: no I/O, no clock, no locks — every source has already been read by
/// the caller (`do_wake`). That is what makes the whole wake-profile
/// assembly unit-testable.
pub fn build_wake_package(inputs: WakeContextInputs<'_>) -> ContextPackage {
    let WakeContextInputs {
        trigger_frame,
        history_frames,
        memory_context,
        wake_reason,
        session,
        session_confidence_floor,
        events,
        tools,
        recommended_next_step,
        now,
        caps,
    } = inputs;

    let split = split_event_sections(events, now, &caps);

    // Spec §4.1 propagation rule (3), fixwave 2 (C1). The wake reason is
    // authored by the triage LLM from what it saw, and for continue-class
    // wakes it is further enriched with the resolved continuation target
    // (`Continue: …` / the disambiguation candidate list). Both quote
    // private content when the trigger frame or the session is
    // `local_only`, so the *whole* reason is tagged and the renderer swaps
    // it for the generic phrase at the cloud egress point.
    let why_woken_local_only =
        frame_is_local_only(trigger_frame) || session.is_some_and(|s| s.local_only);

    // "Just before": deduped events (spec §4.9) — falling back to raw
    // frames when the events table has nothing for the window (fresh
    // install, events disabled, DB read failed).
    let just_before = if split.just_before.is_empty() {
        history_lines_from_frames(history_frames, trigger_frame)
    } else {
        split.just_before
    };

    ContextPackage {
        current_moment: Some(moment_from_frame(trigger_frame)),
        session: session.map(|s| session_section(s, session_confidence_floor)),
        just_before,
        memories: memory_context
            .similar_events
            .iter()
            .map(|event| MemoryLine {
                age: render_age(now, event.ts),
                text: event.summary.clone(),
                // Fixwave 3a (C1): a distilled memory carries the zone
                // sensitivity of whatever it was distilled from, so the
                // renderer's cloud gate can withhold it exactly like the
                // event it came from. Hardcoding `false` here meant text
                // suppressed as an *event* was sent to Anthropic as a
                // *memory* one distillation pass later.
                local_only: event.sensitivity == EventSensitivity::LocalOnly,
            })
            .collect(),
        facts: memory_context
            .relevant_facts
            .iter()
            .map(|fact| FactLine {
                label: format_fact_key(&fact.key),
                value: fact.value.clone(),
                local_only: false,
            })
            .collect(),
        // Vault notes carry their own sensitivity policy: retrieval already
        // dropped `Sensitive` notes unless `[memory.curator]
        // include_sensitive_in_context` explicitly allows them, so tagging
        // them `local_only` here would silently override that decision.
        vault_notes: memory_context
            .vault_notes
            .iter()
            .map(|note| VaultNoteLine {
                node_type: note.node_type.as_str().to_string(),
                title: note.title.clone(),
                snippet: note.snippet.clone(),
                importance: note.importance,
                local_only: false,
            })
            .collect(),
        recent_changes: split.recent_changes,
        failed_attempts: split.failed_attempts,
        last_success: split.last_success,
        tools,
        recommended_next_step,
        pending_decisions: memory_context
            .pending_decisions
            .iter()
            .map(|note| PendingDecisionLine {
                id: note.id.clone(),
                node_type: note.node_type.as_str().to_string(),
                title: note.title.clone(),
                confidence: note.confidence,
                source: note.source.as_str().to_string(),
                local_only: false,
            })
            .collect(),
        why_woken: Some(wake_reason.to_string()),
        why_woken_local_only,
    }
}

/// Builds the complete user message for the orchestrator.
///
/// The minimal, back-compatible entry point: trigger frame + history +
/// memory + reason, rendered at the default wake budget. `do_wake` uses the
/// full assembler ([`build_wake_package`]) instead so the session-state,
/// events, tools and next-step sections are populated; this wrapper stays
/// for callers (and tests) that only have the four classic inputs.
pub fn build_wake_message(
    trigger_frame: &PerceptionFrame,
    history_frames: &[PerceptionFrame],
    memory_context: &MemoryContext,
    wake_reason: &str,
) -> String {
    let budget = PackageBudget::default();
    let package = build_wake_package(WakeContextInputs {
        trigger_frame,
        history_frames,
        memory_context,
        wake_reason,
        session: None,
        session_confidence_floor: 0.0,
        events: &[],
        tools: None,
        recommended_next_step: None,
        now: Utc::now(),
        caps: budget.caps.clone(),
    });
    package.render(&budget)
}

// ---------------------------------------------------------------------------
// Frame → section conversions
// ---------------------------------------------------------------------------

/// Builds the "current moment" section from the trigger frame.
///
/// Screen text is the one-sentence caption (spec §4.10); the compact
/// world-state blob rides its own field with its own cap (Task B4 seam).
/// Frames captured pre-B4 have `world_compact = None` with the blob
/// stranded in `description` — those render as the caption, no guessing.
pub fn moment_from_frame(frame: &PerceptionFrame) -> CurrentMoment {
    let audio = frame
        .audio
        .as_ref()
        .filter(|a| !a.transcript.trim().is_empty())
        .map(|a| {
            if a.language == "en" {
                a.transcript.clone()
            } else {
                format!("{} ({})", a.transcript, a.language)
            }
        });

    CurrentMoment {
        caption: frame.screen.description.clone(),
        window_title: non_empty(&frame.context.foreground_window_title),
        app: non_empty(&frame.context.foreground_process_name),
        world_compact: frame.screen.world_compact.clone(),
        audio,
        local_only: frame_is_local_only(frame),
    }
}

/// Whether the trigger frame came from a privacy-restricted window
/// (spec §4.1).
///
/// **Primary signal:** the [`PrivacyDisposition`] the collector stamped on
/// the observation. That is the zone the privacy filter actually resolved,
/// and no config knob can switch it off.
///
/// **Fallback (legacy frames only):** the two sentinel literals — the
/// `[excluded]` process bucket and the redacted window-title literal. Kept
/// because frames written by pre-fixwave-2 builds carry no tag, and because
/// an extra `true` is always the safe direction. Note the redacted-title
/// literal alone is *not* sufficient: it only exists when
/// `[context].redact_sensitive_titles` is on (default true), and a user who
/// turns it off still has `local_only` windows — that was the leak
/// (fixwave 2, I3).
pub fn frame_is_local_only(frame: &PerceptionFrame) -> bool {
    frame.context.is_privacy_restricted()
        || frame.context.foreground_process_name == EXCLUDED_PROCESS
        || frame.context.foreground_window_title == REDACTED_TITLE
}

/// Builds the session-state section, hiding low-confidence inferences.
///
/// The B5 seam asks the packager to render [`SessionState::cloud_view`] at
/// the cloud egress point. It is applied one layer lower instead: the
/// `local_only` flag rides into the section and the *renderer* generalizes
/// goal/task to [`crate::context::session_state::PRIVATE_CONTEXT_PHRASE`]
/// and drops open files when the budget is cloud-bound. Same output, but
/// the guarantee now covers all three consumer profiles and a
/// not-cloud-bound (local model) render keeps the real text, which
/// `cloud_view` applied here would have already destroyed.
pub fn session_section(state: &SessionState, confidence_floor: f32) -> SessionSection {
    SessionSection {
        project: state.active_project.clone(),
        goal: state
            .goal_if_confident(confidence_floor)
            .map(str::to_string),
        task: state
            .task_if_confident(confidence_floor)
            .map(str::to_string),
        confidence: state.confidence,
        open_files: state.open_files.clone(),
        local_only: state.local_only,
    }
}

/// Renders history frames as "just before" lines (the fallback path).
fn history_lines_from_frames(
    history_frames: &[PerceptionFrame],
    trigger_frame: &PerceptionFrame,
) -> Vec<EventLine> {
    history_frames
        .iter()
        .rev()
        .take(MAX_HISTORY_FRAMES)
        .map(|frame| {
            let ago = (trigger_frame.ts - frame.ts).num_seconds().max(0);
            let mut text =
                truncate_on_char_boundary(&frame.screen.description, HISTORY_CAPTION_CHARS);
            if let Some(audio) = frame
                .audio
                .as_ref()
                .filter(|a| !a.transcript.is_empty())
                .map(|a| truncate_on_char_boundary(&a.transcript, HISTORY_AUDIO_CHARS))
            {
                text.push_str(&format!(" \"{audio}\""));
            }
            EventLine {
                age: format!("{ago}s ago"),
                text,
                count: 1,
                local_only: frame_is_local_only(frame),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Deduped events → sections
// ---------------------------------------------------------------------------

/// The event-derived sections of a package.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventSections {
    /// "Just before" — everything in the window, newest first.
    pub just_before: Vec<EventLine>,
    /// File/git activity, newest first.
    pub recent_changes: Vec<EventLine>,
    /// `error` events with their collapse counts, newest first.
    pub failed_attempts: Vec<EventLine>,
    /// The most recent `success` event.
    pub last_success: Option<EventLine>,
}

/// Splits deduped `context_events` rows (oldest first, as
/// `RawLog::recent_context_events` returns them) into the package's
/// event-derived sections.
///
/// Count-aware by construction: the packager renders `count > 1` as the
/// same "×N" suffix the distiller uses (Task B6), so a repeated failure
/// reads as one line with a multiplier rather than N lines of noise.
pub fn split_event_sections(
    rows: &[ContextEventRow],
    now: DateTime<Utc>,
    caps: &SectionCaps,
) -> EventSections {
    let newest_first = || rows.iter().rev();

    let just_before = newest_first()
        .take(caps.just_before)
        .map(|row| event_line(row, now))
        .collect();

    let recent_changes = newest_first()
        .filter(|row| matches!(row.source, EventSource::File | EventSource::Git))
        .take(caps.recent_changes)
        .map(|row| event_line(row, now))
        .collect();

    let failed_attempts = newest_first()
        .filter(|row| row.event_type == EventType::Error)
        .take(caps.failed_attempts)
        .map(|row| event_line(row, now))
        .collect();

    let last_success = newest_first()
        .find(|row| row.event_type == EventType::Success)
        .map(|row| event_line(row, now));

    EventSections {
        just_before,
        recent_changes,
        failed_attempts,
        last_success,
    }
}

/// Converts one deduped event row into a render line.
fn event_line(row: &ContextEventRow, now: DateTime<Utc>) -> EventLine {
    let mut text = truncate_on_char_boundary(row.summary.trim(), 160);
    if text.is_empty() {
        // A summary-less row still carries its type — never render a blank.
        text = event_enum_token(&row.event_type);
    }
    EventLine {
        age: render_age(now, row.ts_last),
        text,
        count: row.count,
        local_only: row.sensitivity == EventSensitivity::LocalOnly,
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

/// Formats a relative time as a human-readable string against `now`.
pub fn render_age(now: DateTime<Utc>, ts: DateTime<Utc>) -> String {
    let ago = (now - ts).num_seconds().max(0);

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
/// `user.name` → `Name`, `project.continuum.stack` → `continuum stack`
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

/// Truncates `s` so it contains at most `max` bytes while staying on a UTF-8
/// char boundary. Appends `...` when truncated. Never panics on multi-byte
/// inputs (Dutch accents, emoji, CJK window titles).
fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    crate::context::package::truncate_on_char_boundary(s, max)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::package::PRIVATE_WAKE_REASON;
    use crate::context::session_state::{PRIVATE_CONTEXT_PHRASE, PRIVATE_PLACEHOLDER};
    use crate::memory::episodic::{EpisodicEvent, EventKind};
    use crate::memory::semantic::{Fact, FactSource};
    use crate::senses::live_context::PrivacyDisposition;
    use crate::senses::types::*;
    use chrono::Duration;
    use continuum_memory::{NodeStatus, NodeSummary, NodeType, Sensitivity, Source};
    use uuid::Uuid;

    fn test_frame(desc: &str, secs_ago: i64) -> PerceptionFrame {
        PerceptionFrame {
            id: Uuid::new_v4(),
            ts: Utc::now() - Duration::seconds(secs_ago),
            screen: ScreenObservation {
                description: desc.to_string(),
                world_compact: None,
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
                ..Default::default()
            },
            salience_hint: 0.5,
        }
    }

    fn test_memory_context() -> MemoryContext {
        MemoryContext {
            similar_events: vec![EpisodicEvent {
                id: Uuid::new_v4().to_string(),
                ts: Utc::now() - Duration::hours(2),
                kind: EventKind::Remember,
                summary: "User was debugging triage JSON parsing".to_string(),
                importance: 0.8,
                tags: vec!["debugging".to_string()],
                source_frame_id: None,
                project: None,
                sensitivity: crate::memory::events::EventSensitivity::CloudAllowed,
            }],
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
            vault_notes: vec![],
            pending_decisions: vec![],
        }
    }

    fn test_vault_note() -> NodeSummary {
        NodeSummary {
            id: "mem_vault1".to_string(),
            slug: "prefers-dark-mode".to_string(),
            title: "Prefers dark mode".to_string(),
            node_type: NodeType::Preference,
            status: NodeStatus::Confirmed,
            project: None,
            confidence: 1.0,
            importance: 0.9,
            source: Source::Observed,
            sensitivity: Sensitivity::Internal,
            created: Utc::now().to_rfc3339(),
            updated: Utc::now().to_rfc3339(),
            tags: vec![],
            snippet: Some("User asked for dark theme everywhere".to_string()),
        }
    }

    fn test_pending_note() -> NodeSummary {
        NodeSummary {
            id: "mem_pending1".to_string(),
            slug: "switching-to-postgresql".to_string(),
            title: "Switching to PostgreSQL".to_string(),
            node_type: NodeType::Decision,
            status: NodeStatus::Candidate,
            project: None,
            confidence: 0.6,
            importance: 0.5,
            source: Source::Observed,
            sensitivity: Sensitivity::Internal,
            created: (Utc::now() - Duration::hours(1)).to_rfc3339(),
            updated: Utc::now().to_rfc3339(),
            tags: vec![],
            snippet: None,
        }
    }

    fn test_memory_context_with_vault() -> MemoryContext {
        let mut ctx = test_memory_context();
        ctx.vault_notes = vec![test_vault_note()];
        ctx.pending_decisions = vec![test_pending_note()];
        ctx
    }

    fn test_event_row(
        id: i64,
        source: EventSource,
        event_type: EventType,
        summary: &str,
        secs_ago: i64,
        count: i64,
    ) -> ContextEventRow {
        let ts = Utc::now() - Duration::seconds(secs_ago);
        ContextEventRow {
            id,
            ts_first: ts,
            ts_last: ts,
            count,
            source,
            application: "Code.exe".to_string(),
            window_title: "continuum".to_string(),
            project_id: Some("continuum".to_string()),
            event_type,
            summary: summary.to_string(),
            importance: 0.6,
            confidence: 1.0,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: None,
            dedupe_key: format!("k{id}"),
        }
    }

    // --- classic wake-message contract (pre-B7 tests, re-pointed) ---------

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
        // Task B7: the budget moved from a hand-checked "< 3000 chars"
        // (~600 tokens) to the spec §6 `[context_package] token_budget` of
        // **1000 tokens** — the section list doubled (session state,
        // recent changes, failed attempts, last success, tools,
        // recommended next step), which is spec-sanctioned. The renderer
        // enforces the budget itself via the drop ladder; this test keeps
        // asserting that a realistic wake never even reaches it.
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

        let tokens = crate::context::package::estimate_tokens(&msg);
        assert!(
            tokens <= PackageBudget::default().token_budget,
            "Wake message too long: {tokens} tokens (budget {})",
            PackageBudget::default().token_budget
        );
    }

    #[test]
    fn test_build_wake_message_vault_sections_present() {
        let trigger = test_frame("VS Code showing error in terminal", 0);
        let memory = test_memory_context_with_vault();

        let msg = build_wake_message(&trigger, &[], &memory, "User asked a question");

        assert!(msg.contains("## Long-term memory (vault)"));
        assert!(msg.contains("## Pending memory decisions"));
        assert!(msg.contains("[preference] Prefers dark mode"));
        assert!(msg.contains("id: mem_pending1"));
        assert!(msg.contains("[decision] \"Switching to PostgreSQL\""));
        assert!(msg.contains("source observed"));
        assert!(msg.contains(
            "Resolve these with the memory_vault_resolve tool (confirm/reject/supersede) \
             or improve them with memory_vault_save. Skip any you are unsure about."
        ));

        // ORDER CONTRACT (unchanged since Plan B curator): the pending
        // section comes after the vault-notes section, and is the LAST
        // section before the wake reason.
        let vault_idx = msg.find("## Long-term memory (vault)").unwrap();
        let pending_idx = msg.find("## Pending memory decisions").unwrap();
        let reason_idx = msg.find("## Why you were woken").unwrap();
        assert!(vault_idx < pending_idx);
        assert!(pending_idx < reason_idx);
        assert!(!msg[pending_idx..reason_idx]
            .trim_start_matches("## Pending memory decisions")
            .contains("\n## "));
    }

    #[test]
    fn test_build_wake_message_vault_sections_absent_when_empty() {
        let trigger = test_frame("idle desktop", 0);
        let memory = test_memory_context();

        let msg = build_wake_message(&trigger, &[], &memory, "Test wake");

        assert!(!msg.contains("## Long-term memory (vault)"));
        assert!(!msg.contains("## Pending memory decisions"));
    }

    #[test]
    fn test_format_fact_key() {
        assert_eq!(format_fact_key("user.name"), "Name");
        assert_eq!(
            format_fact_key("project.continuum.stack"),
            "continuum stack"
        );
        assert_eq!(
            format_fact_key("routine.morning_start"),
            "routine: morning_start"
        );
    }

    #[test]
    fn truncate_on_char_boundary_preserves_short() {
        assert_eq!(truncate_on_char_boundary("hi", 10), "hi");
    }

    #[test]
    fn truncate_on_char_boundary_handles_multibyte() {
        // Greek letter "β" is 2 bytes; naïve slicing at byte 29 would panic.
        let s = "een hele lange zin met β accenten die ver over de limiet heen gaat";
        let got = truncate_on_char_boundary(s, 30);
        assert!(got.ends_with("..."));
        assert!(got.is_char_boundary(got.len()));
        assert!(got.len() <= 30);
    }

    #[test]
    fn truncate_on_char_boundary_handles_emoji() {
        // 😀 is 4 bytes; cutting at any byte inside would panic.
        let s = "hello 😀 world 😀 this is a long sentence with emoji";
        let got = truncate_on_char_boundary(s, 15);
        assert!(got.ends_with("..."));
        assert!(got.is_char_boundary(got.len()));
    }

    #[test]
    fn test_format_relative_time() {
        let now = Utc::now();
        assert!(render_age(now, now - Duration::seconds(30)).contains("s ago"));
        assert!(render_age(now, now - Duration::minutes(5)).contains("m ago"));
        assert!(render_age(now, now - Duration::hours(3)).contains("h ago"));
        assert!(render_age(now, now - Duration::days(2)).contains("d ago"));
        // Clock skew never yields a negative age.
        assert_eq!(render_age(now, now + Duration::seconds(30)), "0s ago");
    }

    #[test]
    fn test_empty_history_and_memory() {
        let trigger = test_frame("idle desktop", 0);
        let memory = MemoryContext {
            similar_events: vec![],
            relevant_facts: vec![],
            vault_notes: vec![],
            pending_decisions: vec![],
        };

        let msg = build_wake_message(&trigger, &[], &memory, "Test wake");

        assert!(msg.contains("## Current moment"));
        assert!(msg.contains("## Why you were woken"));
        // Should NOT include empty sections.
        assert!(!msg.contains("## Just before"));
        assert!(!msg.contains("## Relevant memories"));
        assert!(!msg.contains("## What you know about the user"));
        assert!(!msg.contains("## Long-term memory (vault)"));
        assert!(!msg.contains("## Pending memory decisions"));
    }

    // --- B7: window title, world_compact, session, events ------------------

    #[test]
    fn current_moment_renders_the_window_title_and_world_compact() {
        let mut trigger = test_frame("VS Code showing error in terminal", 0);
        trigger.screen.world_compact = Some("live-context/v2 seq=3".to_string());
        let msg = build_wake_message(&trigger, &[], &test_memory_context(), "Test wake");

        assert!(msg.contains("Window: test - VS Code"), "{msg}");
        assert!(msg.contains("App: Code.exe"));
        assert!(msg.contains("World state:\nlive-context/v2 seq=3"));
    }

    #[test]
    fn a_frame_from_an_excluded_window_is_local_only() {
        let mut trigger = test_frame("something private", 0);
        trigger.context.foreground_process_name = EXCLUDED_PROCESS.to_string();
        assert!(frame_is_local_only(&trigger));

        let mut redacted = test_frame("something private", 0);
        redacted.context.foreground_window_title = REDACTED_TITLE.to_string();
        assert!(frame_is_local_only(&redacted));

        assert!(!frame_is_local_only(&test_frame("normal", 0)));

        // Cloud-bound render generalizes rather than leaking.
        let msg = build_wake_message(&trigger, &[], &test_memory_context(), "Test wake");
        assert!(!msg.contains("something private"), "{msg}");
    }

    // --- fixwave 2 ---------------------------------------------------------

    /// I3: with `[context].redact_sensitive_titles = false` a `local_only`
    /// window keeps its real (scrubbed) title, so the redacted-title
    /// literal — the only `local_only` marker the gate used to have — is
    /// never produced. The collector's own `PrivacyDisposition` is not
    /// suppressible by that knob, so the gate reads it instead.
    #[test]
    fn a_local_only_frame_without_the_redaction_literal_is_still_gated() {
        let mut trigger = test_frame("InPrivate browsing: severance packages", 0);
        // Exactly what `sanitize_observation` emits with the legacy
        // redaction knob OFF: a real title, a Redacted disposition.
        trigger.context.foreground_window_title =
            "Severance packages - InPrivate - Edge".to_string();
        trigger.context.foreground_process_name = "msedge.exe".to_string();
        trigger.context.privacy = Some(PrivacyDisposition::Redacted);

        assert_ne!(trigger.context.foreground_process_name, EXCLUDED_PROCESS);
        assert_ne!(trigger.context.foreground_window_title, REDACTED_TITLE);
        assert!(
            frame_is_local_only(&trigger),
            "the disposition tag must gate the frame on its own"
        );

        let msg = build_wake_message(&trigger, &[], &test_memory_context(), "Test wake");
        assert!(!msg.contains("Severance packages"), "{msg}");
        assert!(!msg.contains("severance"), "{msg}");
        assert!(msg.contains(PRIVATE_PLACEHOLDER), "{msg}");

        // Rule (3): the wake reason goes generic too.
        assert!(msg.contains(PRIVATE_WAKE_REASON), "{msg}");
        assert!(!msg.contains("Test wake"), "{msg}");

        // An explicitly `Visible` frame is untouched.
        let mut visible = test_frame("normal work", 0);
        visible.context.privacy = Some(PrivacyDisposition::Visible);
        assert!(!frame_is_local_only(&visible));
    }

    /// C1: a `local_only` **session** (spec §4.1 rule 2 — an inference
    /// window containing one local_only event) must not launder its raw
    /// task into the cloud through the wake reason or the next-step
    /// section, both of which used to render verbatim.
    #[test]
    fn a_local_only_session_never_leaks_its_task_through_reason_or_next_step() {
        let trigger = test_frame("a normal-looking screen", 0);
        let hub = crate::context::session_state::SessionStateHub::default();
        hub.apply_inference(
            &crate::context::session_state::InferenceResult {
                goal: Some("wrap up the reorg".to_string()),
                task: Some("reviewing the Q3 layoff list in the private browser".to_string()),
                confidence: 0.9,
            },
            /* local_only */ true,
            Utc::now(),
        );
        let snapshot = hub.snapshot();
        assert!(snapshot.local_only);

        // Exactly what `apply_continuation` builds for "ga door".
        let reason = "Voice command (nl): ga door\nContinue: reviewing the Q3 layoff list in \
                      the private browser (from session_state.current_task, confidence 0.9)";
        let budget = PackageBudget::default();
        let package = build_wake_package(WakeContextInputs {
            trigger_frame: &trigger,
            history_frames: &[],
            memory_context: &test_memory_context(),
            wake_reason: reason,
            session: Some(&snapshot),
            session_confidence_floor: 0.4,
            events: &[],
            tools: None,
            recommended_next_step: Some(NextStep {
                text: "reviewing the Q3 layoff list in the private browser".to_string(),
                confidence: 0.9,
                local_only: true,
            }),
            now: Utc::now(),
            caps: budget.caps.clone(),
        });

        assert!(package.why_woken_local_only);
        let cloud = package.render(&budget);
        assert!(
            !cloud.contains("layoff"),
            "the private task reached the cloud:\n{cloud}"
        );
        assert!(!cloud.contains("## Recommended next step"), "{cloud}");
        assert!(cloud.contains(PRIVATE_WAKE_REASON), "{cloud}");
        // The section that always worked still works.
        assert!(cloud.contains(PRIVATE_CONTEXT_PHRASE), "{cloud}");

        // …and the local render is unharmed.
        let local = package.render(&PackageBudget::local(10_000));
        assert!(local.contains("Q3 layoff list"));
    }

    #[test]
    fn session_section_hides_low_confidence_inferences() {
        let state = SessionState {
            active_project: Some("continuum".to_string()),
            current_goal: Some("ship the context engine".to_string()),
            current_task: Some("wire the packager".to_string()),
            confidence: 0.3,
            ..Default::default()
        };

        let low = session_section(&state, 0.6);
        assert_eq!(low.project.as_deref(), Some("continuum"));
        assert!(low.goal.is_none());
        assert!(low.task.is_none());

        let high = session_section(&state, 0.2);
        assert_eq!(high.goal.as_deref(), Some("ship the context engine"));
        assert_eq!(high.task.as_deref(), Some("wire the packager"));
    }

    #[test]
    fn event_split_routes_rows_to_their_sections_newest_first() {
        let now = Utc::now();
        let rows = vec![
            test_event_row(
                1,
                EventSource::Screen,
                EventType::Success,
                "tests green",
                600,
                1,
            ),
            test_event_row(
                2,
                EventSource::Git,
                EventType::Commit,
                "commit landed",
                400,
                1,
            ),
            test_event_row(
                3,
                EventSource::Screen,
                EventType::Error,
                "build failed",
                300,
                14,
            ),
            test_event_row(
                4,
                EventSource::File,
                EventType::FileModified,
                "src/lib.rs",
                100,
                3,
            ),
        ];
        let sections = split_event_sections(&rows, now, &SectionCaps::default());

        // Just before: everything, newest first.
        assert_eq!(sections.just_before.len(), 4);
        assert_eq!(sections.just_before[0].text, "src/lib.rs");
        assert_eq!(sections.just_before[3].text, "tests green");
        // Recent changes: file + git only.
        assert_eq!(sections.recent_changes.len(), 2);
        assert_eq!(sections.recent_changes[0].text, "src/lib.rs");
        assert_eq!(sections.recent_changes[1].text, "commit landed");
        // Failed attempts carry their collapse count.
        assert_eq!(sections.failed_attempts.len(), 1);
        assert_eq!(sections.failed_attempts[0].count, 14);
        // Last success is the latest success row.
        assert_eq!(sections.last_success.unwrap().text, "tests green");
    }

    #[test]
    fn event_split_honours_caps_and_local_only() {
        let now = Utc::now();
        let mut rows: Vec<ContextEventRow> = (0..12)
            .map(|i| {
                test_event_row(
                    i,
                    EventSource::Screen,
                    EventType::Error,
                    &format!("failure {i}"),
                    (12 - i) * 10,
                    1,
                )
            })
            .collect();
        rows[11].sensitivity = EventSensitivity::LocalOnly;

        let sections = split_event_sections(&rows, now, &SectionCaps::default());
        assert_eq!(sections.just_before.len(), 5);
        assert_eq!(sections.failed_attempts.len(), 3);
        assert!(sections.just_before[0].local_only);
        assert!(sections.last_success.is_none());
    }

    #[test]
    fn deduped_events_replace_the_frame_fallback_in_just_before() {
        let now = Utc::now();
        let trigger = test_frame("VS Code", 0);
        let history = vec![test_frame("older frame caption", 5)];
        let rows = vec![test_event_row(
            1,
            EventSource::Screen,
            EventType::Error,
            "build failed",
            60,
            14,
        )];

        let package = build_wake_package(WakeContextInputs {
            trigger_frame: &trigger,
            history_frames: &history,
            memory_context: &test_memory_context(),
            wake_reason: "stuck",
            session: None,
            session_confidence_floor: 0.4,
            events: &rows,
            tools: None,
            recommended_next_step: None,
            now,
            caps: SectionCaps::default(),
        });
        let msg = package.render(&PackageBudget::default());
        assert!(msg.contains("build failed (×14)"), "{msg}");
        assert!(!msg.contains("older frame caption"));

        // Without events, the frame fallback still produces history.
        let package = build_wake_package(WakeContextInputs {
            trigger_frame: &trigger,
            history_frames: &history,
            memory_context: &test_memory_context(),
            wake_reason: "stuck",
            session: None,
            session_confidence_floor: 0.4,
            events: &[],
            tools: None,
            recommended_next_step: None,
            now,
            caps: SectionCaps::default(),
        });
        assert!(package
            .render(&PackageBudget::default())
            .contains("older frame caption"));
    }

    #[test]
    fn full_assembly_renders_tools_session_and_next_step_in_order() {
        let now = Utc::now();
        let trigger = test_frame("VS Code showing error in terminal", 0);
        let state = SessionState {
            active_project: Some("continuum".to_string()),
            current_task: Some("land B7".to_string()),
            confidence: 0.9,
            open_files: vec!["src/context/package.rs".to_string()],
            ..Default::default()
        };

        let rows = vec![test_event_row(
            1,
            EventSource::File,
            EventType::FileModified,
            "src/context/package.rs",
            30,
            2,
        )];

        let package = build_wake_package(WakeContextInputs {
            trigger_frame: &trigger,
            history_frames: &[],
            memory_context: &test_memory_context_with_vault(),
            wake_reason: "User appears stuck",
            session: Some(&state),
            session_confidence_floor: 0.4,
            events: &rows,
            tools: Some(ToolsSection {
                names: vec!["mcp__continuum__*".to_string()],
                permission_mode: "default".to_string(),
            }),
            recommended_next_step: Some(NextStep {
                text: "run the gates".to_string(),
                confidence: 0.8,
                local_only: false,
            }),
            now,
            caps: SectionCaps::default(),
        });
        let msg = package.render(&PackageBudget::default());

        assert!(msg.contains("## Session state"));
        assert!(msg.contains("Task: land B7 (confidence 0.9)"));
        assert!(msg.contains("Open files: src/context/package.rs"));
        assert!(msg.contains("## Recent changes"));
        assert!(msg.contains("## Available tools"));
        assert!(msg.contains("Permission mode: default"));
        assert!(msg.contains("## Recommended next step"));
        assert!(msg.contains("run the gates (confidence 0.8)"));

        // Order contract holds with the full section list.
        let pending = msg.find("## Pending memory decisions").unwrap();
        let reason = msg.find("## Why you were woken").unwrap();
        assert!(msg.find("## Available tools").unwrap() < pending);
        assert!(msg.find("## Recommended next step").unwrap() < pending);
        assert!(pending < reason);
    }

    // --- fixwave 3a C1: memory sensitivity survives distillation ----------

    #[test]
    fn a_local_only_memory_is_withheld_from_the_cloud_render_only() {
        // The C1 leak: text observed in a `local_only` window is withheld
        // as an *event*, then distilled and sent to Anthropic as a
        // "relevant memory" one distillation pass later.
        let trigger = test_frame("VS Code", 0);
        let private = "the Q3 layoff list";
        let memory = MemoryContext {
            similar_events: vec![
                EpisodicEvent {
                    id: Uuid::new_v4().to_string(),
                    ts: Utc::now() - Duration::hours(1),
                    kind: EventKind::Remember,
                    summary: format!("User was reviewing {private}"),
                    importance: 0.8,
                    tags: vec![],
                    source_frame_id: None,
                    project: None,
                    sensitivity: EventSensitivity::LocalOnly,
                },
                EpisodicEvent {
                    id: Uuid::new_v4().to_string(),
                    ts: Utc::now() - Duration::hours(1),
                    kind: EventKind::Remember,
                    summary: "User was fixing the triage parser".to_string(),
                    importance: 0.8,
                    tags: vec![],
                    source_frame_id: None,
                    project: None,
                    sensitivity: EventSensitivity::CloudAllowed,
                },
            ],
            relevant_facts: vec![],
            vault_notes: vec![],
            pending_decisions: vec![],
        };

        let package = build_wake_package(WakeContextInputs {
            trigger_frame: &trigger,
            history_frames: &[],
            memory_context: &memory,
            wake_reason: "User appears stuck",
            session: None,
            session_confidence_floor: 0.4,
            events: &[],
            tools: None,
            recommended_next_step: None,
            now: Utc::now(),
            caps: SectionCaps::default(),
        });

        // The flag has to reach the package, or the renderer's
        // `!(cloud && local_only)` filter can never fire.
        assert!(package.memories[0].local_only, "{:?}", package.memories);
        assert!(!package.memories[1].local_only);

        let cloud = package.render(&PackageBudget::cloud(10_000));
        assert!(
            !cloud.contains(private),
            "a local_only memory reached the cloud render:\n{cloud}"
        );
        assert!(
            cloud.contains("triage parser"),
            "the cloud-allowed memory must still render:\n{cloud}"
        );

        // The local render is the whole point of keeping the memory: a
        // local model still sees it.
        let local = package.render(&PackageBudget::local(10_000));
        assert!(
            local.contains(private),
            "the local render must keep the memory:\n{local}"
        );
    }

    // --- shared frame ring -------------------------------------------------

    #[test]
    fn frame_ring_is_bounded_and_ordered_oldest_first() {
        let ring = FrameRing::new(3);
        assert!(ring.is_empty());
        for i in 0..5 {
            ring.push(test_frame(&format!("frame {i}"), 0));
        }
        let frames = ring.snapshot();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].screen.description, "frame 2");
        assert_eq!(frames[2].screen.description, "frame 4");
        assert_eq!(ring.latest().unwrap().screen.description, "frame 4");
        assert_eq!(ring.len(), 3);
    }

    #[test]
    fn frame_ring_snapshot_excluding_drops_only_the_trigger() {
        let ring = FrameRing::new(10);
        let trigger = test_frame("trigger", 0);
        ring.push(test_frame("before", 5));
        ring.push(trigger.clone());
        ring.push(test_frame("after", 0));

        let history = ring.snapshot_excluding(trigger.id);
        assert_eq!(history.len(), 2);
        assert!(history.iter().all(|f| f.id != trigger.id));
        assert_eq!(history[0].screen.description, "before");
        assert_eq!(history[1].screen.description, "after");
    }

    #[test]
    fn frame_ring_is_shared_across_threads_without_deadlock() {
        let ring = FrameRing::new(16);
        let writer = ring.clone();
        let handle = std::thread::spawn(move || {
            for i in 0..500 {
                writer.push(test_frame(&format!("w{i}"), 0));
            }
        });

        // Snapshot concurrently from this thread — a guard held across a
        // read would deadlock the writer; it never is.
        let mut seen_max = 0;
        for _ in 0..500 {
            seen_max = seen_max.max(ring.snapshot().len());
            let _ = ring.latest();
        }
        handle.join().expect("writer thread must not panic");

        assert!(seen_max <= 16, "ring exceeded its bound: {seen_max}");
        assert_eq!(ring.len(), 16);
    }
}
