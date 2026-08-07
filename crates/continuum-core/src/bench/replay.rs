//! # Replay harness (context engine spec §9, Task C6)
//!
//! Drives the context-engine pipeline over a recorded/synthetic JSONL under
//! a **fake clock** and with **no live watchers**. Every frame and every
//! event arrives from the file; `now` is always `base + t_ms`, never
//! `Utc::now()`, so two runs over the same input produce byte-identical
//! metrics.
//!
//! ## What it actually drives
//!
//! The same functions the frame loop in `bin/continuum.rs` calls, in the
//! same order — this is a re-wiring, not a re-implementation:
//!
//! 1. [`ProjectResolver::observe`] (spec §4.3 resolution + hysteresis), and
//!    [`project_switch_event`] on a flip.
//! 2. [`SessionStateHub::apply_frame`] (spec §4.8 mechanical fields).
//! 3. Classification (spec §4.7) — mock or the real triage model.
//! 4. [`plan_consumption`] (spec §4.7 consumption: event + vault candidate
//!    + decision column), then [`EventSender::send`].
//! 5. [`SessionStateHub::apply_context_event`] for every emitted event.
//!    The runtime gets this through `EventSender::with_observer`; the
//!    replay calls it directly at the same point in the order, because the
//!    harness also wants the event in its own bookkeeping.
//! 6. The spec §4.8 inference trigger, evaluated per frame (the runtime
//!    ticks it every 30 s; `infer_min_interval_secs` is what actually
//!    rate-limits it either way), skipped while idle per spec §4.11.
//!
//! Deliberately **not** driven: watchers, capture, the publisher, the wake
//! path. Those have their own tests; a bench that spun them would measure
//! the machine it runs on.
//!
//! ## Mock vs live, and what each proves
//!
//! - **Mock** (default): [`mock_classify`] and [`mock_infer_json`] stand in
//!   for the LLM. Deterministic, offline, GPU-free, a second per run. It
//!   proves the *plumbing*: resolution, hysteresis, zone propagation,
//!   dedupe keys, session mechanics, distillation selection, the privacy
//!   gates. It does **not** prove that Qwen classifies a screen correctly —
//!   the mock's classification is scripted, so a recall number from mock
//!   mode is a regression gate on the harness and the mechanical path, not
//!   a model score.
//! - **Live** (`--live`): the real [`TriageLayer`] classifies and the real
//!   model infers. Requires the model file; that is the run whose recall
//!   number says something about model quality.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use continuum_memory::NoteDraft;

use crate::bench::fixture::{Checkpoint, Expected};
use crate::bench::record::RecordLine;
use crate::config::ContinuumConfig;
use crate::context::project::{CurrentProject, FrameInput, ProjectResolver, ProjectSwitch};
use crate::context::session_state::{
    build_inference_prompt, parse_inference, window_is_local_only, EventDigest, SessionState,
    SessionStateHub, INFERENCE_EVENT_WINDOW,
};
use crate::curator::CuratorLlm;
use crate::memory::events::{
    project_switch_event, ContextEvent, EventSender, EventSource, EventType,
};
use crate::senses::privacy::PrivacyFilter;
use crate::senses::types::PerceptionFrame;
use crate::triage::consume::{plan_consumption, CandidateGate, ClassificationPolicy};
use crate::triage::{Classification, TriageDecision};

/// How classification (spec §4.7) is produced during a replay.
pub enum Classifier {
    /// The deterministic stand-in ([`mock_classify`]).
    Mock,
    /// The real triage model.
    Live(Arc<crate::triage::llm::TriageLayer>),
}

/// How session-state inference (spec §4.8) is produced during a replay.
pub enum Inferencer {
    /// The deterministic stand-in ([`mock_infer_json`]), still routed
    /// through the production [`parse_inference`] ladder.
    Mock,
    /// A real background LLM (`TriageLayer` implements [`CuratorLlm`]).
    Live(Arc<dyn CuratorLlm>),
    /// No inference at all — goal/task stay `None` (used by the benches
    /// that only care about events).
    Off,
}

/// Knobs for one replay.
pub struct ReplayOptions {
    /// The instant `t_ms = 0` maps to.
    pub base: DateTime<Utc>,
    /// The config the replay honours — same struct the runtime loads, so
    /// thresholds under test are the shipped ones.
    pub config: ContinuumConfig,
    /// Classification source.
    pub classifier: Classifier,
    /// Inference source.
    pub inference: Inferencer,
    /// When set, every replayed frame is written to this raw log exactly
    /// as the runtime's frame loop does. Only the harnesses that read the
    /// raw log back (memory precision, via the distiller's frame-fallback
    /// query) need it; the others skip the writes.
    pub persist_frames: Option<crate::memory::raw_log::RawLog>,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            base: crate::bench::fixture::fixture_base(),
            config: fixture_config(),
            classifier: Classifier::Mock,
            inference: Inferencer::Mock,
            persist_frames: None,
        }
    }
}

/// A [`ContinuumConfig`] wired for the committed fixture: the fixture's
/// two projects, everything else at shipped defaults.
pub fn fixture_config() -> ContinuumConfig {
    ContinuumConfig {
        projects: crate::bench::fixture::fixture_projects(),
        ..ContinuumConfig::default()
    }
}

/// What the engine believed at a checkpoint, projected onto the five
/// labeled fields.
///
/// The projections are fixed here, once, so every bench scores the same
/// thing:
///
/// | field | source |
/// |---|---|
/// | `project` | [`SessionState::active_project`] |
/// | `goal` / `task` | [`SessionState::current_goal`] / [`SessionState::current_task`] |
/// | `blocker` | [`SessionState::last_error`] |
/// | `last_action` | the newer of `last_user_command` / `last_success`, falling back to the newest **non-routine** event in the session ring |
///
/// The `last_action` fallback skips `routine` events on purpose: spec §4.7
/// defines `routine` as "activity with no particular signal", and answering
/// "what just happened" with "the editor was still open" is not an action.
/// A ring holding nothing but routine events falls all the way back to the
/// newest one rather than reporting nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// Resolved project id.
    pub project: Option<String>,
    /// Inferred goal.
    pub goal: Option<String>,
    /// Inferred task.
    pub task: Option<String>,
    /// Latest observed error.
    pub blocker: Option<String>,
    /// Latest thing the user did or that finished.
    pub last_action: Option<String>,
}

/// Projects a session state (plus its event ring) onto [`Observed`].
pub fn observe_state(state: &SessionState, recent: &[EventDigest]) -> Observed {
    let last_action = match (&state.last_user_command, &state.last_success) {
        (Some(cmd), Some(ok)) => Some(if cmd.at >= ok.at {
            cmd.text.clone()
        } else {
            ok.text.clone()
        }),
        (Some(cmd), None) => Some(cmd.text.clone()),
        (None, Some(ok)) => Some(ok.text.clone()),
        (None, None) => recent
            .iter()
            .rev()
            .find(|digest| digest.event_type != "routine" && !digest.summary.is_empty())
            .or_else(|| recent.iter().rev().find(|d| !d.summary.is_empty()))
            .map(|digest| digest.summary.clone()),
    };
    Observed {
        project: state.active_project.clone(),
        goal: state.current_goal.clone(),
        task: state.current_task.clone(),
        blocker: state.last_error.as_ref().map(|e| e.text.clone()),
        last_action,
    }
}

/// One checkpoint, its label, and what the engine actually held.
#[derive(Debug, Clone)]
pub struct CheckpointObservation {
    /// Offset into the fixture.
    pub t_ms: i64,
    /// The label.
    pub expected: Expected,
    /// The projection.
    pub observed: Observed,
    /// Full session state at that moment (for benches that want more).
    pub state: SessionState,
}

/// Everything one replay produced.
pub struct ReplayResult {
    /// Frames fed through the pipeline.
    pub frames: usize,
    /// Every event handed to the sender, in order: the fixture's collector
    /// events plus the classification-derived ones.
    pub emitted: Vec<ContextEvent>,
    /// Just the classification-derived events (spec §4.7).
    pub classified: Vec<ContextEvent>,
    /// Vault candidates the consumption plan produced *and* the candidate
    /// gate admitted, in order.
    pub candidates: Vec<NoteDraft>,
    /// Candidates the gate suppressed because the events row they belong
    /// to was still open (spec §4.6 compression).
    pub candidates_suppressed: usize,
    /// Post-hysteresis project flips.
    pub switches: Vec<ProjectSwitch>,
    /// One entry per label, in label order.
    pub checkpoints: Vec<CheckpointObservation>,
    /// Inference attempts that produced a parsed result.
    pub inferences: usize,
    /// Inference attempts (including ones whose reply did not parse).
    pub inference_attempts: usize,
    /// Frames whose `idle_seconds` put the engine in idle mode.
    pub idle_frames: usize,
    /// Inference attempts that fired on an idle frame. Spec §4.11 says
    /// this must be zero.
    pub idle_inferences: usize,
    /// Per-frame classification latency in ms (meaningful in live mode).
    pub classify_ms: Vec<f64>,
    /// Session state at the end of the recording.
    pub final_state: SessionState,
}

/// Replays `lines`, scoring at `labels`, sending every event to `events`.
///
/// `events` is the seam: pass [`EventSender::log_only`] for a harness that
/// only needs in-memory results, or the sender from
/// [`crate::memory::events::spawn_event_writer`] to exercise the real
/// dedupe + SQLite path.
pub async fn replay(
    lines: &[RecordLine],
    labels: &[Checkpoint],
    opts: &ReplayOptions,
    events: &EventSender,
) -> anyhow::Result<ReplayResult> {
    let cfg = &opts.config;
    let privacy = PrivacyFilter::from_config(&cfg.context, &cfg.privacy);
    let known_projects: HashSet<String> = cfg.projects.known.iter().map(|p| p.id.clone()).collect();
    let mut resolver = ProjectResolver::from_config(&cfg.projects);
    let session = SessionStateHub::new();
    // Same gate the runtime's `ClassificationConsumer` applies (spec §4.6
    // compression): one vault candidate per collapsed events row. The
    // harness must not measure a memory population the runtime would
    // never have written.
    let mut candidate_gate = CandidateGate::new(cfg.events.collapse_window_minutes);

    let mut result = ReplayResult {
        frames: 0,
        emitted: Vec::new(),
        classified: Vec::new(),
        candidates: Vec::new(),
        candidates_suppressed: 0,
        switches: Vec::new(),
        checkpoints: Vec::new(),
        inferences: 0,
        inference_attempts: 0,
        idle_frames: 0,
        idle_inferences: 0,
        classify_ms: Vec::new(),
        final_state: SessionState::default(),
    };

    let mut pending_labels = labels.iter().peekable();
    let mut current: Option<CurrentProject> = None;

    for line in lines {
        // Capture every checkpoint the clock has passed before this line
        // is applied — a checkpoint at t answers "what did the engine
        // believe at t", not "after the next thing happened".
        while pending_labels
            .peek()
            .is_some_and(|checkpoint| checkpoint.t_ms < line.t_ms)
        {
            let checkpoint = pending_labels.next().expect("peeked");
            result.checkpoints.push(capture(&session, checkpoint));
        }

        let now = opts.base + Duration::milliseconds(line.t_ms);

        if let Some(recorded) = line.as_frame() {
            let mut frame = recorded.clone();
            // The fake clock owns time: a recording replays at its own
            // relative offsets regardless of when it was captured.
            frame.ts = now;
            frame.screen.ts = now;
            frame.context.ts = now;
            if let Some(audio) = frame.audio.as_mut() {
                audio.ts = now;
            }
            result.frames += 1;
            if let Some(raw_log) = &opts.persist_frames {
                raw_log.write_frame(&frame).await?;
            }

            // 1. Resolution + hysteresis (spec §4.3).
            let outcome = resolver.observe(&FrameInput {
                process_name: &frame.context.foreground_process_name,
                window_title: &frame.context.foreground_window_title,
                recent_file_path: None,
                ts: now,
            });
            current = outcome.current.clone();
            if let Some(switch) = outcome.switched.clone() {
                let event = project_switch_event(
                    switch.from.as_deref(),
                    &switch.to,
                    &frame.context.foreground_process_name,
                    &frame.context.foreground_window_title,
                    outcome.current.as_ref().and_then(|p| p.zone),
                    switch.ts,
                );
                emit(&mut result, &session, cfg, events, event);
                result.switches.push(switch);
            }

            // 2. Mechanical session-state update (spec §4.8).
            session.apply_frame(&frame, current.as_ref());

            // 3. Classification (spec §4.7).
            let started = std::time::Instant::now();
            let (decision, classification) = match &opts.classifier {
                Classifier::Mock => mock_classify(&frame),
                Classifier::Live(triage) => {
                    let summary = session.snapshot().render_memory_summary(
                        now,
                        600,
                        cfg.session_state.confidence_floor,
                    );
                    let output = triage.evaluate(&frame, &summary).await;
                    (output.decision, output.classification)
                }
            };
            result
                .classify_ms
                .push(started.elapsed().as_secs_f64() * 1_000.0);

            // 4. Consumption (spec §4.7): event + candidate + column.
            let policy = ClassificationPolicy {
                privacy: &privacy,
                ttl: cfg.memory.candidate_ttl_days,
                known_projects: &known_projects,
            };
            let plan = plan_consumption(
                &policy,
                &frame,
                &decision,
                classification.as_ref(),
                current.as_ref(),
                now,
            );
            let admitted = match (plan.event.as_ref(), plan.candidate.is_some()) {
                (Some(event), true) => {
                    candidate_gate.admit(&crate::memory::events::dedupe_key(event), now)
                }
                _ => true,
            };
            if let Some(event) = plan.event {
                result.classified.push(event.clone());
                emit(&mut result, &session, cfg, events, event);
            }
            if let Some(candidate) = plan.candidate {
                if admitted {
                    result.candidates.push(candidate);
                } else {
                    result.candidates_suppressed += 1;
                }
            }
            if let Some(raw_log) = &opts.persist_frames {
                raw_log.set_triage_decision(frame.id, &plan.column).await?;
            }

            // 6. Inference trigger (spec §4.8), paused while idle (§4.11).
            let idle = cfg.performance.idle_pause_after_secs > 0
                && frame.context.idle_seconds >= cfg.performance.idle_pause_after_secs;
            if idle {
                result.idle_frames += 1;
            }
            run_inference(&session, opts, now, idle, &mut result).await?;
        } else if let Some(recorded) = line.as_event() {
            let mut event = recorded.clone();
            event.ts = now;
            // Producers below the resolver leave `project_id` unset; the
            // events writer stamps it at flush. The replay does the same
            // thing at the same point so in-memory and persisted events
            // agree.
            if event.project_id.is_none() {
                if let Some(project) = &current {
                    event.project_id = Some(project.id.clone());
                }
            }
            // The voice endpoint is what tells session state that the user
            // asked for something (spec §4.8 `last_user_command`).
            if event.event_type == EventType::VoiceCommand {
                session.note_user_command(&event.summary, "voice", now);
            }
            emit(&mut result, &session, cfg, events, event);
        }
    }

    for checkpoint in pending_labels {
        result.checkpoints.push(capture(&session, checkpoint));
    }
    result.final_state = session.snapshot();
    Ok(result)
}

/// Sends an event and applies it everywhere the runtime would.
fn emit(
    result: &mut ReplayResult,
    session: &SessionStateHub,
    cfg: &ContinuumConfig,
    events: &EventSender,
    event: ContextEvent,
) {
    // Registry violations are dropped by the sender and must not be
    // counted as emitted — the harness would otherwise report a collapse
    // rate over events that never existed.
    if !event.event_type.valid_for(event.source) {
        tracing::warn!(
            layer = "bench",
            component = "replay",
            "fixture event violates the §4.6 registry; dropping it like the sender would"
        );
        return;
    }
    session.apply_context_event(&event, &cfg.session_state);
    events.send(event.clone());
    result.emitted.push(event);
}

fn capture(session: &SessionStateHub, checkpoint: &Checkpoint) -> CheckpointObservation {
    let state = session.snapshot();
    let recent = session.recent_events(8);
    CheckpointObservation {
        t_ms: checkpoint.t_ms,
        expected: checkpoint.expected.clone(),
        observed: observe_state(&state, &recent),
        state,
    }
}

async fn run_inference(
    session: &SessionStateHub,
    opts: &ReplayOptions,
    now: DateTime<Utc>,
    idle: bool,
    result: &mut ReplayResult,
) -> anyhow::Result<()> {
    if matches!(opts.inference, Inferencer::Off) {
        return Ok(());
    }
    let cfg = &opts.config.session_state;
    if session.trigger(cfg, now, idle).is_none() {
        return Ok(());
    }
    session.mark_inference_attempt(now);
    result.inference_attempts += 1;
    if idle {
        // Cannot happen — `trigger` returns None while idle — but counted
        // rather than assumed, so the §4.11 assertion is measured.
        result.idle_inferences += 1;
    }
    let state = session.snapshot();
    let window = session.recent_events(INFERENCE_EVENT_WINDOW);
    let local_only = window_is_local_only(&window);
    let raw = match &opts.inference {
        Inferencer::Mock => mock_infer_json(&state, &window, cfg.significant_importance),
        Inferencer::Live(llm) => {
            let prompt = build_inference_prompt(&state, &window, now);
            match llm.complete(&prompt, cfg.infer_max_tokens).await {
                Ok(raw) => raw,
                Err(error) => {
                    tracing::debug!(
                        layer = "bench",
                        component = "replay",
                        error = %error,
                        "inference call failed; keeping previous state"
                    );
                    return Ok(());
                }
            }
        }
        Inferencer::Off => unreachable!("returned above"),
    };
    if let Some(parsed) = parse_inference(&raw, cfg) {
        session.apply_inference(&parsed, local_only, now);
        result.inferences += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deterministic stand-ins
// ---------------------------------------------------------------------------

/// The mock classifier (spec §4.7 stand-in).
///
/// A small rule ladder over the frame's own facts, in priority order:
///
/// 1. The `never_observe` sentinel classifies to nothing — an excluded
///    window is not observed, and [`plan_consumption`] would drop the event
///    anyway.
/// 2. A frame carrying speech becomes a `task_progress` event whose
///    summary is the transcript (source `audio`, per
///    `classification_source`).
/// 3. `has_error_visible` becomes an `error` event whose summary is the
///    caption. Importance 0.7 clears the session-state significance floor,
///    and `should_store` proposes a vault candidate.
/// 4. A caption that reads like a finished run becomes a `success` event.
/// 5. An empty caption (idle) classifies to nothing.
/// 6. Anything else is `routine` — which spec §4.7 maps to *no* vault
///    candidate, so routine activity never pollutes memory.
///
/// The decision is always [`TriageDecision::Ignore`]: a bench must not
/// depend on wake behaviour, and `should_store` alone drives the candidate.
pub fn mock_classify(frame: &PerceptionFrame) -> (TriageDecision, Option<Classification>) {
    let ignore = TriageDecision::Ignore;
    if frame.context.foreground_process_name == crate::senses::privacy::EXCLUDED_PROCESS {
        return (ignore, None);
    }
    if let Some(audio) = frame
        .audio
        .as_ref()
        .filter(|a| !a.transcript.trim().is_empty())
    {
        return (
            ignore,
            Some(Classification {
                event_type: EventType::TaskProgress,
                project: None,
                importance: 0.5,
                confidence: 0.8,
                summary: audio.transcript.trim().to_string(),
                should_store: false,
            }),
        );
    }
    let caption = frame.screen.description.trim();
    if frame.screen.has_error_visible {
        return (
            ignore,
            Some(Classification {
                event_type: EventType::Error,
                project: None,
                importance: 0.7,
                confidence: 0.8,
                summary: caption.to_string(),
                should_store: true,
            }),
        );
    }
    if caption.is_empty() {
        return (ignore, None);
    }
    let lower = caption.to_lowercase();
    if ["finished", "passed", "0 errors", "0 failed", "succeeded"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return (
            ignore,
            Some(Classification {
                event_type: EventType::Success,
                project: None,
                importance: 0.6,
                confidence: 0.8,
                summary: caption.to_string(),
                should_store: true,
            }),
        );
    }
    (
        ignore,
        Some(Classification {
            event_type: EventType::Routine,
            project: None,
            importance: 0.2,
            confidence: 0.6,
            summary: caption.to_string(),
            should_store: false,
        }),
    )
}

/// The mock session-state inference (spec §4.8 stand-in). Returns the JSON
/// an LLM would, so the production [`parse_inference`] ladder still runs.
///
/// Rules, in the spirit of what the prompt asks the model for:
///
/// - Only events belonging to the **current project** (or unattributed)
///   are considered — after a project switch, the previous project's
///   errors say nothing about what is happening now.
/// - `task` is the newest *significant* event summary
///   (`importance >= significant_importance`) — the thing most recently in
///   the user's way or hands.
/// - `goal` is the project plus the **most repeated** normalized template
///   in that window ([`crate::memory::events::normalize_summary`]) — the
///   recurring thing is the larger objective.
/// - Confidence is 0.75 when there was a significant event to reason over,
///   and 0.2 otherwise. The low value is below the shipped
///   `confidence_floor`, so [`parse_inference`] correctly drops goal/task
///   to `None` instead of storing a guess.
pub fn mock_infer_json(
    state: &SessionState,
    window: &[EventDigest],
    significant_importance: f32,
) -> String {
    let relevant: Vec<&EventDigest> = window
        .iter()
        .filter(|digest| match (&state.active_project, &digest.project_id) {
            (Some(active), Some(project)) => active == project,
            (_, None) => true,
            (None, Some(_)) => true,
        })
        .collect();
    let significant: Vec<&&EventDigest> = relevant
        .iter()
        .filter(|digest| digest.importance >= significant_importance && !digest.summary.is_empty())
        .collect();

    let task = significant.last().map(|digest| digest.summary.clone());

    let goal = task.as_ref().and_then(|_| {
        let mut counts: Vec<(String, usize, usize)> = Vec::new();
        for (index, digest) in significant.iter().enumerate() {
            let key = crate::memory::events::normalize_summary(&digest.summary);
            match counts.iter_mut().find(|(existing, _, _)| *existing == key) {
                Some(entry) => {
                    entry.1 += 1;
                    entry.2 = index;
                }
                None => counts.push((key, 1, index)),
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, count, last)| (*count, *last))
            .map(|(template, _, _)| match &state.active_project {
                Some(project) => format!("{project}: {template}"),
                None => template,
            })
    });

    let confidence = if task.is_some() { 0.75 } else { 0.2 };
    serde_json::json!({
        "goal": goal,
        "task": task,
        "confidence": confidence,
    })
    .to_string()
}

/// The event sources whose rows a bench treats as classification output
/// (spec §4.7): everything else came from a deterministic collector.
pub const CLASSIFIED_SOURCES: [EventSource; 2] = [EventSource::Screen, EventSource::Audio];

/// Waits until the events-writer task has accounted for `expected` events
/// (inserted plus collapsed) and drained its queue.
///
/// Benches poll the writer's own health counters rather than sleeping a
/// fixed amount: the writer batches on a 500 ms tick, and a fixed sleep is
/// either flaky or slow. Returns `false` on timeout so the caller can fail
/// loudly instead of scoring a half-written database.
pub async fn wait_for_writer(
    handle: &crate::memory::events::EventWriterHandle,
    expected: u64,
    timeout: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let accounted = handle.rows_written() + handle.rows_collapsed();
        if handle.queue_depth() == 0 && accounted >= expected {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                layer = "bench",
                component = "replay",
                expected = expected,
                accounted = accounted,
                depth = handle.queue_depth(),
                "events writer did not drain before the deadline"
            );
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::fixture;

    async fn run() -> ReplayResult {
        let lines = fixture::synthetic_narrative();
        let labels = fixture::synthetic_labels();
        replay(
            &lines,
            &labels,
            &ReplayOptions::default(),
            &EventSender::log_only(),
        )
        .await
        .expect("replay succeeds")
    }

    #[tokio::test]
    async fn replay_is_deterministic() {
        let a = run().await;
        let b = run().await;
        assert_eq!(a.frames, b.frames);
        assert_eq!(a.emitted.len(), b.emitted.len());
        assert_eq!(a.classified.len(), b.classified.len());
        assert_eq!(a.candidates.len(), b.candidates.len());
        assert_eq!(a.inferences, b.inferences);
        assert_eq!(a.switches, b.switches);
        assert_eq!(a.final_state, b.final_state);
        for (x, y) in a.checkpoints.iter().zip(b.checkpoints.iter()) {
            assert_eq!(x.t_ms, y.t_ms);
            assert_eq!(x.observed, y.observed);
        }
        // Same input, same events, byte for byte.
        assert_eq!(
            serde_json::to_string(&a.emitted).unwrap(),
            serde_json::to_string(&b.emitted).unwrap()
        );
    }

    #[tokio::test]
    async fn replay_resolves_both_projects_and_switches_between_them() {
        let result = run().await;
        let ids: Vec<&str> = result.switches.iter().map(|s| s.to.as_str()).collect();
        assert_eq!(
            ids,
            vec!["continuum", "simcharts", "continuum"],
            "adoption, switch out, switch back"
        );
        // Hysteresis: the flip lags the first simcharts frame by at least
        // `switch_min_secs` (20 s → two 10 s frames).
        let base = ReplayOptions::default().base;
        let flip = result.switches[1]
            .ts
            .signed_duration_since(base)
            .num_seconds();
        assert!((800..=830).contains(&flip), "flipped at t={flip}");
    }

    #[tokio::test]
    async fn excluded_frames_produce_no_events_and_local_only_ones_are_tagged() {
        let result = run().await;
        assert!(
            !result
                .emitted
                .iter()
                .any(|e| e.application == crate::senses::privacy::EXCLUDED_PROCESS),
            "a never_observe window must produce no events row (spec §4.1)"
        );
        let private: Vec<&ContextEvent> = result
            .emitted
            .iter()
            .filter(|e| e.application == "msedge.exe")
            .collect();
        assert!(!private.is_empty(), "the InPrivate frames classify");
        assert!(
            private
                .iter()
                .all(|e| e.sensitivity == crate::memory::events::EventSensitivity::LocalOnly),
            "a local_only window propagates its zone onto the event (spec §4.1)"
        );
    }

    #[tokio::test]
    async fn no_inference_runs_while_idle() {
        let result = run().await;
        assert!(result.idle_frames > 10, "the fixture has an idle gap");
        assert_eq!(
            result.idle_inferences, 0,
            "spec §4.11: session inference pauses while idle"
        );
        assert!(result.inferences > 0, "inference ran outside the gap");
    }

    #[tokio::test]
    async fn the_build_failure_loop_collapses_onto_one_dedupe_key() {
        let result = run().await;
        let keys: HashSet<String> = result
            .classified
            .iter()
            .filter(|e| e.event_type == EventType::Error && e.application == "WindowsTerminal.exe")
            .map(crate::memory::events::dedupe_key)
            .collect();
        assert_eq!(
            keys.len(),
            1,
            "spec §4.6: classified summaries are not part of the key"
        );
        let occurrences = result
            .classified
            .iter()
            .filter(|e| e.event_type == EventType::Error && e.application == "WindowsTerminal.exe")
            .count();
        assert!(occurrences >= 20, "{occurrences} error occurrences");
    }

    #[tokio::test]
    async fn routine_frames_never_propose_a_memory_candidate() {
        let result = run().await;
        assert!(!result.candidates.is_empty());
        for candidate in &result.candidates {
            assert!(
                !candidate.title.to_lowercase().contains("private browsing"),
                "routine captions must not become candidates"
            );
        }
    }

    #[test]
    fn mock_classifier_ladder_covers_every_branch() {
        let lines = fixture::synthetic_narrative();
        let frames: Vec<&PerceptionFrame> = lines.iter().filter_map(|l| l.as_frame()).collect();

        let sentinel = frames
            .iter()
            .find(|f| f.context.foreground_process_name == crate::senses::privacy::EXCLUDED_PROCESS)
            .unwrap();
        assert!(mock_classify(sentinel).1.is_none());

        let speech = frames.iter().find(|f| f.audio.is_some()).unwrap();
        let classified = mock_classify(speech).1.unwrap();
        assert_eq!(classified.event_type, EventType::TaskProgress);
        assert_eq!(classified.summary, "ga door met de dashboard tests");

        let error = frames.iter().find(|f| f.screen.has_error_visible).unwrap();
        assert_eq!(mock_classify(error).1.unwrap().event_type, EventType::Error);

        let success = frames
            .iter()
            .find(|f| f.screen.description.contains("0 errors"))
            .unwrap();
        assert_eq!(
            mock_classify(success).1.unwrap().event_type,
            EventType::Success
        );

        let idle = frames
            .iter()
            .find(|f| f.screen.description.is_empty() && f.context.idle_seconds >= 300)
            .unwrap();
        assert!(mock_classify(idle).1.is_none());

        let routine = frames
            .iter()
            .find(|f| f.screen.description.contains("no errors visible"))
            .unwrap();
        assert_eq!(
            mock_classify(routine).1.unwrap().event_type,
            EventType::Routine
        );
    }

    #[test]
    fn mock_inference_is_below_the_floor_without_significant_events() {
        let cfg = crate::config::SessionStateConfig::default();
        let state = SessionState {
            active_project: Some("continuum".to_string()),
            ..SessionState::default()
        };
        let raw = mock_infer_json(&state, &[], cfg.significant_importance);
        let parsed = parse_inference(&raw, &cfg).unwrap();
        assert!(parsed.goal.is_none() && parsed.task.is_none());
        assert!(parsed.confidence < cfg.confidence_floor);
    }

    #[test]
    fn mock_inference_ignores_the_previous_projects_events() {
        let cfg = crate::config::SessionStateConfig::default();
        let now = Utc::now();
        let stale = EventDigest {
            ts: now,
            source: "screen".into(),
            event_type: "error".into(),
            application: "WindowsTerminal.exe".into(),
            project_id: Some("continuum".into()),
            summary: "cargo build failed: mismatched types".into(),
            importance: 0.7,
            local_only: false,
        };
        let state = SessionState {
            active_project: Some("simcharts".to_string()),
            ..SessionState::default()
        };
        let raw = mock_infer_json(
            &state,
            std::slice::from_ref(&stale),
            cfg.significant_importance,
        );
        let parsed = parse_inference(&raw, &cfg).unwrap();
        assert!(
            parsed.task.is_none(),
            "an event from another project must not become this project's task"
        );
    }

    #[test]
    fn observe_state_prefers_the_newest_of_command_and_success() {
        use crate::context::session_state::StampedText;
        let now = Utc::now();
        let mut state = SessionState {
            last_success: Some(StampedText::new(
                "build finished",
                now - Duration::minutes(5),
            )),
            last_user_command: Some(StampedText::new("ga door", now)),
            ..SessionState::default()
        };
        assert_eq!(
            observe_state(&state, &[]).last_action.as_deref(),
            Some("ga door")
        );
        state.last_user_command = Some(StampedText::new("ga door", now - Duration::hours(1)));
        assert_eq!(
            observe_state(&state, &[]).last_action.as_deref(),
            Some("build finished")
        );
        state.last_success = None;
        state.last_user_command = None;
        let digest = |event_type: &str, summary: &str| EventDigest {
            ts: now,
            source: "file".into(),
            event_type: event_type.into(),
            application: String::new(),
            project_id: None,
            summary: summary.into(),
            importance: 0.2,
            local_only: false,
        };
        // Routine noise after the real action must not become the answer.
        let ring = vec![
            digest("file_modified", "src/main.rs"),
            digest("routine", "VS Code is still open"),
        ];
        assert_eq!(
            observe_state(&state, &ring).last_action.as_deref(),
            Some("src/main.rs"),
            "falls back to the newest NON-routine ring event"
        );
        // …but a ring of nothing but routine still answers something.
        let only_routine = vec![digest("routine", "VS Code is still open")];
        assert_eq!(
            observe_state(&state, &only_routine).last_action.as_deref(),
            Some("VS Code is still open")
        );
    }
}
