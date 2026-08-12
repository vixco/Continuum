//! # Session-state tracker (context engine spec §4.8)
//!
//! Continuum's live answer to **"what is the user doing right now?"**.
//!
//! The state has two halves with very different costs:
//!
//! - **Mechanical fields** — `active_project`, `active_app`, `window_title`,
//!   `open_files`, `last_error`, `last_success`, `last_user_command`. These
//!   update *synchronously* from the frame loop and the context-event
//!   stream. No model, no I/O, microseconds per update.
//! - **Inferred fields** — `current_goal` and `current_task` plus their
//!   `confidence`. These cost a local-LLM call, so they are produced by
//!   [`spawn_inference_task`], an **own spawned task** that is never awaited
//!   by the frame loop, fires only on the spec §4.8 triggers (project
//!   switch, event volume, staleness), never more often than
//!   `infer_min_interval_secs`, never while the machine is idle, and always
//!   through the **background** tier of [`crate::llm_gate::LlmGate`]
//!   (Task B2) with `max_tokens = infer_max_tokens`.
//!
//! ## Gating
//!
//! Everything here that can be ungated **is** ungated: the struct, the
//! renderers, the trigger predicate, the JSON parse, the rehydration
//! arithmetic and the inference task itself (it only needs the ungated
//! [`crate::curator::CuratorLlm`] trait). Only two things touch
//! `runtime`-gated types and are `#[cfg]`-ed accordingly:
//! [`SessionStateHub::apply_context_event`] (takes a
//! `memory::events::ContextEvent`) and [`rehydrate_from_disk`] (reads the
//! raw-log DB). B7's packager reads [`SessionState`] and must compile
//! featureless, which is why the split falls here.
//!
//! ## Zone propagation (spec §4.1)
//!
//! If **any** event in the window an inference ran over was `local_only`,
//! the inferred fields are tagged [`SessionState::local_only`]. Local
//! consumers (triage `memory_summary`) still see the real text — a local
//! model is allowed to. Cloud-bound consumers must render
//! [`SessionState::cloud_view`], which replaces goal/task with the generic
//! "working in a private context".
//!
//! ## Rehydration (spec §4.8)
//!
//! On boot the hub is seeded from the last published `state.json` snapshot
//! plus the most recent `context_events`, with confidence discounted by
//! age ([`staleness_discount`]). This is what lets the §4.12 continuation
//! resolver answer "ga door" after a restart.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Deserializer, Serialize};

use crate::config::SessionStateConfig;
use crate::context::intents::SessionField;
use crate::context::project::CurrentProject;
use crate::senses::privacy::EXCLUDED_PROCESS;
use crate::senses::types::PerceptionFrame;

/// The session-state inference prompt, loaded at compile time from the
/// repo-root `prompts/session-state.md`. `{{STATE}}` and `{{EVENTS}}` are
/// substituted per call in [`build_inference_prompt`].
pub const INFERENCE_PROMPT: &str = include_str!("../../../../prompts/session-state.md");

/// What a `never_observe` window renders as in session state (spec §4.1
/// sentinel semantics: "session state shows `[private]`").
pub const PRIVATE_PLACEHOLDER: &str = "[private]";

/// What consumers render for a goal/task Continuum does not know (spec
/// §4.8: "below `confidence_floor` fields render 'unknown'").
pub const UNKNOWN: &str = "unknown";

/// What a cloud-bound consumer sees instead of `local_only` goal/task
/// (spec §4.1 propagation rule (2)).
pub const PRIVATE_CONTEXT_PHRASE: &str = "working in a private context";

/// Maximum entries kept in [`SessionState::open_files`] (best-effort list,
/// most recent first).
pub const MAX_OPEN_FILES: usize = 8;

/// Maximum characters of a single inferred goal/task line.
pub const MAX_INFERRED_LEN: usize = 120;

/// Maximum characters in the concrete recent-activity sentence.
pub const MAX_ACTIVITY_SUMMARY_LEN: usize = 180;

/// Maximum characters in the evidence-backed interpretation shown locally.
pub const MAX_INTERPRETATION_LEN: usize = 280;

/// Maximum characters in one proactive but optional help suggestion.
pub const MAX_SUGGESTED_HELP_LEN: usize = 180;

/// Events retained in the hub's rolling window. Must be ≥
/// [`INFERENCE_EVENT_WINDOW`] so a full prompt window is always available.
pub const EVENT_RING_CAP: usize = 64;

/// Events handed to one inference call (spec §4.8: bounded prompt).
pub const INFERENCE_EVENT_WINDOW: usize = 40;

/// Character cap on the event block inside an inference prompt — the
/// curator's budget pattern, so a storm of long summaries can't blow the
/// local model's context.
pub const INFERENCE_EVENTS_CHAR_CAP: usize = 2_400;

/// Char cap on the triage `memory_summary` render (spec §4.7 token
/// budget: "`memory_summary` (now fed by session state) char-capped at
/// 600").
pub const MEMORY_SUMMARY_MAX_CHARS: usize = 600;

/// How often the inference task re-evaluates its trigger predicate. Not a
/// cadence — the predicate itself decides, and says "no" on almost every
/// tick. 30 s is fine-grained against a 120 s minimum interval while
/// costing one predicate evaluation per tick.
pub const INFERENCE_TICK_SECS: u64 = 30;

/// Rehydration: a snapshot younger than this keeps its confidence intact.
pub const REHYDRATE_FRESH_MINUTES: i64 = 30;

/// Rehydration: a snapshot older than this is "very stale".
pub const REHYDRATE_VERY_STALE_HOURS: i64 = 4;

/// Rehydration discount applied between [`REHYDRATE_FRESH_MINUTES`] and
/// [`REHYDRATE_VERY_STALE_HOURS`].
pub const REHYDRATE_STALE_DISCOUNT: f32 = 0.5;

/// Rehydration discount applied beyond [`REHYDRATE_VERY_STALE_HOURS`].
pub const REHYDRATE_VERY_STALE_DISCOUNT: f32 = 0.25;

/// How far back [`rehydrate_from_disk`] reads `context_events` to seed the
/// mechanical fields.
pub const REHYDRATE_LOOKBACK_MINUTES: i64 = 60;

/// The Unix epoch as a `DateTime<Utc>` — the deterministic fallback for a
/// timestamp a lenient parse could not recover.
fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(0, 0).unwrap_or_else(Utc::now)
}

/// Truncates `s` to at most `max` characters, never splitting a UTF-8
/// character.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// A one-line fact plus when it was observed.
///
/// **Spec deviation, deliberate:** the spec §4.8 JSON sketch shows
/// `last_error`/`last_success`/`last_user_command` as bare strings, but the
/// same section requires `last_user_command` to carry "+ ts" and §4.12
/// ranks continuation candidates by *recency* × confidence — impossible
/// without the timestamp. So these fields serialize as
/// `{"text": …, "at": …}` and **deserialize from either** shape, so a bare
/// string written by an older/foreign producer still loads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StampedText {
    /// The one-line text (already scrubbed by whoever produced it).
    pub text: String,
    /// When it happened.
    pub at: DateTime<Utc>,
}

impl StampedText {
    /// Convenience constructor.
    pub fn new(text: impl Into<String>, at: DateTime<Utc>) -> Self {
        Self {
            text: text.into(),
            at,
        }
    }
}

impl<'de> Deserialize<'de> for StampedText {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Obj {
                text: String,
                #[serde(default)]
                at: Option<DateTime<Utc>>,
            },
            Text(String),
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Obj { text, at } => StampedText {
                text,
                at: at.unwrap_or_else(epoch),
            },
            Repr::Text(text) => StampedText { text, at: epoch() },
        })
    }
}

/// Continuum's live session state (spec §4.8).
///
/// Field names match the spec JSON exactly; `local_only` is the extra
/// zone-propagation tag the §4.1 rule requires the cloud gate to read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    /// Resolver's post-hysteresis current project id.
    #[serde(default)]
    pub active_project: Option<String>,
    /// Inferred larger goal. `None` means "unknown" (either never
    /// inferred, or the inference came back under `confidence_floor`).
    #[serde(default)]
    pub current_goal: Option<String>,
    /// Inferred concrete task. Same `None` semantics as `current_goal`.
    #[serde(default)]
    pub current_task: Option<String>,
    /// Concrete recent activity inferred from the event sequence. This says
    /// what happened inside apps, not merely which app had focus.
    #[serde(default)]
    pub activity_summary: Option<String>,
    /// Short evidence-backed conclusion about what the observed sequence
    /// probably means. This is a conclusion, never hidden chain-of-thought.
    #[serde(default)]
    pub interpretation: Option<String>,
    /// A specific timely offer Continuum could make, or `None` when silence
    /// is more useful.
    #[serde(default)]
    pub suggested_help: Option<String>,
    /// Foreground process name, or [`PRIVATE_PLACEHOLDER`] for a
    /// `never_observe` sentinel frame.
    #[serde(default)]
    pub active_app: Option<String>,
    /// Foreground window title (already scrubbed/redacted upstream), or
    /// [`PRIVATE_PLACEHOLDER`] for a sentinel frame.
    #[serde(default)]
    pub window_title: Option<String>,
    /// Best-effort recently-touched files, most recent first, capped at
    /// [`MAX_OPEN_FILES`]. Derived from editor window titles and file
    /// events — never authoritative.
    #[serde(default)]
    pub open_files: Vec<String>,
    /// Most recent classified `error` event.
    #[serde(default)]
    pub last_error: Option<StampedText>,
    /// Most recent classified `success` event.
    #[serde(default)]
    pub last_success: Option<StampedText>,
    /// Most recent thing the user actually asked for (voice/chat/hotkey).
    #[serde(default)]
    pub last_user_command: Option<StampedText>,
    /// Confidence in `current_goal`/`current_task`, \[0.0, 1.0\].
    #[serde(default)]
    pub confidence: f32,
    /// Zone propagation (spec §4.1 rule 2): the inference window contained
    /// at least one `local_only` event, so the inferred fields are
    /// local-only. Cloud consumers must render [`SessionState::cloud_view`].
    #[serde(default)]
    pub local_only: bool,
    /// Field names ([`crate::context::intents::SessionField`] tokens) the
    /// user **pinned** on the Context page (spec §4.13, Task C5). A pinned
    /// field is never overwritten by the frame loop or by inference; it is
    /// cleared explicitly, or — for `project` — automatically once the
    /// resolver reports a *different* project above
    /// [`PIN_CLEAR_CONFIDENCE`] for `switch_min_secs` (no pin/resolver
    /// deadlock). Pins block **session-state overwrite only**, never
    /// resolution: the resolver keeps resolving and collectors keep
    /// collecting under the real project.
    #[serde(default)]
    pub pinned: Vec<String>,
    /// Field names the user **corrected** on the Context page. Purely
    /// informational — the page renders "you told me this" next to the
    /// value. Unlike a pin, a correction does not block later inference
    /// (a corrected goal may legitimately move on); for `project` the
    /// persisted `force_project` rule is what makes the correction stick.
    #[serde(default)]
    pub user_confirmed: Vec<String>,
    /// When the current session state began (boot, or the last project
    /// change).
    #[serde(default = "epoch")]
    pub since: DateTime<Utc>,
    /// When any field last changed.
    #[serde(default = "epoch")]
    pub updated: DateTime<Utc>,
    /// When `current_goal`/`current_task` were last *established* — by an
    /// inference or by a user correction. `None` means "never inferred".
    ///
    /// Fixwave 3b (I5): the §4.12 continuation resolver used to age the
    /// session task off `updated`, which the frame loop rewrites every
    /// second (`active_app`/`window_title` change constantly), so
    /// `recency_decay` was permanently 1.0 and "ga door" after lunch
    /// confidently recommended this morning's task instead of asking. This
    /// is the clock that actually describes the task's age.
    #[serde(default)]
    pub inferred_at: Option<DateTime<Utc>>,
}

impl Default for SessionState {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            active_project: None,
            current_goal: None,
            current_task: None,
            activity_summary: None,
            interpretation: None,
            suggested_help: None,
            active_app: None,
            window_title: None,
            open_files: Vec::new(),
            last_error: None,
            last_success: None,
            last_user_command: None,
            confidence: 0.0,
            local_only: false,
            pinned: Vec::new(),
            user_confirmed: Vec::new(),
            since: now,
            updated: now,
            inferred_at: None,
        }
    }
}

impl SessionState {
    /// `current_task` when it clears `confidence_floor`, else `None`
    /// (consumers render [`UNKNOWN`]).
    /// Whether the given field is pinned (spec §4.13).
    pub fn is_pinned(&self, field: SessionField) -> bool {
        self.pinned.iter().any(|f| f == field.as_str())
    }

    /// Whether the user has explicitly corrected the given field.
    pub fn is_user_confirmed(&self, field: SessionField) -> bool {
        self.user_confirmed.iter().any(|f| f == field.as_str())
    }

    /// Drops the inferred goal/task (and the `confidence`/`local_only` tags
    /// that describe them) because the project they described is no longer
    /// current — **except** for fields the user pinned.
    ///
    /// Fixwave 3b (I4): the project-switch path in [`SessionStateHub::apply_frame`]
    /// and the project branch of [`SessionStateHub::apply_correction`] both
    /// used to clear unconditionally. Since
    /// [`SessionStateHub::apply_inference`] then *refuses* to write a
    /// pinned field, the cleared value could never come back: the Context
    /// page showed a pin badge with no value and every wake rendered
    /// `task: unknown`. `confidence` is only zeroed when neither field is
    /// pinned — it is the shared confidence of whatever survived.
    pub fn clear_inferred_unpinned(&mut self) {
        let goal_pinned = self.is_pinned(SessionField::Goal);
        let task_pinned = self.is_pinned(SessionField::Task);
        if !goal_pinned {
            self.current_goal = None;
        }
        if !task_pinned {
            self.current_task = None;
        }
        if !goal_pinned && !task_pinned {
            self.confidence = 0.0;
            self.local_only = false;
            self.inferred_at = None;
        }
        self.activity_summary = None;
        self.interpretation = None;
        self.suggested_help = None;
    }

    pub fn task_if_confident(&self, confidence_floor: f32) -> Option<&str> {
        (self.confidence >= confidence_floor)
            .then_some(self.current_task.as_deref())
            .flatten()
    }

    /// `current_goal` when it clears `confidence_floor`, else `None`.
    pub fn goal_if_confident(&self, confidence_floor: f32) -> Option<&str> {
        (self.confidence >= confidence_floor)
            .then_some(self.current_goal.as_deref())
            .flatten()
    }

    /// `true` when nothing has ever been observed — the frame loop has not
    /// produced a frame yet and no events arrived. The inference trigger
    /// refuses to burn a GPU pass on this.
    pub fn is_empty(&self) -> bool {
        self.active_project.is_none()
            && self.active_app.is_none()
            && self.last_error.is_none()
            && self.last_success.is_none()
            && self.last_user_command.is_none()
            && self.open_files.is_empty()
    }

    /// A cloud-safe projection (spec §4.1 propagation rule 2): when the
    /// inferred fields are `local_only`, goal and task collapse to
    /// [`PRIVATE_CONTEXT_PHRASE`]. Everything else is already scrubbed by
    /// the collectors; zone *stripping* of the mechanical fields stays the
    /// packager's job (B7), which knows its own egress point.
    pub fn cloud_view(&self) -> SessionState {
        if !self.local_only {
            return self.clone();
        }
        let mut out = self.clone();
        if out.current_goal.is_some() {
            out.current_goal = Some(PRIVATE_CONTEXT_PHRASE.to_string());
        }
        if out.current_task.is_some() {
            out.current_task = Some(PRIVATE_CONTEXT_PHRASE.to_string());
        }
        if out.activity_summary.is_some() {
            out.activity_summary = Some(PRIVATE_CONTEXT_PHRASE.to_string());
        }
        if out.interpretation.is_some() {
            out.interpretation = Some(PRIVATE_CONTEXT_PHRASE.to_string());
        }
        if out.suggested_help.is_some() {
            out.suggested_help = Some(PRIVATE_CONTEXT_PHRASE.to_string());
        }
        out
    }

    /// Renders the compact summary fed to the triage layer's
    /// `memory_summary` argument (spec §4.7 token budget: char-capped at
    /// `max_chars`, default [`MEMORY_SUMMARY_MAX_CHARS`]).
    ///
    /// Lines are emitted in priority order and dropped whole once the
    /// budget is exhausted, so the render never ends mid-fact. `now` is a
    /// parameter (not `Utc::now()`) to keep the function pure and
    /// testable. This render is **local-only-safe by destination**: it
    /// feeds the local triage model, which spec §4.1 permits to see
    /// `local_only` content.
    pub fn render_memory_summary(
        &self,
        now: DateTime<Utc>,
        max_chars: usize,
        confidence_floor: f32,
    ) -> String {
        let mut lines: Vec<String> = Vec::new();
        let head = format!(
            "project: {} | app: {}",
            self.active_project.as_deref().unwrap_or(UNKNOWN),
            self.active_app.as_deref().unwrap_or(UNKNOWN)
        );
        lines.push(head);
        lines.push(format!(
            "goal: {}",
            self.goal_if_confident(confidence_floor).unwrap_or(UNKNOWN)
        ));
        lines.push(format!(
            "task: {}",
            self.task_if_confident(confidence_floor).unwrap_or(UNKNOWN)
        ));
        if let Some(cmd) = &self.last_user_command {
            lines.push(format!(
                "last command ({}): {}",
                render_age(now, cmd.at),
                cmd.text
            ));
        }
        if let Some(err) = &self.last_error {
            lines.push(format!(
                "last error ({}): {}",
                render_age(now, err.at),
                err.text
            ));
        }
        if let Some(ok) = &self.last_success {
            lines.push(format!(
                "last success ({}): {}",
                render_age(now, ok.at),
                ok.text
            ));
        }
        if !self.open_files.is_empty() {
            lines.push(format!("open files: {}", self.open_files.join(", ")));
        }

        let mut out = String::new();
        for line in lines {
            let need = if out.is_empty() {
                line.chars().count()
            } else {
                line.chars().count() + 1
            };
            if out.chars().count() + need > max_chars {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&line);
        }
        // Belt and braces: a single first line longer than the budget is
        // hard-truncated on a char boundary rather than dropped, so the
        // render is never empty when there is something to say.
        if out.is_empty() {
            if let Some(first) = self
                .active_project
                .as_deref()
                .or(self.active_app.as_deref())
            {
                out = truncate_chars(first, max_chars);
            }
        }
        truncate_chars(&out, max_chars)
    }
}

/// Renders a coarse "how long ago" label. Deliberately coarse: the exact
/// second is noise to a model and costs tokens.
fn render_age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds();
    if secs < 0 {
        return "just now".to_string();
    }
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    format!("{}d ago", hours / 24)
}

// ---------------------------------------------------------------------------
// Event digests
// ---------------------------------------------------------------------------

/// The ungated projection of a `context_events` row / in-flight
/// `ContextEvent` that session state actually needs.
///
/// Keeping this separate from `memory::events::ContextEvent` is what lets
/// the whole trigger/prompt/rehydration surface be tested under
/// `--no-default-features`; the gated bridges
/// ([`SessionStateHub::apply_context_event`], [`digest_from_row`]) are the
/// only code that knows the real enums.
#[derive(Debug, Clone, PartialEq)]
pub struct EventDigest {
    /// When the event happened.
    pub ts: DateTime<Utc>,
    /// Stable snake_case source token (`screen`, `audio`, `file`, …).
    pub source: String,
    /// Stable snake_case event-type token (`error`, `success`, …).
    pub event_type: String,
    /// Application the event is about.
    pub application: String,
    /// Resolved project id, when known.
    pub project_id: Option<String>,
    /// One-line summary (already scrubbed).
    pub summary: String,
    /// Importance in \[0.0, 1.0\].
    pub importance: f32,
    /// Whether the event carries the `local_only` sensitivity tag.
    pub local_only: bool,
}

/// Event-type tokens whose summary is a file path worth listing in
/// [`SessionState::open_files`].
const FILE_EVENT_TYPES: &[&str] = &["file_modified", "file_created", "file_renamed"];

// ---------------------------------------------------------------------------
// Inference trigger
// ---------------------------------------------------------------------------

/// Why an inference pass fired (spec §4.8 trigger set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceTrigger {
    /// The resolver's current project changed.
    ProjectSwitch,
    /// At least `infer_min_new_events` significant events accumulated.
    EventVolume,
    /// A short sequence of app focus changes suggests that the user's task
    /// or obstacle may have changed.
    ActivitySequence,
    /// The inferred fields are older than `infer_max_age_minutes` (or were
    /// never produced).
    Stale,
}

impl InferenceTrigger {
    /// Stable label for logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::ProjectSwitch => "project_switch",
            Self::EventVolume => "event_volume",
            Self::ActivitySequence => "activity_sequence",
            Self::Stale => "stale",
        }
    }
}

/// Everything [`evaluate_trigger`] needs. A plain struct so the whole
/// predicate is a pure function of explicit inputs (no clock, no locks).
#[derive(Debug, Clone, Copy)]
pub struct TriggerInputs {
    /// Evaluation time.
    pub now: DateTime<Utc>,
    /// `CadenceControl::is_idle()` — spec §4.11: "session inference
    /// pauses" while idle.
    pub idle: bool,
    /// Whether the state has anything to infer over at all.
    pub has_content: bool,
    /// When the last inference *attempt* ran (`None` = never).
    pub last_inference_at: Option<DateTime<Utc>>,
    /// Whether the project changed since the last attempt.
    pub project_switched: bool,
    /// Significant events (importance ≥ `significant_importance`) since
    /// the last attempt.
    pub significant_events: usize,
    /// Focus switches since the last attempt, independent of importance.
    pub focus_switches: usize,
}

/// The spec §4.8 trigger predicate.
///
/// Order of the gates matters and is asserted in tests:
/// 1. **Idle** — never infer while idle (spec §4.11).
/// 2. **Nothing to infer** — an empty state never burns a GPU pass.
/// 3. **Minimum interval** — `infer_min_interval_secs` since the last
///    *attempt* (not the last success: a failed call must not retrigger
///    immediately).
/// 4. Then, in precedence order: project switch → event volume → short app
///    activity sequence → staleness (including "never inferred").
pub fn evaluate_trigger(
    inputs: &TriggerInputs,
    cfg: &SessionStateConfig,
) -> Option<InferenceTrigger> {
    if inputs.idle || !inputs.has_content {
        return None;
    }
    if let Some(last) = inputs.last_inference_at {
        let elapsed = inputs.now - last;
        if elapsed < Duration::seconds(cfg.infer_min_interval_secs as i64) {
            return None;
        }
    }
    if inputs.project_switched {
        return Some(InferenceTrigger::ProjectSwitch);
    }
    if inputs.significant_events >= cfg.infer_min_new_events {
        return Some(InferenceTrigger::EventVolume);
    }
    if inputs.focus_switches >= cfg.infer_min_focus_switches {
        return Some(InferenceTrigger::ActivitySequence);
    }
    match inputs.last_inference_at {
        None => Some(InferenceTrigger::Stale),
        Some(last) => (inputs.now - last >= Duration::minutes(cfg.infer_max_age_minutes as i64))
            .then_some(InferenceTrigger::Stale),
    }
}

// ---------------------------------------------------------------------------
// Inference prompt + parse
// ---------------------------------------------------------------------------

/// One parsed inference reply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InferenceResult {
    /// Inferred goal, `None` when empty or below the confidence floor.
    pub goal: Option<String>,
    /// Inferred task, same semantics.
    pub task: Option<String>,
    /// Concrete activity visible in the evidence sequence.
    pub activity: Option<String>,
    /// Concise interpretation of that sequence.
    pub interpretation: Option<String>,
    /// Specific useful intervention, when warranted.
    pub suggested_help: Option<String>,
    /// Clamped confidence in \[0.0, 1.0\].
    pub confidence: f32,
}

/// Renders the inference prompt from the current state and the recent
/// event window. Pure — `now` is a parameter.
pub fn build_inference_prompt(
    state: &SessionState,
    events: &[EventDigest],
    now: DateTime<Utc>,
) -> String {
    let mut state_block = String::new();
    state_block.push_str(&format!(
        "project: {}\n",
        state.active_project.as_deref().unwrap_or(UNKNOWN)
    ));
    state_block.push_str(&format!(
        "app: {}\n",
        state.active_app.as_deref().unwrap_or(UNKNOWN)
    ));
    state_block.push_str(&format!(
        "window: {}\n",
        state.window_title.as_deref().unwrap_or(UNKNOWN)
    ));
    if !state.open_files.is_empty() {
        state_block.push_str(&format!("open files: {}\n", state.open_files.join(", ")));
    }
    if let Some(cmd) = &state.last_user_command {
        state_block.push_str(&format!(
            "last user command ({}): {}\n",
            render_age(now, cmd.at),
            cmd.text
        ));
    }
    if let Some(err) = &state.last_error {
        state_block.push_str(&format!(
            "last error ({}): {}\n",
            render_age(now, err.at),
            err.text
        ));
    }
    if let Some(ok) = &state.last_success {
        state_block.push_str(&format!(
            "last success ({}): {}\n",
            render_age(now, ok.at),
            ok.text
        ));
    }

    // Oldest first, newest kept: build back-to-front under the char cap so
    // a storm of long summaries drops the *oldest* lines, not the newest.
    let mut picked: Vec<String> = Vec::new();
    let mut used = 0usize;
    for ev in events.iter().rev().take(INFERENCE_EVENT_WINDOW) {
        let line = format!(
            "- [{}] {} ({})",
            render_age(now, ev.ts),
            truncate_chars(&ev.summary, 160),
            ev.event_type
        );
        let need = line.chars().count() + 1;
        if used + need > INFERENCE_EVENTS_CHAR_CAP {
            break;
        }
        used += need;
        picked.push(line);
    }
    picked.reverse();
    let events_block = if picked.is_empty() {
        "(none)".to_string()
    } else {
        picked.join("\n")
    };

    INFERENCE_PROMPT
        .replace("{{STATE}}", state_block.trim_end())
        .replace("{{EVENTS}}", &events_block)
}

/// Leniently parses an inference reply.
///
/// Accepts fenced/prose-wrapped output (the same `extract_json_object`
/// ladder triage uses), clamps `confidence` into \[0, 1\] (NaN → 0),
/// truncates goal/task to [`MAX_INFERRED_LEN`] on a char boundary, maps
/// empty/whitespace strings to `None`, and — per spec §4.8 — drops
/// goal/task to `None` when `confidence < confidence_floor` so a
/// low-confidence guess is never *stored* as fact (consumers render
/// [`UNKNOWN`]).
///
/// Returns `None` only when no JSON object could be recovered at all.
pub fn parse_inference(raw: &str, cfg: &SessionStateConfig) -> Option<InferenceResult> {
    let cleaned = crate::triage::extract_json_object(raw);
    let value: serde_json::Value = serde_json::from_str(cleaned).ok()?;
    let obj = value.as_object()?;

    let field = |key: &str, max: usize| -> Option<String> {
        let s = obj.get(key)?.as_str()?.trim();
        (!s.is_empty() && s.to_ascii_lowercase() != UNKNOWN).then(|| truncate_chars(s, max))
    };

    let raw_conf = obj
        .get("confidence")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0) as f32;
    let confidence = if raw_conf.is_nan() {
        0.0
    } else {
        raw_conf.clamp(0.0, 1.0)
    };

    let (goal, task, activity, interpretation, suggested_help) =
        if confidence < cfg.confidence_floor {
            (None, None, None, None, None)
        } else {
            (
                field("goal", MAX_INFERRED_LEN),
                field("task", MAX_INFERRED_LEN),
                field("activity", MAX_ACTIVITY_SUMMARY_LEN),
                field("interpretation", MAX_INTERPRETATION_LEN),
                field("suggested_help", MAX_SUGGESTED_HELP_LEN),
            )
        };

    Some(InferenceResult {
        goal,
        task,
        activity,
        interpretation,
        suggested_help,
        confidence,
    })
}

// ---------------------------------------------------------------------------
// Open-file extraction
// ---------------------------------------------------------------------------

/// Best-effort file name from an editor window title.
///
/// Handles the `"file — folder — app"` / `"file - folder - app"` shapes
/// (VS Code, JetBrains, most editors) plus VS Code's `●` dirty marker.
/// Returns `None` unless the leading segment actually looks like a file
/// name (a dot followed by a 1–8 char alphanumeric extension), so browser
/// and chat titles never pollute `open_files`.
pub fn file_from_window_title(title: &str) -> Option<String> {
    let head = title
        .split([
            '\u{2014}', // em dash
            '\u{2013}', // en dash
            '|',
        ])
        .next()
        .unwrap_or(title);
    // " - " (spaced hyphen) is the third separator; a bare hyphen inside a
    // file name must survive, so only the spaced form splits.
    let head = head.split(" - ").next().unwrap_or(head);
    let head = head
        .trim()
        .trim_start_matches(['\u{25cf}', '*', '\u{2022}'])
        .trim();
    if head.is_empty() || head.chars().count() > 120 {
        return None;
    }
    let (_, ext) = head.rsplit_once('.')?;
    let ok = !ext.is_empty()
        && ext.len() <= 8
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic());
    ok.then(|| head.to_string())
}

/// Pushes `path` to the front of `files`, de-duplicating and capping at
/// [`MAX_OPEN_FILES`]. Returns `true` when the list changed.
fn push_open_file(files: &mut Vec<String>, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    if files.first().map(String::as_str) == Some(path) {
        return false;
    }
    files.retain(|f| f != path);
    files.insert(0, path.to_string());
    files.truncate(MAX_OPEN_FILES);
    true
}

// ---------------------------------------------------------------------------
// Hub
// ---------------------------------------------------------------------------

/// Rolling event window + inference bookkeeping.
#[derive(Debug, Default)]
struct EventWindow {
    events: VecDeque<EventDigest>,
    significant_since_infer: usize,
    focus_switches_since_infer: usize,
    project_switched_since_infer: bool,
    last_inference_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct HubInner {
    state: RwLock<SessionState>,
    window: RwLock<EventWindow>,
    /// Bumped on every *content* change (mirrors the Task A8 live-context
    /// content-version pattern; `updated` alone moves on no-op frames).
    version: AtomicU64,
}

/// Shared handle onto the live session state (spec §4.8 hub pattern).
/// Cheap to clone; every clone reads and writes the same state.
///
/// All mutators are synchronous and lock-scoped — they are called from the
/// frame loop and from event producers, neither of which may block.
#[derive(Debug, Clone)]
pub struct SessionStateHub {
    inner: Arc<HubInner>,
}

impl Default for SessionStateHub {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStateHub {
    /// An empty hub.
    pub fn new() -> Self {
        Self::with_state(SessionState::default())
    }

    /// A hub seeded with an existing state (boot rehydration).
    pub fn with_state(state: SessionState) -> Self {
        Self {
            inner: Arc::new(HubInner {
                state: RwLock::new(state),
                window: RwLock::new(EventWindow::default()),
                version: AtomicU64::new(0),
            }),
        }
    }

    /// A snapshot clone of the current state.
    pub fn snapshot(&self) -> SessionState {
        self.inner.state.read().clone()
    }

    /// Content version — bumped only when a field other than `updated`
    /// actually changed. Publishers (Plan C) key writes on this.
    pub fn version(&self) -> u64 {
        self.inner.version.load(Ordering::Acquire)
    }

    /// Replaces the whole state (rehydration). Bumps the version.
    pub fn replace(&self, state: SessionState) {
        *self.inner.state.write() = state;
        self.inner.version.fetch_add(1, Ordering::AcqRel);
    }

    /// Runs `f` under the write lock, bumping the version when anything
    /// other than `updated` changed.
    fn mutate<R>(&self, now: DateTime<Utc>, f: impl FnOnce(&mut SessionState) -> R) -> R {
        let mut guard = self.inner.state.write();
        let before = guard.clone();
        let out = f(&mut guard);
        let mut probe = guard.clone();
        probe.updated = before.updated;
        if probe != before {
            guard.updated = now;
            drop(guard);
            self.inner.version.fetch_add(1, Ordering::AcqRel);
        }
        out
    }

    /// Mechanical per-frame update (spec §4.8): project, app, window title,
    /// and a best-effort `open_files` entry from an editor title.
    ///
    /// A `never_observe` sentinel frame renders [`PRIVATE_PLACEHOLDER`] for
    /// app and title and contributes no open file (spec §4.1 sentinel
    /// semantics).
    ///
    /// A **project change** resets `since`, clears the inferred goal/task
    /// (they described the *previous* project) and arms the project-switch
    /// inference trigger.
    pub fn apply_frame(&self, frame: &PerceptionFrame, current_project: Option<&CurrentProject>) {
        let excluded = frame.context.foreground_process_name == EXCLUDED_PROCESS;
        let project = current_project.map(|p| p.id.clone());
        let mut switched = false;

        self.mutate(frame.ts, |s| {
            // Pin guard (spec §4.13): a pinned project field is never
            // rewritten by the frame loop. Resolution is untouched — the
            // resolver still resolves, collectors still collect, events are
            // still stamped with the *real* project; only this display/
            // reasoning field is frozen at what the user asserted.
            if s.is_pinned(SessionField::Project) {
                // fall through to the mechanical app/title update below
            } else if s.active_project != project {
                // First adoption (None → Some) counts as a switch too: it
                // is the first moment there is a project to reason about.
                switched = true;
                s.active_project = project.clone();
                // Fixwave 3b (I4): a *pinned* goal/task survives the
                // switch. Clearing it here while `apply_inference` refuses
                // to write a pinned field left the value `None` forever —
                // a pin badge with no value and `task: unknown` on every
                // wake.
                s.clear_inferred_unpinned();
                s.since = frame.ts;
            }
            if excluded {
                s.active_app = Some(PRIVATE_PLACEHOLDER.to_string());
                s.window_title = Some(PRIVATE_PLACEHOLDER.to_string());
                return;
            }
            s.active_app = Some(frame.context.foreground_process_name.clone());
            s.window_title = Some(frame.context.foreground_window_title.clone());
            // Fixwave 2 (I3): a `local_only` window's title is *observed*
            // but not cloud-safe. `open_files` is rendered to the cloud
            // whenever the session itself is not tagged `local_only` (and
            // the session tag comes from the inference window, not from
            // this frame), so a title-derived path from a private window
            // must never enter it. With `redact_sensitive_titles` on the
            // title is the literal and yields no path anyway; with it off
            // it yields a real one — which is exactly the leak.
            if frame.context.is_privacy_restricted() {
                return;
            }
            if let Some(file) = file_from_window_title(&frame.context.foreground_window_title) {
                push_open_file(&mut s.open_files, &file);
            }
        });

        if switched {
            self.inner.window.write().project_switched_since_infer = true;
        }
    }

    /// Records the most recent thing the user actually asked for (spec
    /// §4.8: "`last_user_command` from voice/chat/hotkey intents + ts").
    /// `source` is logged, not stored — the spec's field is the text.
    pub fn note_user_command(&self, text: &str, source: &str, now: DateTime<Utc>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        tracing::debug!(
            layer = "context",
            component = "session_state",
            source = source,
            len = text.len(),
            "Recorded user command"
        );
        self.mutate(now, |s| {
            s.last_user_command = Some(StampedText::new(truncate_chars(text, 400), now));
        });
    }

    /// Mechanical event update (spec §4.8): `last_error`/`last_success`
    /// from classified events, `open_files` from file events under the
    /// active project. Also feeds the inference window and its counters.
    pub fn apply_event(&self, digest: EventDigest, cfg: &SessionStateConfig) {
        let ts = digest.ts;
        self.mutate(ts, |s| match digest.event_type.as_str() {
            "error" => {
                if !digest.summary.is_empty() {
                    s.last_error = Some(StampedText::new(digest.summary.clone(), ts));
                }
            }
            "success" => {
                if !digest.summary.is_empty() {
                    s.last_success = Some(StampedText::new(digest.summary.clone(), ts));
                }
            }
            t if FILE_EVENT_TYPES.contains(&t) => {
                // Only files belonging to the project we believe is
                // current — a background watcher on another project must
                // not rewrite "what you have open".
                let same_project = match (&s.active_project, &digest.project_id) {
                    (Some(a), Some(b)) => a == b,
                    (None, _) => true,
                    _ => false,
                };
                if same_project {
                    push_open_file(&mut s.open_files, &digest.summary);
                }
            }
            _ => {}
        });

        let mut window = self.inner.window.write();
        if digest.importance >= cfg.significant_importance {
            window.significant_since_infer = window.significant_since_infer.saturating_add(1);
        }
        if digest.event_type == "focus_switch" {
            window.focus_switches_since_infer = window.focus_switches_since_infer.saturating_add(1);
        }
        if window.events.len() >= EVENT_RING_CAP {
            window.events.pop_front();
        }
        window.events.push_back(digest);
    }

    /// Bridges a runtime [`crate::memory::events::ContextEvent`] into
    /// [`SessionStateHub::apply_event`]. This is the only session-state
    /// code that knows the gated event enums.
    #[cfg(feature = "runtime")]
    pub fn apply_context_event(
        &self,
        event: &crate::memory::events::ContextEvent,
        cfg: &SessionStateConfig,
    ) {
        self.apply_event(digest_from_event(event), cfg);
    }

    /// Seeds the event window (boot rehydration). Oldest first.
    pub fn seed_events(&self, digests: Vec<EventDigest>) {
        let mut window = self.inner.window.write();
        window.events.clear();
        for d in digests.into_iter().rev().take(EVENT_RING_CAP).rev() {
            window.events.push_back(d);
        }
    }

    /// The most recent `n` events, oldest first.
    pub fn recent_events(&self, n: usize) -> Vec<EventDigest> {
        let window = self.inner.window.read();
        let skip = window.events.len().saturating_sub(n);
        window.events.iter().skip(skip).cloned().collect()
    }

    /// Evaluates the spec §4.8 trigger predicate against the live
    /// bookkeeping. `idle` comes from
    /// [`crate::senses::cadence::CadenceControl::is_idle`].
    pub fn trigger(
        &self,
        cfg: &SessionStateConfig,
        now: DateTime<Utc>,
        idle: bool,
    ) -> Option<InferenceTrigger> {
        let has_content = !self.inner.state.read().is_empty();
        let window = self.inner.window.read();
        evaluate_trigger(
            &TriggerInputs {
                now,
                idle,
                has_content,
                last_inference_at: window.last_inference_at,
                project_switched: window.project_switched_since_infer,
                significant_events: window.significant_since_infer,
                focus_switches: window.focus_switches_since_infer,
            },
            cfg,
        )
    }

    /// Marks an inference *attempt*: stamps `last_inference_at` and clears
    /// the trigger counters. Called before the LLM call so a failed call
    /// costs the same cooldown as a successful one.
    pub fn mark_inference_attempt(&self, now: DateTime<Utc>) {
        let mut window = self.inner.window.write();
        window.last_inference_at = Some(now);
        window.significant_since_infer = 0;
        window.focus_switches_since_infer = 0;
        window.project_switched_since_infer = false;
    }

    /// Applies a parsed inference result. `local_only` is the §4.1
    /// propagation verdict over the window the inference ran on.
    ///
    /// Pinned goal/task fields are left alone (spec §4.13): inference may
    /// not overwrite what the user pinned.
    ///
    /// Fixwave 3b (I4): `confidence`/`local_only` are the *shared* tags on
    /// those two fields, so when **both** are pinned they describe a value
    /// this inference did not write and must not be overwritten either —
    /// otherwise a pinned task silently inherits a low-confidence run's
    /// score and drops below the render floor. When at least one field is
    /// unpinned the tags describe what just landed, as before.
    ///
    /// Fixwave 3b (I5): a write also stamps `inferred_at`, the clock the
    /// continuation resolver ages the task on.
    pub fn apply_inference(&self, result: &InferenceResult, local_only: bool, now: DateTime<Utc>) {
        self.mutate(now, |s| {
            let goal_pinned = s.is_pinned(SessionField::Goal);
            let task_pinned = s.is_pinned(SessionField::Task);
            if !goal_pinned {
                s.current_goal = result.goal.clone();
            }
            if !task_pinned {
                s.current_task = result.task.clone();
            }
            s.activity_summary = result.activity.clone();
            s.interpretation = result.interpretation.clone();
            s.suggested_help = result.suggested_help.clone();
            if goal_pinned && task_pinned {
                return;
            }
            s.confidence = result.confidence;
            s.local_only = local_only;
            s.inferred_at = Some(now);
        });
    }

    /// Applies a Context-page correction (spec §4.13): sets the field to
    /// the user's value and marks it user-confirmed.
    ///
    /// A corrected goal/task is treated as fully confident — the user just
    /// told us — so `confidence` is raised to 1.0 and `local_only` is
    /// cleared (a value the user typed into the dashboard is not derived
    /// from a private window). A corrected *project* also resets `since`
    /// and clears the inferred goal/task, exactly like a real project
    /// switch, because they described the project we were wrong about.
    pub fn apply_correction(&self, field: SessionField, value: &str, now: DateTime<Utc>) {
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        let text = truncate_chars(value, 400);
        self.mutate(now, |s| {
            match field {
                SessionField::Project => {
                    if s.active_project.as_deref() != Some(text.as_str()) {
                        // Fixwave 3b (I4): same pin guard as `apply_frame`
                        // — a project correction must not silently erase a
                        // goal/task the user pinned.
                        s.clear_inferred_unpinned();
                        s.since = now;
                    }
                    s.active_project = Some(text.clone());
                }
                SessionField::Goal => {
                    s.current_goal = Some(text.clone());
                    s.confidence = 1.0;
                    s.local_only = false;
                    // Fixwave 3b (I5): a correction establishes the value,
                    // so it restarts the continuation resolver's clock.
                    s.inferred_at = Some(now);
                }
                SessionField::Task => {
                    s.current_task = Some(text.clone());
                    s.confidence = 1.0;
                    s.local_only = false;
                    s.inferred_at = Some(now);
                }
            }
            let token = field.as_str().to_string();
            if !s.user_confirmed.contains(&token) {
                s.user_confirmed.push(token);
            }
        });
    }

    /// Sets or clears a pin (spec §4.13). `Some(value)` pins the field to
    /// that value (applying it first, as a correction would); `None`
    /// releases the pin and leaves the current value in place for the next
    /// frame/inference to move.
    pub fn set_pin(&self, field: SessionField, value: Option<&str>, now: DateTime<Utc>) {
        if let Some(value) = value {
            self.apply_correction(field, value, now);
        }
        self.mutate(now, |s| {
            let token = field.as_str().to_string();
            match value {
                Some(_) => {
                    if !s.pinned.contains(&token) {
                        s.pinned.push(token);
                    }
                }
                None => s.pinned.retain(|f| f != &token),
            }
        });
    }
}

/// A resolution must be at least this confident, for at least
/// `[projects] switch_min_secs`, before it clears a pinned project
/// (spec §4.13 "cleared when the resolver reports a different project
/// above confidence C for switch_min_secs").
///
/// `0.7` is the git-root tier: a path in the title (0.9), an editor
/// pattern (0.8) or the repository the user's files live in (0.7) may
/// override a stale pin; a bare keyword match (0.5) never may. Keeping the
/// bar at a *tier boundary* rather than an invented number means the rule
/// stays legible against the §4.3 confidence table.
pub const PIN_CLEAR_CONFIDENCE: f32 = 0.7;

/// Tracks how long the resolver has disagreed with a pinned project, so a
/// pin can expire without ever blocking resolution (spec §4.13, "no
/// pin/resolver deadlock").
///
/// Pure logic, no clock of its own: the frame loop feeds it the resolved
/// project id + confidence each frame and acts on the verdict.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ProjectPinGuard {
    /// The divergent project and when the divergence started.
    divergent: Option<(String, DateTime<Utc>)>,
}

impl ProjectPinGuard {
    /// Fresh guard.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a divergence is currently being timed (test/UI insight).
    pub fn divergent_project(&self) -> Option<&str> {
        self.divergent.as_ref().map(|(id, _)| id.as_str())
    }

    /// Forgets any divergence being timed.
    ///
    /// Fixwave 3b (I4): the frame loop only calls [`Self::observe`] while a
    /// pin exists, so an unpinned stretch left the old `divergent`
    /// timestamp frozen in place. A brand-new pin set minutes later then
    /// inherited that stale start time and could be cleared on its *first*
    /// frame — the pin the user had just asserted, gone before it was ever
    /// honoured. Call this on every frame with no pin.
    pub fn reset(&mut self) {
        self.divergent = None;
    }

    /// Feeds one frame's resolution.
    ///
    /// Returns `true` exactly once, when the pin must be cleared: the
    /// resolver has reported the *same different* project, at or above
    /// [`PIN_CLEAR_CONFIDENCE`], continuously for `switch_min`. Any frame
    /// that agrees with the pin, falls below the confidence bar, or names
    /// yet another project restarts the timer.
    pub fn observe(
        &mut self,
        pinned: &str,
        resolved: Option<(&str, f32)>,
        switch_min: Duration,
        now: DateTime<Utc>,
    ) -> bool {
        let Some((id, confidence)) = resolved else {
            self.divergent = None;
            return false;
        };
        if id == pinned || confidence < PIN_CLEAR_CONFIDENCE {
            self.divergent = None;
            return false;
        }
        let since = match &self.divergent {
            Some((seen, since)) if seen == id => *since,
            _ => {
                self.divergent = Some((id.to_string(), now));
                now
            }
        };
        if now.signed_duration_since(since) >= switch_min {
            self.divergent = None;
            return true;
        }
        false
    }
}

/// `true` when any event in `events` is `local_only` — the spec §4.1
/// propagation verdict for an inference window.
pub fn window_is_local_only(events: &[EventDigest]) -> bool {
    events.iter().any(|e| e.local_only)
}

/// The project a skills [`crate::skills::MatchContext`] should carry
/// (Task B5): the resolver's current project name when it has one,
/// otherwise session state's own project id — which, thanks to boot
/// rehydration, is populated before the resolver has seen a single frame.
///
/// Extracted as a pure function so the wiring in `bin/continuum.rs`'s
/// `compose_wake_config` is unit-testable.
pub fn match_context_project(
    current_project_name: Option<&str>,
    session: Option<&SessionState>,
) -> Option<String> {
    current_project_name
        .map(str::to_string)
        .or_else(|| session.and_then(|s| s.active_project.clone()))
}

// ---------------------------------------------------------------------------
// Rehydration
// ---------------------------------------------------------------------------

/// The confidence multiplier for a snapshot of the given age (spec §4.8:
/// "seed … with lowered confidence, staleness-discounted").
///
/// - ≤ [`REHYDRATE_FRESH_MINUTES`]: ×1.0
/// - ≤ [`REHYDRATE_VERY_STALE_HOURS`]: [`REHYDRATE_STALE_DISCOUNT`]
/// - beyond: [`REHYDRATE_VERY_STALE_DISCOUNT`]
pub fn staleness_discount(age: Duration) -> f32 {
    if age <= Duration::minutes(REHYDRATE_FRESH_MINUTES) {
        1.0
    } else if age <= Duration::hours(REHYDRATE_VERY_STALE_HOURS) {
        REHYDRATE_STALE_DISCOUNT
    } else {
        REHYDRATE_VERY_STALE_DISCOUNT
    }
}

/// Rebuilds a session state at boot from the last persisted snapshot plus
/// the most recent context events (spec §4.8 "boot rehydration").
///
/// Rules:
/// - Inferred goal/task text is **kept** even when the discounted
///   confidence falls under `confidence_floor`: it is not model garbage
///   (it cleared the floor when it was written), it is merely *old*.
///   Renderers hide it via [`SessionState::task_if_confident`]; the §4.12
///   continuation resolver still gets to rank it.
/// - A very stale snapshot additionally has its confidence capped at
///   `confidence_floor` so a restart can never out-rank live inference.
/// - `events` (oldest first) refill `last_error`/`last_success`/
///   `open_files`/`active_project` for anything the snapshot lacked or
///   that happened after it was written.
pub fn rehydrate(
    persisted: Option<SessionState>,
    events: &[EventDigest],
    now: DateTime<Utc>,
    cfg: &SessionStateConfig,
) -> SessionState {
    let mut state = SessionState {
        since: now,
        updated: now,
        ..SessionState::default()
    };

    if let Some(prev) = persisted {
        let age = now - prev.updated;
        let discount = staleness_discount(age);
        let mut confidence = (prev.confidence * discount).clamp(0.0, 1.0);
        if age > Duration::hours(REHYDRATE_VERY_STALE_HOURS) {
            confidence = confidence.min(cfg.confidence_floor);
        }
        state.active_project = prev.active_project;
        state.current_goal = prev.current_goal;
        state.current_task = prev.current_task;
        state.activity_summary = prev.activity_summary;
        state.interpretation = prev.interpretation;
        state.suggested_help = prev.suggested_help;
        state.open_files = prev.open_files;
        state.last_error = prev.last_error;
        state.last_success = prev.last_success;
        state.last_user_command = prev.last_user_command;
        state.local_only = prev.local_only;
        state.confidence = confidence;
        // Fixwave 3b (minor): pins and corrections are user assertions —
        // a restart is not a reason to forget them. Dropping `pinned` here
        // meant the live state came back unpinned while `session_pins`
        // still held the row, so the very first frame overwrote the value
        // the user had frozen.
        state.pinned = prev.pinned;
        state.user_confirmed = prev.user_confirmed;
        // I5: the goal/task text is carried over, so its clock must be
        // too — otherwise the continuation resolver would treat a
        // rehydrated task as having no age at all.
        state.inferred_at = prev.inferred_at;
        // The *app/window* are live facts; a restart invalidates them
        // (they are re-set by the first frame, milliseconds later).
        state.since = if age <= Duration::minutes(REHYDRATE_FRESH_MINUTES) {
            prev.since
        } else {
            now
        };
    }

    for ev in events {
        match ev.event_type.as_str() {
            "error" if !ev.summary.is_empty() => {
                if state.last_error.as_ref().is_none_or(|e| e.at <= ev.ts) {
                    state.last_error = Some(StampedText::new(ev.summary.clone(), ev.ts));
                }
            }
            "success" if !ev.summary.is_empty() => {
                if state.last_success.as_ref().is_none_or(|e| e.at <= ev.ts) {
                    state.last_success = Some(StampedText::new(ev.summary.clone(), ev.ts));
                }
            }
            t if FILE_EVENT_TYPES.contains(&t) => {
                push_open_file(&mut state.open_files, &ev.summary);
            }
            _ => {}
        }
        if state.active_project.is_none() {
            state.active_project.clone_from(&ev.project_id);
        }
    }

    state
}

/// Runtime bridge: converts a live `ContextEvent` into an [`EventDigest`].
#[cfg(feature = "runtime")]
pub fn digest_from_event(event: &crate::memory::events::ContextEvent) -> EventDigest {
    use crate::memory::events::EventSensitivity;
    EventDigest {
        ts: event.ts,
        source: serde_json::to_value(event.source)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        event_type: serde_json::to_value(event.event_type)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        application: event.application.clone(),
        project_id: event.project_id.clone(),
        summary: event.summary.clone(),
        importance: event.importance,
        local_only: matches!(event.sensitivity, EventSensitivity::LocalOnly),
    }
}

/// Runtime bridge: converts a persisted `context_events` row into an
/// [`EventDigest`].
#[cfg(feature = "runtime")]
pub fn digest_from_row(row: &crate::memory::raw_log::ContextEventRow) -> EventDigest {
    use crate::memory::events::EventSensitivity;
    EventDigest {
        ts: row.ts_last,
        source: serde_json::to_value(row.source)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        event_type: serde_json::to_value(row.event_type)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        application: row.application.clone(),
        project_id: row.project_id.clone(),
        summary: row.summary.clone(),
        importance: row.importance,
        local_only: matches!(row.sensitivity, EventSensitivity::LocalOnly),
    }
}

/// Reads the last published `state.json` and pulls its `session_state`
/// object out, if present.
///
/// Deliberately lenient and key-based rather than typed against
/// `RuntimeSnapshot`: **publishing session state is Plan C's job** (Task
/// C1), so today's `state.json` has no such key and this returns `None`.
/// Once C1 publishes it, boot rehydration starts working with no change
/// here.
pub fn read_persisted_state(dev_dir: &std::path::Path) -> Option<SessionState> {
    let raw = std::fs::read_to_string(dev_dir.join("state.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let node = value.get("session_state")?;
    match serde_json::from_value::<SessionState>(node.clone()) {
        Ok(state) => Some(state),
        Err(e) => {
            tracing::debug!(
                layer = "context",
                component = "session_state",
                error = %e,
                "state.json session_state present but unparseable; ignoring"
            );
            None
        }
    }
}

/// Boot rehydration against real storage (spec §4.8).
///
/// Reads the persisted `state.json` snapshot and the last
/// [`REHYDRATE_LOOKBACK_MINUTES`] of `context_events`, and returns the
/// seeded state plus the event digests the caller should hand to
/// [`SessionStateHub::seed_events`]. Never fails: a missing/unreadable
/// snapshot or a DB error degrades to "start empty", which is exactly the
/// pre-B5 behavior.
#[cfg(feature = "runtime")]
pub async fn rehydrate_from_disk(
    dev_dir: &std::path::Path,
    raw_log: &crate::memory::raw_log::RawLog,
    cfg: &SessionStateConfig,
    now: DateTime<Utc>,
) -> (SessionState, Vec<EventDigest>) {
    let persisted = read_persisted_state(dev_dir);
    let since = now - Duration::minutes(REHYDRATE_LOOKBACK_MINUTES);
    let digests = match raw_log
        .recent_context_events(since, EVENT_RING_CAP * 2)
        .await
    {
        Ok(rows) => rows.iter().map(digest_from_row).collect::<Vec<_>>(),
        Err(e) => {
            tracing::warn!(
                layer = "context",
                component = "session_state",
                error = %e,
                "context_events read failed during rehydration; seeding from snapshot only"
            );
            Vec::new()
        }
    };
    let state = rehydrate(persisted, &digests, now, cfg);
    tracing::info!(
        layer = "context",
        component = "session_state",
        project = ?state.active_project,
        task = ?state.current_task,
        confidence = state.confidence,
        events = digests.len(),
        "Session state rehydrated"
    );
    (state, digests)
}

// ---------------------------------------------------------------------------
// Inference task
// ---------------------------------------------------------------------------

/// Spawns the session-state inference task (spec §4.8: "goal/task
/// inference: event-driven, **own spawned task** — never awaited in the
/// frame loop").
///
/// The task ticks every [`INFERENCE_TICK_SECS`] and evaluates
/// [`SessionStateHub::trigger`]; almost every tick answers "no" and costs
/// nothing. When a trigger fires it marks the attempt (starting the
/// cooldown even if the call fails), renders the prompt, and calls
/// `llm.complete` — which for the production [`crate::triage::llm::TriageLayer`]
/// goes through `LlmGate::acquire_background` and clamps `max_tokens` to
/// [`crate::llm_gate::BACKGROUND_MAX_TOKENS`] (Task B2). Interactive triage
/// therefore always outranks it, and a gate timeout is just a skipped pass.
pub fn spawn_inference_task(
    hub: SessionStateHub,
    llm: Arc<dyn crate::curator::CuratorLlm>,
    cfg: SessionStateConfig,
    cadence: crate::senses::cadence::CadenceControl,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_secs(INFERENCE_TICK_SECS.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let now = Utc::now();
                    // Spec §4.11: session inference pauses while idle.
                    let Some(trigger) = hub.trigger(&cfg, now, cadence.is_idle()) else {
                        continue;
                    };
                    hub.mark_inference_attempt(now);
                    let state = hub.snapshot();
                    let events = hub.recent_events(INFERENCE_EVENT_WINDOW);
                    let local_only = window_is_local_only(&events);
                    let prompt = build_inference_prompt(&state, &events, now);
                    match llm.complete(&prompt, cfg.infer_max_tokens).await {
                        Ok(raw) => match parse_inference(&raw, &cfg) {
                            Some(result) => {
                                tracing::info!(
                                    layer = "context",
                                    component = "session_state",
                                    trigger = trigger.label(),
                                    confidence = result.confidence,
                                    local_only = local_only,
                                    inferred = result.goal.is_some()
                                        || result.task.is_some()
                                        || result.activity.is_some()
                                        || result.interpretation.is_some()
                                        || result.suggested_help.is_some(),
                                    "Session-state inference applied"
                                );
                                hub.apply_inference(&result, local_only, Utc::now());
                            }
                            None => tracing::warn!(
                                layer = "context",
                                component = "session_state",
                                trigger = trigger.label(),
                                "Session-state inference reply was not JSON; keeping previous state"
                            ),
                        },
                        Err(e) => tracing::debug!(
                            layer = "context",
                            component = "session_state",
                            trigger = trigger.label(),
                            error = %e,
                            "Session-state inference skipped (LLM unavailable or gate busy)"
                        ),
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
        tracing::debug!(
            layer = "context",
            component = "session_state",
            "Session-state inference task stopped"
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senses::types::{ContextObservation, ScreenObservation};
    use chrono::TimeZone;

    fn cfg() -> SessionStateConfig {
        SessionStateConfig::default()
    }

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 5, 9, 0, 0).unwrap()
    }

    fn frame(process: &str, title: &str, ts: DateTime<Utc>) -> PerceptionFrame {
        PerceptionFrame {
            id: uuid::Uuid::new_v4(),
            ts,
            screen: ScreenObservation {
                description: String::new(),
                world_compact: None,
                foreground_app: process.to_string(),
                has_error_visible: false,
                confidence: 0.0,
                screenshot_path: None,
                ts,
            },
            audio: None,
            context: ContextObservation {
                foreground_window_title: title.to_string(),
                foreground_process_name: process.to_string(),
                idle_seconds: 0,
                in_call: false,
                pid: None,
                exe_path: None,
                active_since_secs: 0,
                monitor_id: None,
                privacy: None,
                ts,
            },
            salience_hint: 0.0,
        }
    }

    fn project(id: &str) -> CurrentProject {
        CurrentProject {
            id: id.to_string(),
            name: id.to_string(),
            root_path: None,
            confidence: 0.9,
            source_tier: 1,
            zone: None,
            status: crate::context::project::ProjectStatus::Configured,
        }
    }

    fn digest(event_type: &str, summary: &str, ts: DateTime<Utc>) -> EventDigest {
        EventDigest {
            ts,
            source: "screen".to_string(),
            event_type: event_type.to_string(),
            application: "Code.exe".to_string(),
            project_id: Some("continuum".to_string()),
            summary: summary.to_string(),
            importance: 0.8,
            local_only: false,
        }
    }

    // --- mechanical updates -------------------------------------------

    #[test]
    fn apply_frame_sets_project_app_and_title() {
        let hub = SessionStateHub::new();
        hub.apply_frame(
            &frame("Code.exe", "main.rs — continuum — VS Code", t0()),
            Some(&project("continuum")),
        );
        let s = hub.snapshot();
        assert_eq!(s.active_project.as_deref(), Some("continuum"));
        assert_eq!(s.active_app.as_deref(), Some("Code.exe"));
        assert_eq!(
            s.window_title.as_deref(),
            Some("main.rs — continuum — VS Code")
        );
        assert_eq!(s.open_files, vec!["main.rs".to_string()]);
        assert_eq!(s.updated, t0());
    }

    #[test]
    fn apply_frame_excluded_sentinel_renders_private_and_adds_no_files() {
        let hub = SessionStateHub::new();
        hub.apply_frame(&frame(EXCLUDED_PROCESS, "", t0()), None);
        let s = hub.snapshot();
        assert_eq!(s.active_app.as_deref(), Some(PRIVATE_PLACEHOLDER));
        assert_eq!(s.window_title.as_deref(), Some(PRIVATE_PLACEHOLDER));
        assert!(s.open_files.is_empty());
    }

    /// Fixwave 2 (I3): with `[context].redact_sensitive_titles = false` a
    /// `local_only` window keeps its real title, so
    /// `file_from_window_title` happily derives a path from it. `open_files`
    /// is cloud-rendered whenever the *session* is not itself tagged
    /// `local_only` — and the session tag comes from the inference window,
    /// not from this frame — so the path must never enter the list.
    #[test]
    fn apply_frame_adds_no_open_files_from_a_local_only_window() {
        let hub = SessionStateHub::new();
        let mut f = frame("Code.exe", "severance-model.xlsx — Private — Excel", t0());
        f.context.privacy = Some(crate::senses::live_context::PrivacyDisposition::Redacted);
        hub.apply_frame(&f, Some(&project("continuum")));

        let s = hub.snapshot();
        assert!(
            s.open_files.is_empty(),
            "a local_only window title must not seed open_files: {:?}",
            s.open_files
        );
        assert!(!s.local_only, "the frame does not tag the session itself");

        // The cloud-safe sibling still works exactly as before.
        let mut ok = frame("Code.exe", "main.rs — continuum — VS Code", t0());
        ok.context.privacy = Some(crate::senses::live_context::PrivacyDisposition::Visible);
        hub.apply_frame(&ok, Some(&project("continuum")));
        assert_eq!(hub.snapshot().open_files, vec!["main.rs".to_string()]);
    }

    #[test]
    fn apply_frame_project_change_clears_inferred_fields_and_arms_trigger() {
        let hub = SessionStateHub::new();
        hub.apply_frame(
            &frame("Code.exe", "a.rs", t0()),
            Some(&project("continuum")),
        );
        hub.apply_inference(
            &InferenceResult {
                goal: Some("ship B5".to_string()),
                task: Some("write tests".to_string()),
                confidence: 0.8,
                ..InferenceResult::default()
            },
            false,
            t0(),
        );
        hub.mark_inference_attempt(t0());
        assert!(hub.snapshot().current_task.is_some());

        let t1 = t0() + Duration::minutes(5);
        hub.apply_frame(&frame("Code.exe", "b.rs", t1), Some(&project("simcharts")));
        let s = hub.snapshot();
        assert_eq!(s.active_project.as_deref(), Some("simcharts"));
        assert_eq!(s.current_goal, None);
        assert_eq!(s.current_task, None);
        assert_eq!(s.confidence, 0.0);
        assert_eq!(s.since, t1);
        assert_eq!(
            hub.trigger(&cfg(), t1 + Duration::minutes(3), false),
            Some(InferenceTrigger::ProjectSwitch)
        );
    }

    #[test]
    fn version_bumps_only_on_content_change() {
        let hub = SessionStateHub::new();
        let f = frame("Code.exe", "a.rs — continuum", t0());
        hub.apply_frame(&f, None);
        let v1 = hub.version();
        assert!(v1 > 0);
        // Same facts, later timestamp: `updated` alone must not bump.
        let mut f2 = f.clone();
        f2.ts = t0() + Duration::seconds(30);
        f2.context.ts = f2.ts;
        hub.apply_frame(&f2, None);
        assert_eq!(hub.version(), v1);
        assert_eq!(hub.snapshot().updated, t0());
    }

    #[test]
    fn apply_event_records_last_error_and_success() {
        let hub = SessionStateHub::new();
        hub.apply_event(digest("error", "cargo build failed", t0()), &cfg());
        hub.apply_event(
            digest("success", "tests green", t0() + Duration::minutes(1)),
            &cfg(),
        );
        let s = hub.snapshot();
        assert_eq!(s.last_error.as_ref().unwrap().text, "cargo build failed");
        assert_eq!(s.last_error.as_ref().unwrap().at, t0());
        assert_eq!(s.last_success.as_ref().unwrap().text, "tests green");
    }

    #[test]
    fn apply_event_ignores_empty_summaries() {
        let hub = SessionStateHub::new();
        hub.apply_event(digest("error", "", t0()), &cfg());
        assert!(hub.snapshot().last_error.is_none());
    }

    #[test]
    fn apply_event_file_events_dedupe_and_cap_open_files() {
        let hub = SessionStateHub::new();
        hub.apply_frame(
            &frame("Code.exe", "no-file-here", t0()),
            Some(&project("continuum")),
        );
        for i in 0..12 {
            let mut d = digest("file_modified", &format!("src/f{i}.rs"), t0());
            d.importance = 0.1;
            hub.apply_event(d, &cfg());
        }
        // Re-touch an older file: it moves to the front, no duplicate.
        hub.apply_event(digest("file_modified", "src/f11.rs", t0()), &cfg());
        let files = hub.snapshot().open_files;
        assert_eq!(files.len(), MAX_OPEN_FILES);
        assert_eq!(files[0], "src/f11.rs");
        let mut sorted = files.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), files.len(), "open_files must be deduped");
    }

    #[test]
    fn apply_event_file_event_from_another_project_is_ignored() {
        let hub = SessionStateHub::new();
        hub.apply_frame(&frame("Code.exe", "x", t0()), Some(&project("continuum")));
        let mut d = digest("file_modified", "other/thing.rs", t0());
        d.project_id = Some("simcharts".to_string());
        hub.apply_event(d, &cfg());
        assert!(hub.snapshot().open_files.is_empty());
    }

    #[test]
    fn note_user_command_stores_text_and_ts_and_skips_empty() {
        let hub = SessionStateHub::new();
        hub.note_user_command("   ", "voice", t0());
        assert!(hub.snapshot().last_user_command.is_none());
        hub.note_user_command("ga door", "voice", t0());
        let cmd = hub.snapshot().last_user_command.unwrap();
        assert_eq!(cmd.text, "ga door");
        assert_eq!(cmd.at, t0());
    }

    #[test]
    fn file_from_window_title_matrix() {
        assert_eq!(
            file_from_window_title("main.rs — continuum — Visual Studio Code").as_deref(),
            Some("main.rs")
        );
        assert_eq!(
            file_from_window_title("● session_state.rs - Continuum - VS Code").as_deref(),
            Some("session_state.rs")
        );
        assert_eq!(
            file_from_window_title("plan-b.md | Continuum").as_deref(),
            Some("plan-b.md")
        );
        // Not files.
        assert_eq!(file_from_window_title("Inbox — Outlook"), None);
        assert_eq!(file_from_window_title("github.com/anthropics"), None);
        assert_eq!(file_from_window_title(""), None);
        // A hyphenated file name must survive (only " - " splits).
        assert_eq!(
            file_from_window_title("my-file.rs").as_deref(),
            Some("my-file.rs")
        );
    }

    // --- trigger predicate --------------------------------------------

    fn inputs(now: DateTime<Utc>) -> TriggerInputs {
        TriggerInputs {
            now,
            idle: false,
            has_content: true,
            last_inference_at: None,
            project_switched: false,
            significant_events: 0,
            focus_switches: 0,
        }
    }

    #[test]
    fn trigger_fires_stale_when_never_inferred() {
        assert_eq!(
            evaluate_trigger(&inputs(t0()), &cfg()),
            Some(InferenceTrigger::Stale)
        );
    }

    #[test]
    fn trigger_suppressed_while_idle() {
        let mut i = inputs(t0());
        i.idle = true;
        i.project_switched = true;
        assert_eq!(evaluate_trigger(&i, &cfg()), None);
    }

    #[test]
    fn trigger_suppressed_when_state_is_empty() {
        let mut i = inputs(t0());
        i.has_content = false;
        assert_eq!(evaluate_trigger(&i, &cfg()), None);
    }

    #[test]
    fn trigger_suppressed_inside_min_interval() {
        let c = cfg();
        let mut i = inputs(t0() + Duration::seconds(c.infer_min_interval_secs as i64 - 1));
        i.last_inference_at = Some(t0());
        i.project_switched = true;
        i.significant_events = 100;
        assert_eq!(evaluate_trigger(&i, &c), None);
    }

    #[test]
    fn trigger_project_switch_outranks_volume_and_staleness() {
        let c = cfg();
        let mut i = inputs(t0() + Duration::hours(1));
        i.last_inference_at = Some(t0());
        i.project_switched = true;
        i.significant_events = 100;
        assert_eq!(
            evaluate_trigger(&i, &c),
            Some(InferenceTrigger::ProjectSwitch)
        );
    }

    #[test]
    fn trigger_event_volume_at_threshold_only() {
        let c = cfg();
        let mut i = inputs(t0() + Duration::seconds(c.infer_min_interval_secs as i64 + 1));
        i.last_inference_at = Some(t0());
        i.significant_events = c.infer_min_new_events - 1;
        assert_eq!(evaluate_trigger(&i, &c), None);
        i.significant_events = c.infer_min_new_events;
        assert_eq!(
            evaluate_trigger(&i, &c),
            Some(InferenceTrigger::EventVolume)
        );
    }

    #[test]
    fn trigger_activity_sequence_after_short_app_loop() {
        let c = cfg();
        let mut i = inputs(t0() + Duration::seconds(c.infer_min_interval_secs as i64 + 1));
        i.last_inference_at = Some(t0());
        i.focus_switches = c.infer_min_focus_switches - 1;
        assert_eq!(evaluate_trigger(&i, &c), None);
        i.focus_switches = c.infer_min_focus_switches;
        assert_eq!(
            evaluate_trigger(&i, &c),
            Some(InferenceTrigger::ActivitySequence)
        );
    }

    #[test]
    fn trigger_staleness_at_max_age() {
        let c = cfg();
        let mut i = inputs(t0() + Duration::minutes(c.infer_max_age_minutes as i64));
        i.last_inference_at = Some(t0());
        assert_eq!(evaluate_trigger(&i, &c), Some(InferenceTrigger::Stale));
    }

    #[test]
    fn hub_counts_only_significant_events_toward_the_volume_trigger() {
        let c = cfg();
        let hub = SessionStateHub::new();
        hub.apply_frame(&frame("Code.exe", "x", t0()), Some(&project("continuum")));
        hub.mark_inference_attempt(t0());
        for _ in 0..20 {
            let mut d = digest("routine", "nothing", t0());
            d.importance = c.significant_importance - 0.01;
            hub.apply_event(d, &c);
        }
        let later = t0() + Duration::seconds(c.infer_min_interval_secs as i64 + 1);
        assert_eq!(hub.trigger(&c, later, false), None);
        for _ in 0..c.infer_min_new_events {
            hub.apply_event(digest("error", "boom", t0()), &c);
        }
        assert_eq!(
            hub.trigger(&c, later, false),
            Some(InferenceTrigger::EventVolume)
        );
    }

    #[test]
    fn hub_counts_low_importance_focus_switches_as_an_activity_sequence() {
        let c = cfg();
        let hub = SessionStateHub::new();
        hub.apply_frame(
            &frame("Code.exe", "main.rs", t0()),
            Some(&project("continuum")),
        );
        hub.mark_inference_attempt(t0());
        for app in ["Brave", "ChatGPT"] {
            let mut switch = digest("focus_switch", app, t0());
            switch.importance = 0.0;
            hub.apply_event(switch, &c);
        }
        let later = t0() + Duration::seconds(c.infer_min_interval_secs as i64 + 1);
        assert_eq!(
            hub.trigger(&c, later, false),
            Some(InferenceTrigger::ActivitySequence)
        );
    }

    #[test]
    fn mark_inference_attempt_clears_counters() {
        let c = cfg();
        let hub = SessionStateHub::new();
        hub.apply_frame(&frame("Code.exe", "x", t0()), Some(&project("continuum")));
        for _ in 0..c.infer_min_new_events {
            hub.apply_event(digest("error", "boom", t0()), &c);
        }
        hub.mark_inference_attempt(t0());
        let later = t0() + Duration::seconds(c.infer_min_interval_secs as i64 + 1);
        assert_eq!(hub.trigger(&c, later, false), None);
    }

    // --- inference parse ----------------------------------------------

    #[test]
    fn parse_inference_happy_path() {
        let r = parse_inference(
            r#"{"goal":"ship the context engine","task":"write B5 tests","activity":"Edited session_state.rs, then searched the compiler error in Brave","interpretation":"The user is debugging the new trigger","suggested_help":"Inspect and fix the failing test","confidence":0.8}"#,
            &cfg(),
        )
        .unwrap();
        assert_eq!(r.goal.as_deref(), Some("ship the context engine"));
        assert_eq!(r.task.as_deref(), Some("write B5 tests"));
        assert_eq!(
            r.activity.as_deref(),
            Some("Edited session_state.rs, then searched the compiler error in Brave")
        );
        assert_eq!(
            r.interpretation.as_deref(),
            Some("The user is debugging the new trigger")
        );
        assert_eq!(
            r.suggested_help.as_deref(),
            Some("Inspect and fix the failing test")
        );
        assert!((r.confidence - 0.8).abs() < 1e-6);
    }

    #[test]
    fn parse_inference_tolerates_fence_and_prose() {
        let raw = "Sure!\n```json\n{\"goal\":\"g\",\"task\":\"t\",\"confidence\":0.9}\n```";
        let r = parse_inference(raw, &cfg()).unwrap();
        assert_eq!(r.task.as_deref(), Some("t"));
    }

    #[test]
    fn parse_inference_below_floor_drops_all_inferred_claims() {
        let c = cfg();
        let r = parse_inference(
            r#"{"goal":"guessing","task":"guessing","activity":"maybe editing","interpretation":"maybe stuck","suggested_help":"maybe click","confidence":0.1}"#,
            &c,
        )
        .unwrap();
        assert_eq!(r.goal, None);
        assert_eq!(r.task, None);
        assert_eq!(r.activity, None);
        assert_eq!(r.interpretation, None);
        assert_eq!(r.suggested_help, None);
        assert!((r.confidence - 0.1).abs() < 1e-6);
    }

    #[test]
    fn parse_inference_clamps_and_truncates() {
        let long = "x".repeat(400);
        let raw = format!(r#"{{"goal":"{long}","task":"t","confidence":5}}"#);
        let r = parse_inference(&raw, &cfg()).unwrap();
        assert_eq!(r.confidence, 1.0);
        assert_eq!(r.goal.as_ref().unwrap().chars().count(), MAX_INFERRED_LEN);

        let neg = parse_inference(r#"{"goal":"g","task":"t","confidence":-3}"#, &cfg()).unwrap();
        assert_eq!(neg.confidence, 0.0);
        assert_eq!(neg.goal, None);
    }

    #[test]
    fn parse_inference_empty_and_unknown_strings_become_none() {
        let r =
            parse_inference(r#"{"goal":"  ","task":"unknown","confidence":0.9}"#, &cfg()).unwrap();
        assert_eq!(r.goal, None);
        assert_eq!(r.task, None);
        assert!((r.confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn parse_inference_rejects_non_json() {
        assert_eq!(parse_inference("I could not tell.", &cfg()), None);
        assert_eq!(parse_inference("", &cfg()), None);
        assert_eq!(parse_inference("[1,2,3]", &cfg()), None);
    }

    #[test]
    fn parse_inference_missing_confidence_defaults_to_zero_and_drops_fields() {
        let r = parse_inference(r#"{"goal":"g","task":"t"}"#, &cfg()).unwrap();
        assert_eq!(r.confidence, 0.0);
        assert_eq!(r.task, None);
    }

    #[test]
    fn apply_inference_propagates_local_only_from_the_window() {
        let hub = SessionStateHub::new();
        let c = cfg();
        let mut d = digest("error", "secret failed", t0());
        d.local_only = true;
        hub.apply_event(d, &c);
        hub.apply_event(digest("success", "ok", t0()), &c);
        let events = hub.recent_events(INFERENCE_EVENT_WINDOW);
        assert!(window_is_local_only(&events));
        hub.apply_inference(
            &InferenceResult {
                goal: Some("g".to_string()),
                task: Some("t".to_string()),
                activity: Some("edited a private customer record".to_string()),
                interpretation: Some("preparing a private release".to_string()),
                suggested_help: Some("fix the private deployment".to_string()),
                confidence: 0.9,
            },
            window_is_local_only(&events),
            t0(),
        );
        let s = hub.snapshot();
        assert!(s.local_only);
        let cloud = s.cloud_view();
        assert_eq!(cloud.current_goal.as_deref(), Some(PRIVATE_CONTEXT_PHRASE));
        assert_eq!(cloud.current_task.as_deref(), Some(PRIVATE_CONTEXT_PHRASE));
        assert_eq!(
            cloud.activity_summary.as_deref(),
            Some(PRIVATE_CONTEXT_PHRASE)
        );
        assert_eq!(
            cloud.interpretation.as_deref(),
            Some(PRIVATE_CONTEXT_PHRASE)
        );
        assert_eq!(
            cloud.suggested_help.as_deref(),
            Some(PRIVATE_CONTEXT_PHRASE)
        );
        // The local view is untouched — local models may see it (§4.1).
        assert_eq!(s.current_task.as_deref(), Some("t"));
    }

    #[test]
    fn cloud_view_is_a_no_op_when_not_local_only() {
        let s = SessionState {
            current_task: Some("t".to_string()),
            ..SessionState::default()
        };
        assert_eq!(s.cloud_view(), s);
    }

    #[test]
    fn recent_events_keeps_the_newest_and_ring_is_bounded() {
        let hub = SessionStateHub::new();
        let c = cfg();
        for i in 0..(EVENT_RING_CAP + 10) {
            hub.apply_event(
                digest(
                    "routine",
                    &format!("e{i}"),
                    t0() + Duration::seconds(i as i64),
                ),
                &c,
            );
        }
        let all = hub.recent_events(usize::MAX);
        assert_eq!(all.len(), EVENT_RING_CAP);
        assert_eq!(
            all.last().unwrap().summary,
            format!("e{}", EVENT_RING_CAP + 9)
        );
        let window = hub.recent_events(5);
        assert_eq!(window.len(), 5);
        assert_eq!(window[0].summary, format!("e{}", EVENT_RING_CAP + 5));
    }

    #[test]
    fn build_inference_prompt_substitutes_and_caps_the_event_block() {
        let state = SessionState {
            active_project: Some("continuum".to_string()),
            active_app: Some("Code.exe".to_string()),
            window_title: Some("main.rs".to_string()),
            ..SessionState::default()
        };
        let events: Vec<EventDigest> = (0..80)
            .map(|i| {
                digest(
                    "error",
                    &"long summary ".repeat(20),
                    t0() + Duration::seconds(i),
                )
            })
            .collect();
        let prompt = build_inference_prompt(&state, &events, t0() + Duration::minutes(5));
        assert!(!prompt.contains("{{STATE}}"));
        assert!(!prompt.contains("{{EVENTS}}"));
        assert!(prompt.contains("project: continuum"));
        assert!(prompt.chars().count() < INFERENCE_PROMPT.chars().count() + 3_000);
    }

    #[test]
    fn build_inference_prompt_with_no_events_says_none() {
        let prompt = build_inference_prompt(&SessionState::default(), &[], t0());
        assert!(prompt.contains("(none)"));
    }

    // --- memory_summary render ----------------------------------------

    #[test]
    fn memory_summary_renders_unknown_below_floor() {
        let c = cfg();
        let mut s = SessionState {
            active_project: Some("continuum".to_string()),
            active_app: Some("Code.exe".to_string()),
            current_goal: Some("stale goal".to_string()),
            current_task: Some("stale task".to_string()),
            confidence: c.confidence_floor - 0.1,
            ..SessionState::default()
        };
        let out = s.render_memory_summary(t0(), MEMORY_SUMMARY_MAX_CHARS, c.confidence_floor);
        assert!(out.contains("goal: unknown"), "{out}");
        assert!(out.contains("task: unknown"), "{out}");

        s.confidence = c.confidence_floor;
        let out = s.render_memory_summary(t0(), MEMORY_SUMMARY_MAX_CHARS, c.confidence_floor);
        assert!(out.contains("task: stale task"), "{out}");
    }

    #[test]
    fn memory_summary_respects_the_600_char_cap() {
        let c = cfg();
        let s = SessionState {
            active_project: Some("continuum".to_string()),
            active_app: Some("Code.exe".to_string()),
            current_goal: Some("g".repeat(120)),
            current_task: Some("t".repeat(120)),
            confidence: 1.0,
            last_error: Some(StampedText::new("e".repeat(200), t0())),
            last_success: Some(StampedText::new("s".repeat(200), t0())),
            last_user_command: Some(StampedText::new("c".repeat(200), t0())),
            open_files: (0..MAX_OPEN_FILES)
                .map(|i| format!("very/long/path/to/file-{i}.rs"))
                .collect(),
            ..SessionState::default()
        };
        let out = s.render_memory_summary(
            t0() + Duration::minutes(5),
            MEMORY_SUMMARY_MAX_CHARS,
            c.confidence_floor,
        );
        assert!(
            out.chars().count() <= MEMORY_SUMMARY_MAX_CHARS,
            "rendered {} chars",
            out.chars().count()
        );
        // The head lines always survive; the tail is what gets dropped.
        assert!(out.starts_with("project: continuum | app: Code.exe"));
    }

    #[test]
    fn memory_summary_is_char_boundary_safe() {
        let s = SessionState {
            active_project: Some("é".repeat(500)),
            ..SessionState::default()
        };
        let out = s.render_memory_summary(t0(), 40, 0.4);
        assert!(out.chars().count() <= 40);
    }

    #[test]
    fn memory_summary_of_an_empty_state_is_all_unknown() {
        let out = SessionState::default().render_memory_summary(t0(), 600, 0.4);
        assert_eq!(
            out,
            "project: unknown | app: unknown\ngoal: unknown\ntask: unknown"
        );
    }

    // --- rehydration ---------------------------------------------------

    fn persisted(confidence: f32, updated: DateTime<Utc>) -> SessionState {
        SessionState {
            active_project: Some("continuum".to_string()),
            current_goal: Some("ship the context engine".to_string()),
            current_task: Some("finish B5".to_string()),
            activity_summary: Some("Editing session state tests".to_string()),
            interpretation: Some("Finishing the context engine".to_string()),
            suggested_help: Some("Run the focused test suite".to_string()),
            active_app: Some("Code.exe".to_string()),
            window_title: Some("main.rs".to_string()),
            open_files: vec!["main.rs".to_string()],
            last_error: None,
            last_success: None,
            last_user_command: Some(StampedText::new("ga door", updated)),
            pinned: Vec::new(),
            user_confirmed: Vec::new(),
            confidence,
            local_only: false,
            since: updated - Duration::hours(1),
            updated,
            inferred_at: Some(updated - Duration::minutes(30)),
        }
    }

    /// Fixwave 3b (minor): a restart is not a reason to forget what the
    /// user pinned or corrected — and the goal/task text is carried over,
    /// so its `inferred_at` clock must come with it.
    #[test]
    fn rehydrate_keeps_pins_confirmations_and_the_inference_clock() {
        let now = t0();
        let updated = now - Duration::minutes(10);
        let mut prev = persisted(0.8, updated);
        prev.pinned = vec!["task".to_string()];
        prev.user_confirmed = vec!["goal".to_string()];
        let s = rehydrate(Some(prev), &[], now, &cfg());
        assert!(s.is_pinned(SessionField::Task));
        assert!(s.is_user_confirmed(SessionField::Goal));
        assert_eq!(s.inferred_at, Some(updated - Duration::minutes(30)));
    }

    #[test]
    fn rehydrate_fresh_snapshot_keeps_confidence() {
        let now = t0();
        let s = rehydrate(
            Some(persisted(0.8, now - Duration::minutes(10))),
            &[],
            now,
            &cfg(),
        );
        assert!((s.confidence - 0.8).abs() < 1e-6);
        assert_eq!(s.current_task.as_deref(), Some("finish B5"));
        assert_eq!(s.active_project.as_deref(), Some("continuum"));
        // Live facts are not rehydrated — the next frame sets them.
        assert_eq!(s.active_app, None);
        assert_eq!(s.window_title, None);
    }

    #[test]
    fn rehydrate_stale_snapshot_halves_confidence() {
        let now = t0();
        let s = rehydrate(
            Some(persisted(0.8, now - Duration::hours(2))),
            &[],
            now,
            &cfg(),
        );
        assert!((s.confidence - 0.4).abs() < 1e-6);
        assert_eq!(s.current_task.as_deref(), Some("finish B5"));
        assert_eq!(s.since, now);
    }

    #[test]
    fn rehydrate_very_stale_snapshot_quarter_discounts_and_caps_at_floor() {
        let c = cfg();
        let now = t0();
        let s = rehydrate(Some(persisted(1.0, now - Duration::hours(9))), &[], now, &c);
        assert!((s.confidence - REHYDRATE_VERY_STALE_DISCOUNT).abs() < 1e-6);
        assert!(s.confidence <= c.confidence_floor);
        // The text survives (§4.12 ranks it), but renderers hide it.
        assert_eq!(s.current_task.as_deref(), Some("finish B5"));
        assert_eq!(s.task_if_confident(c.confidence_floor), None);
    }

    #[test]
    fn staleness_discount_boundaries() {
        assert_eq!(staleness_discount(Duration::minutes(0)), 1.0);
        assert_eq!(
            staleness_discount(Duration::minutes(REHYDRATE_FRESH_MINUTES)),
            1.0
        );
        assert_eq!(
            staleness_discount(Duration::minutes(REHYDRATE_FRESH_MINUTES + 1)),
            REHYDRATE_STALE_DISCOUNT
        );
        assert_eq!(
            staleness_discount(Duration::hours(REHYDRATE_VERY_STALE_HOURS)),
            REHYDRATE_STALE_DISCOUNT
        );
        assert_eq!(
            staleness_discount(Duration::hours(REHYDRATE_VERY_STALE_HOURS) + Duration::seconds(1)),
            REHYDRATE_VERY_STALE_DISCOUNT
        );
    }

    #[test]
    fn rehydrate_without_snapshot_seeds_from_events_only() {
        let now = t0();
        let events = vec![
            digest("error", "build failed", now - Duration::minutes(20)),
            digest("file_modified", "src/lib.rs", now - Duration::minutes(10)),
            digest("success", "tests green", now - Duration::minutes(5)),
        ];
        let s = rehydrate(None, &events, now, &cfg());
        assert_eq!(s.last_error.as_ref().unwrap().text, "build failed");
        assert_eq!(s.last_success.as_ref().unwrap().text, "tests green");
        assert_eq!(s.open_files, vec!["src/lib.rs".to_string()]);
        assert_eq!(s.active_project.as_deref(), Some("continuum"));
        assert_eq!(s.confidence, 0.0);
        assert_eq!(s.current_task, None);
    }

    #[test]
    fn rehydrate_events_newer_than_the_snapshot_win() {
        let now = t0();
        let mut prev = persisted(0.8, now - Duration::minutes(10));
        prev.last_error = Some(StampedText::new("old error", now - Duration::hours(3)));
        let events = vec![digest("error", "new error", now - Duration::minutes(2))];
        let s = rehydrate(Some(prev), &events, now, &cfg());
        assert_eq!(s.last_error.as_ref().unwrap().text, "new error");
    }

    #[test]
    fn rehydrate_with_nothing_at_all_is_empty() {
        let s = rehydrate(None, &[], t0(), &cfg());
        assert!(s.is_empty());
        assert_eq!(s.confidence, 0.0);
    }

    #[test]
    fn read_persisted_state_handles_missing_and_keyless_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_persisted_state(tmp.path()), None);
        std::fs::write(tmp.path().join("state.json"), "{\"paused\":false}").unwrap();
        assert_eq!(read_persisted_state(tmp.path()), None);
        std::fs::write(tmp.path().join("state.json"), "not json").unwrap();
        assert_eq!(read_persisted_state(tmp.path()), None);

        let state = persisted(0.7, t0());
        let wrapper = serde_json::json!({ "session_state": state });
        std::fs::write(
            tmp.path().join("state.json"),
            serde_json::to_string(&wrapper).unwrap(),
        )
        .unwrap();
        let loaded = read_persisted_state(tmp.path()).unwrap();
        assert_eq!(loaded.current_task.as_deref(), Some("finish B5"));
        assert_eq!(loaded.last_user_command.unwrap().at, t0());
    }

    #[test]
    fn stamped_text_deserializes_from_a_bare_string_too() {
        let s: SessionState =
            serde_json::from_str(r#"{"last_error":"boom","confidence":0.5}"#).unwrap();
        assert_eq!(s.last_error.as_ref().unwrap().text, "boom");
        assert_eq!(s.last_error.as_ref().unwrap().at, epoch());
    }

    #[test]
    fn session_state_json_has_the_spec_field_names() {
        let json = serde_json::to_value(SessionState::default()).unwrap();
        for key in [
            "active_project",
            "current_goal",
            "current_task",
            "active_app",
            "window_title",
            "open_files",
            "last_error",
            "last_success",
            "last_user_command",
            "confidence",
            "since",
            "updated",
            "local_only",
        ] {
            assert!(json.get(key).is_some(), "missing spec key {key}");
        }
    }

    // --- MatchContext threading (spec §4.8 consumers) -----------------

    #[test]
    fn match_context_prefers_the_resolver_then_falls_back_to_session_state() {
        let s = SessionState {
            active_project: Some("continuum".to_string()),
            ..SessionState::default()
        };
        // Resolver wins when it has an opinion (human-readable name).
        assert_eq!(
            match_context_project(Some("Continuum"), Some(&s)).as_deref(),
            Some("Continuum")
        );
        // Fallback to the (possibly rehydrated) session project.
        assert_eq!(
            match_context_project(None, Some(&s)).as_deref(),
            Some("continuum")
        );
        // Nothing anywhere.
        assert_eq!(
            match_context_project(None, Some(&SessionState::default())),
            None
        );
        assert_eq!(match_context_project(None, None), None);
    }

    #[test]
    fn match_context_task_is_gated_on_the_confidence_floor() {
        let c = cfg();
        let mut s = SessionState {
            current_task: Some("finish B5".to_string()),
            confidence: c.confidence_floor - 0.01,
            ..SessionState::default()
        };
        assert_eq!(s.task_if_confident(c.confidence_floor), None);
        s.confidence = c.confidence_floor;
        assert_eq!(s.task_if_confident(c.confidence_floor), Some("finish B5"));
        // A confident state with no task inferred yet is still None.
        s.current_task = None;
        s.confidence = 1.0;
        assert_eq!(s.task_if_confident(c.confidence_floor), None);
    }

    #[test]
    fn seed_events_bounds_and_orders_the_window() {
        let hub = SessionStateHub::new();
        let digests: Vec<EventDigest> = (0..(EVENT_RING_CAP + 5))
            .map(|i| {
                digest(
                    "routine",
                    &format!("e{i}"),
                    t0() + Duration::seconds(i as i64),
                )
            })
            .collect();
        hub.seed_events(digests);
        let all = hub.recent_events(usize::MAX);
        assert_eq!(all.len(), EVENT_RING_CAP);
        assert_eq!(
            all.last().unwrap().summary,
            format!("e{}", EVENT_RING_CAP + 4)
        );
    }

    // --- runtime bridges ----------------------------------------------

    /// The gated `ContextEvent` → [`EventDigest`] bridge must map the
    /// registry enums to their stable snake_case tokens (the same strings
    /// the mechanical matcher arms compare against) and translate the
    /// sensitivity tag into the `local_only` flag the §4.1 propagation
    /// rule reads.
    #[cfg(feature = "runtime")]
    #[test]
    fn digest_from_event_maps_tokens_and_sensitivity() {
        use crate::memory::events::{ContextEvent, EventSensitivity, EventSource, EventType};
        let ev = ContextEvent {
            ts: t0(),
            source: EventSource::Screen,
            application: "Code.exe".to_string(),
            window_title: "main.rs".to_string(),
            project_id: Some("continuum".to_string()),
            event_type: EventType::Error,
            summary: "build failed".to_string(),
            importance: 0.9,
            confidence: 0.8,
            sensitivity: EventSensitivity::LocalOnly,
            raw_reference: None,
        };
        let d = digest_from_event(&ev);
        assert_eq!(d.source, "screen");
        assert_eq!(d.event_type, "error");
        assert!(d.local_only);
        assert_eq!(d.summary, "build failed");

        let mut cloud = ev.clone();
        cloud.sensitivity = EventSensitivity::CloudAllowed;
        cloud.event_type = EventType::FileModified;
        let d = digest_from_event(&cloud);
        assert!(!d.local_only);
        assert_eq!(d.event_type, "file_modified");
        assert!(FILE_EVENT_TYPES.contains(&d.event_type.as_str()));
    }

    /// End-to-end through the hub's gated entry point: a real
    /// `ContextEvent` must land in `last_error` exactly as the ungated
    /// digest path does.
    #[cfg(feature = "runtime")]
    #[test]
    fn apply_context_event_updates_the_mechanical_fields() {
        use crate::memory::events::{ContextEvent, EventSensitivity, EventSource, EventType};
        let hub = SessionStateHub::new();
        hub.apply_context_event(
            &ContextEvent {
                ts: t0(),
                source: EventSource::Screen,
                application: "Code.exe".to_string(),
                window_title: String::new(),
                project_id: None,
                event_type: EventType::Success,
                summary: "tests green".to_string(),
                importance: 0.7,
                confidence: 1.0,
                sensitivity: EventSensitivity::CloudAllowed,
                raw_reference: None,
            },
            &cfg(),
        );
        assert_eq!(hub.snapshot().last_success.unwrap().text, "tests green");
        assert_eq!(hub.recent_events(10).len(), 1);
    }

    /// Boot rehydration against real storage. The DB-level window/order
    /// contract of `recent_context_events` is asserted in `memory::raw_log`
    /// (where the insert helper lives); this covers the wiring: a missing
    /// `state.json` and an empty events table must produce an empty state
    /// rather than an error, and a written snapshot must come back.
    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn rehydrate_from_disk_never_fails_and_reads_the_snapshot() {
        use crate::memory::raw_log::RawLog;

        let tmp = tempfile::tempdir().unwrap();
        let raw_log = RawLog::open("sqlite::memory:").await.unwrap();
        let now = t0();

        // No state.json, no events at all.
        let (state, digests) = rehydrate_from_disk(tmp.path(), &raw_log, &cfg(), now).await;
        assert!(state.is_empty());
        assert!(digests.is_empty());

        // A published snapshot comes back, staleness-discounted.
        let snapshot = persisted(0.8, now - Duration::hours(2));
        std::fs::write(
            tmp.path().join("state.json"),
            serde_json::to_string(&serde_json::json!({ "session_state": snapshot })).unwrap(),
        )
        .unwrap();
        let (state, _) = rehydrate_from_disk(tmp.path(), &raw_log, &cfg(), now).await;
        assert_eq!(state.current_task.as_deref(), Some("finish B5"));
        assert!((state.confidence - 0.4).abs() < 1e-6);
        raw_log.close().await;
    }

    // --- pins + corrections (Task C5, spec 4.13) ------------------------

    fn pinned_project(id: &str, confidence: f32) -> CurrentProject {
        CurrentProject {
            id: id.to_string(),
            name: id.to_string(),
            root_path: None,
            confidence,
            source_tier: 1,
            zone: None,
            status: crate::context::project::ProjectStatus::Configured,
        }
    }

    #[test]
    fn a_pinned_project_survives_a_frame_that_resolves_something_else() {
        let hub = SessionStateHub::new();
        let now = Utc::now();
        hub.set_pin(SessionField::Project, Some("continuum"), now);
        assert!(hub.snapshot().is_pinned(SessionField::Project));

        hub.apply_frame(
            &frame("Code.exe", "other.rs", now),
            Some(&pinned_project("other", 0.9)),
        );
        let state = hub.snapshot();
        assert_eq!(state.active_project.as_deref(), Some("continuum"));
        // Mechanical fields are NOT frozen by a project pin.
        assert_eq!(state.active_app.as_deref(), Some("Code.exe"));
        assert_eq!(state.window_title.as_deref(), Some("other.rs"));
    }

    #[test]
    fn a_pinned_task_survives_inference_while_an_unpinned_goal_moves() {
        let hub = SessionStateHub::new();
        let now = Utc::now();
        hub.set_pin(SessionField::Task, Some("ship C5"), now);
        hub.apply_inference(
            &InferenceResult {
                goal: Some("inferred goal".into()),
                task: Some("inferred task".into()),
                confidence: 0.9,
                ..InferenceResult::default()
            },
            false,
            now,
        );
        let state = hub.snapshot();
        assert_eq!(state.current_task.as_deref(), Some("ship C5"), "pin holds");
        assert_eq!(
            state.current_goal.as_deref(),
            Some("inferred goal"),
            "an unpinned field still moves"
        );
        assert!((state.confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn clearing_a_pin_lets_the_next_frame_move_the_field() {
        let hub = SessionStateHub::new();
        let now = Utc::now();
        hub.set_pin(SessionField::Project, Some("continuum"), now);
        hub.set_pin(SessionField::Project, None, now);
        assert!(!hub.snapshot().is_pinned(SessionField::Project));
        hub.apply_frame(
            &frame("Code.exe", "x", now),
            Some(&pinned_project("other", 0.9)),
        );
        assert_eq!(hub.snapshot().active_project.as_deref(), Some("other"));
    }

    #[test]
    fn pinning_is_idempotent_and_never_duplicates_the_token() {
        let hub = SessionStateHub::new();
        let now = Utc::now();
        hub.set_pin(SessionField::Goal, Some("a"), now);
        hub.set_pin(SessionField::Goal, Some("b"), now);
        assert_eq!(hub.snapshot().pinned, vec!["goal".to_string()]);
        assert_eq!(hub.snapshot().current_goal.as_deref(), Some("b"));
    }

    #[test]
    fn correcting_a_project_clears_the_inferred_fields_like_a_switch() {
        let hub = SessionStateHub::new();
        let now = Utc::now();
        hub.apply_inference(
            &InferenceResult {
                goal: Some("old goal".into()),
                task: Some("old task".into()),
                confidence: 0.8,
                ..InferenceResult::default()
            },
            true,
            now,
        );
        hub.apply_correction(SessionField::Project, "continuum", now);
        let state = hub.snapshot();
        assert_eq!(state.active_project.as_deref(), Some("continuum"));
        assert!(state.current_goal.is_none());
        assert!(state.current_task.is_none());
        assert_eq!(state.confidence, 0.0);
        assert!(!state.local_only);
        assert!(state.is_user_confirmed(SessionField::Project));
    }

    #[test]
    fn correcting_a_goal_is_fully_confident_and_never_local_only() {
        let hub = SessionStateHub::new();
        let now = Utc::now();
        {
            let mut s = hub.snapshot();
            s.local_only = true;
            hub.replace(s);
        }
        hub.apply_correction(SessionField::Goal, "  ship it  ", now);
        let state = hub.snapshot();
        assert_eq!(state.current_goal.as_deref(), Some("ship it"), "trimmed");
        assert_eq!(state.confidence, 1.0);
        assert!(!state.local_only);
    }

    // --- fixwave 3b (I4): pins survive the clearing paths ----------------

    /// A pinned task must survive a project switch. Before the fix
    /// `apply_frame` cleared it unconditionally and `apply_inference`
    /// refused to rewrite a pinned field, so the value was gone forever.
    #[test]
    fn a_pinned_task_survives_a_project_switch() {
        let hub = SessionStateHub::new();
        let now = Utc::now();
        hub.set_pin(SessionField::Task, Some("ship C5"), now);
        hub.apply_frame(
            &frame("Code.exe", "x", now),
            Some(&pinned_project("other", 0.9)),
        );
        let state = hub.snapshot();
        assert_eq!(
            state.active_project.as_deref(),
            Some("other"),
            "it did switch"
        );
        assert_eq!(state.current_task.as_deref(), Some("ship C5"), "pin holds");
        assert_eq!(state.confidence, 1.0, "the pinned value keeps its tag");
    }

    /// Same guard on the correction path.
    #[test]
    fn a_pinned_task_survives_a_project_correction() {
        let hub = SessionStateHub::new();
        let now = Utc::now();
        hub.set_pin(SessionField::Task, Some("ship C5"), now);
        hub.apply_correction(SessionField::Project, "simcharts", now);
        let state = hub.snapshot();
        assert_eq!(state.active_project.as_deref(), Some("simcharts"));
        assert_eq!(state.current_task.as_deref(), Some("ship C5"));
    }

    /// An *unpinned* goal is still cleared by the same switch — the guard
    /// is per-field, not a blanket exemption.
    #[test]
    fn an_unpinned_goal_is_still_cleared_when_the_task_is_pinned() {
        let hub = SessionStateHub::new();
        let now = Utc::now();
        hub.apply_inference(
            &InferenceResult {
                goal: Some("old goal".into()),
                task: Some("old task".into()),
                confidence: 0.8,
                ..InferenceResult::default()
            },
            false,
            now,
        );
        hub.set_pin(SessionField::Task, Some("ship C5"), now);
        hub.apply_frame(
            &frame("Code.exe", "x", now),
            Some(&pinned_project("other", 0.9)),
        );
        let state = hub.snapshot();
        assert!(state.current_goal.is_none(), "unpinned goal cleared");
        assert_eq!(state.current_task.as_deref(), Some("ship C5"));
    }

    /// When both inferred fields are pinned, an inference that lands on
    /// neither of them must not rewrite their shared `confidence` either.
    #[test]
    fn inference_does_not_rewrite_the_tags_of_two_pinned_fields() {
        let hub = SessionStateHub::new();
        let now = Utc::now();
        hub.set_pin(SessionField::Goal, Some("ship the context engine"), now);
        hub.set_pin(SessionField::Task, Some("ship C5"), now);
        hub.apply_inference(
            &InferenceResult {
                goal: Some("something else".into()),
                task: Some("something else".into()),
                confidence: 0.2,
                ..InferenceResult::default()
            },
            true,
            now,
        );
        let state = hub.snapshot();
        assert_eq!(state.current_task.as_deref(), Some("ship C5"));
        assert_eq!(state.confidence, 1.0, "a pinned value keeps its confidence");
        assert!(!state.local_only, "and its zone tag");
    }

    // --- fixwave 3b (I5): the inference clock ----------------------------

    /// `inferred_at` marks when the goal/task were *established*, and the
    /// mechanical per-frame update must not refresh it — that is exactly
    /// what made the continuation resolver's decay permanently 1.0.
    #[test]
    fn a_frame_does_not_refresh_the_inference_clock() {
        let hub = SessionStateHub::new();
        let inferred = Utc::now() - Duration::hours(5);
        hub.apply_inference(
            &InferenceResult {
                goal: None,
                task: Some("fix the flaky test".into()),
                confidence: 0.8,
                ..InferenceResult::default()
            },
            false,
            inferred,
        );
        assert_eq!(hub.snapshot().inferred_at, Some(inferred));

        let now = Utc::now();
        hub.apply_frame(&frame("Code.exe", "a-new-title.rs", now), None);
        let state = hub.snapshot();
        assert!(state.updated >= now, "`updated` moved with the frame");
        assert_eq!(
            state.inferred_at,
            Some(inferred),
            "but the inference clock did not"
        );

        // And the resolver ages the task on that clock, not on `updated`.
        let inputs = crate::context::continuation::ContinuationInputs::from_session(&state);
        assert_eq!(inputs.session_updated, Some(inferred));
    }

    /// A correction restarts the clock — the user just established it.
    #[test]
    fn a_correction_restarts_the_inference_clock() {
        let hub = SessionStateHub::new();
        let old = Utc::now() - Duration::hours(5);
        hub.apply_inference(
            &InferenceResult {
                goal: None,
                task: Some("old".into()),
                confidence: 0.8,
                ..InferenceResult::default()
            },
            false,
            old,
        );
        let now = Utc::now();
        hub.apply_correction(SessionField::Task, "new", now);
        assert_eq!(hub.snapshot().inferred_at, Some(now));
    }

    #[test]
    fn an_empty_correction_changes_nothing() {
        let hub = SessionStateHub::new();
        let before = hub.snapshot();
        hub.apply_correction(SessionField::Task, "   ", Utc::now());
        assert_eq!(hub.snapshot(), before);
    }

    #[test]
    fn pins_and_confirmations_round_trip_through_json() {
        let hub = SessionStateHub::new();
        let now = Utc::now();
        hub.set_pin(SessionField::Project, Some("continuum"), now);
        hub.apply_correction(SessionField::Task, "ship C5", now);
        let json = serde_json::to_string(&hub.snapshot()).unwrap();
        let parsed: SessionState = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_pinned(SessionField::Project));
        assert!(parsed.is_user_confirmed(SessionField::Task));
        // A pre-C5 document with neither key still parses.
        let legacy: SessionState = serde_json::from_str("{}").unwrap();
        assert!(legacy.pinned.is_empty());
        assert!(legacy.user_confirmed.is_empty());
    }

    // --- ProjectPinGuard ------------------------------------------------

    #[test]
    fn the_guard_clears_a_pin_after_switch_min_of_confident_disagreement() {
        let mut guard = ProjectPinGuard::new();
        let t0 = Utc::now();
        let switch_min = Duration::seconds(20);
        assert!(!guard.observe("continuum", Some(("other", 0.9)), switch_min, t0));
        assert_eq!(guard.divergent_project(), Some("other"));
        assert!(!guard.observe(
            "continuum",
            Some(("other", 0.9)),
            switch_min,
            t0 + Duration::seconds(19)
        ));
        assert!(guard.observe(
            "continuum",
            Some(("other", 0.9)),
            switch_min,
            t0 + Duration::seconds(20)
        ));
        // Fires once; the timer restarts.
        assert!(!guard.observe(
            "continuum",
            Some(("other", 0.9)),
            switch_min,
            t0 + Duration::seconds(21)
        ));
    }

    /// Fixwave 3b (I4): an unpinned stretch must clear the running timer,
    /// otherwise a brand-new pin inherits a minutes-old start timestamp and
    /// is cleared on its very first frame.
    #[test]
    fn resetting_the_guard_stops_a_stale_divergence_from_clearing_a_new_pin() {
        let mut guard = ProjectPinGuard::new();
        let t0 = Utc::now();
        let switch_min = Duration::seconds(20);
        // A divergence starts timing while an old pin is live.
        assert!(!guard.observe("continuum", Some(("other", 0.9)), switch_min, t0));
        assert_eq!(guard.divergent_project(), Some("other"));

        // The pin is released; the frame loop resets the guard.
        guard.reset();
        assert_eq!(guard.divergent_project(), None);

        // A new pin, well past `switch_min`, must NOT be cleared on its
        // first frame — the timer starts now.
        assert!(!guard.observe(
            "continuum",
            Some(("other", 0.9)),
            switch_min,
            t0 + Duration::seconds(300)
        ));
        assert_eq!(guard.divergent_project(), Some("other"));
    }

    #[test]
    fn the_guard_ignores_low_confidence_and_agreeing_resolutions() {
        let mut guard = ProjectPinGuard::new();
        let t0 = Utc::now();
        let switch_min = Duration::seconds(20);
        // Keyword tier (0.5) never clears a pin.
        for offset in [0, 30, 60] {
            assert!(!guard.observe(
                "continuum",
                Some(("other", 0.5)),
                switch_min,
                t0 + Duration::seconds(offset)
            ));
        }
        assert_eq!(guard.divergent_project(), None);
        // Agreement resets any running timer.
        guard.observe("continuum", Some(("other", 0.9)), switch_min, t0);
        assert_eq!(guard.divergent_project(), Some("other"));
        guard.observe("continuum", Some(("continuum", 0.9)), switch_min, t0);
        assert_eq!(guard.divergent_project(), None);
    }

    #[test]
    fn the_guard_restarts_when_a_third_project_appears_and_on_no_resolution() {
        let mut guard = ProjectPinGuard::new();
        let t0 = Utc::now();
        let switch_min = Duration::seconds(20);
        guard.observe("continuum", Some(("a", 0.9)), switch_min, t0);
        guard.observe(
            "continuum",
            Some(("b", 0.9)),
            switch_min,
            t0 + Duration::seconds(19),
        );
        assert_eq!(guard.divergent_project(), Some("b"));
        assert!(!guard.observe(
            "continuum",
            Some(("b", 0.9)),
            switch_min,
            t0 + Duration::seconds(30)
        ));
        guard.observe("continuum", None, switch_min, t0 + Duration::seconds(40));
        assert_eq!(guard.divergent_project(), None);
    }

    #[test]
    fn the_guard_boundary_is_exactly_the_git_root_tier() {
        let t0 = Utc::now();
        let switch_min = Duration::seconds(0);
        let mut guard = ProjectPinGuard::new();
        assert!(guard.observe(
            "continuum",
            Some(("other", PIN_CLEAR_CONFIDENCE)),
            switch_min,
            t0
        ));
        let mut guard = ProjectPinGuard::new();
        assert!(!guard.observe(
            "continuum",
            Some(("other", PIN_CLEAR_CONFIDENCE - 0.01)),
            switch_min,
            t0
        ));
    }
}
