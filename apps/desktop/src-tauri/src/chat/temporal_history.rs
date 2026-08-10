//! Focused historical context retrieval for desktop chat.
//!
//! This adapter reuses the existing `context_events` store, re-applies the live
//! privacy policy at read time, and reports retrieval availability/completeness so
//! the model cannot confuse an unavailable history store with evidence of absence.

use chrono::{Duration, Utc};
use continuum_core::context::temporal::{
    ContextCompleteness, TemporalInputProvenance, TemporalObservation, TemporalScope,
    TemporalSynthesizer,
};
use continuum_core::memory::events::{event_enum_token, EventSensitivity};
use continuum_core::memory::raw_log::{EventQuery, RawLog, RawLogError};
use continuum_core::senses::privacy::{PrivacyFilter, Zone};

const LOOKBACK_HOURS: i64 = 6;
const EVENT_LIMIT: usize = 80;

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

fn unavailable(reason: &str) -> String {
    format!(
        "## Historical activity context\n- Retrieval status: unavailable ({reason}).\n\nDo not treat unavailable history as evidence that no prior activity exists.\n"
    )
}

pub(super) async fn temporal_context_section(
    dev_dir: &std::path::Path,
    cfg: &continuum_core::config::ContinuumConfig,
    filter: &PrivacyFilter,
    message: &str,
) -> String {
    if !historical_activity_intent(message) {
        return String::new();
    }
    if !cfg.context_tools.enabled || cfg.privacy.toggles.pause_all {
        return unavailable("historical context is disabled or paused");
    }

    let db_path = if dev_dir.join("config.toml").exists() {
        std::path::PathBuf::from(&cfg.storage.db_path)
    } else {
        dev_dir.join("raw_log.sqlite")
    };
    let log = match RawLog::open_read_only(&db_path.to_string_lossy()).await {
        Ok(log) => log,
        Err(RawLogError::NotYetCreated { .. }) => return unavailable("history store not created yet"),
        Err(error) => {
            tracing::debug!(
                layer = "desktop",
                component = "chat_temporal_context",
                error = %error,
                "historical context unavailable"
            );
            return unavailable("history store could not be opened");
        }
    };

    let now = Utc::now();
    let since = now - Duration::hours(LOOKBACK_HOURS);
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
            return unavailable("history query failed");
        }
    };
    log.close().await;

    let source_limit_reached = rows.len() >= EVENT_LIMIT;
    let source_high_water = rows
        .iter()
        .map(|row| row.id)
        .max()
        .map(|id| format!("context_event:{id}"));
    // The existing config layer does not yet expose a stable privacy generation.
    // A per-query token is intentionally non-reusable, which is safe for A7/A6:
    // cached/displayed synthesis cannot cross a later privacy evaluation.
    let privacy_policy_generation = format!("live-policy@{}", now.timestamp_millis());

    let observations = rows.into_iter().map(|row| {
        let live_zone = filter.resolve_zone(&row.application, &row.window_title);
        let sensitivity = if row.sensitivity == EventSensitivity::LocalOnly
            || live_zone != Zone::CloudAllowed
        {
            EventSensitivity::LocalOnly
        } else {
            EventSensitivity::CloudAllowed
        };

        TemporalObservation {
            source_reference: format!("context_event:{}", row.id),
            source: event_enum_token(&row.source),
            started_at: row.ts_first,
            ended_at: row.ts_last,
            project: row.project_id,
            application: (!row.application.is_empty()).then(|| filter.scrub_text(&row.application)),
            event_type: Some(event_enum_token(&row.event_type)),
            summary: filter.scrub_text(&row.summary),
            confidence: row.confidence,
            sensitivity,
        }
    });

    let context = match TemporalSynthesizer::default().synthesize(
        observations,
        &TemporalScope {
            since: Some(since),
            until: None,
            project: None,
        },
        TemporalInputProvenance {
            source_limit_reached,
            source_high_water,
            retention_floor: None,
            privacy_policy_generation,
        },
    ) {
        Ok(context) => context,
        Err(error) => {
            tracing::warn!(
                layer = "desktop",
                component = "chat_temporal_context",
                error = ?error,
                "invalid historical evidence rejected"
            );
            return unavailable("stored evidence failed validation");
        }
    };

    let mut output = String::from("## Historical activity context\n");
    output.push_str(&format!(
        "- Retrieval status: {}.\n",
        match context.completeness {
            ContextCompleteness::Complete => "complete for the bounded query",
            ContextCompleteness::Partial => "partial; confidence has been reduced",
        }
    ));
    if let Some(high_water) = &context.provenance.source_high_water {
        output.push_str(&format!("- Query high-water: {high_water}.\n"));
    }
    output.push_str(&format!(
        "- Privacy generation: {}.\n",
        context.provenance.privacy_policy_generation
    ));

    if context.sessions.is_empty() {
        output.push_str("- No relevant cloud-eligible observations matched this bounded query.\n");
    } else {
        for session in context.sessions.iter().rev().take(2).rev() {
            output.push_str(&format!(
                "- {}–{} [{:?}, {:.0}%, {:?}]: {}\n",
                session.started_at.format("%H:%M"),
                session.updated_at.format("%H:%M"),
                session.conclusion.strength,
                session.conclusion.confidence * 100.0,
                session.completeness,
                session.conclusion.text,
            ));
            if !session.conclusion.source_references.is_empty() {
                output.push_str("  Evidence: ");
                output.push_str(&session.conclusion.source_references.join(", "));
                output.push('\n');
            }
            if session.conflicting_signals {
                output.push_str("  Contradiction state: conflicting signals observed; confidence reduced.\n");
            }
            if session.dropped_evidence > 0 {
                output.push_str(&format!(
                    "  Completeness: {} same-session evidence row(s) omitted from the rendered evidence set.\n",
                    session.dropped_evidence
                ));
            }
        }
    }
    if context.omitted_private > 0 {
        output.push_str(&format!(
            "- {} matching private observation(s) were withheld by policy.\n",
            context.omitted_private
        ));
    }
    output.push_str(
        "\nSynthesized activity descriptions are inferences; do not present them as directly observed facts. Partial or unavailable history must not be treated as evidence of absence.\n",
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_history_change_and_failure_questions_without_triggering_normal_chat() {
        assert!(historical_activity_intent("What was I doing earlier?"));
        assert!(historical_activity_intent("What changed since I last asked?"));
        assert!(historical_activity_intent("Why did this break?"));
        assert!(historical_activity_intent("Waar was ik mee bezig eerder?"));
        assert!(!historical_activity_intent("Write a Rust function for me"));
    }

    #[test]
    fn unavailable_state_warns_against_evidence_of_absence() {
        let text = unavailable("history query failed");
        assert!(text.contains("unavailable"));
        assert!(text.contains("not evidence"));
    }
}
