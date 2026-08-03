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
use crate::curator::session::{write_session_summary, SessionTracker};
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

/// Related-notes context for the extraction prompt's `{{RELATED}}` slot:
/// full-text search over the concatenated text of the most recent (up to
/// 3) events, top 8 hits rendered as `"- title: snippet"` lines. Empty
/// (not an error) when there's nothing to search on or nothing found —
/// the template reads fine either way ("KNOWN MEMORIES possibly related:"
/// followed by nothing).
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
        .map(|h| format!("- {}: {}", h.title, h.snippet.clone().unwrap_or_default()))
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

/// One extraction pass: fetch vault events since `since`, ask the curator
/// LLM which of them are worth remembering, and write the routed
/// candidates into the vault. Public for tests. Returns the ids of the
/// notes actually created — the caller ([`curator_tick`]) feeds these into
/// [`detect_conflicts`] (Task 5), and uses `.len()` wherever the old
/// `usize` count is needed.
///
/// Returns `Ok(vec![])` without ever calling the LLM when there are no
/// events in the window — routine idle periods shouldn't cost a model
/// call. On a parse failure the LLM gets exactly one retry with the parse
/// error appended to the prompt; a second failure is logged and treated as
/// "zero candidates this pass" rather than propagated, since a stubborn
/// malformed-JSON model is a recoverable condition (the next scheduled pass
/// tries again), not a hard error for the caller to handle.
///
/// Only pre-loop failures — the events fetch and both LLM completion
/// attempts — can make this function return `Err`; per-candidate failures
/// are contained by [`write_candidate`] and simply don't add an id to the
/// returned list.
pub async fn extract_pass(
    vault: &Vault,
    llm: &dyn CuratorLlm,
    cfg: &CuratorConfig,
    since: DateTime<Utc>,
) -> anyhow::Result<Vec<String>> {
    let events = vault
        .events(&EventRange {
            since: Some(since),
            until: None,
            limit: Some(200),
        })
        .await?;

    if events.is_empty() {
        tracing::debug!(
            layer = "memory",
            component = "curator",
            "No events since last pass; skipping extraction"
        );
        return Ok(Vec::new());
    }

    let events_block = build_events_block(&events);
    let related_block = build_related_block(vault, &events).await?;

    let prompt = EXTRACT_PROMPT
        .replace("{{MAX}}", &cfg.max_candidates_per_pass.to_string())
        .replace("{{EVENTS}}", &events_block)
        .replace("{{RELATED}}", &related_block);

    let raw = llm.complete(&prompt, 1024).await?;
    let mut candidates = match parse_candidates(&raw) {
        Ok(c) => c,
        Err(first_err) => {
            let retry_prompt = format!(
                "{prompt}\n\nYour previous reply was invalid: {first_err}. Reply with ONLY the JSON array."
            );
            let retry_raw = llm.complete(&retry_prompt, 1024).await?;
            match parse_candidates(&retry_raw) {
                Ok(c) => c,
                Err(second_err) => {
                    tracing::warn!(
                        layer = "memory",
                        component = "curator",
                        error = %second_err,
                        "Curator LLM produced unparsable output twice; skipping this pass"
                    );
                    return Ok(Vec::new());
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
        candidates = candidates.len(),
        written = created_ids.len(),
        "Curator extraction pass complete"
    );

    Ok(created_ids)
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

/// Runs one curator tick: an [`extract_pass`] attempt for `window_since`,
/// a [`detect_conflicts`] pass over any notes it created (Task 5), the
/// bounded-failure window-skip policy (see
/// [`MAX_CONSECUTIVE_WINDOW_FAILURES`]), and a `status` update. Returns the
/// window start to use for the next tick.
///
/// `window_failures` is the caller's running count of consecutive `Err`
/// results *for the current window* — reset to 0 whenever the window
/// advances, whether by a successful pass or by hitting the failure cap.
/// This is deliberately separate from `status.consecutive_failures`, an
/// unbounded lifetime counter for the dashboard/repair agent that this
/// policy never resets on its own (only a genuinely successful pass resets
/// it) — the two answer different questions: "is *this* window stuck?" vs.
/// "how healthy has the curator been overall?".
///
/// Split out of [`run_curator`] so tests can drive individual ticks
/// deterministically without running the real interval/shutdown-driven
/// loop.
async fn curator_tick(
    vault: &Vault,
    llm: &dyn CuratorLlm,
    cfg: &CuratorConfig,
    status: &SharedCuratorStatus,
    window_since: DateTime<Utc>,
    window_failures: &mut u32,
) -> DateTime<Utc> {
    let pass_start = Utc::now();
    let outcome = extract_pass(vault, llm, cfg, window_since).await;

    // Task 5: conflict/supersede detection over whatever this pass just
    // wrote. Deliberately not folded into `outcome`/`window_failures` —
    // a conflict-detection hiccup is a distinct failure mode from
    // extraction itself and must not cause a healthy extraction pass to
    // count as (or be retried like) a failed one.
    if let Ok(ids) = &outcome {
        if !ids.is_empty() {
            if let Err(e) = detect_conflicts(vault, llm, cfg, ids).await {
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
        s.last_pass_at = Some(pass_start);
        if let Some(count) = pending_count {
            s.pending_count = count;
        }
        match &outcome {
            Ok(ids) => {
                s.consecutive_failures = 0;
                s.candidates_written_total += ids.len() as u64;
            }
            Err(_) => {
                s.consecutive_failures += 1;
            }
        }
    }

    match outcome {
        Ok(_) => {
            *window_failures = 0;
            pass_start
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
                    since = %window_since,
                    until = %pass_start,
                    failures = *window_failures,
                    "skipping memory-extraction window after {MAX_CONSECUTIVE_WINDOW_FAILURES} \
                     failures: {window_since}..{pass_start}"
                );
                *window_failures = 0;
                pass_start
            } else {
                window_since
            }
        }
    }
}

/// Feeds `sig` into `tracker` (Task 6: session summaries); if a session
/// boundary fired, writes the summary note. Failures are logged and
/// swallowed here rather than propagated — a session-summary hiccup (a
/// vault I/O error, an unparsable/erroring LLM reply) must never kill the
/// curator's own extraction loop, mirroring [`write_candidate`]'s and
/// [`curator_tick`]'s per-failure containment elsewhere in this module.
async fn observe_session_boundary(
    vault: &Vault,
    llm: &dyn CuratorLlm,
    tracker: &mut SessionTracker,
    sig: &ActivitySignal,
    idle_limit_min: u64,
) {
    let Some(ended) = tracker.observe(sig, idle_limit_min) else {
        return;
    };

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
/// A malformed request file (bad JSON) or a mid-wipe store error is
/// propagated rather than swallowed: unlike a routine extraction-pass
/// hiccup, a wipe request that silently fails to complete (and silently
/// deletes itself, or silently never deletes itself) is exactly the kind
/// of failure a user asking to delete their data needs surfaced, not
/// contained. Callers ([`run_curator`]'s boot drain and daily hygiene
/// tick) still log-and-continue on `Err` rather than panicking, per this
/// module's usual containment policy — but the error reaches them instead
/// of vanishing here.
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

    let request: WipeRequest = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse wipe request at {}", request_path.display()))?;

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

/// Runs the curator's background extraction loop until shutdown, mirroring
/// `crate::memory::distill::run_memory_distiller`'s shape: a disabled-config
/// early return that parks on shutdown, then a `tokio::select!` between a
/// fixed-interval ticker and the shutdown watch channel.
///
/// Each tick delegates to [`curator_tick`] (see its doc comment for the
/// per-tick status update and bounded-failure window-skip policy). The
/// window starts at "one interval ago" so the first pass has something to
/// look at, and only ever advances via `curator_tick`'s return value —
/// on an ordinary success it moves to that pass's start time; on a failed
/// pass it stays put (the next tick retries the same events) *unless* the
/// failure streak has hit the cap, in which case `curator_tick` itself
/// advances it past the stuck window.
///
/// Task 7 also makes this the home of daily hygiene (vault expiry sweep,
/// event pruning, wipe-request drain — see [`run_daily_hygiene`]) and a
/// one-shot wipe-request drain at boot. Both need
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
/// entrypoint spawned once (from the `continuum` binary), passed ten
/// genuinely distinct dependencies — two configs, three
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
    let mut window_since = Utc::now() - Duration::minutes(cfg.interval_minutes.max(1) as i64);
    let mut window_failures: u32 = 0;
    let mut tracker = SessionTracker::new();
    let mut last_hygiene: Option<chrono::NaiveDate> = None;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
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

                window_since = curator_tick(
                    &vault,
                    llm.as_ref(),
                    &cfg,
                    &status,
                    window_since,
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
                    &vault,
                    llm.as_ref(),
                    &mut tracker,
                    &sig,
                    cfg.session_summary_idle_minutes,
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
                    &vault,
                    llm.as_ref(),
                    &mut tracker,
                    &sig,
                    cfg.session_summary_idle_minutes,
                )
                .await;
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
        let written = extract_pass(&vault, &llm, &cfg, Utc::now() - Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(written.len(), 2); // 0.2 discarded

        let pending = vault.pending().await.unwrap();
        assert_eq!(pending.len(), 1); // the 0.5 inferred one

        let hits = vault.search("pnpm", 5).await.unwrap();
        assert!(hits
            .iter()
            .any(|h| h.status == continuum_memory::NodeStatus::Confirmed)); // 0.9 user_statement auto-confirmed

        // Second pass with the same scripted candidate — dedupe drops it.
        let llm2 = MockLlm::scripted(vec![
            r#"[{"type":"preference","title":"Prefers pnpm over npm","body":"again","confidence":0.9,"source":"user_statement"}]"#
                .into(),
        ]);
        let written2 = extract_pass(&vault, &llm2, &cfg, Utc::now() - Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(written2.len(), 0);
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
        let written = extract_pass(&vault, &llm, &cfg, Utc::now() - Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(written.len(), 0);
        assert_eq!(llm.calls(), 2); // initial + one retry with the error appended
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

        let written = extract_pass(&vault, &llm, &cfg, Utc::now() - Duration::hours(1))
            .await
            .unwrap();

        // The first candidate's write errors internally and is contained
        // (logged, not propagated) — the pass still completes and counts
        // the second candidate, rather than the whole pass aborting on the
        // first candidate's error and losing the second write's count.
        assert_eq!(written.len(), 1);
        let hits = vault.search("write fine", 5).await.unwrap();
        assert!(hits.iter().any(|h| h.title == "Will write fine"));
    }

    /// Regression for the "permanently-failing window wedges the curator
    /// forever" review finding: three consecutive `extract_pass` failures
    /// on the same window must cause `curator_tick` to abandon that window
    /// (advance past it) rather than retry it forever, while
    /// `status.consecutive_failures` (the dashboard's unbounded lifetime
    /// counter) keeps counting independently of the bounded local streak.
    ///
    /// Deviation from the fix request's literal test sketch ("6 invalid
    /// replies (3 passes x initial+retry) + then a valid reply"): that
    /// recipe can't actually exercise this policy, because
    /// `extract_pass`'s "LLM produced invalid JSON twice" path returns
    /// `Ok(vec![])` by design (see its doc comment and
    /// `extract_pass_retries_once_then_skips` above) — never `Err`. Feeding
    /// it invalid JSON six times produces three `Ok(vec![])` passes, not
    /// three failures, and would never trip the failure streak. This test
    /// instead uses an empty-script `MockLlm` (every `complete()` call
    /// errors immediately, simulating an unreachable/crashed LLM) to
    /// produce genuine `Err` results from `extract_pass`, which is what the
    /// policy is actually keyed on.
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
        let window_since = Utc::now() - Duration::hours(1);
        let mut window_failures = 0u32;

        // Two failing ticks: below the cap, the window doesn't move yet.
        let after_1 = curator_tick(
            &vault,
            &llm,
            &cfg,
            &status,
            window_since,
            &mut window_failures,
        )
        .await;
        assert_eq!(after_1, window_since);
        assert_eq!(window_failures, 1);

        let after_2 =
            curator_tick(&vault, &llm, &cfg, &status, after_1, &mut window_failures).await;
        assert_eq!(after_2, window_since);
        assert_eq!(window_failures, 2);
        assert_eq!(status.lock().unwrap().consecutive_failures, 2);

        // Third consecutive failure hits the cap: the window is abandoned
        // (advances to "now"), and the local streak resets — but the
        // dashboard's lifetime counter does not.
        let after_3 =
            curator_tick(&vault, &llm, &cfg, &status, after_2, &mut window_failures).await;
        assert!(after_3 > window_since);
        assert_eq!(window_failures, 0);
        assert_eq!(status.lock().unwrap().consecutive_failures, 3);

        // A fourth tick on the new window: the vault's only event now
        // predates the new window start, so `extract_pass` short-circuits
        // to `Ok(0)` without even calling the LLM — a real success outcome
        // that resets the dashboard's consecutive_failures back to 0.
        let _after_4 =
            curator_tick(&vault, &llm, &cfg, &status, after_3, &mut window_failures).await;
        assert_eq!(window_failures, 0);
        assert_eq!(status.lock().unwrap().consecutive_failures, 0);
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
}
