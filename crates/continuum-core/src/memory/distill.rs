//! # Memory distillation
//!
//! Background task that promotes interesting raw perception frames into
//! episodic memory. It keeps Layer 3 memory useful without dumping the raw
//! log into the orchestrator context.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::Result;
use chrono::{Duration, Utc};
use continuum_memory::{NewEvent, Vault};
use tokio::sync::{watch, Mutex};
use uuid::Uuid;

use crate::config::MemoryConfig;
use crate::memory::episodic::{EpisodicEvent, EpisodicStore, EventKind};
use crate::memory::raw_log::RawLog;
use crate::senses::types::PerceptionFrame;

/// Runs memory distillation until shutdown.
pub async fn run_memory_distiller(
    raw_log: RawLog,
    episodic: Arc<Mutex<EpisodicStore>>,
    vault: Arc<Vault>,
    config: MemoryConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    if !config.distillation_enabled {
        tracing::info!(
            layer = "memory",
            component = "distiller",
            "Memory distillation disabled by config"
        );
        let _ = shutdown.changed().await;
        return;
    }

    tracing::info!(
        layer = "memory",
        component = "distiller",
        interval_minutes = config.distillation_interval_minutes,
        lookback_minutes = config.distillation_lookback_minutes,
        "Memory distiller started"
    );

    let interval = StdDuration::from_secs(config.distillation_interval_minutes.max(1) * 60);
    let mut ticker = tokio::time::interval(interval);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(err) = distill_once(&raw_log, &episodic, &vault, &config).await {
                    tracing::warn!(
                        layer = "memory",
                        component = "distiller",
                        error = %err,
                        "Memory distillation pass failed"
                    );
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    tracing::info!(
                        layer = "memory",
                        component = "distiller",
                        "Memory distiller stopping"
                    );
                    break;
                }
            }
        }
    }
}

/// Performs one raw-log to episodic-memory distillation pass.
pub async fn distill_once(
    raw_log: &RawLog,
    episodic: &Arc<Mutex<EpisodicStore>>,
    vault: &Arc<Vault>,
    config: &MemoryConfig,
) -> Result<usize> {
    if !config.distillation_enabled {
        return Ok(0);
    }

    let until = Utc::now();
    let since = until - Duration::minutes(config.distillation_lookback_minutes.max(1) as i64);
    let frames = raw_log
        .query_undistilled_frames(
            since,
            until,
            config.distillation_min_salience,
            config.distillation_batch_size.max(1),
        )
        .await?;

    if frames.is_empty() {
        tracing::debug!(
            layer = "memory",
            component = "distiller",
            "No raw frames qualified for distillation"
        );
        return Ok(0);
    }

    let mut marked = Vec::new();
    {
        let mut store = episodic.lock().await;
        for frame in &frames {
            let event = frame_to_memory_event(frame);
            store.insert_event(&event).await?;
            marked.push(frame.id);

            // Best-effort mirror into the vault's event timeline so the
            // curator / dashboard can see distillation activity. Never
            // fails the distill pass — a vault write hiccup must not lose
            // the episodic memory we already committed above.
            let _ = vault
                .append_event(NewEvent {
                    ts: Some(event.ts),
                    kind: "distilled".to_string(),
                    text: event.summary.clone(),
                    project: None,
                    node_id: None,
                    reference: event.source_frame_id.clone().map(|f| format!("frame:{f}")),
                })
                .await
                .map_err(|e| {
                    tracing::warn!(layer = "memory", component = "distiller",
                        error = %e.user_message(), "vault event append failed");
                });
        }
    }

    raw_log.mark_frames_distilled(&marked).await?;

    tracing::info!(
        layer = "memory",
        component = "distiller",
        distilled = marked.len(),
        "Distilled raw frames into episodic memory"
    );

    Ok(marked.len())
}

/// Builds an episodic event from a perception frame.
pub fn frame_to_memory_event(frame: &PerceptionFrame) -> EpisodicEvent {
    EpisodicEvent {
        id: Uuid::new_v4().to_string(),
        ts: frame.ts,
        kind: EventKind::Remember,
        summary: summarize_frame(frame),
        importance: frame_importance(frame),
        tags: frame_tags(frame),
        source_frame_id: Some(frame.id.to_string()),
    }
}

fn summarize_frame(frame: &PerceptionFrame) -> String {
    let mut parts = Vec::new();
    if let Some(audio) = &frame.audio {
        let text = audio.transcript.trim();
        if !text.is_empty() {
            parts.push(format!("User said: \"{}\"", truncate(text, 160)));
        }
    }
    if frame.screen.has_error_visible {
        parts.push("An error was visible on screen".to_string());
    }
    if !frame.context.foreground_process_name.is_empty()
        || !frame.context.foreground_window_title.is_empty()
    {
        parts.push(format!(
            "Context: {} in {}",
            truncate(&frame.context.foreground_window_title, 120),
            truncate(&frame.context.foreground_process_name, 80)
        ));
    }
    if !frame.screen.description.is_empty() {
        parts.push(format!(
            "Screen: {}",
            truncate(&frame.screen.description, 160)
        ));
    }

    if parts.is_empty() {
        format!("Salient perception frame {}", frame.id)
    } else {
        parts.join(". ")
    }
}

fn frame_importance(frame: &PerceptionFrame) -> f32 {
    let mut score = frame.salience_hint.max(0.4);
    if frame
        .audio
        .as_ref()
        .is_some_and(|a| !a.transcript.trim().is_empty())
    {
        score += 0.2;
    }
    if frame.screen.has_error_visible {
        score += 0.2;
    }
    score.clamp(0.0, 1.0)
}

fn frame_tags(frame: &PerceptionFrame) -> Vec<String> {
    let mut tags = vec!["distilled".to_string()];
    if frame
        .audio
        .as_ref()
        .is_some_and(|a| !a.transcript.trim().is_empty())
    {
        tags.push("audio".to_string());
    }
    if frame.screen.has_error_visible {
        tags.push("error".to_string());
    }
    if !frame.context.foreground_process_name.is_empty() {
        tags.push(frame.context.foreground_process_name.to_lowercase());
    }
    tags
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let target = max_len.saturating_sub(3);
    let mut cut = target.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senses::types::{AudioObservation, ContextObservation, ScreenObservation};

    fn test_frame(audio: Option<&str>, has_error: bool) -> PerceptionFrame {
        PerceptionFrame {
            id: Uuid::new_v4(),
            ts: Utc::now(),
            screen: ScreenObservation {
                description: "Editor with a failing cargo test".to_string(),
                foreground_app: "Code.exe".to_string(),
                has_error_visible: has_error,
                confidence: 0.9,
                screenshot_path: None,
                ts: Utc::now(),
            },
            audio: audio.map(|text| AudioObservation {
                transcript: text.to_string(),
                language: "en".to_string(),
                duration_ms: 1200,
                confidence: 0.8,
                ts: Utc::now(),
            }),
            context: ContextObservation {
                foreground_window_title: "main.rs - continuum".to_string(),
                foreground_process_name: "Code.exe".to_string(),
                idle_seconds: 0,
                in_call: false,
                ts: Utc::now(),
            },
            salience_hint: 0.5,
        }
    }

    #[test]
    fn frame_summary_prefers_audio_and_context() {
        let event = frame_to_memory_event(&test_frame(Some("remember this bug"), false));
        assert_eq!(event.kind, EventKind::Remember);
        assert!(event.summary.contains("remember this bug"));
        assert!(event.summary.contains("main.rs"));
        assert!(event.tags.contains(&"audio".to_string()));
    }

    #[test]
    fn frame_summary_marks_errors() {
        let event = frame_to_memory_event(&test_frame(None, true));
        assert!(event.summary.contains("error"));
        assert!(event.tags.contains(&"error".to_string()));
        assert!(event.importance >= 0.7);
    }

    #[tokio::test]
    async fn distill_once_feeds_vault_events() {
        use continuum_memory::{EventRange, VaultOptions};

        // A single undistilled, high-salience (has audio) frame in the raw log.
        let raw_log = RawLog::open("sqlite::memory:").await.unwrap();
        let frame = test_frame(Some("remember this bug"), false);
        raw_log.write_frame(&frame).await.unwrap();

        let episodic_dir = tempfile::tempdir().unwrap();
        let episodic = Arc::new(Mutex::new(
            EpisodicStore::open_for_test(episodic_dir.path().to_str().unwrap())
                .await
                .unwrap(),
        ));

        let vault_dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(
            Vault::open_with(vault_dir.path(), VaultOptions::default())
                .await
                .unwrap(),
        );

        let config = MemoryConfig::default();

        let distilled = distill_once(&raw_log, &episodic, &vault, &config)
            .await
            .unwrap();
        assert_eq!(distilled, 1);

        let events = vault.events(&EventRange::default()).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "distilled");
        assert!(!events[0].text.is_empty()); // the frame summary
    }
}
