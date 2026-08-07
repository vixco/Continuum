//! # Memory distillation
//!
//! Background task that promotes what mattered into episodic memory. It
//! keeps Layer 3 memory useful without dumping the raw log into the
//! orchestrator context.
//!
//! ## The compression ladder (context engine spec §4.11)
//!
//! Since Task B6 the distiller has two inputs, in priority order:
//!
//! 1. **Deduped `context_events` rows — the primary source.** Triage
//!    classification (B1/B3) turns salient frames into typed events, and
//!    the events writer (A6) collapses repeats into one row with a
//!    `count`. Distilling *those* rows is the compression rung: fourteen
//!    identical build failures become one memory reading
//!    `"build failed (×14)"`, not fourteen near-identical vectors that
//!    drown out everything else in a similarity search.
//! 2. **Raw perception frames — the fallback.** Frames that never
//!    produced an event (triage skipped them, classification was absent
//!    or malformed, or the row predates B3) still distil through the
//!    original salience/audio/error SQL predicate. Frames that *did*
//!    produce an event are excluded so a moment is never recorded twice
//!    (see `RawLog::query_undistilled_unclassified_frames`).
//!
//! Both paths mark their source rows so a restart never re-distils, and
//! both mirror into the vault's timeline as `distilled` events.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::Result;
use chrono::{Duration, Utc};
use continuum_memory::{NewEvent, Vault};
use tokio::sync::{watch, Mutex};
use uuid::Uuid;

use crate::config::{MemoryConfig, StorageConfig};
use crate::memory::episodic::{EpisodicEvent, EpisodicStore, EventKind};
use crate::memory::events::{event_enum_token, EventSensitivity};
use crate::memory::raw_log::{ContextEventRow, RawLog};
// The single definition of the spec §4.1 "is this frame local_only"
// predicate (disposition tag + legacy sentinels). Imported rather than
// re-derived so the distiller and the wake packager can never disagree
// about which frames are private.
use crate::orchestrator::wake_context::frame_is_local_only;
use crate::senses::screenshots::ScreenshotPolicy;
use crate::senses::types::PerceptionFrame;

/// Importance bonus per doubling of a collapsed event's `count`
/// (§4.11 "count-aware"). A thing that happened repeatedly matters more
/// than the same thing once, but the classifier's own judgement stays
/// dominant — hence the small step and the hard cap below.
const COUNT_BOOST_PER_DOUBLING: f32 = 0.05;

/// Ceiling on the count boost, reached at 16 occurrences.
const COUNT_BOOST_CAP: f32 = 0.20;

/// How often the distiller ticker also runs raw-log retention rotation
/// (frames past `[storage] retention_days` + their screenshots + the
/// mtime backstop sweep). Independent of the distillation interval so
/// shortening the latter does not turn rotation into a hot loop.
const ROTATE_INTERVAL: StdDuration = StdDuration::from_secs(60 * 60);

/// Runs memory distillation and raw-log retention rotation until
/// shutdown.
///
/// Rotation lives on this ticker rather than in a component of its own so
/// there is exactly one memory-maintenance task to supervise, and it runs
/// **even when distillation is disabled** — retention is a privacy
/// promise (`docs/senses.md`: "frames older than the retention period are
/// deleted"), not a feature of distillation.
pub async fn run_memory_distiller(
    raw_log: RawLog,
    episodic: Arc<Mutex<EpisodicStore>>,
    vault: Arc<Vault>,
    config: MemoryConfig,
    storage: StorageConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    tracing::info!(
        layer = "memory",
        component = "distiller",
        distillation_enabled = config.distillation_enabled,
        interval_minutes = config.distillation_interval_minutes,
        lookback_minutes = config.distillation_lookback_minutes,
        retention_days = storage.retention_days,
        "Memory distiller started"
    );

    let interval = StdDuration::from_secs(config.distillation_interval_minutes.max(1) * 60);
    let mut ticker = tokio::time::interval(interval);
    let mut last_rotate: Option<tokio::time::Instant> = None;

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
                let rotate_due = last_rotate
                    .map(|at| at.elapsed() >= ROTATE_INTERVAL)
                    .unwrap_or(true);
                if rotate_due {
                    last_rotate = Some(tokio::time::Instant::now());
                    rotate_once(&raw_log, &storage).await;
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

/// One retention pass, never failing the maintenance loop: expired frames
/// and their screenshots go, then the mtime backstop sweep reclaims
/// images no row ever pointed at (Task B6, spec §4.11).
pub async fn rotate_once(raw_log: &RawLog, storage: &StorageConfig) {
    let policy = ScreenshotPolicy::from(storage);
    let dir = std::path::PathBuf::from(&storage.screenshots_dir);
    match raw_log
        .rotate_with_policy(storage.retention_days, Some(dir.as_path()), &policy)
        .await
    {
        Ok(stats) => tracing::debug!(
            layer = "memory",
            component = "distiller",
            frames_deleted = stats.frames_deleted,
            screenshots_deleted = stats.screenshots_deleted,
            swept = stats.swept.deleted,
            "Raw-log retention rotation complete"
        ),
        Err(err) => tracing::warn!(
            layer = "memory",
            component = "distiller",
            error = %err,
            "Raw-log retention rotation failed; retrying next interval"
        ),
    }
}

/// Performs one distillation pass: deduped context events first, then
/// unclassified raw frames as fallback (see the module docs).
///
/// Returns the total number of episodic memories written.
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
    let budget = config.distillation_batch_size.max(1);

    // --- Primary source: deduped, count-aware context events. ---
    let rows = raw_log
        .query_undistilled_events(
            since,
            until,
            config.distillation_min_event_importance,
            budget,
        )
        .await?;

    // --- Fallback source: frames that never produced an event. The
    // events already consumed part of the batch budget, so the fallback
    // gets what is left — one pass never does unbounded work. ---
    let frame_budget = budget.saturating_sub(rows.len());
    let frames = if frame_budget == 0 {
        Vec::new()
    } else {
        raw_log
            .query_undistilled_unclassified_frames(
                since,
                until,
                config.distillation_min_salience,
                frame_budget,
            )
            .await?
    };

    if rows.is_empty() && frames.is_empty() {
        tracing::debug!(
            layer = "memory",
            component = "distiller",
            "Nothing qualified for distillation"
        );
        return Ok(0);
    }

    let mut marked_events = Vec::new();
    let mut marked_frames = Vec::new();
    {
        let mut store = episodic.lock().await;
        for row in &rows {
            let event = event_to_memory_event(row);
            store.insert_event(&event).await?;
            marked_events.push(row.id);
            mirror_into_vault(vault, &event).await;
        }
        for frame in &frames {
            let event = frame_to_memory_event(frame);
            store.insert_event(&event).await?;
            marked_frames.push(frame.id);
            mirror_into_vault(vault, &event).await;
        }
    }

    raw_log.mark_events_distilled(&marked_events).await?;
    raw_log.mark_frames_distilled(&marked_frames).await?;

    let total = marked_events.len() + marked_frames.len();
    tracing::info!(
        layer = "memory",
        component = "distiller",
        distilled_events = marked_events.len(),
        distilled_frames = marked_frames.len(),
        distilled = total,
        "Distilled into episodic memory"
    );

    Ok(total)
}

/// Best-effort mirror into the vault's event timeline so the curator /
/// dashboard can see distillation activity. Never fails the distill pass
/// — a vault write hiccup must not lose the episodic memory already
/// committed. The `project` field carries the same attribution the
/// episodic memory got (Task B6), and `local_only` carries the same §4.1
/// zone sensitivity (fixwave 3a, C1) — the timeline is a local store, so
/// the row is written either way, but it is written *tagged*, so a
/// downstream consumer that leaves the machine can tell the difference.
async fn mirror_into_vault(vault: &Arc<Vault>, event: &EpisodicEvent) {
    let _ = vault
        .append_event(NewEvent {
            ts: Some(event.ts),
            kind: "distilled".to_string(),
            text: event.summary.clone(),
            project: event.project.clone(),
            node_id: None,
            reference: event.source_frame_id.clone().map(|f| format!("frame:{f}")),
            local_only: event.sensitivity == EventSensitivity::LocalOnly,
        })
        .await
        .map_err(|e| {
            tracing::warn!(layer = "memory", component = "distiller",
                error = %e.user_message(), "vault event append failed");
        });
}

/// Builds an episodic memory from one deduped context-events row — the
/// compression rung of the ladder (spec §4.11).
///
/// The row's `count` shows up twice: as a `"(×N)"` suffix on the summary
/// so the memory reads honestly, and as a bounded importance boost
/// ([`count_boost`]).
pub fn event_to_memory_event(row: &ContextEventRow) -> EpisodicEvent {
    EpisodicEvent {
        id: Uuid::new_v4().to_string(),
        // `ts_last`: an episodic memory answers "when did this last
        // happen", and the collapse span is recoverable from the events
        // table if anyone needs `ts_first`.
        ts: row.ts_last,
        kind: EventKind::Remember,
        summary: summarize_event(row),
        importance: (row.importance + count_boost(row.count)).clamp(0.0, 1.0),
        tags: event_tags(row),
        // Since B3 the raw_reference of a screen/audio event is the bare
        // frame id; anything else (a git sha, a file path) is not a frame
        // pointer and must not be stored as one.
        source_frame_id: row
            .raw_reference
            .as_deref()
            .filter(|r| Uuid::parse_str(r).is_ok())
            .map(str::to_string),
        project: row.project_id.clone(),
        // Fixwave 3a (C1): the memory is a summary of the row's own text,
        // so it inherits the row's zone sensitivity. Without this a
        // `local_only` event's text is withheld from the cloud as an event
        // and then sent one distillation pass later as a memory.
        sensitivity: row.sensitivity,
    }
}

/// Importance bonus for a collapsed row: `+0.05` per doubling of `count`,
/// capped at `+0.20` (reached at 16 occurrences). `count <= 1` scores 0,
/// so a one-off memory is scored purely by the classifier.
fn count_boost(count: i64) -> f32 {
    if count <= 1 {
        return 0.0;
    }
    let doublings = (count as f32).log2().floor();
    (COUNT_BOOST_PER_DOUBLING * doublings).min(COUNT_BOOST_CAP)
}

fn summarize_event(row: &ContextEventRow) -> String {
    let mut head = truncate(row.summary.trim(), 160);
    if row.count > 1 {
        head = format!("{head} (×{})", row.count);
    }
    let mut parts = vec![head];
    if !row.window_title.is_empty() || !row.application.is_empty() {
        parts.push(format!(
            "Context: {} in {}",
            truncate(&row.window_title, 120),
            truncate(&row.application, 80)
        ));
    }
    parts.join(". ")
}

fn event_tags(row: &ContextEventRow) -> Vec<String> {
    let mut tags = vec!["distilled".to_string(), "event".to_string()];
    let event_type = event_enum_token(&row.event_type);
    if !event_type.is_empty() {
        tags.push(event_type);
    }
    if row.count > 1 {
        tags.push("repeated".to_string());
    }
    if let Some(project) = &row.project_id {
        tags.push(project.to_lowercase());
    }
    if !row.application.is_empty() {
        tags.push(row.application.to_lowercase());
    }
    tags
}

/// Builds an episodic event from a perception frame (the fallback path).
pub fn frame_to_memory_event(frame: &PerceptionFrame) -> EpisodicEvent {
    EpisodicEvent {
        id: Uuid::new_v4().to_string(),
        ts: frame.ts,
        kind: EventKind::Remember,
        summary: summarize_frame(frame),
        importance: frame_importance(frame),
        tags: frame_tags(frame),
        source_frame_id: Some(frame.id.to_string()),
        // A raw frame carries no project attribution: by construction it
        // is a frame that produced no classified event, and the resolver's
        // *current* project says nothing about a frame from 20 minutes
        // ago. Unattributed memories stay visible to every project's
        // retrieval filter (see `EpisodicStore::search_similar`).
        project: None,
        // Fixwave 3a (C1): the fallback path summarizes the frame's own
        // caption/transcript/title, so it inherits the frame's zone the
        // same way the wake packager reads it off a trigger frame.
        sensitivity: if frame_is_local_only(frame) {
            EventSensitivity::LocalOnly
        } else {
            EventSensitivity::CloudAllowed
        },
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
    // Caption, not `world_compact` (spec §4.10): an episodic memory should
    // record what was on screen semantically, not the machine-shaped
    // multi-monitor world dump — which would also blow the 160-char cap
    // with header text on every single frame.
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
    use crate::memory::events::{ContextEvent, EventSensitivity, EventSource, EventType};
    use crate::senses::types::{AudioObservation, ContextObservation, ScreenObservation};

    fn test_frame(audio: Option<&str>, has_error: bool) -> PerceptionFrame {
        PerceptionFrame {
            id: Uuid::new_v4(),
            ts: Utc::now(),
            screen: ScreenObservation {
                description: "Editor with a failing cargo test".to_string(),
                world_compact: None,
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
                ..Default::default()
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

    #[test]
    fn frame_memories_carry_no_project() {
        // Documented: the fallback path has no attribution to give, and
        // an unattributed memory is visible to every project filter.
        assert_eq!(
            frame_to_memory_event(&test_frame(None, false)).project,
            None
        );
    }

    // --- Task B6: count-aware event distillation (spec §4.11) ---

    fn test_row(summary: &str, count: i64, importance: f32) -> ContextEventRow {
        ContextEventRow {
            id: 1,
            ts_first: Utc::now() - Duration::minutes(10),
            ts_last: Utc::now(),
            count,
            source: EventSource::Screen,
            application: "Code.exe".to_string(),
            window_title: "main.rs - continuum".to_string(),
            project_id: Some("continuum".to_string()),
            event_type: EventType::Error,
            summary: summary.to_string(),
            importance,
            confidence: 0.8,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: None,
            dedupe_key: "k".to_string(),
        }
    }

    #[test]
    fn single_occurrence_event_distills_without_a_count_suffix() {
        let memory = event_to_memory_event(&test_row("build failed", 1, 0.6));
        assert_eq!(memory.kind, EventKind::Remember);
        assert!(memory.summary.starts_with("build failed"));
        assert!(
            !memory.summary.contains('×'),
            "count 1 must not be decorated: {}",
            memory.summary
        );
        assert!(memory.summary.contains("main.rs"), "context is rendered");
        assert!((memory.importance - 0.6).abs() < f32::EPSILON);
        assert!(!memory.tags.contains(&"repeated".to_string()));
        assert_eq!(memory.project.as_deref(), Some("continuum"));
        assert!(memory.tags.contains(&"error".to_string()), "event type tag");
        assert!(memory.tags.contains(&"event".to_string()));
    }

    #[test]
    fn collapsed_event_distills_to_one_count_aware_memory() {
        // The spec's own example: "build failed ×14" — ONE memory, not 14.
        let memory = event_to_memory_event(&test_row("build failed", 14, 0.6));
        assert!(
            memory.summary.contains("build failed (×14)"),
            "expected a count-aware summary, got: {}",
            memory.summary
        );
        // 14 → 3 doublings → +0.15.
        assert!(
            (memory.importance - 0.75).abs() < 1e-5,
            "importance {} should be 0.6 + 0.15",
            memory.importance
        );
        assert!(memory.tags.contains(&"repeated".to_string()));
    }

    #[test]
    fn count_boost_is_logarithmic_and_capped() {
        assert_eq!(count_boost(0), 0.0);
        assert_eq!(count_boost(1), 0.0);
        assert!((count_boost(2) - 0.05).abs() < 1e-6);
        assert!((count_boost(4) - 0.10).abs() < 1e-6);
        assert!((count_boost(14) - 0.15).abs() < 1e-6);
        assert!((count_boost(16) - 0.20).abs() < 1e-6);
        assert!((count_boost(5_000) - 0.20).abs() < 1e-6, "capped");
        // And the boost can never push importance out of range.
        assert!(event_to_memory_event(&test_row("x", 5_000, 0.95)).importance <= 1.0);
    }

    #[test]
    fn event_memory_keeps_only_frame_shaped_references() {
        let frame_id = Uuid::new_v4();
        let mut row = test_row("build failed", 1, 0.6);
        row.raw_reference = Some(frame_id.to_string());
        assert_eq!(
            event_to_memory_event(&row).source_frame_id,
            Some(frame_id.to_string())
        );

        // A git sha / file path is not a frame pointer.
        row.raw_reference = Some("a1b2c3d".to_string());
        assert_eq!(event_to_memory_event(&row).source_frame_id, None);
    }

    async fn test_stores() -> (
        RawLog,
        Arc<Mutex<EpisodicStore>>,
        Arc<Vault>,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        use continuum_memory::VaultOptions;
        let raw_log = RawLog::open("sqlite::memory:").await.unwrap();
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
        (raw_log, episodic, vault, episodic_dir, vault_dir)
    }

    async fn write_event(raw_log: &RawLog, event: &ContextEvent, key: &str) -> i64 {
        let mut conn = raw_log.immediate_conn().await.unwrap();
        let result = crate::memory::raw_log::insert_context_event(&mut conn, event, key).await;
        RawLog::finish_write(conn, result).await.unwrap()
    }

    /// Collapses `count` occurrences into an existing row, the way the
    /// events writer does on a repeat.
    async fn collapse_to(raw_log: &RawLog, id: i64, count: i64) {
        let mut conn = raw_log.immediate_conn().await.unwrap();
        let result =
            crate::memory::raw_log::collapse_context_event(&mut conn, id, count, Utc::now()).await;
        assert!(RawLog::finish_write(conn, result).await.unwrap());
    }

    fn screen_event(summary: &str, frame_id: Option<Uuid>) -> ContextEvent {
        ContextEvent {
            ts: Utc::now(),
            source: EventSource::Screen,
            application: "Code.exe".to_string(),
            window_title: "main.rs - continuum".to_string(),
            project_id: Some("continuum".to_string()),
            event_type: EventType::Error,
            summary: summary.to_string(),
            importance: 0.7,
            confidence: 0.8,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: frame_id.map(|id| id.to_string()),
        }
    }

    #[tokio::test]
    async fn distill_once_prefers_events_and_falls_back_for_unclassified_frames() {
        use continuum_memory::EventRange;

        let (raw_log, episodic, vault, _ep_dir, _vault_dir) = test_stores().await;

        // A classified frame (event + frame row) and an unclassified one.
        let classified = test_frame(Some("the build is broken again"), true);
        let unclassified = test_frame(Some("unrelated thought"), false);
        raw_log.write_frame(&classified).await.unwrap();
        raw_log.write_frame(&unclassified).await.unwrap();
        write_event(
            &raw_log,
            &screen_event("build failed", Some(classified.id)),
            "k-build",
        )
        .await;

        let config = MemoryConfig::default();
        let distilled = distill_once(&raw_log, &episodic, &vault, &config)
            .await
            .unwrap();
        assert_eq!(distilled, 2, "one event + one unclassified frame");

        let vault_events = vault.events(&EventRange::default()).await.unwrap();
        assert_eq!(vault_events.len(), 2);
        assert!(vault_events.iter().all(|e| e.kind == "distilled"));
        // The event-sourced memory carries the project onto the timeline.
        assert!(
            vault_events
                .iter()
                .any(|e| e.project.as_deref() == Some("continuum")),
            "the vault timeline event must carry the project"
        );
        assert!(
            vault_events.iter().any(|e| e.project.is_none()),
            "the frame-sourced memory stays unattributed"
        );
        // The classified frame was NOT distilled a second time as a frame.
        assert!(
            vault_events
                .iter()
                .filter(|e| e.text.contains("the build is broken again"))
                .count()
                == 0,
            "a classified frame must not also distil through the fallback"
        );

        // Second pass: everything is marked, so nothing is re-distilled.
        assert_eq!(
            distill_once(&raw_log, &episodic, &vault, &config)
                .await
                .unwrap(),
            0
        );
        assert_eq!(vault.events(&EventRange::default()).await.unwrap().len(), 2);

        raw_log.close().await;
    }

    #[tokio::test]
    async fn distill_once_writes_one_memory_for_a_collapsed_event() {
        let (raw_log, episodic, vault, _ep_dir, _vault_dir) = test_stores().await;

        let id = write_event(&raw_log, &screen_event("build failed", None), "k-build").await;
        // Simulate 14 collapsed occurrences (what the events writer does).
        collapse_to(&raw_log, id, 14).await;

        let config = MemoryConfig::default();
        assert_eq!(
            distill_once(&raw_log, &episodic, &vault, &config)
                .await
                .unwrap(),
            1,
            "one collapsed row is ONE memory, not 14"
        );

        let memories = episodic.lock().await.recent_events(10).await.unwrap();
        assert_eq!(memories.len(), 1);
        assert!(
            memories[0].summary.contains("(×14)"),
            "got: {}",
            memories[0].summary
        );
        assert_eq!(memories[0].project.as_deref(), Some("continuum"));

        raw_log.close().await;
    }

    #[tokio::test]
    async fn distill_once_skips_events_below_the_importance_threshold() {
        let (raw_log, episodic, vault, _ep_dir, _vault_dir) = test_stores().await;

        let mut trivial = screen_event("nothing much happened", None);
        trivial.importance = 0.05;
        write_event(&raw_log, &trivial, "k-trivial").await;

        let config = MemoryConfig::default(); // min_salience 0.35
        assert_eq!(
            distill_once(&raw_log, &episodic, &vault, &config)
                .await
                .unwrap(),
            0
        );
        raw_log.close().await;
    }

    #[tokio::test]
    async fn distillation_disabled_distils_nothing() {
        let (raw_log, episodic, vault, _ep_dir, _vault_dir) = test_stores().await;
        write_event(&raw_log, &screen_event("build failed", None), "k").await;

        let config = MemoryConfig {
            distillation_enabled: false,
            ..MemoryConfig::default()
        };
        assert_eq!(
            distill_once(&raw_log, &episodic, &vault, &config)
                .await
                .unwrap(),
            0
        );
        raw_log.close().await;
    }

    // --- fixwave 3a C1: sensitivity survives the whole ladder -------------

    #[tokio::test]
    async fn a_local_only_event_distils_into_a_local_only_memory() {
        use continuum_memory::EventRange;

        let (raw_log, episodic, vault, _ep_dir, _vault_dir) = test_stores().await;

        let mut private = screen_event("reviewing the Q3 layoff list", None);
        private.sensitivity = EventSensitivity::LocalOnly;
        write_event(&raw_log, &private, "k-private").await;
        write_event(
            &raw_log,
            &screen_event("build failed in the triage parser", None),
            "k-public",
        )
        .await;

        let config = MemoryConfig::default();
        assert_eq!(
            distill_once(&raw_log, &episodic, &vault, &config)
                .await
                .unwrap(),
            2
        );

        let memories = episodic.lock().await.recent_events(10).await.unwrap();
        let private_memory = memories
            .iter()
            .find(|m| m.summary.contains("layoff"))
            .expect("the local_only memory is still stored — only its egress changes");
        assert_eq!(
            private_memory.sensitivity,
            EventSensitivity::LocalOnly,
            "the memory must inherit the event's zone, or the cloud gate is blind"
        );
        let public_memory = memories
            .iter()
            .find(|m| m.summary.contains("triage parser"))
            .expect("the cloud-allowed memory");
        assert_eq!(public_memory.sensitivity, EventSensitivity::CloudAllowed);

        // The vault timeline mirror carries the same tag.
        let vault_events = vault.events(&EventRange::default()).await.unwrap();
        assert!(
            vault_events
                .iter()
                .any(|e| e.text.contains("layoff") && e.local_only),
            "the vault mirror must tag the local_only memory: {vault_events:?}"
        );
        assert!(vault_events
            .iter()
            .any(|e| e.text.contains("triage parser") && !e.local_only));

        raw_log.close().await;
    }

    #[test]
    fn a_frame_from_a_private_window_distils_local_only() {
        let mut frame = test_frame(Some("the layoff list"), false);
        frame.context.foreground_process_name =
            crate::senses::privacy::EXCLUDED_PROCESS.to_string();
        assert_eq!(
            frame_to_memory_event(&frame).sensitivity,
            EventSensitivity::LocalOnly
        );
        assert_eq!(
            frame_to_memory_event(&test_frame(None, true)).sensitivity,
            EventSensitivity::CloudAllowed
        );
    }

    // --- fixwave 3a C3: collapsed frames are recorded once ----------------

    /// Drives the REAL events writer, because the bug lives in what the
    /// writer does (or used to not do) on a collapse.
    async fn write_via_writer(raw_log: &RawLog, events: Vec<ContextEvent>) {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let expected = events.len() as u64;
        let (sender, handle) = crate::memory::events::spawn_event_writer(
            raw_log.clone(),
            &crate::config::EventsConfig::default(),
            None,
            shutdown_rx,
        );
        for event in events {
            sender.send(event);
        }
        let drained =
            crate::bench::replay::wait_for_writer(&handle, expected, StdDuration::from_secs(10))
                .await;
        let _ = shutdown_tx.send(true);
        handle.wait_for_stop(StdDuration::from_secs(5)).await;
        assert!(drained, "the events writer did not drain in time");
    }

    #[tokio::test]
    async fn fourteen_collapsed_frames_produce_exactly_one_memory() {
        // The C3 inversion: `collapse_context_event` bumps count/ts_last
        // and records nothing about the collapsing frame, so frames #2..14
        // slipped through the fallback's `NOT EXISTS (… raw_reference …)`
        // — `screen_has_error = 1` being exactly the condition that caused
        // the classification in the first place. Result: one
        // "build failed (×14)" memory PLUS 13 raw memories of the same
        // moment, the pollution the ladder exists to prevent.
        let (raw_log, episodic, vault, _ep_dir, _vault_dir) = test_stores().await;

        let mut events = Vec::new();
        for _ in 0..14 {
            let frame = test_frame(None, true);
            raw_log.write_frame(&frame).await.unwrap();
            events.push(screen_event("build failed", Some(frame.id)));
        }
        write_via_writer(&raw_log, events).await;

        let rows = raw_log.list_context_events().await.unwrap();
        assert_eq!(rows.len(), 1, "the 14 events must collapse onto one row");
        assert_eq!(rows[0].count, 14);

        let config = MemoryConfig::default();
        let distilled = distill_once(&raw_log, &episodic, &vault, &config)
            .await
            .unwrap();
        assert_eq!(
            distilled, 1,
            "14 collapsed frames are ONE memory, not 1 + 13 raw frames"
        );

        let memories = episodic.lock().await.recent_events(50).await.unwrap();
        assert_eq!(memories.len(), 1);
        assert!(memories[0].summary.contains("(×14)"), "{memories:?}");

        raw_log.close().await;
    }

    // --- fixwave 3a I2: the primary rung is actually reachable ------------

    #[tokio::test]
    async fn collector_events_reach_the_distiller_under_default_config() {
        use crate::memory::events::COLLECTOR_EVENT_IMPORTANCE;

        let (raw_log, episodic, vault, _ep_dir, _vault_dir) = test_stores().await;

        // Exactly what `senses::context` / `git_watch` / `file_watch` emit:
        // a deterministic collector event at COLLECTOR_EVENT_IMPORTANCE.
        let routine = ContextEvent {
            ts: Utc::now(),
            source: EventSource::Window,
            application: "Code.exe".to_string(),
            window_title: "main.rs - continuum".to_string(),
            project_id: Some("continuum".to_string()),
            event_type: EventType::FocusSwitch,
            summary: "focus moved to main.rs".to_string(),
            importance: COLLECTOR_EVENT_IMPORTANCE,
            confidence: 1.0,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: None,
        };
        write_event(&raw_log, &routine, "k-routine").await;

        let config = MemoryConfig::default();
        assert!(
            config.distillation_min_event_importance < COLLECTOR_EVENT_IMPORTANCE,
            "the default event threshold must sit below collector importance, \
             or the 'primary' distiller input is empty by construction"
        );
        assert_eq!(
            distill_once(&raw_log, &episodic, &vault, &config)
                .await
                .unwrap(),
            1,
            "a collector-importance event must reach the distiller"
        );
        raw_log.close().await;
    }

    #[tokio::test]
    async fn a_blank_summary_event_does_not_suppress_its_frame() {
        // A classification with an empty summary is excluded from the
        // primary query forever (`length(trim(summary)) > 0`). Letting it
        // suppress its frame too erased the moment from memory entirely.
        let (raw_log, episodic, vault, _ep_dir, _vault_dir) = test_stores().await;

        let frame = test_frame(Some("remember this bug"), true);
        raw_log.write_frame(&frame).await.unwrap();
        write_via_writer(&raw_log, vec![screen_event("   ", Some(frame.id))]).await;

        let config = MemoryConfig::default();
        assert_eq!(
            distill_once(&raw_log, &episodic, &vault, &config)
                .await
                .unwrap(),
            1,
            "the frame must still reach memory through the fallback"
        );
        let memories = episodic.lock().await.recent_events(10).await.unwrap();
        assert!(
            memories[0].summary.contains("remember this bug"),
            "{memories:?}"
        );

        raw_log.close().await;
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
