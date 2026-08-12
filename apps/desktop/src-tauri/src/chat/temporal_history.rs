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
use continuum_core::senses::live_context::PrivacyDisposition;
use continuum_core::senses::privacy::{PrivacyFilter, Zone};

const LOOKBACK_HOURS: i64 = 6;
const EVENT_LIMIT: usize = 80;
const FRAME_LIMIT: usize = 240;
const RENDERED_EVIDENCE_LIMIT: usize = 8;
// Enough to preserve a few minutes of quick app switching without returning
// the full six-hour evidence window to the model.
const IMMEDIATE_EVIDENCE_LIMIT: usize = 12;

pub(super) fn historical_activity_intent(message: &str) -> bool {
    let message = message.to_lowercase();
    [
        "what was i doing",
        "what did i just do",
        "what have i just done",
        "what did i do just now",
        "what have i been working on",
        "what changed",
        "since i last asked",
        "why did this break",
        "what failed",
        "earlier",
        "before this",
        "wat deed ik",
        "wat heb ik net gedaan",
        "wat heb k net gedaan",
        "wat had ik net gedaan",
        "wat deed ik net",
        "wat zat ik net",
        "wat heb ik zojuist gedaan",
        "wat deed ik zojuist",
        "wat was ik net aan het doen",
        "waar was ik mee bezig",
        "waar ben ik mee bezig geweest",
        "afgelopen minuten",
        "afgelopen paar minuten",
        "in de tussentijd",
        "nog meer gedaan",
        "meer heb gedaan",
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

/// Immediate-recall questions need only the latest pre-chat activity, while
/// broader "earlier/what changed" questions retain the wider two-session view.
pub(super) fn immediate_activity_intent(message: &str) -> bool {
    let message = message.to_lowercase();
    [
        "what did i just do",
        "what have i just done",
        "what did i do just now",
        "what was i just doing",
        "wat heb ik net gedaan",
        "wat heb k net gedaan",
        "wat had ik net gedaan",
        "wat deed ik net",
        "wat zat ik net",
        "wat heb ik zojuist gedaan",
        "wat deed ik zojuist",
        "wat was ik net aan het doen",
        "waar was ik mee bezig",
        "afgelopen minuten",
        "afgelopen paar minuten",
        "in de tussentijd",
        "nog meer gedaan",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn unavailable(reason: &str) -> String {
    format!(
        "## Historical activity context\n- Retrieval status: unavailable ({reason}).\n\nUnavailable history is not evidence that no prior activity exists.\n"
    )
}

fn bounded_evidence_text(value: &str, max_chars: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
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
        Err(RawLogError::NotYetCreated { .. }) => {
            return unavailable("history store not created yet")
        }
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
    let frames = match log.query_recent_frames(since, now, FRAME_LIMIT).await {
        Ok(frames) => frames,
        Err(error) => {
            log.close().await;
            tracing::debug!(
                layer = "desktop",
                component = "chat_temporal_context",
                error = %error,
                "historical vision query failed"
            );
            return unavailable("historical vision query failed");
        }
    };
    log.close().await;

    let source_limit_reached = rows.len() >= EVENT_LIMIT || frames.len() >= FRAME_LIMIT;
    let source_high_water = rows
        .iter()
        .map(|row| row.id)
        .max()
        .map(|id| format!("context_event:{id}"));
    // The existing config layer does not yet expose a stable privacy generation.
    // A per-query token is intentionally non-reusable, which is safe for A7/A6:
    // cached/displayed synthesis cannot cross a later privacy evaluation.
    let privacy_policy_generation = format!("live-policy@{}", now.timestamp_millis());

    let event_observations = rows.into_iter().map(|row| {
        let live_zone = filter.resolve_zone(&row.application, &row.window_title);
        let sensitivity =
            if row.sensitivity == EventSensitivity::LocalOnly || live_zone != Zone::CloudAllowed {
                EventSensitivity::LocalOnly
            } else {
                EventSensitivity::CloudAllowed
            };

        let summary = bounded_evidence_text(&filter.scrub_text(&row.summary), 280);
        let window_title = bounded_evidence_text(&filter.scrub_text(&row.window_title), 160);
        let summary = if window_title.trim().is_empty() || summary.contains(&window_title) {
            summary
        } else {
            format!("{summary}; window: {window_title}")
        };

        TemporalObservation {
            source_reference: format!("context_event:{}", row.id),
            source: event_enum_token(&row.source),
            started_at: row.ts_first,
            ended_at: row.ts_last,
            project: row.project_id,
            application: (!row.application.is_empty()).then(|| filter.scrub_text(&row.application)),
            event_type: Some(event_enum_token(&row.event_type)),
            summary,
            confidence: row.confidence,
            sensitivity,
        }
    });

    // Frames carry deterministic foreground app/window fields independently
    // of local vision. Always retain those facts, even while the vision worker
    // says "awaiting local vision"; otherwise the exact activity trace exists
    // on disk but disappears from chat recall. Consecutive equal windows are
    // collapsed into one segment. A usable vision caption is attached only as
    // explicitly supporting inference.
    let mut previous_window = None;
    let frame_observations = frames.into_iter().filter_map(|frame| {
        let live_zone = filter.resolve_zone(
            &frame.context.foreground_process_name,
            &frame.context.foreground_window_title,
        );
        if frame
            .context
            .privacy
            .is_some_and(|privacy| privacy != PrivacyDisposition::Visible)
            || live_zone != Zone::CloudAllowed
        {
            return None;
        }
        let app = bounded_evidence_text(
            &filter.scrub_text(&frame.context.foreground_process_name),
            80,
        );
        let title = bounded_evidence_text(
            &filter.scrub_text(&frame.context.foreground_window_title),
            160,
        );
        let window_key = format!("{app}\0{title}");
        if previous_window.as_deref() == Some(window_key.as_str()) {
            return None;
        }
        previous_window = Some(window_key);

        let description = frame.screen.description.trim();
        let usable_caption = !description.is_empty()
            && description != "awaiting local vision"
            && description != "(no vision model loaded)"
            && description != "[redacted by local privacy policy]";
        let summary = if usable_caption {
            let caption = bounded_evidence_text(&filter.scrub_text(description), 280);
            format!(
                "deterministic foreground observation: app {app}; window {title}; local vision inference (supporting evidence): {caption}"
            )
        } else {
            format!("deterministic foreground observation: app {app}; window {title}")
        };
        Some(TemporalObservation {
            source_reference: format!("perception_frame:{}", frame.id),
            source: "foreground_observation".to_string(),
            started_at: frame.ts,
            ended_at: frame.ts,
            project: None,
            application: Some(app),
            event_type: Some("focus_snapshot".to_string()),
            summary,
            confidence: 1.0,
            sensitivity: EventSensitivity::CloudAllowed,
        })
    });

    let observations = event_observations.chain(frame_observations);

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
    if context.sessions.is_empty() {
        output.push_str("- No relevant cloud-eligible observations matched this bounded query.\n");
    } else {
        let immediate = immediate_activity_intent(message);
        let session_limit = if immediate { 1 } else { 2 };
        let evidence_limit = if immediate {
            IMMEDIATE_EVIDENCE_LIMIT
        } else {
            RENDERED_EVIDENCE_LIMIT
        };
        for session in context.sessions.iter().rev().take(session_limit).rev() {
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
                output.push_str(
                    "  Contradiction state: conflicting signals observed; confidence reduced.\n",
                );
            }
            if session.dropped_evidence > 0 {
                output.push_str(&format!(
                    "  Completeness: {} same-session evidence row(s) omitted from the rendered evidence set.\n",
                    session.dropped_evidence
                ));
            }
            output.push_str("  Recent evidence (chronological):\n");
            for observation in session.evidence.iter().rev().take(evidence_limit).rev() {
                output.push_str(&format!(
                    "  - {} [{} / {}] {}\n",
                    observation.ended_at.format("%H:%M:%S"),
                    observation.source,
                    observation.event_type.as_deref().unwrap_or("unknown"),
                    observation.summary,
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
        "\nFor questions equivalent to 'what did I just do?', treat opening or returning to Continuum Chat as the boundary and reconstruct the immediately preceding meaningful activity from the latest evidence. Answer in one or two direct sentences. Calibrate certainty per claim: state deterministic application, window-title, timestamp, duration, and focus-order facts directly, without words such as probably, likely, seems, appears, or waarschijnlijk. State the concrete action first, then its reason or goal only when supported by the session state, conversation, window title, or evidence. Use uncertainty language only for genuinely inferred purpose, intent, or activity. An app being open, visible, or focused does not by itself prove the user worked on it or used it alongside another app. Deterministic application/window facts override a conflicting local vision inference; vision captions are supporting evidence and may be imprecise. Do not narrate tool calls, monitors, retrieval status, or generic limitations, and do not ask a follow-up question. Synthesized activity descriptions are inferences, but the deterministic facts they contain remain facts; distinguish the inferred portion instead of hedging the whole answer. Partial or unavailable history must not be treated as evidence of absence.\n",
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use continuum_core::senses::types::{ContextObservation, PerceptionFrame, ScreenObservation};

    #[test]
    fn triggers_history_change_and_failure_questions_without_triggering_normal_chat() {
        assert!(historical_activity_intent("What was I doing earlier?"));
        assert!(historical_activity_intent("What did I just do?"));
        assert!(historical_activity_intent(
            "What changed since I last asked?"
        ));
        assert!(historical_activity_intent("Why did this break?"));
        assert!(historical_activity_intent("Waar was ik mee bezig eerder?"));
        assert!(historical_activity_intent("Wat heb ik net gedaan?"));
        assert!(historical_activity_intent("Nee, wat had ik net gedaan?"));
        assert!(historical_activity_intent("Wat was ik net aan het doen?"));
        assert!(historical_activity_intent(
            "wat zat ik net op me pc te doen?"
        ));
        assert!(historical_activity_intent(
            "goeie man wat heb k afgelopen minuten gedaan? ooaklweer"
        ));
        assert!(historical_activity_intent("nee ik heb nog meer gedaan"));
        assert!(!historical_activity_intent("Write a Rust function for me"));
        assert!(immediate_activity_intent("Wat heb ik net gedaan?"));
        assert!(immediate_activity_intent("Waar was ik mee bezig?"));
        assert!(!immediate_activity_intent("What changed earlier today?"));
    }

    #[test]
    fn unavailable_state_warns_against_evidence_of_absence() {
        let text = unavailable("history query failed");
        assert!(text.contains("unavailable"));
        assert!(text.contains("not evidence"));
    }

    #[test]
    fn evidence_text_is_single_line_and_bounded() {
        let text = bounded_evidence_text("first\nsecond\tthird", 12);
        assert_eq!(text, "first second");
        assert!(!text.contains(['\n', '\t']));
        assert_eq!(text.chars().count(), 12);
    }

    #[tokio::test]
    async fn recall_includes_privacy_filtered_historical_vision_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("raw_log.sqlite");
        let log = RawLog::open(&db.to_string_lossy()).await.unwrap();
        let ts = Utc::now() - Duration::minutes(2);
        let frame = PerceptionFrame {
            id: uuid::Uuid::new_v4(),
            ts,
            screen: ScreenObservation {
                description: "Editing the payment retry flow\nwith tests visible".into(),
                world_compact: None,
                foreground_app: "brave.exe".into(),
                has_error_visible: false,
                confidence: 0.8,
                screenshot_path: None,
                ts,
            },
            audio: None,
            context: ContextObservation {
                foreground_window_title: "Payment retry tests - Brave".into(),
                foreground_process_name: "brave.exe".into(),
                privacy: Some(PrivacyDisposition::Visible),
                ts,
                ..Default::default()
            },
            salience_hint: 0.5,
        };
        log.write_frame(&frame).await.unwrap();
        log.close().await;

        let cfg = continuum_core::config::ContinuumConfig::default();
        let filter = PrivacyFilter::from_config(&cfg.context, &cfg.privacy);
        let section =
            temporal_context_section(dir.path(), &cfg, &filter, "Wat heb ik net gedaan?").await;

        assert!(section.contains("deterministic foreground observation"));
        assert!(section.contains("local vision inference (supporting evidence)"));
        assert!(section.contains("Editing the payment retry flow with tests visible"));
        assert!(section.contains("window Payment retry tests - Brave"));
        assert!(section.contains("one or two direct sentences"));
        assert!(section.contains("Calibrate certainty per claim"));
        assert!(section.contains("without words such as probably"));
        assert!(section.contains("does not by itself prove the user worked on it"));
    }

    #[tokio::test]
    async fn recall_keeps_deterministic_windows_while_vision_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("raw_log.sqlite");
        let log = RawLog::open(&db.to_string_lossy()).await.unwrap();
        let ts = Utc::now() - Duration::minutes(1);
        let frame = PerceptionFrame {
            id: uuid::Uuid::new_v4(),
            ts,
            screen: ScreenObservation {
                description: "awaiting local vision".into(),
                world_compact: None,
                foreground_app: "explorer.exe".into(),
                has_error_visible: false,
                confidence: 0.8,
                screenshot_path: None,
                ts,
            },
            audio: None,
            context: ContextObservation {
                foreground_window_title: "Downloads - File Explorer".into(),
                foreground_process_name: "explorer.exe".into(),
                privacy: Some(PrivacyDisposition::Visible),
                ts,
                ..Default::default()
            },
            salience_hint: 0.1,
        };
        log.write_frame(&frame).await.unwrap();
        log.close().await;

        let cfg = continuum_core::config::ContinuumConfig::default();
        let filter = PrivacyFilter::from_config(&cfg.context, &cfg.privacy);
        let section =
            temporal_context_section(dir.path(), &cfg, &filter, "nee ik heb nog meer gedaan").await;

        assert!(section.contains("deterministic foreground observation"));
        assert!(section.contains("app explorer.exe"));
        assert!(section.contains("window Downloads - File Explorer"));
        assert!(!section.contains("awaiting local vision"));
    }
}
