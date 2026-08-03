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

use crate::config::CuratorConfig;
use crate::curator::extract::{
    candidate_to_draft, is_duplicate, parse_candidates, route_candidate,
};
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
fn build_events_block(events: &[Event]) -> String {
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

/// One extraction pass: fetch vault events since `since`, ask the curator
/// LLM which of them are worth remembering, and write the routed
/// candidates into the vault. Public for tests.
///
/// Returns `Ok(0)` without ever calling the LLM when there are no events in
/// the window — routine idle periods shouldn't cost a model call. On a
/// parse failure the LLM gets exactly one retry with the parse error
/// appended to the prompt; a second failure is logged and treated as "zero
/// candidates this pass" rather than propagated, since a stubborn
/// malformed-JSON model is a recoverable condition (the next scheduled pass
/// tries again), not a hard error for the caller to handle.
pub async fn extract_pass(
    vault: &Vault,
    llm: &dyn CuratorLlm,
    cfg: &CuratorConfig,
    since: DateTime<Utc>,
) -> anyhow::Result<usize> {
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
        return Ok(0);
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
                    return Ok(0);
                }
            }
        }
    };
    candidates.truncate(cfg.max_candidates_per_pass as usize);

    let mut written = 0usize;
    for c in &candidates {
        if is_duplicate(vault, &c.title).await? {
            tracing::debug!(
                layer = "memory",
                component = "curator",
                title = %c.title,
                "Skipping duplicate candidate"
            );
            continue;
        }
        let Some(status) = route_candidate(c, cfg) else {
            tracing::debug!(
                layer = "memory",
                component = "curator",
                title = %c.title,
                confidence = c.confidence,
                "Discarding low-confidence candidate"
            );
            continue;
        };
        vault.create(candidate_to_draft(c, status)).await?;
        written += 1;
    }

    tracing::info!(
        layer = "memory",
        component = "curator",
        events = events.len(),
        candidates = candidates.len(),
        written,
        "Curator extraction pass complete"
    );

    Ok(written)
}

/// Runs the curator's background extraction loop until shutdown, mirroring
/// `crate::memory::distill::run_memory_distiller`'s shape: a disabled-config
/// early return that parks on shutdown, then a `tokio::select!` between a
/// fixed-interval ticker and the shutdown watch channel.
///
/// Each tick runs one [`extract_pass`] over events since the previous
/// tick's start (first pass looks back one interval from startup) and
/// updates `status` — `last_pass_at` unconditionally (successful or not,
/// per [`crate::curator::CuratorStatus`]'s doc comment), `consecutive_failures`
/// reset to 0 on success / incremented on error, `candidates_written_total`
/// bumped by whatever this pass wrote, and `pending_count` refreshed from
/// the vault. The pass window only advances on success — a failed pass
/// (vault I/O error, etc.) leaves `since` where it was so the next tick
/// retries the same window instead of silently losing events.
pub async fn run_curator(
    vault: Arc<Vault>,
    llm: Arc<dyn CuratorLlm>,
    cfg: CuratorConfig,
    status: SharedCuratorStatus,
    mut activity: watch::Receiver<ActivitySignal>,
    mut shutdown: watch::Receiver<bool>,
) {
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

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let pass_start = Utc::now();
                let outcome = extract_pass(&vault, llm.as_ref(), &cfg, window_since).await;

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

                if let Ok(mut s) = status.lock() {
                    s.last_pass_at = Some(pass_start);
                    if let Some(count) = pending_count {
                        s.pending_count = count;
                    }
                    match &outcome {
                        Ok(written) => {
                            s.consecutive_failures = 0;
                            s.candidates_written_total += *written as u64;
                        }
                        Err(_) => {
                            s.consecutive_failures += 1;
                        }
                    }
                }

                match outcome {
                    Ok(_) => window_since = pass_start,
                    Err(err) => {
                        tracing::warn!(
                            layer = "memory",
                            component = "curator",
                            error = %err,
                            "Curator pass failed"
                        );
                    }
                }

                // Task 5 (conflict resolution) and Task 6 (session summary)
                // add their own per-tick calls here.
            }
            _ = activity.changed() => {
                // Latest signal available via `*activity.borrow()`. Task 9
                // wires `project_hint` through here for project-scoped
                // extraction context; nothing to consume yet.
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
        assert_eq!(written, 2); // 0.2 discarded

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
        assert_eq!(written2, 0);
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
        assert_eq!(written, 0);
        assert_eq!(llm.calls(), 2); // initial + one retry with the error appended
    }
}
