//! # Classification consumption (Task B3, spec §4.7 "Consumption")
//!
//! The triage call is also the Context Model call: every gated frame comes
//! back as a [`TriageOutput`](crate::triage::TriageOutput) carrying both a
//! decision *and* an optional [`Classification`]. B1 parsed it, B2 moved
//! the evaluation off the main loop; this module is what finally *uses* it.
//!
//! Four consumption paths, all normative (spec §4.7):
//!
//! 1. **Events** — every classified frame becomes a [`ContextEvent`] on the
//!    Plan-A events channel, source `screen` (or `audio` when the frame
//!    carried a transcript), zone-tagged per the §4.1 propagation rule.
//! 2. **Vault candidates** — `should_store` or a `Remember` decision
//!    proposes a vault memory candidate (status `Candidate`, source
//!    `Observed`, per-type expiry, epistemic label) through the mapping
//!    table in [`vault_node_type`].
//! 3. **`triage_decision` raw-log column** — the long-dead column is
//!    finally populated (`<decision>` or `<decision>/<event_type>`).
//! 4. **The distiller's SQL predicate is deliberately untouched** — it
//!    stays the fallback for frames with no classification at all (model
//!    down, malformed block, triage disabled).
//!
//! Layer position: this is Layer 2 output plumbing. It reads triage's
//! verdict and hands it to the memory layer — it never calls back up into
//! the senses or sideways into the orchestrator. Nothing here blocks: the
//! event send is a non-blocking `try_send` and the vault/raw-log writes
//! are spawned, because the caller is a `tokio::select!` arm of the main
//! loop (Task B2 invariant: no arm ever awaits I/O or LLM work).

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use continuum_memory::{NodeStatus, NodeType, NoteDraft, Source, Vault};

use crate::config::CandidateTtlDays;
use crate::context::project::CurrentProject;
use crate::curator::extract::is_duplicate;
use crate::memory::events::{ContextEvent, EventSender, EventSensitivity, EventSource, EventType};
use crate::memory::raw_log::RawLog;
use crate::senses::context::REDACTED_TITLE;
use crate::senses::privacy::{strictest, PrivacyFilter, Zone, EXCLUDED_PROCESS};
use crate::senses::types::PerceptionFrame;
use crate::triage::{Classification, TriageDecision};

/// Vault candidate titles are the classification summary, truncated to
/// this many characters (char-boundary safe). Summaries are already capped
/// at 200 chars by [`Classification::sanitized`]; 80 keeps titles readable
/// in the pending-review list and keeps derived slugs short.
const TITLE_MAX_CHARS: usize = 80;

/// Tag on every candidate this module writes: these memories came from
/// passive observation, not from the user telling Continuum something.
const OBSERVED_TAG: &str = "observed";

/// Epistemic label (spec §4.7) for a frame whose audio channel carried
/// speech — the user said this.
const LABEL_USER_STATED: &str = "user_stated";

/// Epistemic label for a screen-only frame — Continuum inferred this by
/// looking at pixels.
const LABEL_SYSTEM_INFERRED: &str = "system_inferred";

/// Extra tag carried by `success` candidates so the `note` bucket stays
/// searchable by outcome (spec §4.7 mapping table: `success → note(tag
/// result)`).
const RESULT_TAG: &str = "result";

/// Upper bound on the dedupe keys [`CandidateGate`] remembers. Bounded
/// like the events writer's own LRU: the gate must never grow without
/// limit in a process that runs for weeks.
const CANDIDATE_GATE_CAPACITY: usize = 256;

/// Suppresses repeat vault candidates for an event the §4.6 dedupe has
/// already collapsed.
///
/// **Why this exists.** The events writer collapses thirty "build failed"
/// occurrences into *one* `context_events` row with `count = 30` — that is
/// the compression ladder (spec §4.6/§4.11). Candidate proposal, however,
/// happens per frame, so without a gate the same failure lands thirty
/// slightly-differently-worded notes in the pending-review queue. That is
/// precisely the memory pollution spec §9's memory-precision harness caps
/// at a 10 % duplicate rate, and the near-duplicate title check the vault
/// write already does cannot catch it: LLM summaries of the same failure
/// share barely 40 % of their tokens, and a threshold loose enough to
/// merge them would merge genuinely different memories too.
///
/// So the gate keys on the thing that *does* know these are the same
/// event: the spec §4.6 dedupe key. One candidate per collapsed row, for
/// as long as the row stays open (`[events] collapse_window_minutes`,
/// anchored on the last occurrence exactly like the row itself). This is
/// consistent rather than lossy: the events table has *already* decided
/// those occurrences are one event and kept only the first summary.
///
/// A candidate with no event behind it (a bare `Remember` decision) is
/// always admitted — those are user-driven and rare.
#[derive(Debug)]
pub struct CandidateGate {
    window: Duration,
    /// dedupe key → last time a candidate was admitted for it.
    seen: std::collections::HashMap<String, DateTime<Utc>>,
}

impl CandidateGate {
    /// A gate whose window matches the events writer's collapse window.
    pub fn new(collapse_window_minutes: u64) -> Self {
        Self {
            window: Duration::minutes(collapse_window_minutes.max(1) as i64),
            seen: std::collections::HashMap::new(),
        }
    }

    /// Whether a candidate for `key` may be proposed at `ts`.
    ///
    /// **Every** occurrence re-anchors the window — admitted or not —
    /// exactly like the events row, whose collapse window anchors on
    /// `ts_last`. A failure loop that keeps producing therefore keeps
    /// suppressing, and one that stops for longer than the window is a new
    /// episode worth remembering separately. The deliberate consequence:
    /// a loop that never stops yields one candidate for as long as it
    /// lasts, which is what "one memory per collapsed row" means.
    pub fn admit(&mut self, key: &str, ts: DateTime<Utc>) -> bool {
        if let Some(last) = self.seen.get_mut(key) {
            // Clock going backwards (a replayed recording, a resumed
            // laptop) must not permanently suppress: only a *forward*
            // interval inside the window suppresses.
            let elapsed = ts.signed_duration_since(*last);
            if elapsed >= Duration::zero() && elapsed <= self.window {
                *last = ts;
                return false;
            }
        }
        if self.seen.len() >= CANDIDATE_GATE_CAPACITY {
            // Evict the coldest entry; at worst this re-admits one
            // candidate for a key nobody has seen in a long time.
            if let Some(coldest) = self
                .seen
                .iter()
                .min_by_key(|(_, seen)| **seen)
                .map(|(key, _)| key.clone())
            {
                self.seen.remove(&coldest);
            }
        }
        self.seen.insert(key.to_string(), ts);
        true
    }

    /// Keys currently held (tests, health detail).
    pub fn tracked(&self) -> usize {
        self.seen.len()
    }
}

/// The §4.7 mapping table: classification `event_type` → vault node type.
///
/// | event_type | vault type |
/// |---|---|
/// | `error` | `error` |
/// | `decision` | `decision` |
/// | `preference` | `preference` |
/// | `task_progress` | `task` |
/// | `success` | `note` (+ `result` tag) |
/// | `communication` | `note` |
/// | `other` | `note` |
/// | `routine` | **no candidate** |
/// | `project_switch` | **no candidate** |
///
/// `routine` is the "nothing happened" bucket — storing it would fill the
/// vault with noise the user then has to reject one by one. `project_switch`
/// is a window-source event the classifier cannot legally emit (B1's
/// sanitizer rejects it), listed here only because the spec's table names
/// it; it is handled for completeness, not reachability.
///
/// Every other registry type (window/git/file/system) returns `None`: they
/// are not classification outputs, and the vault's own types
/// (`attempt`/`result`/`file` in the 11-type audit) are deferred per spec.
pub fn vault_node_type(event_type: EventType) -> Option<NodeType> {
    match event_type {
        EventType::Error => Some(NodeType::Error),
        EventType::Decision => Some(NodeType::Decision),
        EventType::Preference => Some(NodeType::Preference),
        EventType::TaskProgress => Some(NodeType::Task),
        EventType::Success | EventType::Communication | EventType::Other => Some(NodeType::Note),
        // Explicit no-candidate types (spec §4.7).
        EventType::Routine | EventType::ProjectSwitch => None,
        // Not classification outputs at all. Listed exhaustively (no `_`
        // arm) on purpose: the registry is closed and additive-only, so a
        // new event type must fail to compile here until someone decides
        // whether it maps to a vault type.
        EventType::FocusSwitch
        | EventType::Commit
        | EventType::BranchSwitch
        | EventType::Conflict
        | EventType::DirtyChange
        | EventType::FileModified
        | EventType::FileCreated
        | EventType::FileDeleted
        | EventType::FileRenamed
        | EventType::FilesBulkChange
        | EventType::ProcessStarted
        | EventType::ProcessStopped
        | EventType::ResourcePressure
        | EventType::IdleStart
        | EventType::IdleEnd
        | EventType::Wake
        | EventType::WakeResult
        | EventType::VoiceCommand
        | EventType::ToggleChange
        | EventType::SourceUnavailable
        | EventType::EventsDropped => None,
    }
}

/// Whether the frame's audio channel carried actual speech. Drives both
/// the event source (`audio` wins over `screen`, spec §4.7: the transcript
/// is the stronger signal) and the epistemic label.
pub fn frame_has_speech(frame: &PerceptionFrame) -> bool {
    frame
        .audio
        .as_ref()
        .is_some_and(|a| !a.transcript.trim().is_empty())
}

/// Event source for a classified frame: `audio` when the frame carried a
/// transcript, `screen` otherwise.
pub fn classification_source(frame: &PerceptionFrame) -> EventSource {
    if frame_has_speech(frame) {
        EventSource::Audio
    } else {
        EventSource::Screen
    }
}

/// Epistemic label for a classified frame (spec §4.7): `user_stated` when
/// the belief came from something the user said, `system_inferred` when it
/// came from watching the screen. Carried as a vault tag so a human
/// reviewing the pending queue can see *how* Continuum came to believe it.
pub fn epistemic_label(frame: &PerceptionFrame) -> &'static str {
    if frame_has_speech(frame) {
        LABEL_USER_STATED
    } else {
        LABEL_SYSTEM_INFERRED
    }
}

/// Re-derives the privacy zone of an already-sanitized frame (spec §4.1
/// propagation rule): the strictest of the window's zone and the resolved
/// project's zone.
///
/// The frame reaching triage has been through the §4.1 choke point, so the
/// original title may be gone. Three signals survive, in order:
///
/// 1. the `[excluded]` sentinel process → `never_observe` (the window was
///    excluded; nothing derived from it may be persisted at all);
/// 2. the redaction literal in the title → `local_only` (the title keyword
///    that matched has been replaced, so the literal is the marker);
/// 3. otherwise a fresh [`PrivacyFilter::resolve_zone`] over the sanitized
///    process/title — process rules and surviving title keywords still
///    match.
pub fn frame_zone(
    privacy: &PrivacyFilter,
    frame: &PerceptionFrame,
    project_zone: Option<Zone>,
) -> Zone {
    let window_zone = if frame.context.foreground_process_name == EXCLUDED_PROCESS {
        Zone::NeverObserve
    } else if frame.context.foreground_window_title == REDACTED_TITLE {
        Zone::LocalOnly
    } else {
        privacy.resolve_zone(
            &frame.context.foreground_process_name,
            &frame.context.foreground_window_title,
        )
    };
    strictest([window_zone, project_zone.unwrap_or_default()])
}

/// Truncates a summary into a vault title: char-boundary safe, trimmed,
/// with an ellipsis when it was cut (so a reviewer can tell).
fn title_from_summary(summary: &str) -> String {
    let trimmed = summary.trim();
    if trimmed.chars().count() <= TITLE_MAX_CHARS {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(TITLE_MAX_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// The value written into the `triage_decision` raw-log column: the
/// decision's variant token, suffixed with the classification's event type
/// when the frame carried one (e.g. `wake_orchestrator/error`).
pub fn triage_decision_column(
    decision: &TriageDecision,
    classification: Option<&Classification>,
) -> String {
    match classification {
        Some(c) => format!(
            "{}/{}",
            decision.variant_name(),
            serde_json::to_value(c.event_type)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_string())
        ),
        None => decision.variant_name().to_string(),
    }
}

/// Everything one triage output turns into, computed synchronously and
/// without touching I/O so the main loop's select arm stays non-blocking
/// (and so the whole mapping is unit-testable without a vault or a DB).
#[derive(Debug, Clone)]
pub struct ConsumePlan {
    /// The classification event, or `None` when the frame carried no
    /// classification or came from a `never_observe` window (spec §4.1:
    /// excluded windows produce no events row, ever).
    pub event: Option<ContextEvent>,
    /// The vault memory candidate, or `None` when nothing should be
    /// stored (no `should_store`/`Remember`, a no-candidate event type, an
    /// empty summary, or an excluded window).
    pub candidate: Option<NoteDraft>,
    /// Value for the frame's `triage_decision` raw-log column.
    pub column: String,
}

/// Policy inputs for [`plan_consumption`] — the parts of the runtime the
/// mapping depends on, borrowed rather than owned so tests can build them
/// on the stack.
pub struct ClassificationPolicy<'a> {
    /// Zone rules, for re-deriving the frame's sensitivity.
    pub privacy: &'a PrivacyFilter,
    /// Per-type candidate expiry (`[memory].candidate_ttl_days`).
    pub ttl: CandidateTtlDays,
    /// Ids in the Projects table. A classifier-emitted project outside
    /// this set is dropped to the resolver's value (spec §4.6, log-only) —
    /// the events writer does not validate project ids, so this is the
    /// only place that can.
    pub known_projects: &'a HashSet<String>,
}

/// Maps one triage output onto its events/candidate/column consequences
/// (spec §4.7 consumption). Pure: no I/O, no clock reads (`now` is passed
/// in), no channel sends — [`ClassificationConsumer::consume`] performs the
/// effects.
pub fn plan_consumption(
    policy: &ClassificationPolicy<'_>,
    frame: &PerceptionFrame,
    decision: &TriageDecision,
    classification: Option<&Classification>,
    current_project: Option<&CurrentProject>,
    now: DateTime<Utc>,
) -> ConsumePlan {
    let column = triage_decision_column(decision, classification);
    let zone = frame_zone(policy.privacy, frame, current_project.and_then(|p| p.zone));

    // A never_observe window produces no derived artifacts at all: no
    // event row, no vault candidate. Only the decision column (which
    // describes what triage did, not what it saw) is written.
    if zone == Zone::NeverObserve {
        return ConsumePlan {
            event: None,
            candidate: None,
            column,
        };
    }

    let sensitivity = match zone {
        Zone::LocalOnly => EventSensitivity::LocalOnly,
        _ => EventSensitivity::CloudAllowed,
    };
    let resolver_project = current_project.map(|p| p.id.clone());
    let project_id = resolve_project(classification, policy.known_projects, resolver_project);

    let event = classification.map(|c| ContextEvent {
        ts: frame.ts,
        source: classification_source(frame),
        application: frame.context.foreground_process_name.clone(),
        window_title: frame.context.foreground_window_title.clone(),
        project_id: project_id.clone(),
        event_type: c.event_type,
        summary: c.summary.clone(),
        importance: c.importance,
        confidence: c.confidence,
        sensitivity,
        raw_reference: Some(frame.id.to_string()),
    });

    let candidate = plan_candidate(
        policy,
        frame,
        decision,
        classification,
        project_id,
        sensitivity,
        now,
    );

    ConsumePlan {
        event,
        candidate,
        column,
    }
}

/// Project id for the derived artifacts: the classifier's project when it
/// names one the Projects table knows, otherwise the resolver's value
/// (spec §4.6 — "a classifier-emitted project not present in the Projects
/// table is dropped to the resolver's value, log-only").
fn resolve_project(
    classification: Option<&Classification>,
    known: &HashSet<String>,
    resolver_project: Option<String>,
) -> Option<String> {
    let claimed = classification.and_then(|c| c.project.as_deref());
    match claimed {
        Some(id) if known.contains(id) => Some(id.to_string()),
        Some(id) => {
            tracing::debug!(
                layer = "triage",
                component = "consume",
                claimed_project = id,
                "classifier named a project outside the Projects table; using the resolver's value"
            );
            resolver_project
        }
        None => resolver_project,
    }
}

/// Builds the vault candidate for one triage output, or `None` when this
/// frame should not propose a memory (spec §4.7 (b)).
///
/// Stores when `should_store` is set *or* the decision itself is
/// `Remember` — a `Remember` with no classification still produces a
/// candidate (node type `note`, summary from the decision), because the
/// decision is the whole point of that variant and it predates
/// classification entirely.
fn plan_candidate(
    policy: &ClassificationPolicy<'_>,
    frame: &PerceptionFrame,
    decision: &TriageDecision,
    classification: Option<&Classification>,
    project_id: Option<String>,
    sensitivity: EventSensitivity,
    now: DateTime<Utc>,
) -> Option<NoteDraft> {
    let remember_summary = match decision {
        TriageDecision::Remember { summary } => Some(summary.as_str()),
        _ => None,
    };

    let (node_type, summary, importance, confidence, extra_tag) = match classification {
        Some(c) => {
            if !c.should_store && remember_summary.is_none() {
                return None;
            }
            // routine / project_switch: no candidate, whatever the flags say.
            let node_type = vault_node_type(c.event_type)?;
            let extra_tag = (c.event_type == EventType::Success).then_some(RESULT_TAG);
            // Prefer the classifier's summary; fall back to the Remember
            // text if the classification arrived summary-less.
            let summary = if c.summary.trim().is_empty() {
                remember_summary.unwrap_or_default()
            } else {
                c.summary.as_str()
            };
            (node_type, summary, c.importance, c.confidence, extra_tag)
        }
        // Remember without a classification (model omitted the block, or
        // the decision came from a non-triage path): a plain note.
        None => (
            NodeType::Note,
            remember_summary?,
            crate::memory::events::COLLECTOR_EVENT_IMPORTANCE,
            1.0,
            None,
        ),
    };

    let title = title_from_summary(summary);
    if title.is_empty() {
        tracing::debug!(
            layer = "triage",
            component = "consume",
            frame_id = %frame.id,
            "Skipping vault candidate — empty summary"
        );
        return None;
    }

    let mut tags = vec![OBSERVED_TAG.to_string(), epistemic_label(frame).to_string()];
    if let Some(tag) = extra_tag {
        tags.push(tag.to_string());
    }

    let expires = policy
        .ttl
        .for_node_type(node_type)
        .map(|days| now + Duration::days(i64::from(days)));

    Some(NoteDraft {
        node_type,
        title,
        body: candidate_body(frame, summary),
        project: project_id,
        status: NodeStatus::Candidate,
        confidence,
        importance,
        source: Source::Observed,
        source_ref: Some(format!("triage:frame:{}", frame.id)),
        // Propagation rule (spec §4.1 (4)): a note derived from a
        // local_only span is vault-sensitive, so the cloud gate strips it
        // from cloud-bound context.
        sensitivity: match sensitivity {
            EventSensitivity::LocalOnly => continuum_memory::Sensitivity::Sensitive,
            EventSensitivity::CloudAllowed => continuum_memory::Sensitivity::default(),
        },
        expires,
        relations: Vec::new(),
        tags,
    })
}

/// Candidate body: the summary, plus the frame reference a reviewer needs
/// to check the claim against the raw log. Every field interpolated here
/// is already scrubbed (§4.1 choke point).
fn candidate_body(frame: &PerceptionFrame, summary: &str) -> String {
    let mut body = String::with_capacity(summary.len() + 160);
    body.push_str(summary.trim());
    body.push_str("\n\n---\n");
    body.push_str(&format!(
        "Observed in {} — {}\n",
        frame.context.foreground_process_name, frame.context.foreground_window_title
    ));
    body.push_str(&format!(
        "frame: {} at {}\n",
        frame.id,
        frame.ts.to_rfc3339()
    ));
    body
}

/// Runtime wiring for [`plan_consumption`]: owns the handles the effects
/// need and performs them without ever blocking the caller.
#[derive(Clone)]
pub struct ClassificationConsumer {
    events: EventSender,
    vault: Arc<Vault>,
    raw_log: RawLog,
    privacy: Arc<PrivacyFilter>,
    ttl: CandidateTtlDays,
    /// Snapshot of the Projects-table ids at boot. Task C5 seam: when
    /// project-confirm intents mutate the live resolver, refresh this too
    /// — until then a project added mid-session simply falls back to the
    /// resolver's value, which is the safe direction.
    known_projects: Arc<HashSet<String>>,
    /// One candidate per collapsed event row (see [`CandidateGate`]).
    /// `parking_lot::Mutex` and every lock scope is synchronous — the
    /// gate is consulted on the caller's thread, never across an await.
    gate: Arc<parking_lot::Mutex<CandidateGate>>,
}

impl ClassificationConsumer {
    /// Builds a consumer. `known_projects` is the id set of every project
    /// the resolver knows (all statuses).
    pub fn new(
        events: EventSender,
        vault: Arc<Vault>,
        raw_log: RawLog,
        privacy: Arc<PrivacyFilter>,
        ttl: CandidateTtlDays,
        known_projects: HashSet<String>,
        collapse_window_minutes: u64,
    ) -> Self {
        Self {
            events,
            vault,
            raw_log,
            privacy,
            ttl,
            known_projects: Arc::new(known_projects),
            gate: Arc::new(parking_lot::Mutex::new(CandidateGate::new(
                collapse_window_minutes,
            ))),
        }
    }

    /// Consumes one triage output (spec §4.7). Returns immediately: the
    /// event goes out over the non-blocking sender, and the vault +
    /// raw-log writes are spawned so no `select!` arm ever waits on disk.
    /// Every spawned failure is contained and logged — a vault hiccup must
    /// never stall or crash the frame loop.
    pub fn consume(
        &self,
        frame: &PerceptionFrame,
        decision: &TriageDecision,
        classification: Option<&Classification>,
        current_project: Option<&CurrentProject>,
    ) {
        let policy = ClassificationPolicy {
            privacy: &self.privacy,
            ttl: self.ttl,
            known_projects: &self.known_projects,
        };
        let plan = plan_consumption(
            &policy,
            frame,
            decision,
            classification,
            current_project,
            Utc::now(),
        );

        // One candidate per collapsed events row (see [`CandidateGate`]).
        // Decided here, synchronously, from the same dedupe key the writer
        // will collapse on — before the spawn, so two frames in flight
        // cannot both slip a candidate through.
        let mut candidate = plan.candidate;
        if let (Some(event), true) = (plan.event.as_ref(), candidate.is_some()) {
            let key = crate::memory::events::dedupe_key(event);
            if !self.gate.lock().admit(&key, frame.ts) {
                tracing::debug!(
                    layer = "triage",
                    component = "consume",
                    frame_id = %frame.id,
                    dedupe_key = %key,
                    "Suppressing repeat candidate — the events row is still open"
                );
                candidate = None;
            }
        }

        if let Some(event) = plan.event {
            self.events.send(event);
        }

        let vault = self.vault.clone();
        let raw_log = self.raw_log.clone();
        let frame_id = frame.id;
        let column = plan.column;
        tokio::spawn(async move {
            apply_effects(&vault, &raw_log, frame_id, candidate, &column).await;
        });
    }
}

/// The spawned half of consumption: writes the candidate (duplicate-checked
/// the same way the curator does) and the `triage_decision` column. Errors
/// are logged, never propagated — this runs detached.
async fn apply_effects(
    vault: &Vault,
    raw_log: &RawLog,
    frame_id: uuid::Uuid,
    candidate: Option<NoteDraft>,
    column: &str,
) {
    if let Some(draft) = candidate {
        write_candidate(vault, draft).await;
    }
    if let Err(e) = raw_log.set_triage_decision(frame_id, column).await {
        tracing::warn!(
            layer = "triage",
            component = "consume",
            frame_id = %frame_id,
            error = %e,
            "Failed to record triage_decision for frame"
        );
    }
}

/// Writes one observation-derived candidate, reusing the curator's
/// near-duplicate check so repeated observations of the same thing don't
/// pile up identical pending notes. Contained failure: a duplicate-check
/// error skips the write rather than risking a duplicate.
async fn write_candidate(vault: &Vault, draft: NoteDraft) {
    match is_duplicate(vault, &draft.title).await {
        Ok(true) => {
            tracing::debug!(
                layer = "triage",
                component = "consume",
                title = %draft.title,
                "Skipping duplicate observation candidate"
            );
            return;
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(
                layer = "triage",
                component = "consume",
                title = %draft.title,
                error = %e,
                "Duplicate check failed; skipping observation candidate"
            );
            return;
        }
    }

    match vault.create(draft).await {
        Ok(note) => tracing::info!(
            layer = "triage",
            component = "consume",
            id = %note.frontmatter.id,
            node_type = note.frontmatter.node_type.as_str(),
            "Wrote observation candidate to vault"
        ),
        Err(e) => tracing::warn!(
            layer = "triage",
            component = "consume",
            error = %e.user_message(),
            "Failed to write observation candidate to vault"
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ContextConfig, PrivacyConfig};
    use crate::context::project::ProjectStatus;
    use crate::memory::events::ALL_EVENT_TYPES;
    use crate::senses::types::{AudioObservation, ContextObservation, ScreenObservation};
    use crate::triage::TriageOutput;
    use chrono::TimeZone;

    fn frame(process: &str, title: &str) -> PerceptionFrame {
        PerceptionFrame {
            id: uuid::Uuid::new_v4(),
            ts: Utc::now(),
            screen: ScreenObservation {
                description: "a terminal with a failing build".to_string(),
                world_compact: None,
                foreground_app: process.to_string(),
                has_error_visible: true,
                confidence: 0.9,
                screenshot_path: None,
                ts: Utc::now(),
            },
            audio: None,
            context: ContextObservation {
                foreground_window_title: title.to_string(),
                foreground_process_name: process.to_string(),
                idle_seconds: 0,
                in_call: false,
                ts: Utc::now(),
                ..Default::default()
            },
            salience_hint: 0.7,
        }
    }

    fn with_speech(mut f: PerceptionFrame) -> PerceptionFrame {
        f.audio = Some(AudioObservation {
            transcript: "remember that I prefer pnpm".to_string(),
            language: "en".to_string(),
            duration_ms: 1200,
            confidence: 0.9,
            ts: Utc::now(),
        });
        f
    }

    fn classification(event_type: EventType, should_store: bool) -> Classification {
        Classification {
            event_type,
            project: None,
            importance: 0.8,
            confidence: 0.7,
            summary: "cargo build failed with 3 errors".to_string(),
            should_store,
        }
    }

    fn filter() -> PrivacyFilter {
        PrivacyFilter::from_config(&ContextConfig::default(), &PrivacyConfig::default())
    }

    fn policy<'a>(
        privacy: &'a PrivacyFilter,
        known: &'a HashSet<String>,
    ) -> ClassificationPolicy<'a> {
        ClassificationPolicy {
            privacy,
            ttl: CandidateTtlDays::default(),
            known_projects: known,
        }
    }

    fn project(id: &str, zone: Option<Zone>) -> CurrentProject {
        CurrentProject {
            id: id.to_string(),
            name: id.to_string(),
            root_path: None,
            confidence: 0.9,
            source_tier: 1,
            zone,
            status: ProjectStatus::Confirmed,
        }
    }

    // -- mapping table (spec §4.7) --

    #[test]
    fn mapping_table_is_exhaustive_over_the_registry() {
        // Every registry type has a defined answer, and the classification
        // enum's answers are exactly the spec's table.
        let expected = |t: EventType| -> Option<NodeType> {
            match t {
                EventType::Error => Some(NodeType::Error),
                EventType::Decision => Some(NodeType::Decision),
                EventType::Preference => Some(NodeType::Preference),
                EventType::TaskProgress => Some(NodeType::Task),
                EventType::Success => Some(NodeType::Note),
                EventType::Communication => Some(NodeType::Note),
                EventType::Other => Some(NodeType::Note),
                _ => None,
            }
        };
        for t in ALL_EVENT_TYPES {
            assert_eq!(
                vault_node_type(t),
                expected(t),
                "mapping table disagrees for {t:?}"
            );
        }
    }

    #[test]
    fn no_candidate_event_types_produce_nothing() {
        let privacy = filter();
        let known = HashSet::new();
        for t in [EventType::Routine, EventType::ProjectSwitch] {
            let plan = plan_consumption(
                &policy(&privacy, &known),
                &frame("Code.exe", "main.rs"),
                &TriageDecision::Ignore,
                Some(&classification(t, true)),
                None,
                Utc::now(),
            );
            assert!(plan.candidate.is_none(), "{t:?} must not store a candidate");
        }
        // …even when the decision is Remember.
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Remember {
                summary: "routine work".to_string(),
            },
            Some(&classification(EventType::Routine, true)),
            None,
            Utc::now(),
        );
        assert!(plan.candidate.is_none());
    }

    // -- events (spec §4.7 (a)) --

    #[test]
    fn classified_frame_becomes_a_screen_event() {
        let privacy = filter();
        let known = HashSet::new();
        let f = frame("Code.exe", "main.rs - continuum");
        let c = classification(EventType::Error, false);
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &f,
            &TriageDecision::Ignore,
            Some(&c),
            Some(&project("continuum", None)),
            Utc::now(),
        );
        let event = plan.event.expect("classified frame emits an event");
        assert_eq!(event.source, EventSource::Screen);
        assert_eq!(event.event_type, EventType::Error);
        assert_eq!(event.application, "Code.exe");
        assert_eq!(event.window_title, "main.rs - continuum");
        assert_eq!(event.project_id.as_deref(), Some("continuum"));
        assert_eq!(event.summary, c.summary);
        assert_eq!(event.importance, c.importance);
        assert_eq!(event.confidence, c.confidence);
        assert_eq!(event.sensitivity, EventSensitivity::CloudAllowed);
        assert_eq!(
            event.raw_reference.as_deref(),
            Some(f.id.to_string()).as_deref()
        );
        assert_eq!(event.ts, f.ts);
        // Registry legality is what the sender enforces — prove it holds.
        assert!(event.event_type.valid_for(event.source));
    }

    #[test]
    fn audio_wins_over_screen_as_the_event_source() {
        let privacy = filter();
        let known = HashSet::new();
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &with_speech(frame("Code.exe", "main.rs")),
            &TriageDecision::Ignore,
            Some(&classification(EventType::Preference, false)),
            None,
            Utc::now(),
        );
        let event = plan.event.unwrap();
        assert_eq!(event.source, EventSource::Audio);
        assert!(event.event_type.valid_for(event.source));
    }

    #[test]
    fn unclassified_frame_emits_no_event() {
        let privacy = filter();
        let known = HashSet::new();
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Ignore,
            None,
            None,
            Utc::now(),
        );
        assert!(plan.event.is_none());
        assert!(plan.candidate.is_none());
    }

    // -- propagation rule (spec §4.1) --

    #[test]
    fn local_only_frame_produces_local_only_event_and_sensitive_candidate() {
        let context = ContextConfig {
            redact_sensitive_titles: true,
            ..Default::default()
        };
        let privacy = PrivacyFilter::from_config(&context, &PrivacyConfig::default());
        let known = HashSet::new();
        // The title has already been replaced by the redaction literal, so
        // the keyword that matched is gone — the literal is the marker.
        let f = frame("chrome.exe", REDACTED_TITLE);
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &f,
            &TriageDecision::Ignore,
            Some(&classification(EventType::Error, true)),
            None,
            Utc::now(),
        );
        assert_eq!(plan.event.unwrap().sensitivity, EventSensitivity::LocalOnly);
        assert_eq!(
            plan.candidate.unwrap().sensitivity,
            continuum_memory::Sensitivity::Sensitive
        );
    }

    #[test]
    fn local_only_project_zone_propagates_to_the_event() {
        let privacy = filter();
        let known = HashSet::new();
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Ignore,
            Some(&classification(EventType::Error, true)),
            Some(&project("secret-client", Some(Zone::LocalOnly))),
            Utc::now(),
        );
        assert_eq!(plan.event.unwrap().sensitivity, EventSensitivity::LocalOnly);
    }

    #[test]
    fn excluded_window_produces_no_event_and_no_candidate() {
        let privacy = filter();
        let known = HashSet::new();
        let f = frame(EXCLUDED_PROCESS, "");
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &f,
            &TriageDecision::Remember {
                summary: "something in a private window".to_string(),
            },
            Some(&classification(EventType::Error, true)),
            None,
            Utc::now(),
        );
        assert!(plan.event.is_none(), "never_observe rows cannot exist");
        assert!(plan.candidate.is_none());
        // The decision column still records what triage did.
        assert_eq!(plan.column, "remember/error");
    }

    // -- candidates (spec §4.7 (b)) --

    #[test]
    fn should_store_builds_a_candidate_with_the_specified_fields() {
        let privacy = filter();
        let known = HashSet::new();
        let f = frame("Code.exe", "main.rs - continuum");
        let now = Utc::now();
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &f,
            &TriageDecision::Ignore,
            Some(&classification(EventType::Error, true)),
            Some(&project("continuum", None)),
            now,
        );
        let draft = plan.candidate.expect("should_store proposes a candidate");
        assert_eq!(draft.node_type, NodeType::Error);
        assert_eq!(draft.status, NodeStatus::Candidate);
        assert_eq!(draft.source, Source::Observed);
        assert_eq!(draft.project.as_deref(), Some("continuum"));
        assert_eq!(draft.title, "cargo build failed with 3 errors");
        assert!(draft.body.contains("cargo build failed"));
        assert!(draft.body.contains(&f.id.to_string()));
        assert!(draft.tags.contains(&OBSERVED_TAG.to_string()));
        assert!(draft.tags.contains(&LABEL_SYSTEM_INFERRED.to_string()));
        assert_eq!(draft.source_ref, Some(format!("triage:frame:{}", f.id)));
        assert_eq!(draft.importance, 0.8);
        assert_eq!(draft.confidence, 0.7);
        // error → 30-day TTL.
        let expires = draft.expires.expect("error candidates expire");
        assert_eq!((expires - now).num_days(), 30);
    }

    #[test]
    fn expiry_follows_the_per_type_ttl_table() {
        let privacy = filter();
        let known = HashSet::new();
        let now = Utc::now();
        let cases = [
            (EventType::TaskProgress, Some(30)),
            (EventType::Error, Some(30)),
            (EventType::Decision, None),
            (EventType::Preference, None),
            (EventType::Success, Some(90)),
            (EventType::Communication, Some(90)),
            (EventType::Other, Some(90)),
        ];
        for (event_type, days) in cases {
            let plan = plan_consumption(
                &policy(&privacy, &known),
                &frame("Code.exe", "main.rs"),
                &TriageDecision::Ignore,
                Some(&classification(event_type, true)),
                None,
                now,
            );
            let draft = plan.candidate.expect("should_store proposes a candidate");
            match days {
                Some(d) => assert_eq!(
                    (draft.expires.expect("expiry set") - now).num_days(),
                    d,
                    "{event_type:?} TTL"
                ),
                None => assert!(draft.expires.is_none(), "{event_type:?} must not expire"),
            }
        }
    }

    #[test]
    fn success_candidates_carry_the_result_tag() {
        let privacy = filter();
        let known = HashSet::new();
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Ignore,
            Some(&classification(EventType::Success, true)),
            None,
            Utc::now(),
        );
        let draft = plan.candidate.unwrap();
        assert_eq!(draft.node_type, NodeType::Note);
        assert!(draft.tags.contains(&RESULT_TAG.to_string()));
    }

    #[test]
    fn epistemic_label_follows_the_audio_channel() {
        let privacy = filter();
        let known = HashSet::new();
        let spoken = plan_consumption(
            &policy(&privacy, &known),
            &with_speech(frame("Code.exe", "main.rs")),
            &TriageDecision::Ignore,
            Some(&classification(EventType::Preference, true)),
            None,
            Utc::now(),
        )
        .candidate
        .unwrap();
        assert!(spoken.tags.contains(&LABEL_USER_STATED.to_string()));
        assert!(!spoken.tags.contains(&LABEL_SYSTEM_INFERRED.to_string()));

        let seen = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Ignore,
            Some(&classification(EventType::Preference, true)),
            None,
            Utc::now(),
        )
        .candidate
        .unwrap();
        assert!(seen.tags.contains(&LABEL_SYSTEM_INFERRED.to_string()));
    }

    #[test]
    fn no_store_and_no_remember_means_no_candidate() {
        let privacy = filter();
        let known = HashSet::new();
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Ignore,
            Some(&classification(EventType::Error, false)),
            None,
            Utc::now(),
        );
        assert!(plan.event.is_some(), "the event is emitted regardless");
        assert!(plan.candidate.is_none());
    }

    #[test]
    fn remember_forces_a_candidate_even_without_should_store() {
        let privacy = filter();
        let known = HashSet::new();
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Remember {
                summary: "user switched to the release branch".to_string(),
            },
            Some(&classification(EventType::Decision, false)),
            None,
            Utc::now(),
        );
        let draft = plan.candidate.expect("Remember stores");
        assert_eq!(draft.node_type, NodeType::Decision);
        // The classification's own summary wins when it has one.
        assert_eq!(draft.title, "cargo build failed with 3 errors");
    }

    #[test]
    fn remember_without_classification_falls_back_to_a_note() {
        let privacy = filter();
        let known = HashSet::new();
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Remember {
                summary: "user prefers tabs over spaces".to_string(),
            },
            None,
            Some(&project("continuum", None)),
            Utc::now(),
        );
        assert!(plan.event.is_none(), "no classification, no event");
        let draft = plan.candidate.expect("Remember always stores something");
        assert_eq!(draft.node_type, NodeType::Note);
        assert_eq!(draft.title, "user prefers tabs over spaces");
        assert_eq!(draft.project.as_deref(), Some("continuum"));
        assert_eq!(draft.status, NodeStatus::Candidate);
        assert!(draft.expires.is_some(), "notes expire after 90 days");
        assert_eq!(plan.column, "remember");
    }

    #[test]
    fn empty_summary_produces_no_candidate() {
        let privacy = filter();
        let known = HashSet::new();
        let mut c = classification(EventType::Error, true);
        c.summary = "   ".to_string();
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Ignore,
            Some(&c),
            None,
            Utc::now(),
        );
        assert!(plan.candidate.is_none());
        assert!(plan.event.is_some());
    }

    #[test]
    fn long_summaries_truncate_on_a_char_boundary() {
        let privacy = filter();
        let known = HashSet::new();
        let mut c = classification(EventType::Error, true);
        c.summary = "é".repeat(150);
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Ignore,
            Some(&c),
            None,
            Utc::now(),
        );
        let title = plan.candidate.unwrap().title;
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1); // + ellipsis
        assert!(title.ends_with('…'));
    }

    // -- project resolution (spec §4.6) --

    #[test]
    fn classifier_project_is_used_when_the_table_knows_it() {
        let privacy = filter();
        let known: HashSet<String> = ["simcharts".to_string()].into_iter().collect();
        let mut c = classification(EventType::Error, true);
        c.project = Some("simcharts".to_string());
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Ignore,
            Some(&c),
            Some(&project("continuum", None)),
            Utc::now(),
        );
        assert_eq!(plan.event.unwrap().project_id.as_deref(), Some("simcharts"));
        assert_eq!(
            plan.candidate.unwrap().project.as_deref(),
            Some("simcharts")
        );
    }

    #[test]
    fn unknown_classifier_project_drops_to_the_resolver_value() {
        let privacy = filter();
        let known: HashSet<String> = ["continuum".to_string()].into_iter().collect();
        let mut c = classification(EventType::Error, true);
        c.project = Some("hallucinated-project".to_string());
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Ignore,
            Some(&c),
            Some(&project("continuum", None)),
            Utc::now(),
        );
        assert_eq!(plan.event.unwrap().project_id.as_deref(), Some("continuum"));
    }

    #[test]
    fn unknown_classifier_project_with_no_resolver_value_stays_none() {
        let privacy = filter();
        let known = HashSet::new();
        let mut c = classification(EventType::Error, true);
        c.project = Some("hallucinated-project".to_string());
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &TriageDecision::Ignore,
            Some(&c),
            None,
            Utc::now(),
        );
        assert!(plan.event.unwrap().project_id.is_none());
    }

    // -- triage_decision column (spec §4.7 (c)) --

    #[test]
    fn decision_column_pairs_decision_and_event_type() {
        let c = classification(EventType::Error, false);
        assert_eq!(
            triage_decision_column(
                &TriageDecision::WakeOrchestrator {
                    reason: "build broke".to_string(),
                    suggested_skill: None,
                },
                Some(&c)
            ),
            "wake_orchestrator/error"
        );
        assert_eq!(
            triage_decision_column(&TriageDecision::Ignore, None),
            "ignore"
        );
        assert_eq!(
            triage_decision_column(
                &TriageDecision::Ignore,
                Some(&classification(EventType::TaskProgress, false))
            ),
            "ignore/task_progress"
        );
    }

    #[tokio::test]
    async fn effects_write_the_candidate_and_the_decision_column() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(&dir.path().join("vault")).await.unwrap());
        let raw_log = RawLog::open(dir.path().join("raw.sqlite").to_str().unwrap())
            .await
            .unwrap();
        let f = frame("Code.exe", "main.rs");
        raw_log.write_frame(&f).await.unwrap();

        let privacy = filter();
        let known = HashSet::new();
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &f,
            &TriageDecision::Remember {
                summary: "ignored — classification wins".to_string(),
            },
            Some(&classification(EventType::Error, true)),
            None,
            Utc::now(),
        );
        apply_effects(&vault, &raw_log, f.id, plan.candidate, &plan.column).await;

        assert_eq!(
            raw_log.triage_decision(f.id).await.unwrap().as_deref(),
            Some("remember/error")
        );
        let hits = vault.search("cargo build failed", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].status, NodeStatus::Candidate);
        assert_eq!(hits[0].node_type, NodeType::Error);
        // The expiry survived the NoteDraft → frontmatter → index round trip.
        let note = vault.get(&hits[0].id).await.unwrap();
        assert!(note.frontmatter.expires.is_some());

        // A second, identical observation is deduped by the curator's
        // near-duplicate check rather than piling up pending notes.
        let plan2 = plan_consumption(
            &policy(&privacy, &known),
            &f,
            &TriageDecision::Ignore,
            Some(&classification(EventType::Error, true)),
            None,
            Utc::now(),
        );
        apply_effects(&vault, &raw_log, f.id, plan2.candidate, &plan2.column).await;
        assert_eq!(
            vault.search("cargo build failed", 5).await.unwrap().len(),
            1
        );
        assert_eq!(
            raw_log.triage_decision(f.id).await.unwrap().as_deref(),
            Some("ignore/error")
        );
    }

    #[tokio::test]
    async fn column_write_for_a_missing_frame_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let raw_log = RawLog::open(dir.path().join("raw.sqlite").to_str().unwrap())
            .await
            .unwrap();
        assert!(raw_log
            .set_triage_decision(uuid::Uuid::new_v4(), "ignore")
            .await
            .is_ok());
    }

    #[test]
    fn triage_output_classification_flows_straight_into_a_plan() {
        // End-to-end shape check: the B1 parse container's output is
        // exactly what this module consumes, with no adapter in between.
        let raw = r#"{"decision":"ignore","classification":{"event_type":"error",
            "project":"continuum","importance":0.9,"confidence":0.8,
            "summary":"tests failed","should_store":true}}"#;
        let output = TriageOutput::from_json(raw).unwrap();
        let privacy = filter();
        let known: HashSet<String> = ["continuum".to_string()].into_iter().collect();
        let plan = plan_consumption(
            &policy(&privacy, &known),
            &frame("Code.exe", "main.rs"),
            &output.decision,
            output.classification.as_ref(),
            None,
            Utc::now(),
        );
        assert_eq!(plan.event.unwrap().event_type, EventType::Error);
        assert_eq!(
            plan.candidate.unwrap().project.as_deref(),
            Some("continuum")
        );
        assert_eq!(plan.column, "ignore/error");
    }

    // --- CandidateGate (spec §4.6 compression at the candidate layer) ---

    fn t(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 5, 9, 0, 0).unwrap() + Duration::minutes(minutes)
    }

    #[test]
    fn gate_admits_the_first_occurrence_and_suppresses_the_rest() {
        let mut gate = CandidateGate::new(10);
        assert!(gate.admit("k", t(0)));
        for minute in 1..=9 {
            assert!(
                !gate.admit("k", t(minute)),
                "occurrence at +{minute}m is still the same collapsed row"
            );
        }
        assert_eq!(gate.tracked(), 1);
    }

    #[test]
    fn gate_re_anchors_on_every_occurrence_like_the_events_row_does() {
        let mut gate = CandidateGate::new(10);
        assert!(gate.admit("k", t(0)));
        // A steady drip keeps the row open, so it keeps suppressing…
        for minute in [5, 10, 15, 20] {
            assert!(!gate.admit("k", t(minute)));
        }
        // …but a gap longer than the window is a new episode.
        assert!(gate.admit("k", t(31)));
    }

    #[test]
    fn gate_keys_are_independent() {
        let mut gate = CandidateGate::new(10);
        assert!(gate.admit("build-failed", t(0)));
        assert!(
            gate.admit("typescript-error", t(1)),
            "a different collapsed row gets its own candidate"
        );
        assert!(!gate.admit("build-failed", t(2)));
    }

    #[test]
    fn gate_never_suppresses_on_a_backwards_clock() {
        let mut gate = CandidateGate::new(10);
        assert!(gate.admit("k", t(60)));
        assert!(
            gate.admit("k", t(0)),
            "a clock jumping backwards must not lock a key out forever"
        );
    }

    #[test]
    fn gate_is_bounded() {
        let mut gate = CandidateGate::new(10);
        for i in 0..(CANDIDATE_GATE_CAPACITY + 50) {
            assert!(gate.admit(&format!("k{i}"), t(i as i64 / 100)));
        }
        assert!(gate.tracked() <= CANDIDATE_GATE_CAPACITY);
    }

    #[tokio::test]
    async fn consumer_writes_one_candidate_for_a_repeated_failure() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(&dir.path().join("vault")).await.unwrap());
        let raw_log = RawLog::open(&dir.path().join("raw.sqlite").to_string_lossy())
            .await
            .unwrap();
        let consumer = ClassificationConsumer::new(
            EventSender::log_only(),
            vault.clone(),
            raw_log.clone(),
            Arc::new(filter()),
            CandidateTtlDays::default(),
            HashSet::new(),
            10,
        );

        // Five occurrences of the same failure with drifting summaries —
        // the classic LLM-summary variance the dedupe key ignores.
        let summaries = [
            "cargo build failed: mismatched types at events.rs:214",
            "cargo build failed: expected EventType found EventSource",
            "cargo build failed: 2 errors emitted in continuum-core",
            "cargo build failed: mismatched types at events.rs:311",
            "cargo build failed: mismatched types in the dedupe key builder",
        ];
        for (index, summary) in summaries.iter().enumerate() {
            let mut f = frame("WindowsTerminal.exe", "cargo build");
            f.ts = t(index as i64);
            let classification = Classification {
                event_type: EventType::Error,
                project: None,
                importance: 0.7,
                confidence: 0.8,
                summary: (*summary).to_string(),
                should_store: true,
            };
            consumer.consume(&f, &TriageDecision::Ignore, Some(&classification), None);
        }

        // The vault writes are spawned; wait for the queue to settle.
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if !vault.pending().await.unwrap().is_empty() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let pending = vault.pending().await.unwrap();
        assert_eq!(
            pending.len(),
            1,
            "five occurrences of one collapsed row propose one candidate, got {:?}",
            pending.iter().map(|n| &n.title).collect::<Vec<_>>()
        );
        raw_log.close().await;
    }
}
