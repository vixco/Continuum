//! # Curator run loop
//!
//! The extraction pass (turns recent vault timeline events into candidate
//! memories, see [`crate::curator::extract`]) plus the ticker loop that
//! drives it on a schedule. This is the curator's own background task,
//! spawned once from the `continuum` binary — it never talks to the
//! orchestrator directly (non-negotiable #4: data flows up, commands flow
//! down; the curator writes to the vault, nothing more).

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use continuum_memory::{Event, EventRange, Vault};
use tokio::sync::watch;

use crate::config::{CuratorConfig, MemoryVaultConfig};
use crate::curator::conflict::detect_conflicts;
use crate::curator::extract::{
    candidate_to_draft, is_duplicate, parse_candidates, route_candidate, CandidateJson,
};
use crate::curator::session::{write_session_summary, EndedSession, SessionTracker};
use crate::curator::{CuratorLlm, SharedCuratorStatus};

/// The extraction prompt template, loaded at compile time from the
/// repo-root `prompts/curator-extract.md`. `{{MAX}}`, `{{EVENTS}}`, and
/// `{{RELATED}}` are substituted per-pass in [`extract_pass`].
pub const EXTRACT_PROMPT: &str = include_str!("../../../../prompts/curator-extract.md");

/// Per-frame signal from the perception loop (watch channel — latest wins).
///
/// The curator reads the most recent value opportunistically rather than
/// consuming a queue: a watch channel never blocks the sender (the main
/// perception loop) and the curator only cares about "what's true right
/// now", not a full history of every frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivitySignal {
    /// Best-effort project slug inferred from the foreground window/process
    /// (see `crate::memory::retrieval::infer_project_hint`, wired in Task 9).
    pub project_hint: Option<String>,
    /// Foreground process name at the time of the signal.
    pub process: String,
    /// Seconds since the user last interacted with the system.
    pub idle_seconds: u64,
    /// When this signal was captured. `None` before the first frame arrives.
    pub ts: Option<DateTime<Utc>>,
}

/// Renders vault events as `"HH:MM kind — text"` lines (oldest first) for
/// the extraction prompt's `{{EVENTS}}` slot. Falls back to the raw stored
/// timestamp string if it doesn't parse as RFC3339 (defensive — event
/// timestamps are always written via `Vault::append_event`, but a
/// hand-edited or migrated row should degrade gracefully, not panic).
///
/// `pub(crate)` (not private) so [`crate::curator::session::write_session_summary`]
/// can reuse the exact same rendering for its own `{{EVENTS}}` slot rather
/// than duplicating it.
pub(crate) fn build_events_block(events: &[Event]) -> String {
    events
        .iter()
        .map(|e| {
            let hm = DateTime::parse_from_rfc3339(&e.ts)
                .map(|dt| dt.format("%H:%M").to_string())
                .unwrap_or_else(|_| e.ts.clone());
            format!("{hm} {} — {}", e.kind, e.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Truncates `s` to at most `max` **characters** (not bytes — safe on
/// multi-byte UTF-8), appending an ellipsis when truncated. Used by
/// [`extract_pass`]'s prompt budgeting (I1 fix): a `char`-count proxy for
/// token count, deliberately simple and documented as such rather than
/// pulling in a real tokenizer just to bound prompt size.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Related-notes context for the extraction prompt's `{{RELATED}}` slot:
/// full-text search over the concatenated text of the most recent (up to
/// 3) events, top 8 hits rendered as `"- title: snippet"` lines with each
/// snippet capped to 100 characters (I1 fix — part of [`extract_pass`]'s
/// prompt budgeting). Empty (not an error) when there's nothing to search
/// on or nothing found — the template reads fine either way ("KNOWN
/// MEMORIES possibly related:" followed by nothing).
async fn build_related_block(vault: &Vault, events: &[Event]) -> anyhow::Result<String> {
    let recent = &events[events.len().saturating_sub(3)..];
    let query: String = recent
        .iter()
        .map(|e| e.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if query.trim().is_empty() {
        return Ok(String::new());
    }
    let hits = vault.search(&query, 8).await?;
    Ok(hits
        .iter()
        .map(|h| {
            format!(
                "- {}: {}",
                h.title,
                truncate_chars(&h.snippet.clone().unwrap_or_default(), 100)
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Attempts to write one candidate to the vault: near-duplicate check,
/// confidence-threshold routing, then `Vault::create`. Returns `Some(id)`
/// (the newly created note's id) on a successful write, `None` if the
/// candidate was skipped for any reason — duplicate, discarded by
/// threshold, *or* a vault operation erroring. The returned ids feed
/// [`detect_conflicts`] (Task 5): only freshly-written notes need a
/// conflict/supersede check.
///
/// Every internal error is caught and logged here rather than propagated:
/// one candidate hitting a vault I/O hiccup (a removed folder, a full
/// disk, a transient lock) must not abort the rest of the pass. Before this
/// was split out, `is_duplicate(..).await?` and `vault.create(..).await?`
/// used `extract_pass`'s own `?`, so a single bad candidate mid-batch threw
/// away every candidate after it *and* every candidate already written
/// before it went uncounted in the returned total — worse, since the pass
/// window still only advances on `Ok`, the next tick would re-fetch the
/// same events and re-propose the (rewritten, possibly reworded) already-
/// written candidates, risking near-duplicates slipping past `is_duplicate`
/// on retitled output.
async fn write_candidate(vault: &Vault, c: &CandidateJson, cfg: &CuratorConfig) -> Option<String> {
    match is_duplicate(vault, &c.title).await {
        Ok(true) => {
            tracing::debug!(
                layer = "memory",
                component = "curator",
                title = %c.title,
                "Skipping duplicate candidate"
            );
            return None;
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(
                layer = "memory",
                component = "curator",
                title = %c.title,
                error = %e,
                "Duplicate check failed for candidate; skipping it (pass continues)"
            );
            return None;
        }
    }

    let Some(status) = route_candidate(c, cfg) else {
        tracing::debug!(
            layer = "memory",
            component = "curator",
            title = %c.title,
            confidence = c.confidence,
            "Discarding low-confidence candidate"
        );
        return None;
    };

    match vault.create(candidate_to_draft(c, status)).await {
        Ok(note) => Some(note.frontmatter.id),
        Err(e) => {
            tracing::warn!(
                layer = "memory",
                component = "curator",
                title = %c.title,
                error = %e.user_message(),
                "Failed to write candidate to vault; skipping it (pass continues)"
            );
            None
        }
    }
}

/// Hard character ceiling on the assembled extraction prompt (I1 fix). A
/// `char`-count proxy for token count (documented, not exact — good enough
/// to keep a `context_size = 2048` / `max_tokens = 1024` local model from
/// overflowing without pulling in a real tokenizer just to bound prompt
/// size). See [`extract_pass`]'s budgeting loop.
const PROMPT_CHAR_BUDGET: usize = 3500;

/// Maximum number of (already-fetched) events folded into the extraction
/// prompt itself, most-recent-first (I1 fix). [`extract_pass`] can fetch up
/// to 200 events per pass (see its `EventRange` query) to keep the id
/// watermark advancing even when a backlog piles up, but only the newest
/// [`MAX_PROMPT_EVENTS`] of that batch are ever shown to the LLM — older
/// events within an overloaded window are deliberately dropped rather than
/// carried forward, since the watermark still advances past the whole
/// fetched batch (see the `max_id_seen` computation below).
const MAX_PROMPT_EVENTS: usize = 40;

/// Per-event text ceiling inside the prompt's `{{EVENTS}}` block (I1 fix).
const EVENT_TEXT_CHAR_CAP: usize = 160;

/// Outcome of one [`extract_pass`] call.
pub struct ExtractOutcome {
    /// Ids of vault notes actually created this pass — feeds
    /// [`detect_conflicts`] (Task 5) in [`curator_tick`].
    pub created_ids: Vec<String>,
    /// Highest event id seen in the *fetched* batch this pass (before the
    /// [`MAX_PROMPT_EVENTS`]/budget trimming applied to what's actually
    /// shown to the LLM) — `None` only when the window had no events at
    /// all. [`curator_tick`] advances its watermark to this value on any
    /// outcome where it's `Some`, success or LLM failure alike, since the
    /// whole batch was genuinely fetched and considered (C1 fix).
    pub max_id_seen: Option<i64>,
    /// `false` when the curator LLM completion call itself errored (model
    /// crashed/OOM/unreachable) rather than merely replying with unparsable
    /// JSON — the latter is handled internally via the existing
    /// one-retry-then-skip policy and still counts as `true` here. Drives
    /// [`curator_tick`]'s bounded-failure window-skip policy the same way a
    /// propagated `Err` used to, but without losing `max_id_seen` in the
    /// process (a bare `Err` can't carry it).
    pub llm_reachable: bool,
}

impl ExtractOutcome {
    fn empty(max_id_seen: Option<i64>, llm_reachable: bool) -> Self {
        Self {
            created_ids: Vec::new(),
            max_id_seen,
            llm_reachable,
        }
    }
}

/// One extraction pass: fetch vault events with id greater than
/// `since_id` (the curator's persisted watermark — C1 fix, replacing the
/// old `since: DateTime<Utc>` ts-window), ask the curator LLM which of them
/// are worth remembering, and write the routed candidates into the vault.
/// Public for tests. See [`ExtractOutcome`] for what's returned.
///
/// Returns an empty, `max_id_seen: None` outcome without ever calling the
/// LLM when there are no events past the watermark — routine idle periods
/// shouldn't cost a model call. On a parse failure the LLM gets exactly one
/// retry with the parse error appended to the *same* prompt (never
/// reconstructed from scratch — I1 fix), not a whole new copy of the
/// (already budgeted) events/related blocks; a second failure is logged
/// and treated as "zero candidates this pass" (`llm_reachable: true`,
/// since the model *did* respond, just not usefully) rather than
/// propagated, since a stubborn malformed-JSON model is a recoverable
/// condition (the next scheduled pass tries again), not a hard error for
/// the caller to handle.
///
/// Only one failure — the events fetch itself — can make this function
/// return `Err` (there is no batch to report a watermark for in that case).
/// Every other failure, including both LLM completion attempts, is
/// contained in the returned [`ExtractOutcome`] (`llm_reachable: false`)
/// specifically so the watermark information survives; per-candidate write
/// failures are separately contained by [`write_candidate`] and simply
/// don't add an id to `created_ids`.
pub async fn extract_pass(
    vault: &Vault,
    llm: &dyn CuratorLlm,
    cfg: &CuratorConfig,
    since_id: i64,
) -> anyhow::Result<ExtractOutcome> {
    // Scoped re-review fix: `Vault::events` orders by `id` ascending (not
    // `ts` ascending) whenever `since_id` is set — see `EventRange::since_id`'s
    // doc comment for why (a ts-ordered fetch under this `limit` could skip
    // a lower id whose `ts` is backdated later than a higher id's, and this
    // function's watermark advance would then lose it permanently). This
    // batch — and the events/related blocks built from it below, and the
    // "most recent 3" slice `build_related_block` takes off its tail — is
    // therefore in id (insertion) order, not strictly chronological order.
    // That's an acceptable approximation for the prompt: id order tracks
    // insertion order, which for real-time events (wake/session) is exactly
    // chronological and for backdated distilled events is off only by the
    // distillation lag, not scrambled.
    let events = vault
        .events(&EventRange {
            since: None,
            until: None,
            since_id: Some(since_id),
            limit: Some(200),
        })
        .await?;

    if events.is_empty() {
        tracing::debug!(
            layer = "memory",
            component = "curator",
            watermark = since_id,
            "No events since last pass; skipping extraction"
        );
        return Ok(ExtractOutcome::empty(None, true));
    }

    // I1 fix: the watermark advances past the *whole* fetched batch (up to
    // 200 events) regardless of how many of them actually make it into the
    // prompt below — computed now, before any trimming, so it's never lost
    // on an LLM-failure return path either.
    let max_id_seen = events.iter().map(|e| e.id).max();

    // Cap to the most recent MAX_PROMPT_EVENTS, then per-event text
    // truncation, then re-check the assembled prompt against
    // PROMPT_CHAR_BUDGET and drop oldest-first until it fits.
    let mut capped: Vec<Event> = events[events.len().saturating_sub(MAX_PROMPT_EVENTS)..]
        .iter()
        .map(|e| Event {
            text: truncate_chars(&e.text, EVENT_TEXT_CHAR_CAP),
            ..e.clone()
        })
        .collect();

    let related_block = build_related_block(vault, &events).await?;

    let build_prompt = |capped: &[Event], related_block: &str| -> String {
        EXTRACT_PROMPT
            .replace("{{MAX}}", &cfg.max_candidates_per_pass.to_string())
            .replace("{{EVENTS}}", &build_events_block(capped))
            .replace("{{RELATED}}", related_block)
    };

    let mut prompt = build_prompt(&capped, &related_block);
    while prompt.len() > PROMPT_CHAR_BUDGET && !capped.is_empty() {
        capped.remove(0); // drop the oldest of the capped set
        prompt = build_prompt(&capped, &related_block);
    }

    let raw = match llm.complete(&prompt, 1024).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                layer = "memory",
                component = "curator",
                error = %e,
                "Curator LLM completion failed for this extraction pass"
            );
            return Ok(ExtractOutcome::empty(max_id_seen, false));
        }
    };
    let mut candidates = match parse_candidates(&raw) {
        Ok(c) => c,
        Err(first_err) => {
            let retry_prompt = format!(
                "{prompt}\n\nYour previous reply was invalid: {first_err}. Reply with ONLY the JSON array."
            );
            let retry_raw = match llm.complete(&retry_prompt, 1024).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        layer = "memory",
                        component = "curator",
                        error = %e,
                        "Curator LLM completion failed on the retry attempt"
                    );
                    return Ok(ExtractOutcome::empty(max_id_seen, false));
                }
            };
            match parse_candidates(&retry_raw) {
                Ok(c) => c,
                Err(second_err) => {
                    tracing::warn!(
                        layer = "memory",
                        component = "curator",
                        error = %second_err,
                        "Curator LLM produced unparsable output twice; skipping this pass"
                    );
                    return Ok(ExtractOutcome::empty(max_id_seen, true));
                }
            }
        }
    };
    candidates.truncate(cfg.max_candidates_per_pass as usize);

    let mut created_ids = Vec::new();
    for c in &candidates {
        if let Some(id) = write_candidate(vault, c, cfg).await {
            created_ids.push(id);
        }
    }

    tracing::info!(
        layer = "memory",
        component = "curator",
        events = events.len(),
        prompt_events = capped.len(),
        candidates = candidates.len(),
        written = created_ids.len(),
        "Curator extraction pass complete"
    );

    Ok(ExtractOutcome {
        created_ids,
        max_id_seen,
        llm_reachable: true,
    })
}

/// Bounded-failure window-skip policy (self-healing non-negotiable #5:
/// every component must bound its own failure modes rather than wedging
/// forever). A window that fails [`extract_pass`] this many times in a row
/// is abandoned rather than retried indefinitely: [`curator_tick`] advances
/// past it, logging a `warn!`, instead of re-fetching the same slice of
/// history on every future tick. Note this counts genuine `Err` results
/// only — a pass that ran cleanly and simply found nothing worth
/// remembering (including the "LLM produced invalid JSON twice" case,
/// which [`extract_pass`] treats as `Ok(vec![])` by design) is a success,
/// not a failure, and never contributes to this streak.
const MAX_CONSECUTIVE_WINDOW_FAILURES: u32 = 3;

/// Runs one curator tick: an [`extract_pass`] attempt for `watermark`, a
/// [`detect_conflicts`] pass over any notes it created (Task 5), the
/// bounded-failure window-skip policy (see
/// [`MAX_CONSECUTIVE_WINDOW_FAILURES`]), and a `status` update. Returns the
/// watermark to use for the next tick (C1 fix — an event-id watermark,
/// replacing the old ts-based `window_since`/`pass_start` pair; persisted
/// in-memory by the caller exactly as the ts window was).
///
/// `window_failures` is the caller's running count of consecutive
/// LLM-unreachable/fetch-error results *for the current watermark* — reset
/// to 0 whenever the watermark advances, whether by a successful pass or by
/// hitting the failure cap. This is deliberately separate from
/// `status.consecutive_failures`, an unbounded lifetime counter for the
/// dashboard/repair agent that this policy never resets on its own (only a
/// genuinely successful pass resets it) — the two answer different
/// questions: "is *this* watermark stuck?" vs. "how healthy has the
/// curator been overall?".
///
/// Split out of [`run_curator`] so tests can drive individual ticks
/// deterministically without running the real interval/shutdown-driven
/// loop.
async fn curator_tick(
    vault: &Vault,
    llm: &dyn CuratorLlm,
    cfg: &CuratorConfig,
    status: &SharedCuratorStatus,
    watermark: i64,
    window_failures: &mut u32,
) -> i64 {
    let outcome = extract_pass(vault, llm, cfg, watermark).await;

    // Task 5: conflict/supersede detection over whatever this pass just
    // wrote. Deliberately not folded into `outcome`/`window_failures` —
    // a conflict-detection hiccup is a distinct failure mode from
    // extraction itself and must not cause a healthy extraction pass to
    // count as (or be retried like) a failed one.
    if let Ok(result) = &outcome {
        if !result.created_ids.is_empty() {
            if let Err(e) = detect_conflicts(vault, llm, cfg, &result.created_ids).await {
                tracing::warn!(
                    layer = "memory",
                    component = "curator",
                    error = %e,
                    "Conflict detection failed for this pass's new notes"
                );
            }
        }
    }

    let pending_count = match vault.pending().await {
        Ok(p) => Some(p.len() as u64),
        Err(e) => {
            tracing::debug!(
                layer = "memory",
                component = "curator",
                error = %e.user_message(),
                "Failed to refresh pending count"
            );
            None
        }
    };

    {
        // Poison recovery (a prior panicking holder must not permanently
        // stop status updates), matching continuum.rs's runtime_state
        // convention.
        let mut s = status.lock().unwrap_or_else(|p| p.into_inner());
        s.last_pass_at = Some(Utc::now());
        if let Some(count) = pending_count {
            s.pending_count = count;
        }
        match &outcome {
            Ok(result) if result.llm_reachable => {
                s.consecutive_failures = 0;
                s.candidates_written_total += result.created_ids.len() as u64;
            }
            Ok(_) => {
                // LLM unreachable this pass — a failure, even though the
                // events fetch itself succeeded.
                s.consecutive_failures += 1;
            }
            Err(_) => {
                s.consecutive_failures += 1;
            }
        }
    }

    match outcome {
        Ok(result) if result.llm_reachable => {
            *window_failures = 0;
            result.max_id_seen.unwrap_or(watermark)
        }
        Ok(result) => {
            *window_failures += 1;
            if *window_failures >= MAX_CONSECUTIVE_WINDOW_FAILURES {
                tracing::warn!(
                    layer = "memory",
                    component = "curator",
                    watermark,
                    failures = *window_failures,
                    "skipping memory-extraction window after {MAX_CONSECUTIVE_WINDOW_FAILURES} \
                     failures: watermark {watermark} unreachable LLM"
                );
                *window_failures = 0;
                // C1 fix: skip by advancing the watermark to the max id of
                // the failing batch (still known — the events fetch itself
                // succeeded even though the LLM did not), not to "now".
                result.max_id_seen.unwrap_or(watermark)
            } else {
                watermark
            }
        }
        Err(err) => {
            tracing::warn!(
                layer = "memory",
                component = "curator",
                error = %err,
                "Curator pass failed"
            );
            *window_failures += 1;
            if *window_failures >= MAX_CONSECUTIVE_WINDOW_FAILURES {
                tracing::warn!(
                    layer = "memory",
                    component = "curator",
                    watermark,
                    failures = *window_failures,
                    "skipping memory-extraction window after {MAX_CONSECUTIVE_WINDOW_FAILURES} \
                     failures: watermark {watermark} events fetch failing"
                );
                *window_failures = 0;
                // No batch was ever fetched on this failure path, so there
                // is no max id to skip to — the watermark stays put. A
                // persistently failing vault (not just an unreachable LLM)
                // is a deeper health problem the repair agent's
                // consecutive_failures signal (updated above regardless)
                // surfaces separately.
                watermark
            } else {
                watermark
            }
        }
    }
}

/// Feeds `sig` into `tracker` (Task 6: session summaries); if a session
/// boundary fired, stashes it in `pending_sessions` rather than writing the
/// summary immediately (C1 fix — see [`flush_due_sessions`] for why session
/// writes are delayed).
fn observe_session_boundary(
    tracker: &mut SessionTracker,
    pending_sessions: &mut Vec<EndedSession>,
    sig: &ActivitySignal,
    idle_limit_min: u64,
) {
    let Some(ended) = tracker.observe(sig, idle_limit_min) else {
        return;
    };
    tracing::debug!(
        layer = "memory",
        component = "curator",
        process = %ended.process,
        ended = %ended.ended,
        "Session boundary detected; queued for delayed summary write"
    );
    pending_sessions.push(ended);
}

/// Writes summaries for any [`EndedSession`]s in `pending_sessions` whose
/// distillation lag has elapsed (`ended.ended + distill_lag_minutes <=
/// now`), leaving the rest queued. C1 fix: `write_session_summary` queries
/// the vault's timeline by `ts` range (`started..ended`), but the
/// distiller can write an event whose `ts` falls in that range up to
/// `distillation_interval_minutes` *after* that `ts` — a query issued right
/// at boundary time can silently miss the session's own last few events.
/// Delaying the query by `distill_lag_minutes`
/// (`distillation_interval_minutes + 1`, threaded in from
/// `bin/continuum.rs` — see [`run_curator`]'s doc comment) gives the
/// distiller time to catch up first. Failures are logged and swallowed
/// here rather than propagated — a session-summary hiccup (a vault I/O
/// error, an unparsable/erroring LLM reply) must never kill the curator's
/// own extraction loop, mirroring [`write_candidate`]'s and
/// [`curator_tick`]'s per-failure containment elsewhere in this module.
async fn flush_due_sessions(
    vault: &Vault,
    llm: &dyn CuratorLlm,
    pending_sessions: &mut Vec<EndedSession>,
    distill_lag_minutes: u64,
    now: DateTime<Utc>,
) {
    let lag = Duration::minutes(distill_lag_minutes as i64);
    let (due, still_pending): (Vec<EndedSession>, Vec<EndedSession>) = pending_sessions
        .drain(..)
        .partition(|ended| ended.ended + lag <= now);
    *pending_sessions = still_pending;

    for ended in due {
        if let Err(e) = write_session_summary(vault, llm, &ended).await {
            tracing::warn!(
                layer = "memory",
                component = "curator",
                process = %ended.process,
                error = %e,
                "Failed to write session summary; continuing"
            );
        }
    }
}

/// On-disk contract for a pending derived-data wipe request (Task 7):
/// `<dev_dir>/wipe-request.json`, written by the dashboard's `wipe_memory`
/// command (and, per Task 8, the `memory__wipe_all` MCP tool) and drained
/// by [`process_wipe_request`]. Neither writer can touch
/// [`RawLog`](crate::memory::raw_log::RawLog) or
/// [`EpisodicStore`](crate::memory::episodic::EpisodicStore) directly —
/// those live inside this headless `continuum`
/// runtime process — so they leave this file as a request the runtime
/// picks up on its own schedule (boot, or the next daily hygiene tick).
#[cfg(feature = "runtime")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WipeRequest {
    /// When the request was made. Not currently used to gate anything
    /// (there is no "too old to honor" cutoff) — kept for the audit trail
    /// a `warn!` log line leaves behind.
    #[allow(dead_code)]
    requested_at: DateTime<Utc>,
    /// Which stores to wipe: any of `"raw_log"`, `"episodic"`, `"events"`.
    /// Unrecognized entries are silently ignored rather than treated as an
    /// error — forward-compatible with a future scope this runtime
    /// doesn't know about yet.
    scopes: Vec<String>,
}

#[cfg(feature = "runtime")]
fn wipe_request_path(dev_dir: &std::path::Path) -> std::path::PathBuf {
    dev_dir.join("wipe-request.json")
}

/// Drains a pending derived-data wipe request (see [`WipeRequest`]) at
/// `<dev_dir>/wipe-request.json`. Returns `Ok(false)` with no side effects
/// when no request file exists — the overwhelmingly common case on every
/// call. When a request is present, wipes each named scope via the
/// corresponding store, deletes the request file (so a later boot/tick
/// doesn't repeat the wipe), and returns `Ok(true)`.
///
/// Gated on the `runtime` feature (unlike the rest of this module) because
/// it's the only function here that touches
/// [`RawLog`](crate::memory::raw_log::RawLog) /
/// [`EpisodicStore`](crate::memory::episodic::EpisodicStore),
/// both of which only exist in a `runtime`-feature build (see
/// `crate::memory`'s `#[cfg(feature = "runtime")]` gate in `lib.rs`). The
/// desktop crate links this module with `default-features = false` and
/// never calls this function — it only ever *writes* the request file for
/// the `continuum` runtime binary to pick up.
///
/// A mid-wipe store error is propagated rather than swallowed: unlike a
/// routine extraction-pass hiccup, a wipe request that silently fails to
/// complete (and silently deletes itself, or silently never deletes
/// itself) is exactly the kind of failure a user asking to delete their
/// data needs surfaced, not contained. Callers ([`run_curator`]'s boot
/// drain, per-tick drain, and daily hygiene tick) still log-and-continue on
/// `Err` rather than panicking, per this module's usual containment policy
/// — but the error reaches them instead of vanishing here.
///
/// A malformed request file (bad JSON), by contrast, is *not* propagated
/// (M3 fix): it's renamed to `wipe-request.json.bad` (mirroring
/// `workers::intent::drain_intents`'s bad-json quarantine pattern) and this
/// returns `Ok(false)`, exactly as if no request had been present. Without
/// this, a hand-corrupted or partially-written request file would
/// propagate `Err` forever on every single call (boot, every tick, and
/// daily hygiene, per the I3 fix) without ever being consumed — a
/// permanent, silently-repeating failure loop rather than a one-time,
/// inspectable quarantine.
#[cfg(feature = "runtime")]
pub async fn process_wipe_request(
    dev_dir: &std::path::Path,
    raw_log: &crate::memory::raw_log::RawLog,
    episodic: &Arc<tokio::sync::Mutex<crate::memory::episodic::EpisodicStore>>,
    vault: &Vault,
) -> anyhow::Result<bool> {
    use anyhow::Context;

    let request_path = wipe_request_path(dev_dir);
    let raw = match tokio::fs::read_to_string(&request_path).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(e).with_context(|| format!("Failed to read {}", request_path.display()))
        }
    };

    let request: WipeRequest = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                layer = "memory",
                component = "curator",
                path = %request_path.display(),
                error = %e,
                "Unparsable wipe request; quarantining as .json.bad and skipping"
            );
            let bad_path = dev_dir.join("wipe-request.json.bad");
            if let Err(rename_err) = tokio::fs::rename(&request_path, &bad_path).await {
                tracing::warn!(
                    layer = "memory",
                    component = "curator",
                    error = %rename_err,
                    "Failed to quarantine corrupt wipe request"
                );
            }
            return Ok(false);
        }
    };

    if request.scopes.iter().any(|s| s == "raw_log") {
        let deleted = raw_log.wipe_all().await?;
        tracing::info!(
            layer = "memory",
            component = "curator",
            deleted,
            "Wiped raw log per pending request"
        );
    }
    if request.scopes.iter().any(|s| s == "episodic") {
        episodic.lock().await.wipe_all().await?;
        tracing::info!(
            layer = "memory",
            component = "curator",
            "Wiped episodic memory per pending request"
        );
    }
    if request.scopes.iter().any(|s| s == "events") {
        let deleted = vault.prune_events(0).await?;
        tracing::info!(
            layer = "memory",
            component = "curator",
            deleted,
            "Wiped vault events per pending request"
        );
    }

    tokio::fs::remove_file(&request_path)
        .await
        .with_context(|| format!("Failed to remove {}", request_path.display()))?;

    tracing::info!(
        layer = "memory",
        component = "curator",
        scopes = ?request.scopes,
        "Processed derived-data wipe request"
    );

    Ok(true)
}

/// Runs once per local calendar day (Task 7), driven by [`run_curator`]'s
/// `last_hygiene` tracking: expires stale vault nodes, prunes old timeline
/// events past `vault_cfg.events_retention_days`, and drains any pending
/// wipe request (see [`process_wipe_request`]). Every step's failure is
/// logged and swallowed here rather than propagated — a hygiene hiccup (a
/// vault I/O error, a wipe mid-flight) must not stop the curator's own
/// extraction loop, mirroring this module's other per-failure containment
/// ([`write_candidate`], [`observe_session_boundary`]).
async fn run_daily_hygiene(
    vault: &Vault,
    vault_cfg: &MemoryVaultConfig,
    #[cfg(feature = "runtime")] dev_dir: &std::path::Path,
    #[cfg(feature = "runtime")] raw_log: &crate::memory::raw_log::RawLog,
    #[cfg(feature = "runtime")] episodic: &Arc<
        tokio::sync::Mutex<crate::memory::episodic::EpisodicStore>,
    >,
) {
    if let Err(e) = vault.sweep_expired().await {
        tracing::warn!(
            layer = "memory",
            component = "curator",
            error = %e.user_message(),
            "Daily hygiene: sweep_expired failed"
        );
    }
    if let Err(e) = vault.prune_events(vault_cfg.events_retention_days).await {
        tracing::warn!(
            layer = "memory",
            component = "curator",
            error = %e.user_message(),
            "Daily hygiene: prune_events failed"
        );
    }
    #[cfg(feature = "runtime")]
    if let Err(e) = process_wipe_request(dev_dir, raw_log, episodic, vault).await {
        tracing::warn!(
            layer = "memory",
            component = "curator",
            error = %e,
            "Daily hygiene: wipe-request drain failed"
        );
    }
}

/// I3 fix: drains a pending wipe request at the top of *every* curator
/// tick, not just once per local calendar day via [`run_daily_hygiene`].
/// Before this, a request written after that day's one hygiene run (or on
/// a day when the runtime never restarted) sat untouched until the next
/// calendar day rolled over — `wake_vault_notes_max`/`interval_minutes`
/// aside, a user asking to wipe their data has no reason to expect it
/// might take up to 24 hours. Called unconditionally from the
/// `ticker.tick()` arm in [`run_curator`], ahead of the daily-hygiene gate.
/// Failure is logged and swallowed here rather than propagated, mirroring
/// every other per-tick step in this module.
#[cfg(feature = "runtime")]
async fn drain_wipe_request_tick(
    dev_dir: &std::path::Path,
    raw_log: &crate::memory::raw_log::RawLog,
    episodic: &Arc<tokio::sync::Mutex<crate::memory::episodic::EpisodicStore>>,
    vault: &Vault,
) {
    if let Err(e) = process_wipe_request(dev_dir, raw_log, episodic, vault).await {
        tracing::warn!(
            layer = "memory",
            component = "curator",
            error = %e,
            "Per-tick wipe-request drain failed"
        );
    }
}

/// Runs the curator's background extraction loop until shutdown, mirroring
/// `crate::memory::distill::run_memory_distiller`'s shape: a disabled-config
/// early return that parks on shutdown, then a `tokio::select!` between a
/// fixed-interval ticker and the shutdown watch channel.
///
/// Each tick delegates to [`curator_tick`] (see its doc comment for the
/// per-tick status update and bounded-failure window-skip policy). C1 fix:
/// the watermark starts at `0` (the beginning of whatever history the
/// vault's 30-day event retention still has) rather than "one interval
/// ago" — the old ts-window start meant every restart permanently lost any
/// event older than one interval, which is the exact bug C1 fixes for
/// ongoing operation too. Starting at `0` costs a few bounded (200 events
/// per tick, capped further by [`MAX_PROMPT_EVENTS`]) catch-up passes after
/// a restart with backlog; it never silently drops history the way the ts
/// window did. The watermark only ever advances via `curator_tick`'s
/// return value — in-memory only, matching the ts window's own lifetime
/// (reset on every restart, per the fix request).
///
/// Task 7 also makes this the home of daily hygiene (vault expiry sweep,
/// event pruning, wipe-request drain — see [`run_daily_hygiene`]) and a
/// one-shot wipe-request drain at boot. I3 fix: a pending wipe request is
/// now *also* drained at the top of every tick (see
/// [`drain_wipe_request_tick`]), not just once per day via daily hygiene —
/// see the `ticker.tick()` arm below.
///
/// C1 fix (session-summary delay-write): `pending_sessions` holds session
/// boundaries that have fired but haven't been written yet — see
/// [`flush_due_sessions`]'s doc comment for why. `distill_lag_minutes`
/// (`MemoryConfig::distillation_interval_minutes + 1`, threaded in from
/// `bin/continuum.rs`) is how long a boundary waits in that queue before
/// its summary is actually written.
///
/// Both hygiene and the wipe-request drain need
/// [`RawLog`](crate::memory::raw_log::RawLog) and
/// [`EpisodicStore`](crate::memory::episodic::EpisodicStore) handles,
/// which only exist in a `runtime`-feature
/// build, so `dev_dir`/`raw_log`/`episodic` are declared with
/// `#[cfg(feature = "runtime")]` directly on the parameters rather than
/// wrapped in an `Option` or a separate struct: the one caller (the
/// `continuum` binary, gated `required-features = ["runtime"]` in
/// `Cargo.toml`) always has all three available, and `cfg` on a function
/// parameter is legal, stable Rust — the featureless build simply compiles
/// this function without them, and every use site of the three params
/// below is itself `#[cfg(feature = "runtime")]`-gated so nothing
/// references them when they don't exist.
///
/// `#[allow(clippy::too_many_arguments)]`: this is the single wiring
/// entrypoint spawned once (from the `continuum` binary), passed eleven
/// genuinely distinct dependencies — two configs, a lag value, three
/// channel/status/shutdown handles, and (runtime build only) three store
/// handles for the wipe-request path. Folding them into a params struct
/// wouldn't reduce the real complexity here, just relocate it one level
/// down (and the featureless build would still need every field but the
/// three runtime-gated ones, reproducing the same `#[cfg]`-per-field
/// pattern this function already uses on its parameters directly).
#[allow(clippy::too_many_arguments)]
pub async fn run_curator(
    vault: Arc<Vault>,
    llm: Arc<dyn CuratorLlm>,
    cfg: CuratorConfig,
    vault_cfg: MemoryVaultConfig,
    status: SharedCuratorStatus,
    mut activity: watch::Receiver<ActivitySignal>,
    mut shutdown: watch::Receiver<bool>,
    distill_lag_minutes: u64,
    #[cfg(feature = "runtime")] dev_dir: std::path::PathBuf,
    #[cfg(feature = "runtime")] raw_log: crate::memory::raw_log::RawLog,
    #[cfg(feature = "runtime")] episodic: Arc<
        tokio::sync::Mutex<crate::memory::episodic::EpisodicStore>,
    >,
) {
    // Boot drain: a pending wipe request must execute on the very next
    // runtime start even if curator extraction itself is disabled below —
    // wiping derived data on request is a privacy/maintenance operation
    // independent of whether the extraction pipeline is turned on.
    #[cfg(feature = "runtime")]
    if let Err(e) = process_wipe_request(&dev_dir, &raw_log, &episodic, &vault).await {
        tracing::warn!(
            layer = "memory",
            component = "curator",
            error = %e,
            "Boot-time wipe-request drain failed"
        );
    }

    if !cfg.enabled {
        tracing::info!(
            layer = "memory",
            component = "curator",
            "Curator disabled by config"
        );
        let _ = shutdown.changed().await;
        return;
    }

    tracing::info!(
        layer = "memory",
        component = "curator",
        interval_minutes = cfg.interval_minutes,
        "Curator started"
    );

    let interval = StdDuration::from_secs(cfg.interval_minutes.max(1) * 60);
    let mut ticker = tokio::time::interval(interval);
    let mut watermark: i64 = 0;
    let mut window_failures: u32 = 0;
    // M1 fix: honors the configured session-boundary floor instead of the
    // tracker's own hardcoded default.
    let mut tracker = SessionTracker::with_session_min_minutes(cfg.session_min_minutes);
    let mut pending_sessions: Vec<EndedSession> = Vec::new();
    let mut last_hygiene: Option<chrono::NaiveDate> = None;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // I3 fix: drain any pending wipe request at the top of
                // *every* tick — not just once per day via daily hygiene
                // below — so a request written mid-day doesn't sit until
                // the next calendar day rolls over.
                #[cfg(feature = "runtime")]
                drain_wipe_request_tick(&dev_dir, &raw_log, &episodic, &vault).await;

                // Task 7: once per local calendar day, run vault
                // expiry/event-pruning + drain any pending wipe request —
                // ahead of extraction so a stale/expired window never gets
                // extracted from a day it should have already been swept.
                let today = chrono::Local::now().date_naive();
                if last_hygiene != Some(today) {
                    run_daily_hygiene(
                        &vault,
                        &vault_cfg,
                        #[cfg(feature = "runtime")] &dev_dir,
                        #[cfg(feature = "runtime")] &raw_log,
                        #[cfg(feature = "runtime")] &episodic,
                    )
                    .await;
                    last_hygiene = Some(today);
                }

                watermark = curator_tick(
                    &vault,
                    llm.as_ref(),
                    &cfg,
                    &status,
                    watermark,
                    &mut window_failures,
                )
                .await;

                // Task 5 (conflict resolution) runs inside `curator_tick`
                // itself, right after `extract_pass` — see its body.
                //
                // Task 6 (session summary): also check the boundary state
                // machine on every tick, not just on `activity.changed()`
                // below — this is what actually catches the *idle* boundary
                // in practice, since a truly idle user produces no new
                // distinct signal to trigger a `changed()` wakeup.
                let sig = activity.borrow().clone();
                observe_session_boundary(
                    &mut tracker,
                    &mut pending_sessions,
                    &sig,
                    cfg.session_summary_idle_minutes,
                );

                // C1 fix: flush any stashed session boundaries whose
                // distillation lag has elapsed. Driven off the same
                // periodic ticker as everything else in this arm — an
                // idle-only stretch produces no `activity.changed()` wakeup
                // to hang this off of instead.
                flush_due_sessions(
                    &vault,
                    llm.as_ref(),
                    &mut pending_sessions,
                    distill_lag_minutes,
                    Utc::now(),
                )
                .await;
            }
            _ = activity.changed() => {
                // Task 9 wires `project_hint` through here for
                // project-scoped extraction context. Task 6 (session
                // summary) feeds every signal change into the boundary
                // state machine so a process handoff is caught as soon as
                // it happens, not just at the next ticker interval.
                let sig = activity.borrow_and_update().clone();
                observe_session_boundary(
                    &mut tracker,
                    &mut pending_sessions,
                    &sig,
                    cfg.session_summary_idle_minutes,
                );
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    tracing::info!(
                        layer = "memory",
                        component = "curator",
                        "Curator stopping"
                    );
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curator::MockLlm;
    use continuum_memory::NewEvent;

    #[tokio::test]
    async fn extract_pass_writes_routed_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();
        vault
            .append_event(NewEvent {
                ts: None,
                kind: "distilled".to_string(),
                text: "User said they prefer pnpm over npm".to_string(),
                project: None,
                node_id: None,
                reference: None,
            })
            .await
            .unwrap();

        let llm = MockLlm::scripted(vec![
            r#"[{"type":"preference","title":"Prefers pnpm over npm","body":"Stated in terminal.","confidence":0.9,"source":"user_statement"},
                {"type":"fact","title":"Maybe uses Unity","body":"Seen once.","confidence":0.5,"source":"inferred"},
                {"type":"fact","title":"Noise","body":"x","confidence":0.2,"source":"observed"}]"#
                .into(),
        ]);
        let cfg = CuratorConfig::default();
        let outcome = extract_pass(&vault, &llm, &cfg, 0).await.unwrap();
        assert_eq!(outcome.created_ids.len(), 2); // 0.2 discarded
        assert!(outcome.llm_reachable);
        assert!(outcome.max_id_seen.is_some());

        let pending = vault.pending().await.unwrap();
        assert_eq!(pending.len(), 1); // the 0.5 inferred one

        let hits = vault.search("pnpm", 5).await.unwrap();
        assert!(hits
            .iter()
            .any(|h| h.status == continuum_memory::NodeStatus::Confirmed)); // 0.9 user_statement auto-confirmed

        // Second pass with the same scripted candidate, from the same
        // watermark (0) — dedupe drops it even though nothing advanced.
        let llm2 = MockLlm::scripted(vec![
            r#"[{"type":"preference","title":"Prefers pnpm over npm","body":"again","confidence":0.9,"source":"user_statement"}]"#
                .into(),
        ]);
        let outcome2 = extract_pass(&vault, &llm2, &cfg, 0).await.unwrap();
        assert_eq!(outcome2.created_ids.len(), 0);
    }

    #[tokio::test]
    async fn extract_pass_retries_once_then_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();
        vault
            .append_event(NewEvent {
                ts: None,
                kind: "distilled".to_string(),
                text: "Something happened".to_string(),
                project: None,
                node_id: None,
                reference: None,
            })
            .await
            .unwrap();

        let llm = MockLlm::scripted(vec!["not json".into(), "still not json".into()]);
        let cfg = CuratorConfig::default();
        let outcome = extract_pass(&vault, &llm, &cfg, 0).await.unwrap();
        assert_eq!(outcome.created_ids.len(), 0);
        assert!(outcome.llm_reachable); // model responded, just not usefully
        assert!(outcome.max_id_seen.is_some()); // the watermark still advances
        assert_eq!(llm.calls(), 2); // initial + one retry with the error appended
    }

    /// I1 fix: the retry prompt must be the *same* prompt with a short
    /// suffix appended, not a whole new copy of the (already budgeted)
    /// events/related blocks. Regression against a "rebuild the full
    /// prompt from scratch on retry" implementation, which would either
    /// duplicate the events block or silently diverge from what the first
    /// attempt saw.
    #[tokio::test]
    async fn extract_pass_retry_prompt_is_same_prompt_plus_suffix_once() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();
        vault
            .append_event(NewEvent {
                ts: None,
                kind: "distilled".to_string(),
                text: "Something worth remembering happened here".to_string(),
                project: None,
                node_id: None,
                reference: None,
            })
            .await
            .unwrap();

        let llm = MockLlm::scripted(vec![
            "not json".into(),
            r#"[{"type":"fact","title":"T","body":"b","confidence":0.9,"source":"observed"}]"#
                .into(),
        ]);
        let cfg = CuratorConfig::default();
        let outcome = extract_pass(&vault, &llm, &cfg, 0).await.unwrap();
        assert_eq!(outcome.created_ids.len(), 1);

        let prompts = llm.prompts();
        assert_eq!(prompts.len(), 2);
        let suffix = "Your previous reply was invalid:";
        // The retry prompt starts with the exact first prompt, followed by
        // exactly one occurrence of the invalid-reply suffix.
        assert!(prompts[1].starts_with(&prompts[0]));
        assert_eq!(prompts[1].matches(suffix).count(), 1);
        assert!(!prompts[0].contains(suffix));
    }

    /// I1 fix: a 200-event fixture must still produce a prompt under
    /// `PROMPT_CHAR_BUDGET`, even though the extraction query itself fetches
    /// up to 200 events per pass — the cap-to-40-most-recent plus
    /// per-event/related truncation plus the final budget-trim loop must
    /// keep the assembled prompt bounded regardless of how much raw text
    /// the fetched batch contains.
    #[tokio::test]
    async fn extract_pass_budgets_a_200_event_fixture_under_the_prompt_char_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();
        for i in 0..200 {
            vault
                .append_event(NewEvent {
                    ts: None,
                    kind: "distilled".to_string(),
                    text: format!(
                        "Event {i}: {}",
                        "a very long descriptive sentence about what happened ".repeat(5)
                    ),
                    project: None,
                    node_id: None,
                    reference: None,
                })
                .await
                .unwrap();
        }

        let llm = MockLlm::scripted(vec!["[]".into()]);
        let cfg = CuratorConfig::default();
        let outcome = extract_pass(&vault, &llm, &cfg, 0).await.unwrap();
        assert_eq!(outcome.created_ids.len(), 0);
        assert!(outcome.llm_reachable);
        // The watermark must still advance past the *entire* fetched batch
        // (up to 200), not just the ~40 events that made it into the
        // prompt — otherwise the untouched older events would be silently
        // skipped forever.
        assert_eq!(outcome.max_id_seen, Some(200));

        let prompts = llm.prompts();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].len() <= PROMPT_CHAR_BUDGET,
            "prompt was {} chars, budget is {PROMPT_CHAR_BUDGET}",
            prompts[0].len()
        );
    }

    /// Regression for the "non-atomic per-candidate loop" review finding:
    /// one candidate's `Vault::create` failing must not abort the pass or
    /// lose the other candidate's write.
    ///
    /// `Vault::create` has exactly one built-in validation error (a blank
    /// title), which `parse_candidates` already rejects for the whole batch
    /// before any candidate reaches this loop — so there's no JSON payload
    /// that reaches `write_candidate` pre-broken. Instead this deterministically
    /// breaks the *write path* itself: removing the "facts" folder out from
    /// under an already-open vault makes `Vault::create`'s `atomic_write`
    /// fail with a genuine I/O error for any `fact`-typed candidate — a
    /// realistic failure mode (externally deleted directory, disk hiccup)
    /// that needs no vault mocking.
    #[tokio::test]
    async fn extract_pass_contains_per_candidate_write_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();
        vault
            .append_event(NewEvent {
                ts: None,
                kind: "distilled".to_string(),
                text: "Two candidates, one whose vault write will fail".to_string(),
                project: None,
                node_id: None,
                reference: None,
            })
            .await
            .unwrap();

        std::fs::remove_dir_all(tmp.path().join("facts")).unwrap();

        let llm = MockLlm::scripted(vec![
            r#"[{"type":"fact","title":"Will fail to write","body":"b","confidence":0.9,"source":"user_statement"},
                {"type":"preference","title":"Will write fine","body":"b","confidence":0.9,"source":"user_statement"}]"#
                .into(),
        ]);
        let cfg = CuratorConfig::default();

        let outcome = extract_pass(&vault, &llm, &cfg, 0).await.unwrap();

        // The first candidate's write errors internally and is contained
        // (logged, not propagated) — the pass still completes and counts
        // the second candidate, rather than the whole pass aborting on the
        // first candidate's error and losing the second write's count.
        assert_eq!(outcome.created_ids.len(), 1);
        let hits = vault.search("write fine", 5).await.unwrap();
        assert!(hits.iter().any(|h| h.title == "Will write fine"));
    }

    /// Regression for the "permanently-failing window wedges the curator
    /// forever" review finding: three consecutive `extract_pass` failures
    /// on the same watermark must cause `curator_tick` to abandon it
    /// (advance past it) rather than retry it forever, while
    /// `status.consecutive_failures` (the dashboard's unbounded lifetime
    /// counter) keeps counting independently of the bounded local streak.
    ///
    /// C1 fix: the poisoned-window skip now asserts a *watermark* advance
    /// (to the max event id of the failing batch) rather than a ts advance
    /// (to "now") — this test's own vault event never changes ts, only the
    /// watermark moves.
    ///
    /// Uses an empty-script `MockLlm` (every `complete()` call errors
    /// immediately, simulating an unreachable/crashed LLM) so the events
    /// fetch itself succeeds on every tick (the batch — and its max id —
    /// is always known) while the completion call is what actually fails,
    /// which is what the bounded-failure policy is keyed on.
    #[tokio::test]
    async fn curator_tick_skips_window_after_three_consecutive_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();
        vault
            .append_event(NewEvent {
                ts: None,
                kind: "distilled".to_string(),
                text: "Something happened".to_string(),
                project: None,
                node_id: None,
                reference: None,
            })
            .await
            .unwrap();

        let llm = MockLlm::scripted(vec![]);
        let cfg = CuratorConfig::default();
        let status: SharedCuratorStatus = Default::default();
        let watermark = 0i64;
        let mut window_failures = 0u32;

        // Two failing ticks: below the cap, the watermark doesn't move yet.
        let after_1 =
            curator_tick(&vault, &llm, &cfg, &status, watermark, &mut window_failures).await;
        assert_eq!(after_1, watermark);
        assert_eq!(window_failures, 1);

        let after_2 =
            curator_tick(&vault, &llm, &cfg, &status, after_1, &mut window_failures).await;
        assert_eq!(after_2, watermark);
        assert_eq!(window_failures, 2);
        assert_eq!(status.lock().unwrap().consecutive_failures, 2);

        // Third consecutive failure hits the cap: the watermark is
        // advanced to the max id of the failing batch (not to "now" — the
        // C1 fix), and the local streak resets — but the dashboard's
        // lifetime counter does not.
        let after_3 =
            curator_tick(&vault, &llm, &cfg, &status, after_2, &mut window_failures).await;
        assert!(after_3 > watermark);
        assert_eq!(window_failures, 0);
        assert_eq!(status.lock().unwrap().consecutive_failures, 3);

        // A fourth tick past the new watermark: the vault's only event now
        // has id <= the new watermark, so `extract_pass` short-circuits to
        // an empty, llm_reachable outcome without even calling the LLM — a
        // real success that resets the dashboard's consecutive_failures
        // back to 0.
        let _after_4 =
            curator_tick(&vault, &llm, &cfg, &status, after_3, &mut window_failures).await;
        assert_eq!(window_failures, 0);
        assert_eq!(status.lock().unwrap().consecutive_failures, 0);
    }

    /// C1 fix: a session boundary must not immediately write its summary —
    /// the write is delayed until `distill_lag_minutes` has elapsed past
    /// `ended`, giving the distiller time to land any tail events whose
    /// `ts` falls inside the session span but whose row hasn't been
    /// written yet (see `write_session_summary`'s doc comment). This drives
    /// `observe_session_boundary`/`flush_due_sessions` directly, the same
    /// two calls `run_curator`'s loop makes.
    #[tokio::test]
    async fn session_boundary_write_is_delayed_until_lag_elapses() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        let started = Utc::now() - Duration::minutes(60);
        let ended_ts = started + Duration::minutes(30);
        for (i, text) in ["one", "two", "three"].iter().enumerate() {
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

        let mut tracker = SessionTracker::new();
        let mut pending_sessions: Vec<EndedSession> = Vec::new();

        // idle_limit_min is deliberately larger than the 30-minute gap
        // between sig0/sig1 below — otherwise the idle-boundary check
        // (case 3) would fire before the process-change check (case 4)
        // this test means to exercise.
        let idle_limit_min = 45;

        let sig0 = ActivitySignal {
            project_hint: None,
            process: "vscode".to_string(),
            idle_seconds: 0,
            ts: Some(started),
        };
        observe_session_boundary(&mut tracker, &mut pending_sessions, &sig0, idle_limit_min);
        assert!(pending_sessions.is_empty());

        let sig1 = ActivitySignal {
            project_hint: None,
            process: "vscode".to_string(),
            idle_seconds: 0,
            ts: Some(ended_ts),
        };
        observe_session_boundary(&mut tracker, &mut pending_sessions, &sig1, idle_limit_min);
        assert!(pending_sessions.is_empty()); // same process, no boundary yet

        // Process change after the session has run >= MIN_SESSION_MINUTES
        // (30 min here) — a real boundary fires and is queued, not written.
        let sig2 = ActivitySignal {
            project_hint: None,
            process: "chrome".to_string(),
            idle_seconds: 0,
            ts: Some(ended_ts + Duration::seconds(10)),
        };
        observe_session_boundary(&mut tracker, &mut pending_sessions, &sig2, idle_limit_min);
        assert_eq!(pending_sessions.len(), 1);
        assert_eq!(pending_sessions[0].ended, ended_ts);
        assert_eq!(vault.info().await.unwrap().note_count, 0);

        // Flush right at boundary time — the 16-minute lag hasn't elapsed,
        // so nothing is written yet. An empty-script MockLlm proves it: any
        // unexpected `complete()` call would error and fail this test.
        let llm_not_called = MockLlm::scripted(vec![]);
        flush_due_sessions(&vault, &llm_not_called, &mut pending_sessions, 16, ended_ts).await;
        assert_eq!(
            pending_sessions.len(),
            1,
            "still queued before the lag elapses"
        );
        assert_eq!(vault.info().await.unwrap().note_count, 0);

        // Flush again once the lag has elapsed — now it's written.
        let summary_md = "## Goal\nX\n## Changed\n- none\n## Problem\nnone\n## Tried\n\u{2013}\n## Result\nDone\n## Next step\nnone";
        let llm = MockLlm::scripted(vec![summary_md.to_string()]);
        flush_due_sessions(
            &vault,
            &llm,
            &mut pending_sessions,
            16,
            ended_ts + Duration::minutes(16),
        )
        .await;
        assert!(pending_sessions.is_empty());
        assert_eq!(vault.info().await.unwrap().note_count, 1);
    }

    /// Regression/coverage for Task 7's real derived-data wipe path:
    /// `process_wipe_request` must actually clear every named scope (raw
    /// log rows, episodic events, vault timeline events) and delete the
    /// request file so a later boot/tick doesn't repeat the wipe — then a
    /// second call with no request file present must be a harmless no-op.
    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn process_wipe_request_wipes_all_scopes_and_deletes_request_file() {
        use crate::memory::episodic::{EpisodicEvent, EpisodicStore, EventKind};
        use crate::memory::raw_log::RawLog;
        use crate::senses::types::{ContextObservation, PerceptionFrame, ScreenObservation};
        use tokio::sync::Mutex;

        let dev_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(vault_dir.path()).await.unwrap();
        for text in ["first event", "second event"] {
            vault
                .append_event(NewEvent {
                    ts: None,
                    kind: "distilled".to_string(),
                    text: text.to_string(),
                    project: None,
                    node_id: None,
                    reference: None,
                })
                .await
                .unwrap();
        }
        let empty_range = EventRange {
            since: None,
            until: None,
            since_id: None,
            limit: None,
        };
        assert_eq!(vault.events(&empty_range).await.unwrap().len(), 2);

        let raw_log = RawLog::open("sqlite::memory:").await.unwrap();
        let now = Utc::now();
        raw_log
            .write_frame(&PerceptionFrame {
                id: uuid::Uuid::new_v4(),
                ts: now,
                screen: ScreenObservation {
                    description: "editor".to_string(),
                    foreground_app: "Code.exe".to_string(),
                    has_error_visible: false,
                    confidence: 0.9,
                    screenshot_path: None,
                    ts: now,
                },
                audio: None,
                context: ContextObservation {
                    foreground_window_title: "main.rs".to_string(),
                    foreground_process_name: "Code.exe".to_string(),
                    idle_seconds: 0,
                    in_call: false,
                    ts: now,
                },
                salience_hint: 0.5,
            })
            .await
            .unwrap();
        assert_eq!(raw_log.frame_count().await.unwrap(), 1);

        let episodic_dir = tempfile::tempdir().unwrap();
        let mut episodic_store =
            EpisodicStore::open_for_test(episodic_dir.path().to_str().unwrap())
                .await
                .unwrap();
        episodic_store
            .insert_event(&EpisodicEvent {
                id: uuid::Uuid::new_v4().to_string(),
                ts: now,
                kind: EventKind::Remember,
                summary: "an episodic memory".to_string(),
                importance: 0.5,
                tags: vec![],
                source_frame_id: None,
            })
            .await
            .unwrap();
        assert_eq!(episodic_store.event_count().await.unwrap(), 1);
        let episodic = Arc::new(Mutex::new(episodic_store));

        let request_path = dev_dir.path().join("wipe-request.json");
        std::fs::write(
            &request_path,
            serde_json::json!({
                "requested_at": now.to_rfc3339(),
                "scopes": ["raw_log", "episodic", "events"],
            })
            .to_string(),
        )
        .unwrap();

        let processed = process_wipe_request(dev_dir.path(), &raw_log, &episodic, &vault)
            .await
            .unwrap();
        assert!(processed);
        assert!(!request_path.exists());
        assert_eq!(raw_log.frame_count().await.unwrap(), 0);
        assert_eq!(episodic.lock().await.event_count().await.unwrap(), 0);
        assert_eq!(vault.events(&empty_range).await.unwrap().len(), 0);

        // Second call: no request file present anymore — must not error
        // and must not re-run any wipe.
        let processed_again = process_wipe_request(dev_dir.path(), &raw_log, &episodic, &vault)
            .await
            .unwrap();
        assert!(!processed_again);
    }

    /// A `dev_dir` that has never had a wipe request written to it must
    /// behave identically to one that's already been drained — no error,
    /// `Ok(false)`.
    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn process_wipe_request_returns_false_when_no_request_file() {
        use crate::memory::episodic::EpisodicStore;
        use crate::memory::raw_log::RawLog;
        use tokio::sync::Mutex;

        let dev_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(vault_dir.path()).await.unwrap();
        let raw_log = RawLog::open("sqlite::memory:").await.unwrap();
        let episodic_dir = tempfile::tempdir().unwrap();
        let episodic = Arc::new(Mutex::new(
            EpisodicStore::open_for_test(episodic_dir.path().to_str().unwrap())
                .await
                .unwrap(),
        ));

        let processed = process_wipe_request(dev_dir.path(), &raw_log, &episodic, &vault)
            .await
            .unwrap();
        assert!(!processed);
    }

    /// M3 fix regression: a corrupt/unparseable `wipe-request.json` must be
    /// quarantined (renamed to `wipe-request.json.bad`) and reported as
    /// `Ok(false)`, not retried forever. Before this fix, a bad-JSON parse
    /// error propagated as `Err` on every call without ever consuming the
    /// file — the exact same request would fail again on the very next
    /// tick, boot, or hygiene run, forever.
    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn process_wipe_request_quarantines_corrupt_json_instead_of_retrying_forever() {
        use crate::memory::episodic::EpisodicStore;
        use crate::memory::raw_log::RawLog;
        use tokio::sync::Mutex;

        let dev_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(vault_dir.path()).await.unwrap();
        let raw_log = RawLog::open("sqlite::memory:").await.unwrap();
        let episodic_dir = tempfile::tempdir().unwrap();
        let episodic = Arc::new(Mutex::new(
            EpisodicStore::open_for_test(episodic_dir.path().to_str().unwrap())
                .await
                .unwrap(),
        ));

        let request_path = dev_dir.path().join("wipe-request.json");
        std::fs::write(&request_path, "{ this is not valid json").unwrap();

        let processed = process_wipe_request(dev_dir.path(), &raw_log, &episodic, &vault)
            .await
            .unwrap();
        assert!(!processed, "quarantine is Ok(false), not Err");
        assert!(
            !request_path.exists(),
            "the corrupt file must be moved out of the way"
        );
        assert!(
            dev_dir.path().join("wipe-request.json.bad").exists(),
            "quarantined under the .json.bad name"
        );

        // A later call — simulating the next tick — must not retry or error:
        // the corrupt file is already gone.
        let processed_again = process_wipe_request(dev_dir.path(), &raw_log, &episodic, &vault)
            .await
            .unwrap();
        assert!(!processed_again);
    }

    /// I3 fix regression: the per-tick drain helper (called unconditionally
    /// from `run_curator`'s `ticker.tick()` arm, independent of
    /// `last_hygiene`'s once-per-day gate) must pick up a wipe request that
    /// appears *between* two ticks on the very same calendar day — i.e.
    /// without `run_daily_hygiene` ever running again. Calling
    /// `drain_wipe_request_tick` twice in a row, with the request written
    /// only after the first call, exercises exactly that: no day boundary,
    /// no hygiene call, anywhere in this test.
    #[cfg(feature = "runtime")]
    #[tokio::test]
    async fn drain_wipe_request_tick_processes_without_daily_hygiene() {
        use crate::memory::episodic::EpisodicStore;
        use crate::memory::raw_log::RawLog;
        use tokio::sync::Mutex;

        let dev_dir = tempfile::tempdir().unwrap();
        let vault_dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(vault_dir.path()).await.unwrap();
        vault
            .append_event(NewEvent {
                ts: None,
                kind: "distilled".to_string(),
                text: "should be wiped".to_string(),
                project: None,
                node_id: None,
                reference: None,
            })
            .await
            .unwrap();
        let raw_log = RawLog::open("sqlite::memory:").await.unwrap();
        let episodic_dir = tempfile::tempdir().unwrap();
        let episodic = Arc::new(Mutex::new(
            EpisodicStore::open_for_test(episodic_dir.path().to_str().unwrap())
                .await
                .unwrap(),
        ));

        // "Tick 1": nothing to drain yet.
        drain_wipe_request_tick(dev_dir.path(), &raw_log, &episodic, &vault).await;
        let empty_range = EventRange {
            since: None,
            until: None,
            since_id: None,
            limit: None,
        };
        assert_eq!(vault.events(&empty_range).await.unwrap().len(), 1);

        // A wipe request arrives mid-day, well after any daily-hygiene run
        // would have already happened.
        std::fs::write(
            dev_dir.path().join("wipe-request.json"),
            serde_json::json!({
                "requested_at": Utc::now().to_rfc3339(),
                "scopes": ["events"],
            })
            .to_string(),
        )
        .unwrap();

        // "Tick 2", same day — the per-tick drain (not hygiene) must pick
        // it up.
        drain_wipe_request_tick(dev_dir.path(), &raw_log, &episodic, &vault).await;
        assert!(!dev_dir.path().join("wipe-request.json").exists());
        assert_eq!(vault.events(&empty_range).await.unwrap().len(), 0);
    }
}
