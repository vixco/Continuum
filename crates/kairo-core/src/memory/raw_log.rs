//! # Raw log storage
//!
//! SQLite-backed store for every perception frame produced by the senses layer.
//! One row per frame, including screenshot paths, transcripts, and context.
//!
//! Retention: default 30 days, configurable 1–365. Rotated nightly.
//! Screenshots are saved to `~/.kairo-dev/screenshots/<date>/` as files,
//! with paths stored in the database (not blobs).
//!
//! Part of the memory system in the Kairo cognitive architecture.
//! The raw log is the foundational store — episodic and semantic memory
//! (Phases 2–3) are distilled from it.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite};
use std::path::Path;
use std::str::FromStr;
use tracing::{debug, info, warn};

use crate::senses::types::PerceptionFrame;

/// Manages the SQLite raw log database for perception frames.
///
/// Each frame is stored as a row in the `perception_frames` table.
/// The schema is created automatically on first connection.
pub struct RawLog {
    pool: Pool<Sqlite>,
}

impl RawLog {
    /// Opens (or creates) the raw log database at the given path.
    ///
    /// Creates the schema if it doesn't exist. The database file and parent
    /// directories are created automatically by SQLite.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or the schema
    /// migration fails.
    pub async fn open(db_path: &str) -> Result<Self> {
        // Ensure parent directory exists.
        if let Some(parent) = Path::new(db_path).parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create database directory: {}", parent.display())
            })?;
        }

        let connect_opts = SqliteConnectOptions::from_str(db_path)
            .with_context(|| format!("Invalid database path: {db_path}"))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(connect_opts)
            .await
            .with_context(|| format!("Failed to open raw log database at {db_path}"))?;

        info!(
            layer = "senses",
            component = "raw_log",
            db_path = db_path,
            "Raw log database opened"
        );

        let raw_log = Self { pool };
        raw_log.create_schema().await?;
        Ok(raw_log)
    }

    /// Creates the database schema if it doesn't already exist.
    async fn create_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS perception_frames (
                id TEXT PRIMARY KEY,
                ts TEXT NOT NULL,
                screen_description TEXT,
                screen_foreground_app TEXT,
                screen_has_error INTEGER,
                screen_confidence REAL,
                screen_screenshot_path TEXT,
                audio_transcript TEXT,
                audio_language TEXT,
                audio_duration_ms INTEGER,
                audio_confidence REAL,
                context_window_title TEXT,
                context_process_name TEXT,
                context_idle_seconds INTEGER,
                context_in_call INTEGER,
                salience REAL,
                triage_decision TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create perception_frames table")?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_frames_ts ON perception_frames(ts)",
        )
        .execute(&self.pool)
        .await
        .context("Failed to create timestamp index")?;

        debug!(
            layer = "senses",
            component = "raw_log",
            "Schema verified"
        );

        Ok(())
    }

    /// Writes a perception frame to the raw log.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn write_frame(&self, frame: &PerceptionFrame) -> Result<()> {
        let (audio_transcript, audio_language, audio_duration_ms, audio_confidence) =
            match &frame.audio {
                Some(a) => (
                    Some(a.transcript.as_str()),
                    Some(a.language.as_str()),
                    Some(a.duration_ms as i64),
                    Some(a.confidence as f64),
                ),
                None => (None, None, None, None),
            };

        sqlx::query(
            r#"
            INSERT INTO perception_frames (
                id, ts, screen_description, screen_foreground_app,
                screen_has_error, screen_confidence, screen_screenshot_path,
                audio_transcript, audio_language, audio_duration_ms, audio_confidence,
                context_window_title, context_process_name,
                context_idle_seconds, context_in_call,
                salience
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16
            )
            "#,
        )
        .bind(frame.id.to_string())
        .bind(frame.ts.to_rfc3339())
        .bind(&frame.screen.description)
        .bind(&frame.screen.foreground_app)
        .bind(frame.screen.has_error_visible as i32)
        .bind(frame.screen.confidence as f64)
        .bind(frame.screen.screenshot_path.as_deref())
        .bind(audio_transcript)
        .bind(audio_language)
        .bind(audio_duration_ms)
        .bind(audio_confidence)
        .bind(&frame.context.foreground_window_title)
        .bind(&frame.context.foreground_process_name)
        .bind(frame.context.idle_seconds as i64)
        .bind(frame.context.in_call as i32)
        .bind(frame.salience_hint as f64)
        .execute(&self.pool)
        .await
        .context("Failed to insert perception frame")?;

        debug!(
            layer = "senses",
            component = "raw_log",
            frame_id = %frame.id,
            salience = frame.salience_hint,
            "Wrote frame to raw log"
        );

        Ok(())
    }

    /// Queries frames within a time range.
    ///
    /// Returns frames ordered by timestamp ascending.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn query_frames(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<PerceptionFrame>> {
        let rows = sqlx::query(
            r#"
            SELECT id, ts, screen_description, screen_foreground_app,
                   screen_has_error, screen_confidence, screen_screenshot_path,
                   audio_transcript, audio_language, audio_duration_ms, audio_confidence,
                   context_window_title, context_process_name,
                   context_idle_seconds, context_in_call,
                   salience
            FROM perception_frames
            WHERE ts >= ?1 AND ts <= ?2
            ORDER BY ts ASC
            "#,
        )
        .bind(since.to_rfc3339())
        .bind(until.to_rfc3339())
        .fetch_all(&self.pool)
        .await
        .context("Failed to query perception frames")?;

        let mut frames = Vec::with_capacity(rows.len());
        for row in rows {
            let ts_str: String = row.get("ts");
            let ts = DateTime::parse_from_rfc3339(&ts_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            let id_str: String = row.get("id");
            let id = uuid::Uuid::parse_str(&id_str).unwrap_or_else(|_| uuid::Uuid::new_v4());

            let audio = {
                let transcript: Option<String> = row.get("audio_transcript");
                transcript.map(|t| crate::senses::types::AudioObservation {
                    transcript: t,
                    language: row
                        .get::<Option<String>, _>("audio_language")
                        .unwrap_or_default(),
                    duration_ms: row
                        .get::<Option<i64>, _>("audio_duration_ms")
                        .unwrap_or(0) as u64,
                    confidence: row
                        .get::<Option<f64>, _>("audio_confidence")
                        .unwrap_or(0.0) as f32,
                    ts,
                })
            };

            frames.push(PerceptionFrame {
                id,
                ts,
                screen: crate::senses::types::ScreenObservation {
                    description: row
                        .get::<Option<String>, _>("screen_description")
                        .unwrap_or_default(),
                    foreground_app: row
                        .get::<Option<String>, _>("screen_foreground_app")
                        .unwrap_or_default(),
                    has_error_visible: row
                        .get::<Option<i32>, _>("screen_has_error")
                        .unwrap_or(0)
                        != 0,
                    confidence: row
                        .get::<Option<f64>, _>("screen_confidence")
                        .unwrap_or(0.0) as f32,
                    screenshot_path: row.get("screen_screenshot_path"),
                    ts,
                },
                audio,
                context: crate::senses::types::ContextObservation {
                    foreground_window_title: row
                        .get::<Option<String>, _>("context_window_title")
                        .unwrap_or_default(),
                    foreground_process_name: row
                        .get::<Option<String>, _>("context_process_name")
                        .unwrap_or_default(),
                    idle_seconds: row
                        .get::<Option<i64>, _>("context_idle_seconds")
                        .unwrap_or(0) as u64,
                    in_call: row
                        .get::<Option<i32>, _>("context_in_call")
                        .unwrap_or(0)
                        != 0,
                    ts,
                },
                salience_hint: row
                    .get::<Option<f64>, _>("salience")
                    .unwrap_or(0.0) as f32,
            });
        }

        Ok(frames)
    }

    /// Returns the total number of frames in the raw log.
    pub async fn frame_count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM perception_frames")
            .fetch_one(&self.pool)
            .await
            .context("Failed to count frames")?;
        Ok(row.get::<i64, _>("cnt"))
    }

    /// Deletes frames older than the given retention period.
    ///
    /// Also deletes associated screenshot files from disk.
    ///
    /// # Returns
    ///
    /// The number of frames deleted.
    pub async fn rotate(&self, retention_days: u32) -> Result<u64> {
        let cutoff = Utc::now() - Duration::days(retention_days as i64);
        let cutoff_str = cutoff.to_rfc3339();

        // First, collect screenshot paths to delete.
        let paths: Vec<String> = sqlx::query(
            "SELECT screen_screenshot_path FROM perception_frames WHERE ts < ?1 AND screen_screenshot_path IS NOT NULL",
        )
        .bind(&cutoff_str)
        .fetch_all(&self.pool)
        .await
        .context("Failed to query old screenshot paths")?
        .iter()
        .filter_map(|row| row.get::<Option<String>, _>("screen_screenshot_path"))
        .collect();

        // Delete screenshot files.
        for path in &paths {
            if let Err(e) = std::fs::remove_file(path) {
                warn!(
                    layer = "senses",
                    component = "raw_log",
                    path = path,
                    error = %e,
                    "Failed to delete old screenshot file"
                );
            }
        }

        // Delete old frames from the database.
        let result = sqlx::query("DELETE FROM perception_frames WHERE ts < ?1")
            .bind(&cutoff_str)
            .execute(&self.pool)
            .await
            .context("Failed to delete old frames")?;

        let deleted = result.rows_affected();

        if deleted > 0 {
            info!(
                layer = "senses",
                component = "raw_log",
                deleted_frames = deleted,
                deleted_screenshots = paths.len(),
                retention_days = retention_days,
                "Rotated old frames"
            );
        }

        Ok(deleted)
    }

    /// Closes the database connection pool.
    pub async fn close(&self) {
        self.pool.close().await;
        debug!(
            layer = "senses",
            component = "raw_log",
            "Raw log database closed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::senses::types::*;
    use uuid::Uuid;

    /// Creates a test frame with the given parameters.
    fn test_frame(description: &str, has_audio: bool) -> PerceptionFrame {
        PerceptionFrame {
            id: Uuid::new_v4(),
            ts: Utc::now(),
            screen: ScreenObservation {
                description: description.to_string(),
                foreground_app: "Code.exe".to_string(),
                has_error_visible: false,
                confidence: 0.9,
                screenshot_path: None,
                ts: Utc::now(),
            },
            audio: if has_audio {
                Some(AudioObservation {
                    transcript: "hello world".to_string(),
                    language: "en".to_string(),
                    duration_ms: 1500,
                    confidence: 0.85,
                    ts: Utc::now(),
                })
            } else {
                None
            },
            context: ContextObservation {
                foreground_window_title: "main.rs - kairo".to_string(),
                foreground_process_name: "Code.exe".to_string(),
                idle_seconds: 0,
                in_call: false,
                ts: Utc::now(),
            },
            salience_hint: 0.5,
        }
    }

    #[tokio::test]
    async fn test_schema_creation() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();
        // Schema should be created without error.
        let count = log.frame_count().await.unwrap();
        assert_eq!(count, 0);
        log.close().await;
    }

    #[tokio::test]
    async fn test_write_and_read_frame() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();

        let frame = test_frame("VS Code editing main.rs", false);
        log.write_frame(&frame).await.unwrap();

        let count = log.frame_count().await.unwrap();
        assert_eq!(count, 1);

        let since = frame.ts - Duration::seconds(1);
        let until = frame.ts + Duration::seconds(1);
        let frames = log.query_frames(since, until).await.unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, frame.id);
        assert_eq!(frames[0].screen.description, "VS Code editing main.rs");
        assert!(frames[0].audio.is_none());

        log.close().await;
    }

    #[tokio::test]
    async fn test_write_and_read_frame_with_audio() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();

        let frame = test_frame("code editor", true);
        log.write_frame(&frame).await.unwrap();

        let since = frame.ts - Duration::seconds(1);
        let until = frame.ts + Duration::seconds(1);
        let frames = log.query_frames(since, until).await.unwrap();
        assert_eq!(frames.len(), 1);

        let audio = frames[0].audio.as_ref().expect("should have audio");
        assert_eq!(audio.transcript, "hello world");
        assert_eq!(audio.language, "en");
        assert_eq!(audio.duration_ms, 1500);

        log.close().await;
    }

    #[tokio::test]
    async fn test_query_empty_range_returns_nothing() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();

        let frame = test_frame("editor", false);
        log.write_frame(&frame).await.unwrap();

        // Query a range that doesn't include the frame.
        let far_future = Utc::now() + Duration::days(100);
        let frames = log
            .query_frames(far_future, far_future + Duration::seconds(1))
            .await
            .unwrap();
        assert!(frames.is_empty());

        log.close().await;
    }

    #[tokio::test]
    async fn test_multiple_frames_ordered_by_ts() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();

        for i in 0..5 {
            let mut frame = test_frame(&format!("frame {i}"), false);
            frame.ts = Utc::now() + Duration::seconds(i);
            log.write_frame(&frame).await.unwrap();
        }

        let count = log.frame_count().await.unwrap();
        assert_eq!(count, 5);

        let since = Utc::now() - Duration::seconds(1);
        let until = Utc::now() + Duration::seconds(10);
        let frames = log.query_frames(since, until).await.unwrap();
        assert_eq!(frames.len(), 5);

        // Verify ordering.
        for i in 1..frames.len() {
            assert!(frames[i].ts >= frames[i - 1].ts);
        }

        log.close().await;
    }

    #[tokio::test]
    async fn test_rotation_deletes_old_frames() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();

        // Insert a frame dated 60 days ago.
        let mut old_frame = test_frame("old frame", false);
        old_frame.ts = Utc::now() - Duration::days(60);
        log.write_frame(&old_frame).await.unwrap();

        // Insert a recent frame.
        let recent = test_frame("recent frame", false);
        log.write_frame(&recent).await.unwrap();

        assert_eq!(log.frame_count().await.unwrap(), 2);

        // Rotate with 30-day retention.
        let deleted = log.rotate(30).await.unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(log.frame_count().await.unwrap(), 1);

        log.close().await;
    }
}
