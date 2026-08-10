//! Focused historical context retrieval for desktop chat.
//!
//! This adapter intentionally reuses the existing `context_events` store. It is
//! activated only for deterministic activity/change/failure question families,
//! re-applies the live privacy policy at read time, and renders at most two
//! synthesized sessions.

use chrono::{Duration, Utc};
use continuum_core::context::temporal::{
    TemporalObservation, TemporalScope, TemporalSensitivity, TemporalSynthesizer,
};
use continuum_core::memory::events::{event_enum_token, EventSensitivity};
use continuum_core::memory::raw_log::{EventQuery, RawLog, RawLogError};
use continuum_core::senses::privacy::{PrivacyFilter, Zone};

const LOOKBACK_HOURS: i64 = 6;
const EVENT_LIMIT: usize = 80;

/// Deterministic trigger for questions that need history rather than just live state.
pub(super) fn historical_activity_intent(message: &str) -> bool {
    let message = message.to_lowercase();
    [
        "what was i doing",
        "what have i been working on",
        "what changed",
        "since i last asked",
        "why did this break",
        "what failed",
        "earlier",
        "before this",
        "wat deed ik",
        "waar was ik mee bezig",
        "waar ben ik mee bezig geweest",
        "wat is er veranderd",
        "wat veranderde",
        "sinds ik het laatst vroeg",
        "waarom ging dit kapot",
        "wat faalde",
        "eerder",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// Build a small provenance-bearing historical prompt section.
///
/// The database is opened query-only. Rows are bounded by time/count and then
/// re-gated against the current privacy policy before synthesis. A cloud-bound
/// chat can therefore never recover a row that has become local-only since it
/// was originally recorded.
pub(super) async fn temporal_context_section(
    dev_dir: &std::path::Path,
    cfg: &continuum_core::config::ContinuumConfig,
    filter: &PrivacyFilter,
    message: &str,
) -> String {
    if !historical_activity_intent(message)
        || !cfg.context_tools.enabled
        || cfg.privacy.toggles.pause_all
    {
        return String::new();
    }

    let db_path = if dev_dir.join("config.toml").exists() {
        std::path::PathBuf::from(&cfg.storage.db_path)
    } else {
        dev_dir.join("raw_log.sqlite")
    };
    let log = match RawLog::open_read_only(&db_path.to_string_lossy()).await {
        Ok(log) => log,
        Err(RawLogError::NotYetCreated { .. }) => return String::new(),
        Err(error) => {
            tracing::debug!(
                layer = "desktop",
                component = "chat_temporal_context",
                error = %error,
                "historical context unavailable"
            );
            return String::new();
        }
    };

    let since = Utc::now() - Duration::hours(LOOKBACK_HOURS);
    let query = EventQuery {
        since: Some(since),
        limit: EVENT_LIMIT,
        ..EventQuery::default()
    };
    let rows = match log.query_context_events(&query).await {
        Ok(rows) => rows,
        Err(error) => {
            log.close().await;
            tracing::debug!(
                layer = "desktop",
                component = "chat_temporal_context",
                error = %error,
                "historical context query failed"
            );
            return String::new();
        }
    };
    log.close().await;

    let observations = rows.into_iter().map(|row| {
        // Privacy decisions are intentionally re-evaluated at read/egress time.
        let live_zone = filter.resolve_zone(&row.application, &row.window_title);
        let sensitivity =
            if row.sensitivity == EventSensitivity::LocalOnly || live_zone != Zone::CloudAllowed {
                TemporalSensitivity::LocalOnly
            } else {
                TemporalSensitivity::CloudAllowed
            };

        TemporalObservation {
            source_reference: format!("context_event:{}", row.id),
            source: event_enum_token(&row.source).to_string(),
            started_at: row.ts_first,
            ended_at: row.ts_last,
            project: row.project_id,
            application: (!row.application.is_empty()).then(|| filter.scrub_text(&row.application)),
            event_type: Some(event_enum_token(&row.event_type).to_string()),
            summary: filter.scrub_text(&row.summary),
            confidence: row.confidence,
            sensitivity,
        }
    });

    let context = TemporalSynthesizer::default().synthesize(
        observations,
        &TemporalScope {
            since: Some(since),
            until: None,
            project: None,
            include_local_only: false,
        },
    );
    if context.sessions.is_empty() {
        return String::new();
    }

    let mut output = String::from("## Historical activity context\n");
    for session in context.sessions.iter().rev().take(2).rev() {
        output.push_str(&format!(
            "- {}–{} [{:?}, {:.0}%]: {}\n",
            session.started_at.format("%H:%M"),
            session.updated_at.format("%H:%M"),
            session.conclusion.strength,
            session.conclusion.confidence * 100.0,
            session.conclusion.text,
        ));
        if !session.conclusion.source_references.is_empty() {
            output.push_str("  Evidence: ");
            output.push_str(&session.conclusion.source_references.join(", "));
            output.push('\n');
        }
    }
    if context.omitted_private > 0 {
        output.push_str(&format!(
            "- {} matching private observation(s) were withheld by policy.\n",
            context.omitted_private
        ));
    }
    output.push_str(
        "\nSynthesized activity descriptions are inferences; do not present them as directly observed facts.\n",
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_history_change_and_failure_questions_without_triggering_normal_chat() {
        assert!(historical_activity_intent("What was I doing earlier?"));
        assert!(historical_activity_intent(
            "What changed since I last asked?"
        ));
        assert!(historical_activity_intent("Why did this break?"));
        assert!(historical_activity_intent("Waar was ik mee bezig eerder?"));
        assert!(!historical_activity_intent("Write a Rust function for me"));
    }
}
