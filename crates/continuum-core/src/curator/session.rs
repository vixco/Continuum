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
use continuum_memory::{EventRange, NodeStatus, NodeType, NoteDraft, Relation, Source, Vault};

use crate::curator::run::{build_events_block, ActivitySignal};
use crate::curator::CuratorLlm;

/// The session-summary prompt template, loaded at compile time from the
/// repo-root `prompts/curator-session.md`. `{{START}}`, `{{END}}`,
/// `{{PROCESS}}`, `{{PROJECT}}`, and `{{EVENTS}}` are substituted per-session
/// in [`write_session_summary`].
pub const SESSION_PROMPT: &str = include_str!("../../../../prompts/curator-session.md");

/// A session boundary from a foreground-process change is only worth
/// acting on once the user has spent at least this long on the process
/// being left — otherwise a quick alt-tab (checking Slack, glancing at a
/// browser tab) would fragment one real session into several noise-sized
/// ones. See [`SessionTracker::observe`].
const MIN_SESSION_MINUTES: i64 = 5;

/// In-progress session state tracked by [`SessionTracker`].
#[derive(Debug, Clone)]
struct SessionState {
    started: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    project_hint: Option<String>,
    process: String,
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
#[derive(Debug, Default)]
pub struct SessionTracker {
    current: Option<SessionState>,
}

impl SessionTracker {
    /// A tracker with no session in progress.
    pub fn new() -> Self {
        Self { current: None }
    }

    /// Feed the latest signal. Returns `Some(ended_session)` exactly when a
    /// boundary fired on this call; `None` otherwise — including when
    /// `sig.ts` is `None`, which happens before the perception loop has
    /// produced its first frame and leaves nothing to drive the state
    /// machine with.
    ///
    /// Boundary rules, checked in this order:
    /// 1. No session in progress: start one from `sig`. Never a boundary.
    /// 2. `sig.ts - last_activity > idle_limit_min`: the user went idle.
    ///    Ends the session at `last_activity` (the last moment there was
    ///    actually activity, not the idle-detecting signal's own time).
    /// 3. The foreground process changed *and* the session so far
    ///    (`last_activity - started`) is at least [`MIN_SESSION_MINUTES`]:
    ///    a real handoff to different work. Ends the session at
    ///    `last_activity` (the previous signal's time, before the change).
    /// 4. Otherwise: not a boundary. `last_activity` and `project_hint`
    ///    track the latest signal, but `process` is left untouched — a
    ///    brief flick to another window (case 3's condition without enough
    ///    elapsed time) never renames the session out from under itself, so
    ///    a string of short flicks can never itself accumulate into a false
    ///    boundary. If the flicked-to process sticks around long enough
    ///    that the *original* session's own elapsed time alone crosses
    ///    [`MIN_SESSION_MINUTES`], case 3 will still eventually fire and
    ///    close out the original session under its original process name.
    ///
    /// Whenever a boundary fires, the tracker immediately starts a fresh
    /// session from `sig` (case 1's logic) — the signal that closed the old
    /// session is also the first signal of the new one.
    pub fn observe(&mut self, sig: &ActivitySignal, idle_limit_min: u64) -> Option<EndedSession> {
        let ts = sig.ts?;

        let Some(state) = &mut self.current else {
            self.current = Some(SessionState {
                started: ts,
                last_activity: ts,
                project_hint: sig.project_hint.clone(),
                process: sig.process.clone(),
            });
            return None;
        };

        let idle_limit = Duration::minutes(idle_limit_min as i64);
        if ts - state.last_activity > idle_limit {
            let ended = EndedSession {
                started: state.started,
                ended: state.last_activity,
                project_hint: state.project_hint.clone(),
                process: state.process.clone(),
            };
            self.current = Some(SessionState {
                started: ts,
                last_activity: ts,
                project_hint: sig.project_hint.clone(),
                process: sig.process.clone(),
            });
            return Some(ended);
        }

        let session_len = state.last_activity - state.started;
        if sig.process != state.process && session_len >= Duration::minutes(MIN_SESSION_MINUTES) {
            let ended = EndedSession {
                started: state.started,
                ended: state.last_activity,
                project_hint: state.project_hint.clone(),
                process: state.process.clone(),
            };
            self.current = Some(SessionState {
                started: ts,
                last_activity: ts,
                project_hint: sig.project_hint.clone(),
                process: sig.process.clone(),
            });
            return Some(ended);
        }

        state.last_activity = ts;
        state.project_hint = sig.project_hint.clone();
        None
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
pub async fn write_session_summary(
    vault: &Vault,
    llm: &dyn CuratorLlm,
    ended: &EndedSession,
) -> anyhow::Result<Option<String>> {
    let events = vault
        .events(&EventRange {
            since: Some(ended.started),
            until: Some(ended.ended),
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

    let raw = llm.complete(&prompt, 700).await?;
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

    let draft = NoteDraft {
        node_type: NodeType::Session,
        title: format!(
            "Session: {} — {}",
            ended.process,
            ended.ended.format("%Y-%m-%d %H:%M")
        ),
        body: trimmed.to_string(),
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
        relations,
        tags: vec![],
    };

    let note = vault.create(draft).await?;
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

        let t2 = t1 + Duration::seconds(10);
        let ended = tracker
            .observe(&mk_sig(t2, "chrome", Some("web")), 20)
            .expect("process change after >=5min session should emit a boundary");
        assert_eq!(ended.started, t0);
        assert_eq!(ended.ended, t1); // previous ts, not the process-change ts
        assert_eq!(ended.process, "vscode");
        assert_eq!(ended.project_hint.as_deref(), Some("continuum"));

        // Tracker restarted with chrome as the new session; not itself a
        // boundary.
        let t3 = t2 + Duration::seconds(5);
        assert_eq!(
            tracker.observe(&mk_sig(t3, "chrome", Some("web")), 20),
            None
        );
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
        // well under the 5 min floor: absorbed, not a boundary.
        let t1 = t0 + Duration::minutes(1);
        assert_eq!(
            tracker.observe(&mk_sig(t1, "notepad", Some("scratch")), 20),
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
        assert_eq!(note.body, summary_md);
        assert_eq!(note.frontmatter.project.as_deref(), Some("continuum"));
        assert!(note
            .frontmatter
            .relations
            .iter()
            .any(|r| r.to == "continuum" && r.rel == "belongs_to"));
        assert_eq!(llm.calls(), 1);
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
