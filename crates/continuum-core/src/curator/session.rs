//! # Session summaries
//!
//! Tracks the user's current work session from the perception layer's
//! [`ActivitySignal`] watch channel and, when a session boundary fires (the
//! foreground process changes for long enough to mean something, or the
//! user goes idle), asks the curator LLM to summarize what happened into a
//! single vault note. Session summaries are records of what the vault's own
//! timeline already shows happened, not extracted claims that need human
//! review — see [`write_session_summary`]'s doc comment for why they skip
//! candidate review, unlike [`crate::curator::extract`]'s output.

use chrono::{DateTime, Duration, Utc};
use continuum_memory::{
    EventRange, NewEvent, NodeStatus, NodeType, NoteDraft, Relation, Source, Vault,
};

use crate::curator::run::{build_events_block, ActivitySignal};
use crate::curator::CuratorLlm;

/// The session-summary prompt template, loaded at compile time from the
/// repo-root `prompts/curator-session.md`. `{{START}}`, `{{END}}`,
/// `{{PROCESS}}`, `{{PROJECT}}`, and `{{EVENTS}}` are substituted per-session
/// in [`write_session_summary`].
pub const SESSION_PROMPT: &str = include_str!("../../../../prompts/curator-session.md");

/// Default for [`SessionTracker`]'s `session_min_minutes` (see
/// [`SessionTracker::new`]) — matches `CuratorConfig::session_min_minutes`'s
/// own default (M1 fix). A session boundary from a foreground-process
/// change is only worth acting on once the user has spent at least this
/// long on the process being left — otherwise a quick alt-tab (checking
/// Slack, glancing at a browser tab) would fragment one real session into
/// several noise-sized ones. See [`SessionTracker::observe`].
const DEFAULT_SESSION_MIN_MINUTES: u64 = 5;

/// In-progress session tracked by [`SessionTracker`].
///
/// Named `ActiveSession` (not `SessionState`) to keep it distinct from
/// [`crate::context::session_state::SessionState`], the context engine's
/// live "what is the user doing" state — they are different things at
/// different layers.
#[derive(Debug, Clone)]
struct ActiveSession {
    started: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    project_hint: Option<String>,
    process: String,
}

/// Public read-only view of the session in progress (Task B5, spec §4.8:
/// "`SessionTracker` gains a public snapshot accessor").
///
/// Consumers: the context package (B7) renders "this session started at
/// …", and the continuation resolver (B8) uses `project_hint`/`process` to
/// rank candidates. Returned by [`SessionTracker::snapshot`].
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSnapshot {
    /// When the session in progress started.
    pub started: DateTime<Utc>,
    /// Timestamp of the most recent activity signal folded in.
    pub last_activity: DateTime<Utc>,
    /// Project the session is attributed to — post-A4 this is the
    /// resolver's post-hysteresis project id, not a keyword guess.
    pub project_hint: Option<String>,
    /// Foreground process the session is named after.
    pub process: String,
}

/// A session that just ended, ready for [`write_session_summary`].
#[derive(Debug, Clone, PartialEq)]
pub struct EndedSession {
    pub started: DateTime<Utc>,
    pub ended: DateTime<Utc>,
    pub project_hint: Option<String>,
    pub process: String,
}

/// Pure state machine that turns a stream of [`ActivitySignal`]s into
/// session boundaries. Driven entirely by `sig.ts` — never by wall-clock
/// time — so it is deterministic and trivially testable with fabricated
/// timestamps; the caller ([`crate::curator::run::run_curator`]) feeds it
/// real signals off a watch channel on every tick and every signal change.
#[derive(Debug)]
pub struct SessionTracker {
    current: Option<ActiveSession>,
    /// M1 fix: configurable replacement for the old hardcoded
    /// `MIN_SESSION_MINUTES` constant — see [`SessionTracker::observe`]'s
    /// case 4 and `CuratorConfig::session_min_minutes`.
    session_min_minutes: u64,
}

impl Default for SessionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionTracker {
    /// A tracker with no session in progress, using the default minimum
    /// session length ([`DEFAULT_SESSION_MIN_MINUTES`]). Most call sites
    /// (including every test in this module) want this; the real curator
    /// loop uses [`SessionTracker::with_session_min_minutes`] to honor the
    /// configured `CuratorConfig::session_min_minutes` instead.
    pub fn new() -> Self {
        Self::with_session_min_minutes(DEFAULT_SESSION_MIN_MINUTES)
    }

    /// A tracker with no session in progress and an explicit minimum
    /// session length (M1 fix — see `CuratorConfig::session_min_minutes`).
    pub fn with_session_min_minutes(session_min_minutes: u64) -> Self {
        Self {
            current: None,
            session_min_minutes,
        }
    }

    /// Feed the latest signal. Returns `Some(ended_session)` exactly when a
    /// boundary fired on this call; `None` otherwise — including when
    /// `sig.ts` is `None`, which happens before the perception loop has
    /// produced its first frame and leaves nothing to drive the state
    /// machine with.
    ///
    /// Boundary rules, checked in this order:
    /// 1. No session in progress: start one from `sig`. Never a boundary.
    /// 2. `sig.ts` is *before* `last_activity`: a non-monotonic signal
    ///    (clock skew, sleep/resume, an out-of-order perception frame).
    ///    Ignored outright — no boundary, no state mutation at all — so it
    ///    can't rewind `last_activity` and inflate the next idle-gap check.
    /// 3. `sig.ts - last_activity > idle_limit_min`: the user went idle.
    ///    Ends the session at `last_activity` (the last moment there was
    ///    actually activity, not the idle-detecting signal's own time).
    ///    3b. **The project changed** (Task B5, spec §4.8: "a
    ///    project-change boundary (post-hysteresis)"). Both the old and new
    ///    `project_hint` must be `Some` and different. No minimum session
    ///    length applies, unlike case 4: post-A4 `project_hint` is the
    ///    resolver's *post-hysteresis* output, which already required the
    ///    new project to win continuously for `switch_min_secs` — adding
    ///    another dwell floor here would double-debounce the same signal.
    ///    Ends the session at `last_activity`, like the other cases.
    ///    First adoption (`None` → `Some`) is deliberately not a boundary:
    ///    the session simply gains attribution it always had.
    /// 4. The foreground process changed *and* the session so far
    ///    (`last_activity - started`) is at least `self.session_min_minutes`
    ///    (M1 fix — configurable, default [`DEFAULT_SESSION_MIN_MINUTES`]):
    ///    a real handoff to different work. Ends the session at
    ///    `last_activity` (the previous signal's time, before the change).
    /// 5. Otherwise: not a boundary. `last_activity` and `project_hint`
    ///    track the latest signal, but `process` is left untouched — a
    ///    brief flick to another window (case 4's condition without enough
    ///    elapsed time) never renames the session out from under itself, so
    ///    a string of short flicks can never itself accumulate into a false
    ///    boundary. If the flicked-to process sticks around long enough
    ///    that the *original* session's own elapsed time alone crosses
    ///    `self.session_min_minutes`, case 4 will still eventually fire and
    ///    close out the original session under its original process name.
    ///
    /// Cases 3 and 4 are mutually exclusive in practice (an idle gap and a
    /// process change can't both look at the *same* prior `last_activity`
    /// without the idle check firing first), but if a caller somehow feeds
    /// a signal that satisfies both, the idle check (case 3) is evaluated
    /// first and wins. Case 3b sits between them for the same reason: an
    /// idle gap that *also* changed project is an idle boundary, and a
    /// project change that also changed process is a project boundary
    /// (the stronger, unconditional signal).
    ///
    /// Whenever a boundary fires, the tracker immediately starts a fresh
    /// session from `sig` (case 1's logic) — the signal that closed the old
    /// session is also the first signal of the new one.
    pub fn observe(&mut self, sig: &ActivitySignal, idle_limit_min: u64) -> Option<EndedSession> {
        let ts = sig.ts?;

        let Some(state) = &mut self.current else {
            self.current = Some(ActiveSession {
                started: ts,
                last_activity: ts,
                project_hint: sig.project_hint.clone(),
                process: sig.process.clone(),
            });
            return None;
        };

        // Non-monotonic input (clock skew, sleep/resume, an out-of-order
        // perception frame) is expected, not a bug in the caller — ignore
        // it rather than rewinding `last_activity`, which would otherwise
        // inflate the *next* idle-gap computation and fragment one real
        // session into spurious short ones.
        if ts < state.last_activity {
            tracing::debug!(
                layer = "memory",
                component = "curator",
                ts = %ts,
                last_activity = %state.last_activity,
                "Ignoring non-monotonic activity signal"
            );
            return None;
        }

        let idle_limit = Duration::minutes(idle_limit_min as i64);
        if ts - state.last_activity > idle_limit {
            return Some(Self::roll(&mut self.current, sig, ts));
        }

        // Case 3b (Task B5, spec §4.8): project-change boundary. The hint
        // is the resolver's post-hysteresis project id, so a genuine
        // change here already survived `switch_min_secs` of continuous
        // winning — it needs no further dwell floor.
        let project_changed = match (&state.project_hint, &sig.project_hint) {
            (Some(old), Some(new)) => old != new,
            _ => false,
        };
        if project_changed {
            return Some(Self::roll(&mut self.current, sig, ts));
        }

        let session_len = state.last_activity - state.started;
        if sig.process != state.process
            && session_len >= Duration::minutes(self.session_min_minutes as i64)
        {
            return Some(Self::roll(&mut self.current, sig, ts));
        }

        state.last_activity = ts;
        state.project_hint = sig.project_hint.clone();
        None
    }

    /// Closes the session in progress at its `last_activity` and starts a
    /// fresh one from `sig`/`ts`. Shared by every boundary case so they
    /// cannot drift apart. Panics-free: only called with `current`
    /// already `Some`, and falls back to `sig`'s own facts otherwise.
    fn roll(
        current: &mut Option<ActiveSession>,
        sig: &ActivitySignal,
        ts: DateTime<Utc>,
    ) -> EndedSession {
        let ended = current
            .as_ref()
            .map(|s| EndedSession {
                started: s.started,
                ended: s.last_activity,
                project_hint: s.project_hint.clone(),
                process: s.process.clone(),
            })
            .unwrap_or_else(|| EndedSession {
                started: ts,
                ended: ts,
                project_hint: sig.project_hint.clone(),
                process: sig.process.clone(),
            });
        *current = Some(ActiveSession {
            started: ts,
            last_activity: ts,
            project_hint: sig.project_hint.clone(),
            process: sig.process.clone(),
        });
        ended
    }

    /// Read-only view of the session in progress (Task B5, spec §4.8
    /// "public snapshot accessor"). `None` before the first timestamped
    /// activity signal.
    pub fn snapshot(&self) -> Option<SessionSnapshot> {
        self.current.as_ref().map(|s| SessionSnapshot {
            started: s.started,
            last_activity: s.last_activity,
            project_hint: s.project_hint.clone(),
            process: s.process.clone(),
        })
    }
}

/// Turns an [`EndedSession`] into a vault note: fetches the vault's timeline
/// events for the session's time span, asks the curator LLM to summarize
/// them, and writes the result as a `Session`-typed note.
///
/// Unlike [`crate::curator::extract::extract_pass`]'s candidates, session
/// summaries are written straight to `Confirmed` — never held as
/// `Candidate` for human review. They are a compression of events the
/// vault's own timeline already recorded, not a new claim being extracted
/// and asserted for the first time; the candidate-review gate exists to
/// catch the LLM inventing or misjudging a *claim*, which doesn't apply
/// here — the entire content is "here's what the timeline already shows
/// happened between X and Y".
///
/// Returns `Ok(None)` — without calling the LLM at all — when the session
/// window has fewer than 3 events (too little happened to be worth a
/// summary and a model call). Also returns `Ok(None)` when the LLM's
/// trimmed reply is exactly `"SKIP"` (see `prompts/curator-session.md`):
/// the model itself judged the window as trivial/idle-only even though it
/// cleared the 3-event floor.
///
/// C1 fix note: this still queries the vault's timeline by `ts` range
/// (`started..ended`) rather than by [`continuum_memory::EventRange::since_id`]
/// — a session is inherently time-bounded, so a ts range is the right
/// query shape here (unlike [`crate::curator::run::extract_pass`], which
/// switched to an id watermark). But the same backdating hazard the id
/// watermark exists to avoid still applies at the *tail* of the window: an
/// event whose `ts` falls inside `started..ended` can be written by the
/// distiller up to `distillation_interval_minutes` after that `ts`, so a
/// query issued right at boundary time can still miss the session's own
/// last few events. The caller ([`crate::curator::run::run_curator`])
/// mitigates this by delaying the call to [`write_session_summary`] itself
/// until `distill_lag_minutes` has elapsed past `ended` (see
/// `run_curator`'s `pending_sessions`/`flush_due_sessions`), rather than
/// reworking this query — by the time this function actually runs, the
/// distiller has had time to catch up.
pub async fn write_session_summary(
    vault: &Vault,
    llm: &dyn CuratorLlm,
    ended: &EndedSession,
) -> anyhow::Result<Option<String>> {
    let events = vault
        .events(&EventRange {
            since: Some(ended.started),
            until: Some(ended.ended),
            since_id: None,
            limit: Some(300),
        })
        .await?;

    if events.len() < 3 {
        tracing::debug!(
            layer = "memory",
            component = "curator",
            events = events.len(),
            "Session too short to summarize; skipping"
        );
        return Ok(None);
    }

    let events_block = build_events_block(&events);
    let prompt = SESSION_PROMPT
        .replace("{{START}}", &ended.started.to_rfc3339())
        .replace("{{END}}", &ended.ended.to_rfc3339())
        .replace("{{PROCESS}}", &ended.process)
        .replace(
            "{{PROJECT}}",
            ended.project_hint.as_deref().unwrap_or("none"),
        )
        .replace("{{EVENTS}}", &events_block);

    // Task B2 (spec §4.7): background-caller token budget — capped at
    // BACKGROUND_MAX_TOKENS per call (was 700 pre-B2), so session
    // summaries got tighter. The prompt already asks for a compact
    // markdown summary; ~256 tokens comfortably covers it.
    let raw = llm
        .complete(&prompt, crate::llm_gate::BACKGROUND_MAX_TOKENS)
        .await?;
    let trimmed = raw.trim();
    if trimmed == "SKIP" {
        tracing::debug!(
            layer = "memory",
            component = "curator",
            "Curator LLM judged session trivial; skipping"
        );
        return Ok(None);
    }

    let mut relations = Vec::new();
    if let Some(hint) = &ended.project_hint {
        relations.push(Relation {
            to: hint.clone(),
            rel: "belongs_to".to_string(),
            confidence: 1.0,
        });
    }

    // Task B7 (spec §4.9): the session-summary contract gains a structured
    // `open_task:` trailer the continuation resolver (Task B8, spec §4.12)
    // reads without an LLM call. The prompt asks for it; this normalizes
    // what actually came back so the line is ALWAYS present, always last,
    // and always well-formed — including when the model omitted it (the
    // "## Next step" section is then used) or emitted it twice.
    let body = crate::context::package::normalize_open_task_trailer(trimmed);

    let draft = NoteDraft {
        node_type: NodeType::Session,
        title: format!(
            "Session: {} — {}",
            ended.process,
            ended.ended.format("%Y-%m-%d %H:%M")
        ),
        body,
        project: ended.project_hint.clone(),
        // The session genuinely happened — this isn't a probabilistic
        // extraction over ambiguous signals, it's a compression of events
        // already logged in the vault's own timeline.
        status: NodeStatus::Confirmed,
        confidence: 1.0,
        importance: 0.5,
        source: Source::Observed,
        source_ref: Some("curator:session".to_string()),
        sensitivity: Default::default(),
        expires: None,
        relations,
        tags: vec![],
    };

    let note = vault.create(draft).await?;

    // Spec-gap 1 (cheap timeline win): record the summary itself on the
    // vault's own event timeline, linked via `node_id` so the Memory tab's
    // timeline strip can jump straight to the note. Best-effort — a vault
    // hiccup here must never fail the summary write that already
    // succeeded.
    let _ = vault
        .append_event(NewEvent {
            ts: None,
            kind: "session".to_string(),
            text: note.frontmatter.title.clone(),
            project: ended.project_hint.clone(),
            node_id: Some(note.frontmatter.id.clone()),
            reference: None,
            local_only: false,
        })
        .await
        .map_err(|e| {
            tracing::warn!(
                layer = "memory",
                component = "curator",
                id = %note.frontmatter.id,
                error = %e.user_message(),
                "Failed to append session event to vault timeline"
            );
        });

    tracing::info!(
        layer = "memory",
        component = "curator",
        id = %note.frontmatter.id,
        process = %ended.process,
        "Wrote session summary"
    );
    Ok(Some(note.frontmatter.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curator::MockLlm;
    use chrono::TimeZone;
    use continuum_memory::NewEvent;

    fn base_ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 9, 0, 0).unwrap()
    }

    fn mk_sig(ts: DateTime<Utc>, process: &str, project_hint: Option<&str>) -> ActivitySignal {
        ActivitySignal {
            project_hint: project_hint.map(|s| s.to_string()),
            process: process.to_string(),
            idle_seconds: 0,
            ts: Some(ts),
        }
    }

    // --- SessionTracker table tests -----------------------------------

    #[test]
    fn observe_starts_a_session_without_emitting() {
        let mut tracker = SessionTracker::new();
        let t0 = base_ts();
        assert_eq!(
            tracker.observe(&mk_sig(t0, "vscode", Some("continuum")), 20),
            None
        );
    }

    #[test]
    fn observe_with_no_timestamp_is_a_no_op() {
        let mut tracker = SessionTracker::new();
        let untimestamped = ActivitySignal::default();
        assert_eq!(tracker.observe(&untimestamped, 20), None);

        // No session was started by the untimestamped signal — the next
        // real signal still starts fresh (not a boundary).
        let t0 = base_ts();
        assert_eq!(tracker.observe(&mk_sig(t0, "vscode", None), 20), None);
    }

    #[test]
    fn observe_emits_boundary_on_idle_gap_ending_at_last_activity() {
        let mut tracker = SessionTracker::new();
        let t0 = base_ts();
        assert_eq!(
            tracker.observe(&mk_sig(t0, "vscode", Some("continuum")), 20),
            None
        );

        let t1 = t0 + Duration::minutes(3);
        assert_eq!(
            tracker.observe(&mk_sig(t1, "vscode", Some("continuum")), 20),
            None
        );

        // 25 min gap > 20 min idle limit.
        let t2 = t1 + Duration::minutes(25);
        let ended = tracker
            .observe(&mk_sig(t2, "vscode", Some("continuum")), 20)
            .expect("idle gap should emit a boundary");
        assert_eq!(ended.started, t0);
        assert_eq!(ended.ended, t1); // last activity, not the idle-detecting ts
        assert_eq!(ended.process, "vscode");
        assert_eq!(ended.project_hint.as_deref(), Some("continuum"));
    }

    #[test]
    fn observe_emits_boundary_on_process_change_after_min_session_length() {
        let mut tracker = SessionTracker::new();
        let t0 = base_ts();
        assert_eq!(
            tracker.observe(&mk_sig(t0, "vscode", Some("continuum")), 20),
            None
        );

        // Still vscode 6 minutes later -- session now >= 5 min old.
        let t1 = t0 + Duration::minutes(6);
        assert_eq!(
            tracker.observe(&mk_sig(t1, "vscode", Some("continuum")), 20),
            None
        );

        // Task B5: the project hint stays put so this test isolates the
        // *process*-change rule — a changed hint would fire the
        // unconditional project boundary (case 3b) first.
        let t2 = t1 + Duration::seconds(10);
        let ended = tracker
            .observe(&mk_sig(t2, "chrome", Some("continuum")), 20)
            .expect("process change after >=5min session should emit a boundary");
        assert_eq!(ended.started, t0);
        assert_eq!(ended.ended, t1); // previous ts, not the process-change ts
        assert_eq!(ended.process, "vscode");
        assert_eq!(ended.project_hint.as_deref(), Some("continuum"));

        // Tracker restarted with chrome as the new session; not itself a
        // boundary.
        let t3 = t2 + Duration::seconds(5);
        assert_eq!(
            tracker.observe(&mk_sig(t3, "chrome", Some("continuum")), 20),
            None
        );
    }

    /// M1 fix regression: `with_session_min_minutes` must actually change
    /// the process-change boundary threshold — a ~2-minute-old session that
    /// would be absorbed under the 5-minute default must fire a boundary
    /// once the tracker is configured with a lower floor.
    #[test]
    fn with_session_min_minutes_changes_the_process_change_floor() {
        let mut tracker = SessionTracker::with_session_min_minutes(1);
        let t0 = base_ts();
        assert_eq!(
            tracker.observe(&mk_sig(t0, "vscode", Some("continuum")), 20),
            None
        );

        // Still vscode 2 minutes later -- advances last_activity so the
        // next check's session_len is actually 2 minutes, not 0.
        let t1 = t0 + Duration::minutes(2);
        assert_eq!(
            tracker.observe(&mk_sig(t1, "vscode", Some("continuum")), 20),
            None
        );

        // Process change: 2-minute session_len would be absorbed under the
        // 5-minute default (see observe_never_emits_on_a_brief_process_flick
        // below), but clears this tracker's configured 1-minute floor.
        let t2 = t1 + Duration::seconds(10);
        let ended = tracker
            .observe(&mk_sig(t2, "chrome", Some("continuum")), 20)
            .expect("process change after >=1min session should emit a boundary");
        assert_eq!(ended.started, t0);
        assert_eq!(ended.process, "vscode");
    }

    #[test]
    fn observe_never_emits_on_a_brief_process_flick() {
        let mut tracker = SessionTracker::new();
        let t0 = base_ts();
        assert_eq!(
            tracker.observe(&mk_sig(t0, "vscode", Some("continuum")), 20),
            None
        );

        // Flick to notepad for 1 minute -- session so far is 0 min old,
        // well under the 5 min floor: absorbed, not a boundary. Task B5:
        // the project hint is unchanged (a notepad window inside the same
        // project); a changed hint is case 3b's job, tested separately in
        // `observe_emits_boundary_on_project_change_without_dwell_floor`.
        let t1 = t0 + Duration::minutes(1);
        assert_eq!(
            tracker.observe(&mk_sig(t1, "notepad", Some("continuum")), 20),
            None
        );

        // Back to vscode a minute later -- still no boundary, and the
        // flick never renamed the tracked process.
        let t2 = t1 + Duration::minutes(1);
        assert_eq!(
            tracker.observe(&mk_sig(t2, "vscode", Some("continuum")), 20),
            None
        );

        // Confirm via an idle boundary that the tracker's process/started
        // were never disturbed by the flick.
        let t3 = t2 + Duration::minutes(30);
        let ended = tracker
            .observe(&mk_sig(t3, "vscode", Some("continuum")), 20)
            .expect("idle gap should still emit");
        assert_eq!(ended.started, t0);
        assert_eq!(ended.ended, t2);
        assert_eq!(ended.process, "vscode");
    }

    #[test]
    fn observe_resets_after_emitting_and_can_emit_again() {
        let mut tracker = SessionTracker::new();
        let t0 = base_ts();
        tracker.observe(&mk_sig(t0, "vscode", Some("continuum")), 20);

        let t1 = t0 + Duration::minutes(25);
        let ended1 = tracker
            .observe(&mk_sig(t1, "vscode", Some("continuum")), 20)
            .expect("first idle boundary");
        assert_eq!(ended1.started, t0);
        assert_eq!(ended1.ended, t0);

        // A second, independent session/boundary cycle starting from t1.
        let t2 = t1 + Duration::minutes(25);
        let ended2 = tracker
            .observe(&mk_sig(t2, "vscode", Some("continuum")), 20)
            .expect("second idle boundary");
        assert_eq!(ended2.started, t1);
        assert_eq!(ended2.ended, t1);
    }

    /// Regression for the "unguarded non-monotonic `sig.ts` corrupts
    /// `SessionTracker` state" review finding: a signal that arrives with a
    /// timestamp *earlier* than the tracked `last_activity` (clock skew,
    /// sleep/resume, an out-of-order perception frame) must be ignored
    /// outright, not treated as a legitimate update that rewinds
    /// `last_activity` backwards.
    ///
    /// Asserted indirectly (no internal state accessor exists, matching
    /// this module's existing test style): a small idle limit is chosen
    /// such that the very next *in-order* signal would spuriously trigger
    /// an idle boundary if the out-of-order signal had been allowed to
    /// rewind `last_activity`, but correctly does not once the guard drops
    /// it. A genuine idle boundary afterward still fires and reports the
    /// correct (never-rewound) `last_activity`.
    #[test]
    fn observe_ignores_non_monotonic_signal_without_rewinding_state() {
        let mut tracker = SessionTracker::new();
        let idle_limit_min = 2;

        let t0 = base_ts();
        assert_eq!(
            tracker.observe(&mk_sig(t0, "vscode", Some("continuum")), idle_limit_min),
            None
        );

        // last_activity advances to t0 + 1min.
        let t1 = t0 + Duration::minutes(1);
        assert_eq!(
            tracker.observe(&mk_sig(t1, "vscode", Some("continuum")), idle_limit_min),
            None
        );

        // Out-of-order signal 30s *before* t0 (well before last_activity =
        // t1): must be dropped with no boundary and no state mutation.
        let out_of_order = t0 - Duration::seconds(30);
        assert_eq!(
            tracker.observe(
                &mk_sig(out_of_order, "vscode", Some("continuum")),
                idle_limit_min
            ),
            None
        );

        // t2 is 1min50s after the correct last_activity (t1) -- under the
        // 2 min idle limit, so no boundary. Had the out-of-order signal
        // above rewound last_activity to `out_of_order` (t0 - 30s), the
        // gap here would be 2min50s > 2min and would have incorrectly
        // fired a boundary.
        let t2 = t1 + Duration::seconds(110);
        assert_eq!(
            tracker.observe(&mk_sig(t2, "vscode", Some("continuum")), idle_limit_min),
            None
        );

        // A genuine idle gap now correctly ends the session at t2 (the
        // last real activity), started at t0 -- neither disturbed by the
        // dropped out-of-order signal.
        let t3 = t2 + Duration::minutes(5);
        let ended = tracker
            .observe(&mk_sig(t3, "vscode", Some("continuum")), idle_limit_min)
            .expect("genuine idle gap should still emit a boundary");
        assert_eq!(ended.started, t0);
        assert_eq!(ended.ended, t2);
    }

    /// Low-priority review nit: when a single `observe` call's signal
    /// satisfies both the idle-gap condition *and* the process-change
    /// condition at once, the idle check (evaluated first) must win --
    /// documented in [`SessionTracker::observe`]'s doc comment as cases 3
    /// vs. 4.
    #[test]
    fn observe_prefers_idle_boundary_when_both_conditions_coincide() {
        let mut tracker = SessionTracker::new();
        let idle_limit_min = 5;

        let t0 = base_ts();
        tracker.observe(&mk_sig(t0, "vscode", Some("continuum")), idle_limit_min);

        // Age the session past the 5 min process-change floor using two
        // small (<5min) gaps, so neither intermediate step trips the idle
        // check on its own -- only the cumulative `last_activity - started`
        // crosses 5 min.
        let t1 = t0 + Duration::minutes(3);
        tracker.observe(&mk_sig(t1, "vscode", Some("continuum")), idle_limit_min);
        let t2 = t1 + Duration::minutes(3);
        tracker.observe(&mk_sig(t2, "vscode", Some("continuum")), idle_limit_min);

        // t3 is both >5min idle-limit past t2 AND a different process from
        // a session already >=5min old (t2 - t0 = 6min) -- both boundary
        // conditions hold for this one `observe` call.
        let t3 = t2 + Duration::minutes(10);
        let ended = tracker
            .observe(&mk_sig(t3, "chrome", Some("web")), idle_limit_min)
            .expect("a boundary should fire");

        assert_eq!(ended.started, t0);
        assert_eq!(ended.ended, t2);
        assert_eq!(ended.process, "vscode");

        // Tracker restarted fresh from the triggering (chrome) signal.
        let t4 = t3 + Duration::seconds(1);
        assert_eq!(
            tracker.observe(&mk_sig(t4, "chrome", Some("web")), idle_limit_min),
            None
        );
    }

    /// Task B5 (spec §4.8): a project change is its own boundary, with no
    /// minimum-dwell floor — `project_hint` is the resolver's
    /// post-hysteresis output, which already debounced the switch.
    #[test]
    fn observe_emits_boundary_on_project_change_without_dwell_floor() {
        let mut tracker = SessionTracker::new();
        let t0 = base_ts();
        assert_eq!(
            tracker.observe(&mk_sig(t0, "vscode", Some("continuum")), 20),
            None
        );

        // Only 1 minute in — far under the 5 min process-change floor, so
        // case 4 could not fire here. The project change still ends it.
        let t1 = t0 + Duration::minutes(1);
        let ended = tracker
            .observe(&mk_sig(t1, "vscode", Some("simcharts")), 20)
            .expect("a project change should emit a boundary regardless of session length");
        assert_eq!(ended.started, t0);
        assert_eq!(ended.ended, t0); // last_activity before the change
        assert_eq!(ended.project_hint.as_deref(), Some("continuum"));
        assert_eq!(ended.process, "vscode");

        // The new session is attributed to the new project.
        let snap = tracker.snapshot().expect("a session is in progress");
        assert_eq!(snap.project_hint.as_deref(), Some("simcharts"));
        assert_eq!(snap.started, t1);
    }

    /// First adoption (`None` → `Some`) must NOT be a boundary: the
    /// session simply gains attribution it always had. Same for a hint
    /// that disappears (`Some` → `None`), which happens when the resolver
    /// loses confidence — that is not a new session.
    #[test]
    fn observe_does_not_emit_on_project_hint_adoption_or_loss() {
        let mut tracker = SessionTracker::new();
        let t0 = base_ts();
        assert_eq!(tracker.observe(&mk_sig(t0, "vscode", None), 20), None);

        let t1 = t0 + Duration::minutes(1);
        assert_eq!(
            tracker.observe(&mk_sig(t1, "vscode", Some("continuum")), 20),
            None
        );
        assert_eq!(
            tracker.snapshot().unwrap().project_hint.as_deref(),
            Some("continuum")
        );

        let t2 = t1 + Duration::minutes(1);
        assert_eq!(tracker.observe(&mk_sig(t2, "vscode", None), 20), None);
        assert_eq!(tracker.snapshot().unwrap().project_hint, None);
        // Still the original session.
        assert_eq!(tracker.snapshot().unwrap().started, t0);
    }

    /// An idle gap that *also* changes project is an idle boundary — the
    /// idle check is evaluated first (documented precedence).
    #[test]
    fn observe_prefers_idle_boundary_over_a_coincident_project_change() {
        let mut tracker = SessionTracker::new();
        let t0 = base_ts();
        tracker.observe(&mk_sig(t0, "vscode", Some("continuum")), 5);

        let t1 = t0 + Duration::minutes(30);
        let ended = tracker
            .observe(&mk_sig(t1, "vscode", Some("simcharts")), 5)
            .expect("a boundary should fire");
        // Idle semantics: ends at last_activity (t0), not at t1.
        assert_eq!(ended.ended, t0);
        assert_eq!(ended.project_hint.as_deref(), Some("continuum"));
    }

    #[test]
    fn snapshot_is_none_before_the_first_timestamped_signal() {
        let mut tracker = SessionTracker::new();
        assert_eq!(tracker.snapshot(), None);
        tracker.observe(&ActivitySignal::default(), 20);
        assert_eq!(tracker.snapshot(), None);
    }

    #[test]
    fn snapshot_tracks_last_activity_without_a_boundary() {
        let mut tracker = SessionTracker::new();
        let t0 = base_ts();
        tracker.observe(&mk_sig(t0, "vscode", Some("continuum")), 20);
        let t1 = t0 + Duration::minutes(2);
        assert_eq!(
            tracker.observe(&mk_sig(t1, "vscode", Some("continuum")), 20),
            None
        );
        let snap = tracker.snapshot().unwrap();
        assert_eq!(snap.started, t0);
        assert_eq!(snap.last_activity, t1);
        assert_eq!(snap.process, "vscode");
    }

    // --- write_session_summary tests -----------------------------------

    async fn append_events(vault: &Vault, started: DateTime<Utc>, texts: &[&str]) {
        for (i, text) in texts.iter().enumerate() {
            vault
                .append_event(NewEvent {
                    ts: Some(started + Duration::minutes(i as i64 + 1)),
                    kind: "distilled".to_string(),
                    text: text.to_string(),
                    project: None,
                    node_id: None,
                    reference: None,
                    local_only: false,
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn write_session_summary_happy_path_creates_confirmed_session_note() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        let started = base_ts();
        let ended_ts = started + Duration::minutes(30);
        append_events(
            &vault,
            started,
            &[
                "Opened continuum-core",
                "Fixed a bug in curator",
                "Ran tests",
            ],
        )
        .await;

        let ended = EndedSession {
            started,
            ended: ended_ts,
            project_hint: Some("continuum".to_string()),
            process: "Code.exe".to_string(),
        };

        let summary_md = "## Goal\nFix curator bug\n## Changed\n- curator/session.rs\n\
             ## Problem\nnone\n## Tried\n\u{2013}\n## Result\nDone\n## Next step\nShip it";
        let llm = MockLlm::scripted(vec![summary_md.to_string()]);

        let id = write_session_summary(&vault, &llm, &ended)
            .await
            .unwrap()
            .expect("should create a note");
        let note = vault.get(&id).await.unwrap();

        assert_eq!(note.frontmatter.node_type, NodeType::Session);
        assert_eq!(note.frontmatter.status, NodeStatus::Confirmed);
        assert_eq!(note.frontmatter.source, Source::Observed);
        assert_eq!(
            note.frontmatter.title,
            "Session: Code.exe — 2026-01-01 09:30"
        );
        // Task B7 (spec §4.9): the body is the model's summary PLUS the
        // normalized `open_task:` trailer the continuation resolver reads.
        // The model omitted the line here, so it is derived from the
        // "## Next step" section.
        assert!(note.body.starts_with(summary_md), "{}", note.body);
        assert!(note.body.ends_with("open_task: Ship it"), "{}", note.body);
        assert_eq!(note.frontmatter.project.as_deref(), Some("continuum"));
        assert!(note
            .frontmatter
            .relations
            .iter()
            .any(|r| r.to == "continuum" && r.rel == "belongs_to"));
        assert_eq!(llm.calls(), 1);
    }

    /// Spec-gap 1 regression: writing a session summary must also append a
    /// `"session"`-kind vault timeline event linked to the new note via
    /// `node_id`, so the Memory tab's timeline strip can jump straight to
    /// it.
    #[tokio::test]
    async fn write_session_summary_appends_timeline_event_with_node_id() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        let started = base_ts();
        let ended_ts = started + Duration::minutes(30);
        append_events(
            &vault,
            started,
            &["Opened continuum-core", "Fixed a bug", "Ran tests"],
        )
        .await;

        let ended = EndedSession {
            started,
            ended: ended_ts,
            project_hint: Some("continuum".to_string()),
            process: "Code.exe".to_string(),
        };

        let summary_md = "## Goal\nX\n## Changed\n- none\n## Problem\nnone\n## Tried\n\u{2013}\n## Result\nDone\n## Next step\nnone";
        let llm = MockLlm::scripted(vec![summary_md.to_string()]);

        let id = write_session_summary(&vault, &llm, &ended)
            .await
            .unwrap()
            .expect("should create a note");

        let events = vault.events(&EventRange::default()).await.unwrap();
        let session_event = events
            .iter()
            .find(|e| e.kind == "session")
            .expect("a session-kind timeline event should have been appended");
        assert_eq!(session_event.node_id.as_deref(), Some(id.as_str()));
        assert_eq!(session_event.text, "Session: Code.exe — 2026-01-01 09:30");
        assert_eq!(session_event.project.as_deref(), Some("continuum"));
    }

    #[tokio::test]
    async fn write_session_summary_without_project_hint_has_no_relation() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        let started = base_ts();
        let ended_ts = started + Duration::minutes(15);
        append_events(&vault, started, &["event one", "event two", "event three"]).await;

        let ended = EndedSession {
            started,
            ended: ended_ts,
            project_hint: None,
            process: "explorer.exe".to_string(),
        };

        let summary_md = "## Goal\nBrowse files\n## Changed\n- none\n## Problem\nnone\n\
             ## Tried\n\u{2013}\n## Result\nDone\n## Next step\nnone";
        let llm = MockLlm::scripted(vec![summary_md.to_string()]);

        let id = write_session_summary(&vault, &llm, &ended)
            .await
            .unwrap()
            .expect("should create a note");
        let note = vault.get(&id).await.unwrap();

        assert!(note.frontmatter.relations.is_empty());
        assert_eq!(note.frontmatter.project, None);
    }

    #[tokio::test]
    async fn write_session_summary_skip_reply_returns_none_and_creates_no_note() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        let started = base_ts();
        let ended_ts = started + Duration::minutes(15);
        append_events(&vault, started, &["idle event", "idle event", "idle event"]).await;

        let ended = EndedSession {
            started,
            ended: ended_ts,
            project_hint: None,
            process: "explorer.exe".to_string(),
        };

        // Reply carries whitespace around SKIP -- must still be recognized
        // after trimming.
        let llm = MockLlm::scripted(vec!["  SKIP  \n".to_string()]);

        let result = write_session_summary(&vault, &llm, &ended).await.unwrap();
        assert!(result.is_none());
        assert_eq!(llm.calls(), 1);
        assert_eq!(vault.info().await.unwrap().note_count, 0);
    }

    /// Task B7 (spec §4.9): whatever the model returns, the stored body
    /// ends with exactly one well-formed `open_task:` line — the
    /// continuation resolver reads it without an LLM.
    #[tokio::test]
    async fn write_session_summary_normalizes_the_open_task_trailer() {
        for (reply, expected_tail) in [
            // Model emitted the trailer where it was asked to.
            (
                "## Goal\nX\n## Next step\nrun the gates\n\nopen_task: finish the packager",
                "open_task: finish the packager",
            ),
            // Model emitted it mid-body and wrapped in markdown.
            (
                "## Goal\nX\n- **open_task:** finish the packager\n## Result\nok",
                "open_task: finish the packager",
            ),
            // Model omitted it — derived from "## Next step".
            (
                "## Goal\nX\n## Next step\nrun the gates",
                "open_task: run the gates",
            ),
            // Nothing left open.
            ("## Goal\nX\n## Next step\nnone", "open_task: none"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let vault = Vault::open(tmp.path()).await.unwrap();
            let started = base_ts();
            append_events(&vault, started, &["one", "two", "three"]).await;
            let ended = EndedSession {
                started,
                ended: started + Duration::minutes(20),
                project_hint: None,
                process: "Code.exe".to_string(),
            };
            let llm = MockLlm::scripted(vec![reply.to_string()]);

            let id = write_session_summary(&vault, &llm, &ended)
                .await
                .unwrap()
                .expect("should create a note");
            let note = vault.get(&id).await.unwrap();

            assert!(
                note.body.trim_end().ends_with(expected_tail),
                "reply {reply:?} produced body {:?}",
                note.body
            );
            assert_eq!(
                note.body.matches("open_task:").count(),
                1,
                "exactly one trailer line, body {:?}",
                note.body
            );
        }
    }

    #[tokio::test]
    async fn write_session_summary_too_few_events_skips_without_calling_llm() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        let started = base_ts();
        let ended_ts = started + Duration::minutes(10);
        // Only 2 events in the window -- below the 3-event floor.
        append_events(&vault, started, &["one", "two"]).await;

        let ended = EndedSession {
            started,
            ended: ended_ts,
            project_hint: None,
            process: "explorer.exe".to_string(),
        };

        // Empty script: any unexpected `complete()` call errors and this
        // test fails via the `.unwrap()` below, proving the LLM is never
        // called for a too-short session.
        let llm = MockLlm::scripted(vec![]);

        let result = write_session_summary(&vault, &llm, &ended).await.unwrap();
        assert!(result.is_none());
        assert_eq!(llm.calls(), 0);
        assert_eq!(vault.info().await.unwrap().note_count, 0);
    }
}
