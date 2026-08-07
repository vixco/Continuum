//! # Continuation resolver v1 (context engine spec §4.12)
//!
//! Answers one question: **when the user says "ga door", what did they
//! mean?**
//!
//! The resolver is deliberately *dumb and fast* — pure logic, no LLM call,
//! no I/O, no clock (`now` is always a parameter). The caller collects the
//! candidates from their live producers, hands them over as
//! [`ContinuationInputs`], and gets back one of three outcomes:
//!
//! - [`ContinuationOutcome::Recommend`] — one candidate cleared
//!   `[continuation] confidence_floor`; the packager renders it as
//!   "## Recommended next step" and the wake reason says `Continue: …`.
//! - [`ContinuationOutcome::Ask`] — nothing was confident enough, so the
//!   wake reason instructs the orchestrator to ask **one** short
//!   disambiguation question naming up to [`MAX_ASK_CANDIDATES`] options.
//! - [`ContinuationOutcome::Nothing`] — there is nothing to continue; the
//!   wake behaves exactly as it did before this task existed.
//!
//! ## Gating
//!
//! Ungated (spec §3), like [`crate::context::package`] and most of
//! [`crate::context::session_state`]: plain owned structs plus arithmetic,
//! so the runtime, the MCP server and the desktop all link it and the
//! `--no-default-features` parity gate proves it.
//!
//! ## Ranking
//!
//! `score = base_confidence × kind_prior × recency_decay(age)`, clamped to
//! `[0, 1]`.
//!
//! **Priors** ([`CandidateKind::prior`]) encode *what kind of evidence this
//! is about the user's intent*:
//!
//! | Kind | Prior | Rationale |
//! |---|---|---|
//! | [`CandidateKind::SessionTask`] | 1.00 | Continuum's own live belief about what the user is doing — the most direct statement of intent it has, and it survives a restart via §4.8 rehydration. |
//! | [`CandidateKind::WakeNextStep`] | 0.90 | The orchestrator itself said what should happen next, one wake ago. Explicit, but it is *Continuum's* plan, not the user's. |
//! | [`CandidateKind::OpenTask`] | 0.80 | The curator's structured `open_task:` trailer on the last session summary. Explicit and cheap to read, but a session-boundary artefact — possibly hours old. |
//! | [`CandidateKind::LastUserCommand`] | 0.70 | What the user last actually asked for. Real intent, but it may already be *done*, which is exactly why "continue" is ambiguous. |
//! | [`CandidateKind::LastError`] | 0.50 | Lowest on purpose: **an error is a symptom, a task is intent.** "Continue" after a red build usually means "keep working on the thing", not "keep staring at the error". |
//!
//! **Recency decay** ([`recency_decay`]): full weight for the first
//! [`DECAY_GRACE_MINUTES`], then halved per [`DECAY_HALF_LIFE_HOURS`] of
//! additional age, with a floor of [`DECAY_FLOOR`] so an old-but-only
//! candidate can still be *offered* (it will simply land in `Ask` rather
//! than `Recommend`).

use chrono::{DateTime, Duration, Utc};

use crate::config::ContinuationConfig;
use crate::context::session_state::{SessionState, StampedText};

// ---------------------------------------------------------------------------
// Tuning constants
// ---------------------------------------------------------------------------

/// Age below which a candidate keeps its full confidence.
pub const DECAY_GRACE_MINUTES: i64 = 30;

/// Half-life applied to age beyond [`DECAY_GRACE_MINUTES`].
pub const DECAY_HALF_LIFE_HOURS: f32 = 1.0;

/// Floor on the recency multiplier: an ancient candidate is still worth
/// *offering* as a disambiguation option, just never worth recommending on
/// its own.
pub const DECAY_FLOOR: f32 = 0.1;

/// Age assumed for a `session_task` whose
/// [`crate::context::session_state::SessionState::inferred_at`] is unset
/// (a snapshot persisted before fixwave 3b added that field). Far beyond
/// the half-life, so such a task scores at [`DECAY_FLOOR`] — offered, never
/// recommended.
pub const UNSTAMPED_SESSION_TASK_AGE_DAYS: i64 = 30;

/// Max candidates named in a [`ContinuationOutcome::Ask`] — the wake reason
/// asks for **one** short question, so the list has to stay speakable.
pub const MAX_ASK_CANDIDATES: usize = 3;

/// Max characters kept per candidate text (a runaway session summary must
/// not become an unbounded wake reason).
pub const CANDIDATE_TEXT_MAX_CHARS: usize = 200;

/// Base confidence of the curator's `open_task:` trailer. It is a
/// structured, deliberate statement — but written at a session boundary,
/// so it may already be stale (recency decay handles that).
pub const OPEN_TASK_BASE_CONFIDENCE: f32 = 0.8;

/// Base confidence of the last `wake_result.next_step`.
pub const WAKE_NEXT_STEP_BASE_CONFIDENCE: f32 = 0.8;

/// Base confidence of `last_user_command`.
pub const LAST_USER_COMMAND_BASE_CONFIDENCE: f32 = 0.7;

/// Base confidence of the last `error` event.
pub const LAST_ERROR_BASE_CONFIDENCE: f32 = 0.6;

// ---------------------------------------------------------------------------
// Candidates
// ---------------------------------------------------------------------------

/// Where a continuation candidate came from (spec §4.12 candidate set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    /// `SessionState::current_task` — including a rehydrated one whose
    /// confidence was discounted by [`crate::context::session_state`].
    SessionTask,
    /// `wake_result.next_step` from the last orchestrator wake (§4.9).
    WakeNextStep,
    /// The `open_task:` trailer on the last curator session summary (§4.9).
    OpenTask,
    /// `SessionState::last_user_command`.
    LastUserCommand,
    /// `SessionState::last_error`.
    LastError,
}

impl CandidateKind {
    /// The per-kind prior — see the module doc's rationale table.
    pub fn prior(self) -> f32 {
        match self {
            CandidateKind::SessionTask => 1.0,
            CandidateKind::WakeNextStep => 0.9,
            CandidateKind::OpenTask => 0.8,
            CandidateKind::LastUserCommand => 0.7,
            CandidateKind::LastError => 0.5,
        }
    }

    /// Stable producer token, rendered into wake reasons and logs.
    pub fn source(self) -> &'static str {
        match self {
            CandidateKind::SessionTask => "session_state.current_task",
            CandidateKind::WakeNextStep => "wake_result.next_step",
            CandidateKind::OpenTask => "session_summary.open_task",
            CandidateKind::LastUserCommand => "session_state.last_user_command",
            CandidateKind::LastError => "session_state.last_error",
        }
    }

    /// Short human label used in the disambiguation question.
    /// Stable snake_case token for UI badges and logs (Task C5).
    pub fn token(self) -> &'static str {
        match self {
            CandidateKind::SessionTask => "session_task",
            CandidateKind::WakeNextStep => "wake_next_step",
            CandidateKind::OpenTask => "open_task",
            CandidateKind::LastUserCommand => "last_user_command",
            CandidateKind::LastError => "last_error",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CandidateKind::SessionTask => "current task",
            CandidateKind::WakeNextStep => "next step from the last wake",
            CandidateKind::OpenTask => "open task from the last session",
            CandidateKind::LastUserCommand => "your last request",
            CandidateKind::LastError => "the last error",
        }
    }

    /// Deterministic tie-break order (lower sorts first): identical scores
    /// resolve by prior, which this mirrors.
    fn order(self) -> u8 {
        match self {
            CandidateKind::SessionTask => 0,
            CandidateKind::WakeNextStep => 1,
            CandidateKind::OpenTask => 2,
            CandidateKind::LastUserCommand => 3,
            CandidateKind::LastError => 4,
        }
    }
}

/// One ranked thing the user might have meant by "continue".
#[derive(Debug, Clone, PartialEq)]
pub struct ContinuationCandidate {
    /// Which producer this came from.
    pub kind: CandidateKind,
    /// The one-line text, trimmed and capped at
    /// [`CANDIDATE_TEXT_MAX_CHARS`].
    pub text: String,
    /// The **ranked score** (`base × prior × decay`), clamped to `[0, 1]`.
    /// This is what `confidence_floor` is compared against and what the
    /// packager renders next to "## Recommended next step".
    pub confidence: f32,
    /// How old the underlying evidence is.
    pub age: Duration,
    /// Stable producer token — [`CandidateKind::source`].
    pub source: &'static str,
    /// Whether this candidate is derived from `local_only` evidence
    /// (spec §4.1 propagation).
    ///
    /// Fixwave 2 (C1): the resolver reads `SessionState::current_task`
    /// *raw* — deliberately, so a rehydrated task still ranks — which means
    /// it bypasses `cloud_view`. Without this tag the resolved text reached
    /// the cloud through "## Recommended next step" and the wake reason
    /// while the very same package's "## Session state" printed
    /// "working in a private context".
    pub local_only: bool,
}

/// Everything the resolver ranks. The caller reads each field from its own
/// live producer; every one of them is optional because a fresh install has
/// none of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContinuationInputs {
    /// `SessionState::current_task` — the **raw** field, not
    /// `task_if_confident`: a rehydrated task deliberately sits below the
    /// session-state floor and the resolver still gets to rank it.
    pub session_task: Option<String>,
    /// Confidence attached to `session_task`.
    pub session_confidence: f32,
    /// When `session_task` was **established** —
    /// [`SessionState::inferred_at`], not `updated`.
    ///
    /// Fixwave 3b (I5): this used to be `SessionState::updated`, which the
    /// frame loop bumps every second (`active_app`/`window_title` are
    /// rewritten on every frame), so `recency_decay` was permanently 1.0
    /// and a morning task still scored as brand new after lunch. The
    /// stale-loses-to-fresh ranking below could never fire in production.
    pub session_updated: Option<DateTime<Utc>>,
    /// The `open_task:` trailer of the most recent session summary.
    pub open_task: Option<StampedText>,
    /// The most recent `wake_result` event's next step.
    pub wake_next_step: Option<StampedText>,
    /// The most recent user command (voice/chat).
    pub last_user_command: Option<StampedText>,
    /// The most recent classified `error` event.
    pub last_error: Option<StampedText>,
    /// `SessionState::local_only` — the §4.1 propagation verdict over the
    /// inference window that produced `session_task`.
    ///
    /// Taints every session-derived candidate (`session_task`,
    /// `last_user_command`, `last_error`), so the caller can gate what it
    /// renders to the cloud. The other two candidates carry their own
    /// policy: `open_task` comes from a vault note whose `sensitivity`
    /// already decided whether retrieval may surface it, and
    /// `wake_next_step` is the *orchestrator's* own prior sentence — it
    /// never saw the private window.
    pub session_local_only: bool,
}

impl ContinuationInputs {
    /// Fills everything session state already knows (task + confidence +
    /// `last_user_command` + `last_error`). The two remaining candidates
    /// live outside the hub and are added by the caller with
    /// [`ContinuationInputs::with_open_task`] /
    /// [`ContinuationInputs::with_wake_next_step`].
    pub fn from_session(state: &SessionState) -> Self {
        Self {
            session_task: state.current_task.clone(),
            session_confidence: state.confidence,
            // I5: `inferred_at`, never `updated` — see the field doc.
            session_updated: state.inferred_at,
            open_task: None,
            wake_next_step: None,
            last_user_command: state.last_user_command.clone(),
            last_error: state.last_error.clone(),
            session_local_only: state.local_only,
        }
    }

    /// Builder: the curator's `open_task:` trailer.
    pub fn with_open_task(mut self, open_task: Option<StampedText>) -> Self {
        self.open_task = open_task;
        self
    }

    /// Builder: the last `wake_result.next_step`.
    pub fn with_wake_next_step(mut self, next_step: Option<StampedText>) -> Self {
        self.wake_next_step = next_step;
        self
    }
}

/// What the resolver decided (spec §4.12 routing).
#[derive(Debug, Clone, PartialEq)]
pub enum ContinuationOutcome {
    /// One candidate cleared `confidence_floor`.
    Recommend(ContinuationCandidate),
    /// Nothing was confident enough — ask one short question naming these
    /// (already truncated to [`MAX_ASK_CANDIDATES`], best first).
    Ask(Vec<ContinuationCandidate>),
    /// No candidates at all.
    Nothing,
}

impl ContinuationOutcome {
    /// The best candidate, whichever arm this is.
    pub fn best(&self) -> Option<&ContinuationCandidate> {
        match self {
            ContinuationOutcome::Recommend(c) => Some(c),
            ContinuationOutcome::Ask(list) => list.first(),
            ContinuationOutcome::Nothing => None,
        }
    }

    /// Stable token for logs/tests.
    pub fn variant_name(&self) -> &'static str {
        match self {
            ContinuationOutcome::Recommend(_) => "recommend",
            ContinuationOutcome::Ask(_) => "ask",
            ContinuationOutcome::Nothing => "nothing",
        }
    }
}

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

/// The recency multiplier for evidence of the given age.
///
/// Full weight for the first [`DECAY_GRACE_MINUTES`], then halved per
/// [`DECAY_HALF_LIFE_HOURS`], floored at [`DECAY_FLOOR`]. A *negative* age
/// (clock skew, or a timestamp written slightly in the future) is treated
/// as zero rather than amplifying the score.
pub fn recency_decay(age: Duration) -> f32 {
    let grace = Duration::minutes(DECAY_GRACE_MINUTES);
    if age <= grace {
        return 1.0;
    }
    let beyond_hours = (age - grace).num_seconds() as f32 / 3_600.0;
    let decayed = 0.5f32.powf(beyond_hours / DECAY_HALF_LIFE_HOURS);
    decayed.max(DECAY_FLOOR)
}

/// Trims + caps a candidate text; `None` when nothing usable is left.
fn clean_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(crate::context::package::truncate_on_char_boundary(
        trimmed,
        CANDIDATE_TEXT_MAX_CHARS,
    ))
}

fn build_candidate(
    kind: CandidateKind,
    text: &str,
    base_confidence: f32,
    at: DateTime<Utc>,
    now: DateTime<Utc>,
    local_only: bool,
) -> Option<ContinuationCandidate> {
    let text = clean_text(text)?;
    let age = now - at;
    let confidence = (base_confidence.max(0.0) * kind.prior() * recency_decay(age)).clamp(0.0, 1.0);
    Some(ContinuationCandidate {
        kind,
        text,
        confidence,
        age,
        source: kind.source(),
        local_only,
    })
}

/// Ranks every available candidate and routes the result (spec §4.12).
///
/// Deterministic: ties break by prior, then by freshness, then by kind
/// order, so the same inputs always produce the same outcome. Duplicate
/// texts (the same sentence arriving from two producers, e.g. the session
/// task and the curator's `open_task`) collapse to the highest-scoring
/// occurrence so the disambiguation question never offers the same thing
/// twice.
pub fn rank(
    inputs: &ContinuationInputs,
    now: DateTime<Utc>,
    cfg: &ContinuationConfig,
) -> ContinuationOutcome {
    let mut candidates: Vec<ContinuationCandidate> = Vec::with_capacity(5);

    if let Some(task) = inputs.session_task.as_deref() {
        candidates.extend(build_candidate(
            CandidateKind::SessionTask,
            task,
            inputs.session_confidence,
            // Fixwave 3b (I5): a task with no `inferred_at` stamp is a
            // snapshot persisted before that field existed. Its real age
            // is unknown, so it is treated as maximally old (scoring at
            // `DECAY_FLOOR`) rather than "just now" — an unknown-age task
            // must lose to fresh evidence and, alone, drop to a question.
            inputs
                .session_updated
                .unwrap_or(now - Duration::days(UNSTAMPED_SESSION_TASK_AGE_DAYS)),
            now,
            inputs.session_local_only,
        ));
    }
    for (kind, slot, base, local_only) in [
        (
            CandidateKind::WakeNextStep,
            &inputs.wake_next_step,
            WAKE_NEXT_STEP_BASE_CONFIDENCE,
            false,
        ),
        (
            CandidateKind::OpenTask,
            &inputs.open_task,
            OPEN_TASK_BASE_CONFIDENCE,
            false,
        ),
        (
            CandidateKind::LastUserCommand,
            &inputs.last_user_command,
            LAST_USER_COMMAND_BASE_CONFIDENCE,
            inputs.session_local_only,
        ),
        (
            CandidateKind::LastError,
            &inputs.last_error,
            LAST_ERROR_BASE_CONFIDENCE,
            inputs.session_local_only,
        ),
    ] {
        if let Some(stamped) = slot {
            candidates.extend(build_candidate(
                kind,
                &stamped.text,
                base,
                stamped.at,
                now,
                local_only,
            ));
        }
    }

    // Stable order first, so the dedupe below deterministically keeps the
    // best occurrence of a repeated text.
    candidates.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.kind
                    .prior()
                    .partial_cmp(&a.kind.prior())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.age.cmp(&b.age))
            .then_with(|| a.kind.order().cmp(&b.kind.order()))
    });
    // Dedupe by text, keeping the highest-scoring occurrence — but the
    // *zone* of the survivor is the strictest of the collapsed set, never
    // the survivor's own (fixwave 2, C1: an identical sentence arriving
    // from a cloud-safe producer must not launder the local_only one).
    let mut kept: Vec<(String, usize)> = Vec::with_capacity(candidates.len());
    let mut taint: Vec<bool> = Vec::with_capacity(candidates.len());
    let mut survivors: Vec<ContinuationCandidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let key = candidate.text.trim().to_lowercase();
        if let Some((_, idx)) = kept.iter().find(|(k, _)| k == &key) {
            taint[*idx] |= candidate.local_only;
            continue;
        }
        kept.push((key, survivors.len()));
        taint.push(candidate.local_only);
        survivors.push(candidate);
    }
    let mut candidates = survivors;
    for (candidate, tainted) in candidates.iter_mut().zip(taint) {
        candidate.local_only = tainted;
    }

    match candidates.first() {
        None => ContinuationOutcome::Nothing,
        Some(top) if top.confidence >= cfg.confidence_floor => {
            ContinuationOutcome::Recommend(top.clone())
        }
        Some(_) => {
            candidates.truncate(MAX_ASK_CANDIDATES);
            ContinuationOutcome::Ask(candidates)
        }
    }
}

// ---------------------------------------------------------------------------
// Trigger matching
// ---------------------------------------------------------------------------

/// Normalizes an utterance for trigger matching: lowercase, trimmed of
/// whitespace/quotes/terminal punctuation, internal whitespace collapsed.
fn normalize_utterance(text: &str) -> String {
    let stripped = text.trim().trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '.' | '!' | '?' | ',' | '…')
    });
    stripped
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `text` is one of the configured continue phrases.
///
/// Matching rules (deliberately narrow — a false positive hijacks a real
/// question):
///
/// - case-insensitive, trimmed, internal whitespace collapsed, surrounding
///   quotes and terminal punctuation ignored;
/// - a **whole-utterance** match ("ga door") always counts;
/// - a **prefix** match counts only on a word boundary ("ga door met de
///   tests" → yes; "continued" → no). A prefixed utterance still carries
///   its own target, and the orchestrator sees the full text in the wake
///   reason — the resolver's suggestion is additive context, never a
///   replacement for what the user actually said;
/// - empty phrases in config are ignored (an empty phrase would match
///   everything).
pub fn is_continue_trigger(text: &str, cfg: &ContinuationConfig) -> bool {
    let normalized = normalize_utterance(text);
    if normalized.is_empty() {
        return false;
    }
    cfg.trigger_phrases.iter().any(|phrase| {
        let phrase = normalize_utterance(phrase);
        if phrase.is_empty() {
            return false;
        }
        if normalized == phrase {
            return true;
        }
        normalized
            .strip_prefix(&phrase)
            .is_some_and(|rest| rest.starts_with(' '))
    })
}

/// Whether a wake is *continue-class*: either the user's utterance matched
/// [`is_continue_trigger`], or there was **no ask at all** — a bare hotkey
/// press / empty chat continue, which spec §4.12 counts as a trigger by
/// itself ("the user summoned Continuum without saying what for").
pub fn is_continue_class(ask: Option<&str>, cfg: &ContinuationConfig) -> bool {
    match ask {
        None => true,
        Some(text) if text.trim().is_empty() => true,
        Some(text) => is_continue_trigger(text, cfg),
    }
}

/// Extracts the user's own words from a runtime wake reason.
///
/// The runtime formats voice wakes as `Voice command (nl): ga door`
/// (`bin/continuum.rs`, `update_voice_session`). Trigger matching has to
/// see `ga door`, not the whole label. Anything that does not look like a
/// labelled voice command passes through unchanged, and only the first
/// line is ever considered (a wake reason may carry trailing context).
pub fn utterance_from_wake_reason(reason: &str) -> &str {
    let first_line = reason.lines().next().unwrap_or("").trim();
    let lowered = first_line.to_lowercase();
    if lowered.starts_with(VOICE_REASON_PREFIX) {
        if let Some((_, rest)) = first_line.split_once(':') {
            return rest.trim();
        }
    }
    first_line
}

/// The label the runtime prefixes spoken wakes with. Lives here, next to
/// the parser that depends on it.
const VOICE_REASON_PREFIX: &str = "voice command";

// ---------------------------------------------------------------------------
// Wake-reason rendering
// ---------------------------------------------------------------------------

/// The wake reason for a confident continuation: the original trigger plus
/// a `Continue: <text>` line naming the resolved target.
pub fn recommend_reason(reason: &str, candidate: &ContinuationCandidate) -> String {
    format!(
        "{reason}\nContinue: {} (from {}, confidence {:.1})",
        candidate.text, candidate.source, candidate.confidence
    )
}

/// The wake reason for an ambiguous continuation: the original trigger plus
/// an instruction to ask **one** short question naming the candidates.
///
/// Deliberately an instruction and not a question: the orchestrator speaks,
/// the resolver does not.
pub fn ask_reason(reason: &str, candidates: &[ContinuationCandidate]) -> String {
    let listed = candidates
        .iter()
        .map(|c| format!("\"{}\" ({})", c.text, c.kind.label()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{reason}\nThe user asked to continue but the target is ambiguous; \
         ask one short question naming the candidates: {listed}. \
         Do not start work until they pick one."
    )
}

// ---------------------------------------------------------------------------
// Tests — these run under BOTH feature sets (the module is ungated, spec §3)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ContinuationConfig {
        ContinuationConfig::default()
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_770_000_000, 0).expect("fixed clock")
    }

    fn minutes_ago(m: i64) -> DateTime<Utc> {
        now() - Duration::minutes(m)
    }

    fn stamped(text: &str, mins: i64) -> Option<StampedText> {
        Some(StampedText::new(text, minutes_ago(mins)))
    }

    // --- decay -------------------------------------------------------------

    #[test]
    fn recency_decay_is_flat_inside_the_grace_window() {
        assert_eq!(recency_decay(Duration::zero()), 1.0);
        assert_eq!(recency_decay(Duration::minutes(29)), 1.0);
        assert_eq!(recency_decay(Duration::minutes(30)), 1.0);
        // Clock skew must not amplify.
        assert_eq!(recency_decay(Duration::minutes(-5)), 1.0);
    }

    #[test]
    fn recency_decay_halves_per_hour_beyond_the_grace_window() {
        let one_hour_past = recency_decay(Duration::minutes(90));
        assert!(
            (one_hour_past - 0.5).abs() < 1e-4,
            "expected 0.5, got {one_hour_past}"
        );
        let two_hours_past = recency_decay(Duration::minutes(150));
        assert!(
            (two_hours_past - 0.25).abs() < 1e-4,
            "expected 0.25, got {two_hours_past}"
        );
    }

    #[test]
    fn recency_decay_never_falls_below_the_floor() {
        assert_eq!(recency_decay(Duration::days(30)), DECAY_FLOOR);
    }

    // --- ranking matrix: each kind alone ------------------------------------

    #[test]
    fn nothing_at_all_yields_nothing() {
        assert_eq!(
            rank(&ContinuationInputs::default(), now(), &cfg()),
            ContinuationOutcome::Nothing
        );
    }

    #[test]
    fn a_fresh_confident_session_task_is_recommended() {
        let inputs = ContinuationInputs {
            session_task: Some("fix the wake packager".into()),
            session_confidence: 0.8,
            session_updated: Some(minutes_ago(5)),
            ..Default::default()
        };
        let ContinuationOutcome::Recommend(top) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Recommend");
        };
        assert_eq!(top.kind, CandidateKind::SessionTask);
        assert_eq!(top.text, "fix the wake packager");
        assert!((top.confidence - 0.8).abs() < 1e-4, "{}", top.confidence);
        assert_eq!(top.source, "session_state.current_task");
    }

    #[test]
    fn a_fresh_wake_next_step_alone_is_recommended() {
        let inputs = ContinuationInputs::default().with_wake_next_step(stamped("run clippy", 10));
        let ContinuationOutcome::Recommend(top) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Recommend");
        };
        assert_eq!(top.kind, CandidateKind::WakeNextStep);
        // 0.8 base x 0.9 prior x 1.0 decay
        assert!((top.confidence - 0.72).abs() < 1e-4, "{}", top.confidence);
    }

    #[test]
    fn a_fresh_open_task_alone_is_recommended() {
        let inputs = ContinuationInputs::default().with_open_task(stamped("ship B8", 20));
        let ContinuationOutcome::Recommend(top) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Recommend");
        };
        assert_eq!(top.kind, CandidateKind::OpenTask);
        // 0.8 base x 0.8 prior
        assert!((top.confidence - 0.64).abs() < 1e-4, "{}", top.confidence);
    }

    #[test]
    fn a_last_user_command_alone_is_only_enough_to_ask() {
        let inputs = ContinuationInputs {
            last_user_command: stamped("draai de tests", 3),
            ..Default::default()
        };
        let ContinuationOutcome::Ask(list) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Ask");
        };
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, CandidateKind::LastUserCommand);
        // 0.7 base x 0.7 prior = 0.49, under the 0.6 floor.
        assert!((list[0].confidence - 0.49).abs() < 1e-4);
    }

    /// An error is a symptom, a task is intent — the lowest prior of all.
    #[test]
    fn a_last_error_alone_is_only_enough_to_ask() {
        let inputs = ContinuationInputs {
            last_error: stamped("cargo test failed", 2),
            ..Default::default()
        };
        let ContinuationOutcome::Ask(list) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Ask");
        };
        assert_eq!(list[0].kind, CandidateKind::LastError);
        assert!((list[0].confidence - 0.30).abs() < 1e-4);
    }

    // --- ranking matrix: combinations --------------------------------------

    #[test]
    fn all_kinds_fresh_rank_in_prior_order() {
        let inputs = ContinuationInputs {
            session_task: Some("fix the packager".into()),
            session_confidence: 0.8,
            session_updated: Some(minutes_ago(1)),
            open_task: stamped("ship B8", 1),
            wake_next_step: stamped("run clippy", 1),
            last_user_command: stamped("draai de tests", 1),
            last_error: stamped("build failed", 1),
            session_local_only: false,
        };
        // Every candidate present, so the Ask list would be truncated — use
        // a floor of 1.1 to force Ask and inspect the full ordering.
        let forced = ContinuationConfig {
            confidence_floor: 1.1,
            ..cfg()
        };
        let ContinuationOutcome::Ask(list) = rank(&inputs, now(), &forced) else {
            panic!("expected Ask");
        };
        assert_eq!(list.len(), MAX_ASK_CANDIDATES);
        assert_eq!(
            list.iter().map(|c| c.kind).collect::<Vec<_>>(),
            vec![
                CandidateKind::SessionTask,
                CandidateKind::WakeNextStep,
                CandidateKind::OpenTask
            ]
        );

        // With the real floor, the session task wins outright.
        let ContinuationOutcome::Recommend(top) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Recommend");
        };
        assert_eq!(top.kind, CandidateKind::SessionTask);
    }

    #[test]
    fn a_stale_high_prior_candidate_loses_to_a_fresh_lower_prior_one() {
        let inputs = ContinuationInputs {
            // 0.8 x 1.0 x decay(4h30m) = 0.8 x 0.0625 -> 0.05
            session_task: Some("yesterday's task".into()),
            session_confidence: 0.8,
            session_updated: Some(minutes_ago(270)),
            wake_next_step: stamped("run clippy", 5),
            ..Default::default()
        };
        let ContinuationOutcome::Recommend(top) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Recommend");
        };
        assert_eq!(top.kind, CandidateKind::WakeNextStep);
    }

    /// Fixwave 3b (I5). The session task's age comes from
    /// `SessionState::inferred_at`, which the per-frame mechanical update
    /// must not refresh — otherwise this decay never happens in production
    /// and "ga door" after lunch confidently recommends the morning's task.
    #[test]
    fn a_frame_does_not_refresh_the_session_task_clock() {
        let inferred = now() - Duration::hours(4);
        // What the frame loop leaves behind: the task was inferred four
        // hours ago, but `updated` was rewritten by this second's frame.
        let state = SessionState {
            current_task: Some("yesterday's task".into()),
            confidence: 0.8,
            inferred_at: Some(inferred),
            updated: now(),
            ..SessionState::default()
        };

        let inputs = ContinuationInputs::from_session(&state);
        assert_eq!(
            inputs.session_updated,
            Some(inferred),
            "ages on inferred_at"
        );
        // Decayed to well under the floor: the resolver asks instead of
        // recommending a four-hour-old task.
        assert!(matches!(
            rank(&inputs, now(), &cfg()),
            ContinuationOutcome::Ask(_)
        ));
    }

    /// A snapshot persisted before `inferred_at` existed has an unknown
    /// age; it must be treated as maximally old, never as "just now".
    #[test]
    fn an_unstamped_session_task_scores_at_the_decay_floor() {
        let inputs = ContinuationInputs {
            session_task: Some("from a legacy snapshot".into()),
            session_confidence: 1.0,
            session_updated: None,
            ..Default::default()
        };
        let ContinuationOutcome::Ask(list) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Ask — an unknown-age task may never be recommended");
        };
        assert_eq!(list.len(), 1);
        assert!(
            (list[0].confidence - DECAY_FLOOR).abs() < 1e-4,
            "{}",
            list[0].confidence
        );
    }

    #[test]
    fn duplicate_texts_collapse_to_the_best_producer() {
        let inputs = ContinuationInputs {
            session_task: Some("Ship B8".into()),
            session_confidence: 0.5,
            session_updated: Some(minutes_ago(1)),
            ..Default::default()
        }
        // Same sentence, different case + whitespace, from the curator.
        .with_open_task(stamped("  ship b8  ", 1));

        // Forced Ask so the whole list is observable.
        let floorless = ContinuationConfig {
            confidence_floor: 1.1,
            ..cfg()
        };
        let ContinuationOutcome::Ask(list) = rank(&inputs, now(), &floorless) else {
            panic!("expected Ask");
        };
        assert_eq!(list.len(), 1, "duplicate texts must not be offered twice");
        // open_task (0.8 x 0.8 = 0.64) outscored the session task (0.5),
        // so the curator's phrasing is the survivor.
        assert_eq!(list[0].kind, CandidateKind::OpenTask);
        assert_eq!(list[0].text, "ship b8");

        // At the real floor the collapsed candidate is simply recommended.
        assert!(matches!(
            rank(&inputs, now(), &cfg()),
            ContinuationOutcome::Recommend(_)
        ));
    }

    #[test]
    fn the_ask_list_is_capped_at_three() {
        let floorless = ContinuationConfig {
            confidence_floor: 1.1,
            ..cfg()
        };
        let inputs = ContinuationInputs {
            session_task: Some("a".into()),
            session_confidence: 0.9,
            session_updated: Some(minutes_ago(1)),
            open_task: stamped("b", 1),
            wake_next_step: stamped("c", 1),
            last_user_command: stamped("d", 1),
            last_error: stamped("e", 1),
            session_local_only: false,
        };
        let ContinuationOutcome::Ask(list) = rank(&inputs, now(), &floorless) else {
            panic!("expected Ask");
        };
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn empty_and_whitespace_texts_are_not_candidates() {
        let inputs = ContinuationInputs {
            session_task: Some("   ".into()),
            session_confidence: 0.9,
            session_updated: Some(minutes_ago(1)),
            open_task: stamped("", 1),
            last_error: stamped("\n\t ", 1),
            ..Default::default()
        };
        assert_eq!(rank(&inputs, now(), &cfg()), ContinuationOutcome::Nothing);
    }

    #[test]
    fn candidate_text_is_capped() {
        let long = "x".repeat(600);
        let inputs = ContinuationInputs::default().with_open_task(stamped(&long, 1));
        let outcome = rank(&inputs, now(), &cfg());
        let top = outcome.best().expect("a candidate");
        assert!(
            top.text.len() <= CANDIDATE_TEXT_MAX_CHARS,
            "{}",
            top.text.len()
        );
    }

    #[test]
    fn a_zero_confidence_session_task_still_ranks_below_everything() {
        let inputs = ContinuationInputs {
            session_task: Some("no idea".into()),
            session_confidence: 0.0,
            session_updated: Some(minutes_ago(1)),
            last_error: stamped("build failed", 1),
            ..Default::default()
        };
        let ContinuationOutcome::Ask(list) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Ask");
        };
        assert_eq!(list[0].kind, CandidateKind::LastError);
    }

    // --- post-restart -------------------------------------------------------

    /// Post-restart, fresh: rehydration kept the confidence intact (the
    /// snapshot was < 30 min old), so "ga door" still resolves outright.
    #[test]
    fn post_restart_within_the_fresh_window_still_recommends() {
        let inputs = ContinuationInputs {
            session_task: Some("fix the wake packager".into()),
            // rehydrate() keeps confidence x1.0 under 30 min
            session_confidence: 0.8,
            session_updated: Some(minutes_ago(10)),
            ..Default::default()
        }
        .with_open_task(stamped("ship the context engine", 200));
        let ContinuationOutcome::Recommend(top) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Recommend");
        };
        assert_eq!(top.kind, CandidateKind::SessionTask);
    }

    /// Post-restart, stale: rehydration halved the confidence (0.8 -> 0.4)
    /// and both survivors have aged, so nothing clears the floor and the
    /// orchestrator is told to ask — deterministically, with the session
    /// task first.
    #[test]
    fn post_restart_after_a_stale_gap_asks_with_both_survivors() {
        let inputs = ContinuationInputs {
            session_task: Some("fix the wake packager".into()),
            // rehydrate(): 0.8 x REHYDRATE_STALE_DISCOUNT
            session_confidence: 0.4,
            session_updated: Some(minutes_ago(45)),
            ..Default::default()
        }
        .with_open_task(stamped("ship the context engine", 45));
        let ContinuationOutcome::Ask(list) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Ask");
        };
        assert_eq!(list.len(), 2);
        // open_task: 0.8 x 0.8 x decay(15m past grace) ~ 0.8x0.8x0.84 = 0.54
        // session:   0.4 x 1.0 x 0.84 = 0.34
        assert_eq!(list[0].kind, CandidateKind::OpenTask);
        assert_eq!(list[1].kind, CandidateKind::SessionTask);
        assert!(list[0].confidence < cfg().confidence_floor);
        assert!(list[0].confidence > list[1].confidence);
    }

    /// Only the events survived the restart (no snapshot at all): the
    /// resolver still has something to offer.
    #[test]
    fn post_restart_with_only_events_still_offers_something() {
        let inputs = ContinuationInputs {
            last_error: stamped("cargo test failed", 5),
            ..Default::default()
        };
        assert!(matches!(
            rank(&inputs, now(), &cfg()),
            ContinuationOutcome::Ask(_)
        ));
    }

    // --- floor boundary -----------------------------------------------------

    #[test]
    fn the_floor_is_inclusive() {
        let at_floor = ContinuationInputs {
            session_task: Some("exactly at the floor".into()),
            session_confidence: 0.6,
            session_updated: Some(minutes_ago(1)),
            ..Default::default()
        };
        assert!(matches!(
            rank(&at_floor, now(), &cfg()),
            ContinuationOutcome::Recommend(_)
        ));

        let below = ContinuationInputs {
            session_confidence: 0.59,
            ..at_floor
        };
        assert!(matches!(
            rank(&below, now(), &cfg()),
            ContinuationOutcome::Ask(_)
        ));
    }

    #[test]
    fn a_configured_floor_of_zero_always_recommends() {
        let permissive = ContinuationConfig {
            confidence_floor: 0.0,
            ..cfg()
        };
        let inputs = ContinuationInputs {
            last_error: stamped("build failed", 600),
            ..Default::default()
        };
        assert!(matches!(
            rank(&inputs, now(), &permissive),
            ContinuationOutcome::Recommend(_)
        ));
    }

    // --- trigger matching ---------------------------------------------------

    #[test]
    fn default_trigger_phrases_match_whole_utterances() {
        for text in [
            "ga door",
            "Ga door",
            "  GA DOOR  ",
            "ga door.",
            "\"ga door\"",
            "continue",
            "Continue!",
            "verder",
            "waar was ik",
            "waar   was   ik",
            "pak weer op",
        ] {
            assert!(is_continue_trigger(text, &cfg()), "should trigger: {text}");
        }
    }

    #[test]
    fn trigger_matches_on_a_word_boundary_prefix_only() {
        assert!(is_continue_trigger("ga door met de tests", &cfg()));
        assert!(is_continue_trigger("continue where we left off", &cfg()));
        // No word boundary -> not a trigger.
        assert!(!is_continue_trigger("continued", &cfg()));
        assert!(!is_continue_trigger("verderop staat een fout", &cfg()));
    }

    #[test]
    fn non_triggers_do_not_match() {
        for text in [
            "",
            "   ",
            "wat is de status",
            "please stop",
            "ik ga door de code heen", // phrase not at the start
            "should I continue?",      // phrase not at the start
        ] {
            assert!(
                !is_continue_trigger(text, &cfg()),
                "should NOT trigger: {text}"
            );
        }
    }

    #[test]
    fn empty_configured_phrases_never_match_everything() {
        let broken = ContinuationConfig {
            trigger_phrases: vec![String::new(), "   ".into()],
            ..cfg()
        };
        assert!(!is_continue_trigger("anything at all", &broken));
    }

    #[test]
    fn custom_trigger_phrases_are_honoured() {
        let custom = ContinuationConfig {
            trigger_phrases: vec!["resume".into()],
            ..cfg()
        };
        assert!(is_continue_trigger("Resume", &custom));
        assert!(!is_continue_trigger("ga door", &custom));
    }

    #[test]
    fn an_empty_ask_is_continue_class_but_a_real_question_is_not() {
        assert!(is_continue_class(None, &cfg()));
        assert!(is_continue_class(Some(""), &cfg()));
        assert!(is_continue_class(Some("   "), &cfg()));
        assert!(is_continue_class(Some("ga door"), &cfg()));
        assert!(!is_continue_class(Some("wat is de status"), &cfg()));
    }

    // --- wake-reason plumbing ----------------------------------------------

    #[test]
    fn utterance_is_extracted_from_a_voice_wake_reason() {
        assert_eq!(
            utterance_from_wake_reason("Voice command (nl): ga door"),
            "ga door"
        );
        assert_eq!(
            utterance_from_wake_reason("Voice command (en): continue where we left off"),
            "continue where we left off"
        );
        // Non-voice reasons pass through unchanged (first line only).
        assert_eq!(
            utterance_from_wake_reason("User appears stuck on test failures\nmore context"),
            "User appears stuck on test failures"
        );
        assert_eq!(utterance_from_wake_reason(""), "");
    }

    #[test]
    fn a_voice_continue_reason_is_recognised_end_to_end() {
        let reason = "Voice command (nl): ga door";
        assert!(is_continue_class(
            Some(utterance_from_wake_reason(reason)),
            &cfg()
        ));
    }

    #[test]
    fn recommend_reason_names_the_target() {
        let candidate = ContinuationCandidate {
            kind: CandidateKind::SessionTask,
            text: "fix the wake packager".into(),
            confidence: 0.8,
            age: Duration::minutes(3),
            source: CandidateKind::SessionTask.source(),
            local_only: false,
        };
        let reason = recommend_reason("Voice command (nl): ga door", &candidate);
        assert!(reason.starts_with("Voice command (nl): ga door"));
        assert!(reason.contains("Continue: fix the wake packager"));
        assert!(reason.contains("session_state.current_task"));
    }

    #[test]
    fn ask_reason_names_every_candidate_and_asks_for_one_question() {
        let candidates = vec![
            ContinuationCandidate {
                kind: CandidateKind::OpenTask,
                text: "ship the context engine".into(),
                confidence: 0.54,
                age: Duration::minutes(45),
                source: CandidateKind::OpenTask.source(),
                local_only: false,
            },
            ContinuationCandidate {
                kind: CandidateKind::LastError,
                text: "cargo test failed".into(),
                confidence: 0.3,
                age: Duration::minutes(5),
                source: CandidateKind::LastError.source(),
                local_only: false,
            },
        ];
        let reason = ask_reason("Voice command (nl): ga door", &candidates);
        assert!(reason.contains("ask one short question"));
        assert!(reason.contains("\"ship the context engine\" (open task from the last session)"));
        assert!(reason.contains("\"cargo test failed\" (the last error)"));
        assert!(reason.contains("Do not start work until they pick one."));
    }

    // --- fixwave 2 (C1): zone propagation ----------------------------------

    /// The resolver reads `SessionState::current_task` **raw** (that is the
    /// point — a rehydrated task must still rank), so it bypasses
    /// `cloud_view`. The zone therefore has to ride along on the candidate,
    /// or the packager has nothing to gate on.
    #[test]
    fn session_derived_candidates_inherit_the_session_zone() {
        let inputs = ContinuationInputs {
            session_task: Some("reviewing the Q3 layoff list".into()),
            session_confidence: 0.9,
            session_updated: Some(minutes_ago(2)),
            last_user_command: stamped("open the spreadsheet", 3),
            last_error: stamped("export failed", 4),
            session_local_only: true,
            ..Default::default()
        };
        let floorless = ContinuationConfig {
            confidence_floor: 1.1,
            ..cfg()
        };
        let ContinuationOutcome::Ask(list) = rank(&inputs, now(), &floorless) else {
            panic!("expected Ask");
        };
        assert_eq!(list.len(), 3);
        assert!(
            list.iter().all(|c| c.local_only),
            "every session-derived candidate must carry the zone: {list:?}"
        );

        let ContinuationOutcome::Recommend(top) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Recommend");
        };
        assert!(top.local_only);
    }

    /// The two candidates that are *not* session-derived carry their own
    /// policy and are not tainted by the session's zone: `open_task` came
    /// from a vault note retrieval already filtered on `sensitivity`, and
    /// `wake_next_step` is the orchestrator's own prior sentence.
    #[test]
    fn non_session_candidates_are_not_tainted_by_the_session_zone() {
        let inputs = ContinuationInputs {
            session_local_only: true,
            ..Default::default()
        }
        .with_open_task(stamped("ship the context engine", 1))
        .with_wake_next_step(stamped("run the gates", 1));
        let floorless = ContinuationConfig {
            confidence_floor: 1.1,
            ..cfg()
        };
        let ContinuationOutcome::Ask(list) = rank(&inputs, now(), &floorless) else {
            panic!("expected Ask");
        };
        assert!(list.iter().all(|c| !c.local_only), "{list:?}");
    }

    /// Dedupe keeps the highest-scoring occurrence — but must not let a
    /// cloud-safe producer launder an identical `local_only` sentence.
    #[test]
    fn a_collapsed_duplicate_keeps_the_strictest_zone() {
        let inputs = ContinuationInputs {
            session_task: Some("Ship B8".into()),
            session_confidence: 0.5,
            session_updated: Some(minutes_ago(1)),
            session_local_only: true,
            ..Default::default()
        }
        .with_open_task(stamped("ship b8", 1));

        let floorless = ContinuationConfig {
            confidence_floor: 1.1,
            ..cfg()
        };
        let ContinuationOutcome::Ask(list) = rank(&inputs, now(), &floorless) else {
            panic!("expected Ask");
        };
        assert_eq!(list.len(), 1);
        // The curator's phrasing survives (higher score) …
        assert_eq!(list[0].kind, CandidateKind::OpenTask);
        // … but the zone of the collapsed set does not evaporate with the
        // occurrence that carried it.
        assert!(list[0].local_only);
    }

    /// `from_session` is the only production constructor — the zone has to
    /// come across it or nothing above matters.
    #[test]
    fn from_session_carries_the_local_only_flag() {
        let mut state = SessionState {
            current_task: Some("reviewing the Q3 layoff list".into()),
            confidence: 0.9,
            local_only: true,
            ..Default::default()
        };
        assert!(ContinuationInputs::from_session(&state).session_local_only);

        state.local_only = false;
        assert!(!ContinuationInputs::from_session(&state).session_local_only);
    }

    #[test]
    fn a_cloud_safe_session_yields_cloud_safe_candidates() {
        let inputs = ContinuationInputs {
            session_task: Some("fix the wake packager".into()),
            session_confidence: 0.8,
            session_updated: Some(minutes_ago(5)),
            ..Default::default()
        };
        let ContinuationOutcome::Recommend(top) = rank(&inputs, now(), &cfg()) else {
            panic!("expected Recommend");
        };
        assert!(!top.local_only);
    }

    #[test]
    fn outcome_accessors_are_stable() {
        assert_eq!(ContinuationOutcome::Nothing.variant_name(), "nothing");
        assert!(ContinuationOutcome::Nothing.best().is_none());
        let c = ContinuationCandidate {
            kind: CandidateKind::OpenTask,
            text: "x".into(),
            confidence: 0.7,
            age: Duration::zero(),
            source: CandidateKind::OpenTask.source(),
            local_only: false,
        };
        assert_eq!(
            ContinuationOutcome::Recommend(c.clone()).variant_name(),
            "recommend"
        );
        assert_eq!(ContinuationOutcome::Ask(vec![c.clone()]).best(), Some(&c));
    }
}
