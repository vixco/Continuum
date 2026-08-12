//! # Raw log storage
//!
//! SQLite-backed store for every perception frame produced by the senses layer.
//! One row per frame, including screenshot paths, transcripts, and context.
//!
//! Retention: default 30 days, configurable 1–365. Rotated nightly.
//! Screenshots are saved to `~/.continuum-dev/screenshots/<date>/` as files,
//! with paths stored in the database (not blobs).
//!
//! Part of the memory system in the Continuum cognitive architecture.
//! The raw log is the foundational store — episodic and semantic memory
//! (Phases 2–3) are distilled from it.
//!
//! The raw-log DB also hosts the **Projects table family** (context engine
//! spec §4.3, Task A4): `projects` (config mirror + discovered candidates +
//! per-project stats + zone), `project_rules` (tier-0 overrides), and
//! `session_pins`, plus the **context_events table** (spec §4.6, Task A6)
//! with its `(dedupe_key, ts_last)` index and `context_events_fts` FTS5
//! external-content mirror. Per spec §4.6, all DDL for this DB lives here
//! and the runtime is its single writer; the events-writer task in
//! `memory::events` is the only component that inserts/collapses event
//! rows (Plan C only reads).

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite, SqliteConnection};
use std::path::Path;
use std::str::FromStr;
use std::time::SystemTime;
use tracing::{debug, info, warn};

use crate::context::project::{OverrideRule, ProjectEntry, RuleAction};
use crate::memory::events::{
    event_enum_token, parse_event_enum, ContextEvent, EventSensitivity, EventSource, EventType,
};
use crate::senses::screenshots::{sweep_screenshots_dir, ScreenshotPolicy, SweepStats};
use crate::senses::types::PerceptionFrame;

/// How long a writer connection waits for a SQLite lock before giving up.
///
/// Deliberately far below sqlx's 5 s default: every writer on this pool is
/// a best-effort background write, and the runtime would rather drop one
/// observation (logged) than stall a caller for seconds behind the events
/// writer's batch transaction or its hourly rotation.
const WRITER_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// What one [`RawLog::rotate_with_policy`] pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RotateStats {
    /// `perception_frames` rows deleted (age cutoff).
    pub frames_deleted: u64,
    /// Screenshot files deleted because a deleted row referenced them.
    pub screenshots_deleted: usize,
    /// What the mtime backstop sweep did (all zero when no
    /// `screenshots_dir` was supplied or the policy disables it).
    pub swept: SweepStats,
}

/// What one [`RawLog::delete_range`] pass removed (spec §4.13).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeleteRangeStats {
    /// `perception_frames` rows deleted.
    pub frames_deleted: u64,
    /// `context_events` rows deleted (their FTS mirrors go with them).
    pub events_deleted: u64,
    /// Screenshot files deleted because a deleted frame referenced them.
    pub screenshots_deleted: usize,
}

/// Why a read-only open of the raw log failed (context engine spec §5.2
/// preamble).
///
/// Separated from `anyhow` on purpose: the MCP context tools must be able
/// to tell "the runtime has not created this database yet" (answer
/// `{stale: true, events: []}`, never a wake-killing error) apart from a
/// genuine open failure.
#[derive(Debug, thiserror::Error)]
pub enum RawLogError {
    /// The database file does not exist yet, or exists without the
    /// context-engine tables. The runtime creates both on first boot; until
    /// then a reader has nothing to read, which is not an error condition.
    #[error("raw log database is not created yet at {path}")]
    NotYetCreated {
        /// The path that was probed.
        path: String,
    },
    /// The database exists but could not be opened or probed.
    #[error("failed to open raw log database at {path}: {source}")]
    Open {
        /// The path that was probed.
        path: String,
        /// The underlying sqlx/IO failure.
        #[source]
        source: anyhow::Error,
    },
}

/// Filters for [`RawLog::query_context_events`] — the Plan C read path the
/// `context_timeline` / `context_files` MCP tools (Task C4) are built on
/// (spec §5.2).
///
/// All filters are ANDed. An event's `[ts_first, ts_last]` span is treated
/// as overlapping the requested window, so a still-collapsing event that
/// started before `since` is returned as long as its latest occurrence
/// falls inside it.
#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    /// Only events whose latest occurrence is at or after this instant.
    pub since: Option<DateTime<Utc>>,
    /// Only events whose first occurrence is at or before this instant.
    pub until: Option<DateTime<Utc>>,
    /// Restrict to these event types (empty = no type filter).
    pub types: Vec<EventType>,
    /// Restrict to one resolved project id.
    pub project: Option<String>,
    /// Restrict to one collector family.
    pub source: Option<EventSource>,
    /// Maximum rows returned, clamped to `1..=`[`EVENT_QUERY_MAX_LIMIT`].
    /// The **newest** matching rows are kept.
    pub limit: usize,
}

/// Hard cap on [`EventQuery::limit`] (spec §5.2: `limit ≤ 200`). Enforced
/// in the store, not only in the tool, so no caller can widen it.
pub const EVENT_QUERY_MAX_LIMIT: usize = 200;

/// Hard cap on [`RawLog::search_context_events`] results (spec §5.2:
/// `limit ≤ 50` for `context_search`).
pub const EVENT_SEARCH_MAX_LIMIT: usize = 50;

/// Normalizes an arbitrary user/model search string into a safe FTS5
/// `MATCH` expression: each whitespace-separated token is stripped to its
/// alphanumeric characters, empties are dropped, and the survivors become
/// quoted `"tok"*` prefix terms joined by spaces (implicit `AND`).
///
/// This mirrors `continuum_memory::index::fts_query` deliberately — the
/// vault search tool and `context_search` should not disagree about what a
/// query means. Stripping punctuation is what makes the result safe: FTS5
/// operators (`"`, `*`, `^`, `:`, `-`, `NEAR(`) can never survive into the
/// expression, so a model-supplied query can neither raise a syntax error
/// nor change the shape of the search. Whitespace-only or
/// punctuation-only input yields `""`, which callers treat as "no rows".
///
/// Consequence worth knowing: a hyphenated word is glued rather than split
/// (`build-failed` → `"buildfailed"*`), so searching for it finds nothing.
/// That matches the vault behaviour and is preferable to accepting raw
/// FTS syntax from a model.
pub fn fts_match_query(user: &str) -> String {
    user.split_whitespace()
        .map(|token| {
            token
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Manages the SQLite raw log database for perception frames.
///
/// Each frame is stored as a row in the `perception_frames` table.
/// The schema is created automatically on first connection.
#[derive(Clone)]
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
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            // Explicit, short lock wait. sqlx's default is 5 s, which is a
            // latency cliff for the runtime's main select loop: the events
            // writer holds a connection across a `BEGIN IMMEDIATE` batch of
            // up to 256 rows plus hourly retention rotation, and a frame
            // write landing inside that window would otherwise be allowed
            // to block for seconds. Failing fast is the right trade for a
            // best-effort observation write — the error is logged and the
            // next frame is a second away.
            .busy_timeout(WRITER_BUSY_TIMEOUT);

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

    /// Opens an **existing** raw log database for reading only (context
    /// engine spec §5.2 preamble). This is the path the MCP server process
    /// uses; the runtime remains the single writer (spec §4.6).
    ///
    /// Contract, and why each part of it is what it is:
    ///
    /// - **No DDL.** [`RawLog::open`]'s `create_schema` runs `CREATE TABLE`
    ///   / `ALTER TABLE` migrations. A second process must never do that:
    ///   it would race the runtime's migrations and violate single-writer.
    ///   This constructor runs *no* statement that can change the schema.
    /// - **`PRAGMA query_only = ON`.** Enforcement, not politeness — any
    ///   `INSERT`/`UPDATE`/`DELETE` on this handle fails at SQLite level,
    ///   so a future tool cannot accidentally write through it.
    /// - **`busy_timeout = 2000 ms`.** The runtime writes continuously; a
    ///   reader that gives up instantly would surface spurious errors to
    ///   the orchestrator.
    /// - **WAL-recovery tolerant.** The handle is opened *read-write* at
    ///   the SQLite level (`create_if_missing(false)`, not
    ///   `SQLITE_OPEN_READONLY`) precisely so it can recover a hot WAL
    ///   left behind by a crashed writer. A true read-only handle returns
    ///   `SQLITE_CANTOPEN` in that situation, which would make every
    ///   context tool fail exactly when the runtime is unhealthy. The
    ///   `query_only` pragma provides the read-only guarantee instead.
    /// - **Missing database → [`RawLogError::NotYetCreated`].** A typed
    ///   variant so callers answer `{stale: true, events: []}` rather than
    ///   raising a wake-killing error. A file that exists but has no
    ///   `context_events` table is the same situation for a reader (the
    ///   runtime creates the whole schema in one pass), so it maps to the
    ///   same variant.
    ///
    /// # Errors
    ///
    /// [`RawLogError::NotYetCreated`] when there is nothing to read yet,
    /// [`RawLogError::Open`] for a genuine open/probe failure.
    pub async fn open_read_only(db_path: &str) -> Result<Self, RawLogError> {
        if !Path::new(db_path).exists() {
            return Err(RawLogError::NotYetCreated {
                path: db_path.to_string(),
            });
        }

        let connect_opts = SqliteConnectOptions::from_str(db_path)
            .map_err(|error| RawLogError::Open {
                path: db_path.to_string(),
                source: anyhow::Error::new(error),
            })?
            .create_if_missing(false)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_millis(2_000))
            // Applied after the built-in pragmas (sqlx preserves insertion
            // order), so journal-mode setup still succeeds and every
            // statement issued afterwards is read-only.
            .pragma("query_only", "ON");

        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(connect_opts)
            .await
            .map_err(|error| RawLogError::Open {
                path: db_path.to_string(),
                source: anyhow::Error::new(error),
            })?;

        let raw_log = Self { pool };

        let has_events: Option<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'context_events'",
        )
        .fetch_optional(&raw_log.pool)
        .await
        .map_err(|error| RawLogError::Open {
            path: db_path.to_string(),
            source: anyhow::Error::new(error),
        })?;

        if has_events.is_none() {
            raw_log.close().await;
            return Err(RawLogError::NotYetCreated {
                path: db_path.to_string(),
            });
        }

        debug!(
            layer = "memory",
            component = "raw_log",
            db_path = db_path,
            "Raw log database opened read-only"
        );
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
                triage_decision TEXT,
                memory_distilled_at TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create perception_frames table")?;

        self.ensure_optional_column("perception_frames", "memory_distilled_at", "TEXT")
            .await?;

        // Window/process enrichment columns (context engine spec §4.2) —
        // additive, so older databases migrate in place.
        self.ensure_optional_column("perception_frames", "context_pid", "INTEGER")
            .await?;
        self.ensure_optional_column("perception_frames", "context_exe_path", "TEXT")
            .await?;
        self.ensure_optional_column("perception_frames", "context_active_since_secs", "INTEGER")
            .await?;
        self.ensure_optional_column("perception_frames", "context_monitor_id", "TEXT")
            .await?;

        // The collector's resolved privacy zone (fixwave 2, I3) — additive,
        // NULL for rows written before it existed. Consumers treat NULL as
        // "untagged" and fall back to the sentinel-literal gate, never as
        // "cloud-safe".
        self.ensure_optional_column("perception_frames", "context_privacy", "TEXT")
            .await?;

        // Compact world-state blob split out of `screen_description`
        // (context engine spec §4.10, Task B4) — additive, so databases
        // written before the split keep their rows (their
        // `screen_description` still holds the old blob; nothing reparses
        // it, the caption simply reads long for those historical frames).
        self.ensure_optional_column("perception_frames", "screen_world_compact", "TEXT")
            .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_frames_ts ON perception_frames(ts)")
            .execute(&self.pool)
            .await
            .context("Failed to create timestamp index")?;

        // Projects table family (context engine spec §4.3, Task A4). All
        // DDL for the raw-log DB lives here (spec §4.6 single-writer):
        // config mirror + discovered candidates + override rules + session
        // pins + per-project stats + zone.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                root_paths TEXT NOT NULL DEFAULT '[]',
                repo TEXT,
                keywords TEXT NOT NULL DEFAULT '[]',
                zone TEXT,
                status TEXT NOT NULL DEFAULT 'discovered',
                created_ts TEXT NOT NULL,
                last_active_ts TEXT,
                frames_count INTEGER NOT NULL DEFAULT 0
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create projects table")?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS project_rules (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                match_process TEXT,
                match_title_substring TEXT,
                action TEXT NOT NULL,
                project_id TEXT NOT NULL,
                created_ts TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create project_rules table")?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS session_pins (
                key TEXT PRIMARY KEY,
                value TEXT,
                project_id TEXT,
                created_ts TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create session_pins table")?;

        // Context events table (context engine spec §4.6, Task A6). One
        // row per deduped event; `count`/`ts_last` grow as repeats
        // collapse. Written only by the events-writer task in
        // `memory::events` (single-writer).
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS context_events (
                id INTEGER PRIMARY KEY,
                ts_first TEXT NOT NULL,
                ts_last TEXT NOT NULL,
                count INTEGER NOT NULL DEFAULT 1,
                source TEXT NOT NULL,
                application TEXT NOT NULL DEFAULT '',
                window_title TEXT NOT NULL DEFAULT '',
                project_id TEXT,
                event_type TEXT NOT NULL,
                summary TEXT NOT NULL,
                importance REAL NOT NULL DEFAULT 0,
                confidence REAL NOT NULL DEFAULT 0,
                sensitivity TEXT NOT NULL,
                raw_reference TEXT,
                dedupe_key TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create context_events table")?;

        // Dedupe lookup path: latest open row per key (spec §4.6).
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_context_events_dedupe \
             ON context_events(dedupe_key, ts_last)",
        )
        .execute(&self.pool)
        .await
        .context("Failed to create context_events dedupe index")?;

        // FTS5 external-content mirror over the two free-text columns
        // (spec §4.6). External content keeps the text stored once, in
        // the base table.
        sqlx::query(
            r#"
            CREATE VIRTUAL TABLE IF NOT EXISTS context_events_fts USING fts5(
                summary,
                window_title,
                content='context_events',
                content_rowid='id'
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create context_events_fts table")?;

        // FTS sync triggers. The UPDATE trigger is restricted to
        // text-bearing changes so the hot dedupe path (count/ts_last
        // bumps) never rewrites index rows (spec §4.6).
        sqlx::query(
            r#"
            CREATE TRIGGER IF NOT EXISTS context_events_fts_ai
            AFTER INSERT ON context_events BEGIN
                INSERT INTO context_events_fts(rowid, summary, window_title)
                VALUES (new.id, new.summary, new.window_title);
            END
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create context_events_fts insert trigger")?;

        sqlx::query(
            r#"
            CREATE TRIGGER IF NOT EXISTS context_events_fts_ad
            AFTER DELETE ON context_events BEGIN
                INSERT INTO context_events_fts(context_events_fts, rowid, summary, window_title)
                VALUES ('delete', old.id, old.summary, old.window_title);
            END
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create context_events_fts delete trigger")?;

        sqlx::query(
            r#"
            CREATE TRIGGER IF NOT EXISTS context_events_fts_au
            AFTER UPDATE ON context_events
            WHEN old.summary IS NOT new.summary
              OR old.window_title IS NOT new.window_title
            BEGIN
                INSERT INTO context_events_fts(context_events_fts, rowid, summary, window_title)
                VALUES ('delete', old.id, old.summary, old.window_title);
                INSERT INTO context_events_fts(rowid, summary, window_title)
                VALUES (new.id, new.summary, new.window_title);
            END
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create context_events_fts update trigger")?;

        // Distillation bookkeeping on the events table (Task B6, spec
        // §4.11): the distiller now promotes deduped `context_events`
        // rows into episodic memory and stamps them here, exactly as it
        // stamps `perception_frames.memory_distilled_at` on the fallback
        // path. Additive column so databases written before B6 open
        // unchanged (every existing row reads NULL = "not distilled",
        // which is the correct answer for them).
        self.ensure_optional_column("context_events", "distilled_at", "TEXT")
            .await?;

        // Classified-frame bookkeeping (fixwave 3a, C3). A frame that
        // *collapsed* into an existing event row leaves no trace in
        // `context_events` — the row keeps the FIRST occurrence's
        // `raw_reference` — so the fallback distiller's
        // `NOT EXISTS (… ce.raw_reference = perception_frames.id)`
        // predicate only ever excluded frame #1 of N. Frames #2..N then
        // distilled a second time as raw frames, inverting the compression
        // ladder: "build failed (×14)" PLUS 13 raw memories of the same
        // moment. The events writer stamps this column for every frame
        // whose classification produced a *usable* event row, insert or
        // collapse. Additive: existing rows read NULL and fall back to the
        // raw_reference predicate, which is correct for them.
        self.ensure_optional_column("perception_frames", "context_event_at", "TEXT")
            .await?;

        debug!(layer = "senses", component = "raw_log", "Schema verified");

        Ok(())
    }

    async fn ensure_optional_column(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<()> {
        let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("Failed to inspect {table} schema"))?;

        let exists = rows
            .iter()
            .any(|row| row.get::<String, _>("name") == column);

        if !exists {
            sqlx::query(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))
            .execute(&self.pool)
            .await
            .with_context(|| format!("Failed to add {column} to {table}"))?;
            info!(
                layer = "memory",
                component = "raw_log",
                table,
                column,
                "Added optional raw-log schema column"
            );
        }

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
                context_pid, context_exe_path,
                context_active_since_secs, context_monitor_id,
                salience, screen_world_compact, context_privacy
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20, ?21, ?22
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
        .bind(frame.context.pid.map(i64::from))
        .bind(frame.context.exe_path.as_deref())
        .bind(frame.context.active_since_secs as i64)
        .bind(frame.context.monitor_id.as_deref())
        .bind(frame.salience_hint as f64)
        // Caption and world-state blob are stored side by side (spec §4.10):
        // the packager reads the blob, everything else reads the caption.
        .bind(frame.screen.world_compact.as_deref())
        // The collector's resolved zone (fixwave 2, I3): persisted so a
        // frame read back out of the log is gated on the tag rather than on
        // a string literal a config knob can suppress.
        .bind(frame.context.privacy.map(|d| d.as_str()))
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

    /// Records what triage decided about an already-written frame
    /// (context engine spec §4.7 consumption (c), Task B3).
    ///
    /// The `triage_decision` column has existed since the first schema but
    /// was never written — frames carried the decision only in logs, so
    /// nothing downstream could ask "what did triage do with this frame?".
    /// The classification-consumption path fills it in after the fact
    /// (the frame row is inserted by [`RawLog::write_frame`] the moment
    /// the frame arrives; triage runs off the main loop and answers
    /// later), which is why this is an update by frame id rather than an
    /// extra INSERT column.
    ///
    /// `decision` is the decision's variant token, optionally suffixed
    /// with `/<event_type>` when the frame also carried a classification
    /// (e.g. `wake_orchestrator/error`) — see
    /// `triage::consume::triage_decision_column`. A frame id that isn't in
    /// the table (rotated away, or the frame write failed) updates zero
    /// rows and is not an error.
    ///
    /// The distiller's own SQL predicate deliberately does *not* read this
    /// column: it stays a salience/audio/error fallback so classification
    /// being absent (model down, malformed block) can never stop frames
    /// from being distilled (spec §4.7 consumption (d)).
    pub async fn set_triage_decision(&self, frame_id: uuid::Uuid, decision: &str) -> Result<()> {
        sqlx::query("UPDATE perception_frames SET triage_decision = ?1 WHERE id = ?2")
            .bind(decision)
            .bind(frame_id.to_string())
            .execute(&self.pool)
            .await
            .context("Failed to update triage_decision")?;
        Ok(())
    }

    /// Reads back the `triage_decision` column for one frame. `None` when
    /// the row is missing or the column was never written.
    pub async fn triage_decision(&self, frame_id: uuid::Uuid) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT triage_decision FROM perception_frames WHERE id = ?1")
                .bind(frame_id.to_string())
                .fetch_optional(&self.pool)
                .await
                .context("Failed to read triage_decision")?;
        Ok(row.and_then(|(value,)| value))
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
                   context_pid, context_exe_path,
                   context_active_since_secs, context_monitor_id,
                   salience, screen_world_compact, context_privacy
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

        Ok(rows.iter().map(row_to_frame).collect())
    }

    /// Returns the newest bounded perception frames in a time range, ordered
    /// chronologically. This is the read path used by short historical-recall
    /// consumers that need local vision captions without loading an entire
    /// multi-hour capture window into memory.
    pub async fn query_recent_frames(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<PerceptionFrame>> {
        let limit = limit.clamp(1, 500);
        let rows = sqlx::query(
            r#"
            SELECT id, ts, screen_description, screen_foreground_app,
                   screen_has_error, screen_confidence, screen_screenshot_path,
                   audio_transcript, audio_language, audio_duration_ms, audio_confidence,
                   context_window_title, context_process_name,
                   context_idle_seconds, context_in_call,
                   context_pid, context_exe_path,
                   context_active_since_secs, context_monitor_id,
                   salience, screen_world_compact, context_privacy
            FROM perception_frames
            WHERE ts >= ?1 AND ts <= ?2
            ORDER BY ts DESC
            LIMIT ?3
            "#,
        )
        .bind(since.to_rfc3339())
        .bind(until.to_rfc3339())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("Failed to query recent perception frames")?;

        let mut frames = rows.iter().map(row_to_frame).collect::<Vec<_>>();
        frames.reverse();
        Ok(frames)
    }

    /// Returns undistilled frames worth promoting into episodic memory.
    ///
    /// Frames qualify if they have audio, visible errors, or salience above
    /// `min_salience`. The distiller marks successful frames with
    /// [`mark_frames_distilled`] so restarts do not duplicate memories.
    pub async fn query_undistilled_frames(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        min_salience: f32,
        limit: usize,
    ) -> Result<Vec<PerceptionFrame>> {
        let rows = sqlx::query(
            r#"
            SELECT id, ts, screen_description, screen_foreground_app,
                   screen_has_error, screen_confidence, screen_screenshot_path,
                   audio_transcript, audio_language, audio_duration_ms, audio_confidence,
                   context_window_title, context_process_name,
                   context_idle_seconds, context_in_call,
                   context_pid, context_exe_path,
                   context_active_since_secs, context_monitor_id,
                   salience, screen_world_compact, context_privacy
            FROM perception_frames
            WHERE ts >= ?1
              AND ts <= ?2
              AND memory_distilled_at IS NULL
              AND (
                (audio_transcript IS NOT NULL AND length(trim(audio_transcript)) > 0)
                OR screen_has_error = 1
                OR salience >= ?3
              )
            ORDER BY ts ASC
            LIMIT ?4
            "#,
        )
        .bind(since.to_rfc3339())
        .bind(until.to_rfc3339())
        .bind(min_salience as f64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("Failed to query undistilled perception frames")?;

        let mut frames = Vec::with_capacity(rows.len());
        for row in rows {
            frames.push(row_to_frame(&row));
        }
        Ok(frames)
    }

    /// The fallback half of the distiller's input (Task B6, spec §4.11):
    /// [`Self::query_undistilled_frames`] restricted to frames that never
    /// produced a `context_events` row.
    ///
    /// Since B3, a classified frame becomes a deduped event whose
    /// `raw_reference` is the frame id, and the distiller reads those
    /// events as its primary source. Distilling the frame *as well* would
    /// record the same moment twice, so this predicate subtracts them.
    /// The consequence is deliberate: a frame that produced an event
    /// below the event importance threshold is not rescued by its raw
    /// salience — classification is the more informed judgement.
    ///
    /// Two corrections from fixwave 3a:
    ///
    /// - **`context_event_at IS NULL` (C3).** A frame that *collapsed*
    ///   into an existing row is not reachable through `raw_reference`
    ///   (the row keeps the first occurrence's), so only the writer's own
    ///   per-frame stamp can exclude it. Without this, N repeats produced
    ///   one collapsed memory *plus* N−1 raw-frame memories.
    /// - **`length(trim(ce.summary)) > 0` (I2).** A blank-summary event
    ///   is permanently excluded from
    ///   [`Self::query_undistilled_events`] by the same predicate, so
    ///   letting it suppress its frame meant the moment was recorded
    ///   nowhere at all. A blank event no longer speaks for its frame.
    pub async fn query_undistilled_unclassified_frames(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        min_salience: f32,
        limit: usize,
    ) -> Result<Vec<PerceptionFrame>> {
        let rows = sqlx::query(
            r#"
            SELECT id, ts, screen_description, screen_foreground_app,
                   screen_has_error, screen_confidence, screen_screenshot_path,
                   audio_transcript, audio_language, audio_duration_ms, audio_confidence,
                   context_window_title, context_process_name,
                   context_idle_seconds, context_in_call,
                   context_pid, context_exe_path,
                   context_active_since_secs, context_monitor_id,
                   salience, screen_world_compact, context_privacy
            FROM perception_frames
            WHERE ts >= ?1
              AND ts <= ?2
              AND memory_distilled_at IS NULL
              AND (
                (audio_transcript IS NOT NULL AND length(trim(audio_transcript)) > 0)
                OR screen_has_error = 1
                OR salience >= ?3
              )
              AND context_event_at IS NULL
              AND NOT EXISTS (
                SELECT 1 FROM context_events ce
                WHERE ce.raw_reference = perception_frames.id
                  AND length(trim(ce.summary)) > 0
              )
            ORDER BY ts ASC
            LIMIT ?4
            "#,
        )
        .bind(since.to_rfc3339())
        .bind(until.to_rfc3339())
        .bind(min_salience as f64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("Failed to query undistilled unclassified frames")?;

        let mut frames = Vec::with_capacity(rows.len());
        for row in rows {
            frames.push(row_to_frame(&row));
        }
        Ok(frames)
    }

    /// The distiller's **primary** input (Task B6, spec §4.11): deduped
    /// `context_events` rows in `[since, until]` that have not been
    /// distilled yet and carry at least `min_importance`.
    ///
    /// Ordered oldest-first (`ts_last`) so episodic memory is written in
    /// the order things happened. A row is distilled exactly once: later
    /// occurrences that collapse into an already-distilled row bump its
    /// `count` without producing a second memory, so the "×N" a memory
    /// carries is the count *at distillation time*.
    pub async fn query_undistilled_events(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        min_importance: f32,
        limit: usize,
    ) -> Result<Vec<ContextEventRow>> {
        let rows = sqlx::query(
            r#"
            SELECT id, ts_first, ts_last, count, source, application, window_title,
                   project_id, event_type, summary, importance, confidence,
                   sensitivity, raw_reference, dedupe_key
            FROM context_events
            WHERE ts_last >= ?1
              AND ts_last <= ?2
              AND distilled_at IS NULL
              AND importance >= ?3
              AND length(trim(summary)) > 0
            ORDER BY ts_last ASC, id ASC
            LIMIT ?4
            "#,
        )
        .bind(since.to_rfc3339())
        .bind(until.to_rfc3339())
        .bind(min_importance as f64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("Failed to query undistilled context events")?;
        Ok(rows.iter().filter_map(row_to_context_event).collect())
    }

    /// Marks `context_events` rows as distilled into episodic memory.
    ///
    /// Idempotent by construction: the update only touches rows whose
    /// `distilled_at` is still NULL, so a repeated call returns 0 and the
    /// original distillation timestamp is never rewritten.
    pub async fn mark_events_distilled(&self, event_ids: &[i64]) -> Result<u64> {
        if event_ids.is_empty() {
            return Ok(0);
        }
        let now = Utc::now().to_rfc3339();
        let mut affected = 0;
        for id in event_ids {
            let result = sqlx::query(
                "UPDATE context_events SET distilled_at = ?1 \
                 WHERE id = ?2 AND distilled_at IS NULL",
            )
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await
            .context("Failed to mark context event distilled")?;
            affected += result.rows_affected();
        }
        Ok(affected)
    }

    /// Marks frames as distilled into episodic memory.
    pub async fn mark_frames_distilled(&self, frame_ids: &[uuid::Uuid]) -> Result<u64> {
        if frame_ids.is_empty() {
            return Ok(0);
        }

        let now = Utc::now().to_rfc3339();
        let mut affected = 0;
        for id in frame_ids {
            let result =
                sqlx::query("UPDATE perception_frames SET memory_distilled_at = ?1 WHERE id = ?2")
                    .bind(&now)
                    .bind(id.to_string())
                    .execute(&self.pool)
                    .await
                    .context("Failed to mark frame distilled")?;
            affected += result.rows_affected();
        }

        Ok(affected)
    }

    /// Returns the total number of frames in the raw log.
    pub async fn frame_count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM perception_frames")
            .fetch_one(&self.pool)
            .await
            .context("Failed to count frames")?;
        Ok(row.get::<i64, _>("cnt"))
    }

    /// Deletes frames older than the given retention period, deleting the
    /// screenshot file each removed row referenced.
    ///
    /// Equivalent to [`Self::rotate_with_policy`] with the default
    /// screenshot policy and no backstop sweep.
    ///
    /// # Returns
    ///
    /// The number of frames deleted.
    pub async fn rotate(&self, retention_days: u32) -> Result<u64> {
        let stats = self
            .rotate_with_policy(retention_days, None, &ScreenshotPolicy::default())
            .await?;
        Ok(stats.frames_deleted)
    }

    /// Rotation with the `[storage]` screenshot policy applied (Task B6,
    /// spec §4.11).
    ///
    /// Two deletions happen here, and both obey
    /// `delete_screenshots_with_rotation`:
    ///
    /// 1. every screenshot file referenced by a row this call deletes;
    /// 2. when `screenshots_dir` is supplied, the mtime backstop sweep of
    ///    that directory ([`crate::senses::screenshots`]) — the only
    ///    thing that reclaims images orphaned by a crash between "file
    ///    written" and "row written".
    ///
    /// Frame rows are deleted regardless of the screenshot policy; the
    /// policy governs image files only.
    pub async fn rotate_with_policy(
        &self,
        retention_days: u32,
        screenshots_dir: Option<&std::path::Path>,
        policy: &ScreenshotPolicy,
    ) -> Result<RotateStats> {
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

        // Delete screenshot files. This is unbounded synchronous
        // filesystem work — one `remove_file` per expiring screenshot, a
        // day's worth at a time — so it runs on the blocking pool rather
        // than parking a tokio worker thread (the distiller calls this
        // from an async task).
        let mut screenshots_deleted = 0usize;
        if policy.deletes_referenced_files() {
            let to_delete = paths.clone();
            screenshots_deleted = tokio::task::spawn_blocking(move || {
                let mut deleted = 0usize;
                for path in &to_delete {
                    match std::fs::remove_file(path) {
                        Ok(()) => deleted += 1,
                        Err(e) => warn!(
                            layer = "senses",
                            component = "raw_log",
                            path = path,
                            error = %e,
                            "Failed to delete old screenshot file"
                        ),
                    }
                }
                deleted
            })
            .await
            .context("Screenshot deletion task failed")?;
        }

        // Delete old frames from the database.
        let result = sqlx::query("DELETE FROM perception_frames WHERE ts < ?1")
            .bind(&cutoff_str)
            .execute(&self.pool)
            .await
            .context("Failed to delete old frames")?;

        let deleted = result.rows_affected();

        // Same reasoning as the deletion loop above: the backstop sweep is
        // a recursive depth-4 directory walk with a `remove_file` per stale
        // image, which must not run on a tokio worker thread.
        let swept = match screenshots_dir {
            Some(dir) => {
                let dir = dir.to_path_buf();
                let policy = *policy;
                tokio::task::spawn_blocking(move || {
                    sweep_screenshots_dir(&dir, &policy, SystemTime::now())
                })
                .await
                .context("Screenshot sweep task failed")?
            }
            None => SweepStats::default(),
        };

        if deleted > 0 || screenshots_deleted > 0 || swept.deleted > 0 {
            info!(
                layer = "senses",
                component = "raw_log",
                deleted_frames = deleted,
                deleted_screenshots = screenshots_deleted,
                swept_screenshots = swept.deleted,
                retention_days = retention_days,
                "Rotated old frames"
            );
        }

        Ok(RotateStats {
            frames_deleted: deleted,
            screenshots_deleted,
            swept,
        })
    }

    /// Deletes every frame from the raw log — the derived-data wipe path
    /// (`curator::run::process_wipe_request`), as opposed to [`Self::rotate`]'s
    /// age-based cutoff. Also deletes each frame's screenshot file from
    /// disk, best-effort, mirroring `rotate`'s cleanup: a file that's
    /// already missing or fails to delete is logged and does not abort the
    /// wipe.
    ///
    /// # Returns
    ///
    /// The number of frame rows deleted.
    pub async fn wipe_all(&self) -> Result<u64> {
        let paths: Vec<String> = sqlx::query(
            "SELECT screen_screenshot_path FROM perception_frames \
             WHERE screen_screenshot_path IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to query screenshot paths before wipe")?
        .iter()
        .filter_map(|row| row.get::<Option<String>, _>("screen_screenshot_path"))
        .collect();

        for path in &paths {
            if let Err(e) = std::fs::remove_file(path) {
                warn!(
                    layer = "senses",
                    component = "raw_log",
                    path = path,
                    error = %e,
                    "Failed to delete screenshot file during wipe"
                );
            }
        }

        let result = sqlx::query("DELETE FROM perception_frames")
            .execute(&self.pool)
            .await
            .context("Failed to wipe perception frames")?;

        let deleted = result.rows_affected();
        info!(
            layer = "senses",
            component = "raw_log",
            deleted_frames = deleted,
            deleted_screenshots = paths.len(),
            "Wiped raw log"
        );

        Ok(deleted)
    }

    // -----------------------------------------------------------------
    // Projects table (context engine spec §4.3, Task A4)
    // -----------------------------------------------------------------

    /// Lists every project row (all statuses), configured first, then by
    /// id.
    pub async fn list_projects(&self) -> Result<Vec<StoredProject>> {
        let rows = sqlx::query(
            r#"
            SELECT id, name, root_paths, repo, keywords, zone, status,
                   created_ts, last_active_ts, frames_count
            FROM projects
            ORDER BY status ASC, id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list projects")?;
        Ok(rows.iter().map(row_to_project).collect())
    }

    /// Inserts or updates a project row by id. Identity fields (name,
    /// roots, repo, keywords, zone, status) are replaced; stats
    /// (`created_ts`, `last_active_ts`, `frames_count`) are preserved on
    /// update.
    pub async fn upsert_project(&self, entry: &ProjectEntry) -> Result<()> {
        let root_paths = serde_json::to_string(&entry.root_paths).unwrap_or_else(|_| "[]".into());
        let keywords = serde_json::to_string(&entry.keywords).unwrap_or_else(|_| "[]".into());
        let zone = entry
            .zone
            .as_ref()
            .and_then(|z| serde_json::to_value(z).ok())
            .and_then(|v| v.as_str().map(str::to_owned));
        sqlx::query(
            r#"
            INSERT INTO projects (id, name, root_paths, repo, keywords, zone, status, created_ts)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                root_paths = excluded.root_paths,
                repo = excluded.repo,
                keywords = excluded.keywords,
                zone = excluded.zone,
                status = excluded.status
            "#,
        )
        .bind(&entry.id)
        .bind(&entry.name)
        .bind(root_paths)
        .bind(entry.repo.as_deref())
        .bind(keywords)
        .bind(zone)
        .bind(entry.status.as_str())
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to upsert project {}", entry.id))?;
        Ok(())
    }

    /// Marks a project row `confirmed` (Task C5 seam: the Context page's
    /// confirm intent). Returns `false` when the id does not exist.
    pub async fn confirm_project(&self, id: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE projects SET status = 'confirmed' WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .with_context(|| format!("Failed to confirm project {id}"))?;
        Ok(result.rows_affected() > 0)
    }

    /// Bumps a project's frame stats: `frames_count += 1`,
    /// `last_active_ts = ts`.
    pub async fn bump_project_stats(&self, id: &str, ts: DateTime<Utc>) -> Result<()> {
        sqlx::query(
            "UPDATE projects SET frames_count = frames_count + 1, last_active_ts = ?2 WHERE id = ?1",
        )
        .bind(id)
        .bind(ts.to_rfc3339())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to bump stats for project {id}"))?;
        Ok(())
    }

    /// Boot reconciliation (spec §4.3): `[[projects.known]]` config seeds
    /// or updates rows — **config wins by id** (identity fields replaced,
    /// stats preserved); discovered/confirmed rows without a config entry
    /// persist untouched. Returns the full post-reconciliation table.
    pub async fn reconcile_projects(
        &self,
        config_entries: &[ProjectEntry],
    ) -> Result<Vec<StoredProject>> {
        for entry in config_entries {
            let configured = ProjectEntry {
                status: crate::context::project::ProjectStatus::Configured,
                ..entry.clone()
            };
            self.upsert_project(&configured).await?;
        }
        self.list_projects().await
    }

    /// Persists a user override rule (tier 0) and returns its row id.
    pub async fn add_project_rule(&self, rule: &OverrideRule) -> Result<i64> {
        let result = sqlx::query(
            r#"
            INSERT INTO project_rules
                (match_process, match_title_substring, action, project_id, created_ts)
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
        )
        .bind(rule.match_process.as_deref())
        .bind(rule.match_title_substring.as_deref())
        .bind(rule.action.as_str())
        .bind(&rule.project_id)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .context("Failed to insert project rule")?;
        Ok(result.last_insert_rowid())
    }

    /// Lists persisted override rules in insertion order. Rows with an
    /// unknown action string are skipped with a warning (never invent
    /// semantics for corrupt rows).
    pub async fn list_project_rules(&self) -> Result<Vec<OverrideRule>> {
        let rows = sqlx::query(
            "SELECT match_process, match_title_substring, action, project_id \
             FROM project_rules ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list project rules")?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let action_str: String = row.get("action");
                let Some(action) = RuleAction::parse(&action_str) else {
                    warn!(
                        layer = "memory",
                        component = "raw_log",
                        action = %action_str,
                        "project_rules row has unknown action; skipping"
                    );
                    return None;
                };
                Some(OverrideRule {
                    match_process: row.get("match_process"),
                    match_title_substring: row.get("match_title_substring"),
                    action,
                    project_id: row.get("project_id"),
                })
            })
            .collect())
    }

    /// Sets (or replaces) a session pin (Task C5 seam: pin intents).
    pub async fn set_session_pin(
        &self,
        key: &str,
        value: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO session_pins (key, value, project_id, created_ts)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                project_id = excluded.project_id,
                created_ts = excluded.created_ts
            "#,
        )
        .bind(key)
        .bind(value)
        .bind(project_id)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to set session pin {key}"))?;
        Ok(())
    }

    /// Removes a session pin. Returns `false` when the key did not exist.
    pub async fn clear_session_pin(&self, key: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM session_pins WHERE key = ?1")
            .bind(key)
            .execute(&self.pool)
            .await
            .with_context(|| format!("Failed to clear session pin {key}"))?;
        Ok(result.rows_affected() > 0)
    }

    /// Lists session pins as `(key, value, project_id)` tuples.
    pub async fn list_session_pins(&self) -> Result<Vec<(String, Option<String>, Option<String>)>> {
        let rows = sqlx::query("SELECT key, value, project_id FROM session_pins ORDER BY key ASC")
            .fetch_all(&self.pool)
            .await
            .context("Failed to list session pins")?;
        Ok(rows
            .iter()
            .map(|row| (row.get("key"), row.get("value"), row.get("project_id")))
            .collect())
    }

    // -----------------------------------------------------------------
    // Context events table (context engine spec §4.6, Task A6)
    // -----------------------------------------------------------------

    /// Begins an IMMEDIATE transaction: takes SQLite's write lock at
    /// BEGIN time so a batch's check-then-write dedupe sequence cannot
    /// interleave with another writer (a deferred `pool.begin()` only
    /// locks at the first write). sqlx has no built-in `BEGIN IMMEDIATE`,
    /// so it is issued raw — always pair with [`RawLog::finish_write`].
    pub(crate) async fn immediate_conn(&self) -> Result<PoolConnection<Sqlite>> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .context("Failed to acquire raw-log connection")?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .context("Failed to BEGIN IMMEDIATE")?;
        Ok(conn)
    }

    /// Finishes an [`immediate_conn`](RawLog::immediate_conn)
    /// transaction whose body already produced `result`: commits on
    /// `Ok`, rolls back on `Err` — and rolls back on a *failed commit*
    /// too, so the pooled connection is never returned with a
    /// transaction still open (the vault index `finish()` precedent).
    pub(crate) async fn finish_write<T>(
        mut conn: PoolConnection<Sqlite>,
        result: Result<T>,
    ) -> Result<T> {
        async fn rollback_quiet(conn: &mut PoolConnection<Sqlite>) {
            let _ = sqlx::query("ROLLBACK").execute(&mut **conn).await;
        }
        match result {
            Ok(value) => match sqlx::query("COMMIT").execute(&mut *conn).await {
                Ok(_) => Ok(value),
                Err(e) => {
                    rollback_quiet(&mut conn).await;
                    Err(anyhow::Error::new(e).context("Failed to COMMIT events batch"))
                }
            },
            Err(e) => {
                rollback_quiet(&mut conn).await;
                Err(e)
            }
        }
    }

    /// Total number of `context_events` rows (dedupe rows, not raw
    /// occurrence count).
    pub async fn context_event_count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM context_events")
            .fetch_one(&self.pool)
            .await
            .context("Failed to count context events")?;
        Ok(row.get::<i64, _>("cnt"))
    }

    /// Lists every `context_events` row in insertion order. Rows whose
    /// registry strings no longer parse are skipped with a warning
    /// (never invent semantics for corrupt rows).
    pub async fn list_context_events(&self) -> Result<Vec<ContextEventRow>> {
        let rows = sqlx::query(
            r#"
            SELECT id, ts_first, ts_last, count, source, application, window_title,
                   project_id, event_type, summary, importance, confidence,
                   sensitivity, raw_reference, dedupe_key
            FROM context_events
            ORDER BY id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to list context events")?;
        Ok(rows.iter().filter_map(row_to_context_event).collect())
    }

    /// The most recent context events whose last occurrence falls at or
    /// after `since`, **oldest first**, capped at `limit` (the newest
    /// `limit` rows are kept when more match).
    ///
    /// Task B5 (spec §4.8 boot rehydration): session state seeds its
    /// mechanical fields and its inference window from this. Ordering is
    /// oldest-first because every consumer replays it chronologically.
    pub async fn recent_context_events(
        &self,
        since: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ContextEventRow>> {
        let rows = sqlx::query(
            r#"
            SELECT id, ts_first, ts_last, count, source, application, window_title,
                   project_id, event_type, summary, importance, confidence,
                   sensitivity, raw_reference, dedupe_key
            FROM context_events
            WHERE ts_last >= ?1
            ORDER BY ts_last DESC, id DESC
            LIMIT ?2
            "#,
        )
        .bind(since.to_rfc3339())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list recent context events")?;
        let mut out: Vec<ContextEventRow> = rows.iter().filter_map(row_to_context_event).collect();
        out.reverse();
        Ok(out)
    }

    /// Filtered `context_events` read (context engine spec §5.2) — the
    /// query the `context_timeline` / `context_files` MCP tools (Task C4)
    /// are built on, usable over a [`RawLog::open_read_only`] handle.
    ///
    /// Every filter in [`EventQuery`] is ANDed; the **newest**
    /// `limit` matching rows are selected (clamped to
    /// [`EVENT_QUERY_MAX_LIMIT`]) and returned **oldest first**, matching
    /// [`RawLog::recent_context_events`] — every consumer replays events
    /// chronologically.
    ///
    /// Rows whose registry strings no longer parse are skipped rather than
    /// guessed at, exactly like the other event readers.
    pub async fn query_context_events(&self, query: &EventQuery) -> Result<Vec<ContextEventRow>> {
        let limit = query.limit.clamp(1, EVENT_QUERY_MAX_LIMIT);
        let mut sql = String::from(
            "SELECT id, ts_first, ts_last, count, source, application, window_title, \
             project_id, event_type, summary, importance, confidence, sensitivity, \
             raw_reference, dedupe_key FROM context_events WHERE 1 = 1",
        );
        if query.since.is_some() {
            sql.push_str(" AND ts_last >= ?");
        }
        if query.until.is_some() {
            sql.push_str(" AND ts_first <= ?");
        }
        if query.source.is_some() {
            sql.push_str(" AND source = ?");
        }
        if query.project.is_some() {
            sql.push_str(" AND project_id = ?");
        }
        if !query.types.is_empty() {
            sql.push_str(" AND event_type IN (");
            for index in 0..query.types.len() {
                if index > 0 {
                    sql.push(',');
                }
                sql.push('?');
            }
            sql.push(')');
        }
        sql.push_str(" ORDER BY ts_last DESC, id DESC LIMIT ?");

        let mut statement = sqlx::query(&sql);
        if let Some(since) = query.since {
            statement = statement.bind(since.to_rfc3339());
        }
        if let Some(until) = query.until {
            statement = statement.bind(until.to_rfc3339());
        }
        if let Some(source) = query.source {
            statement = statement.bind(event_enum_token(&source));
        }
        if let Some(project) = &query.project {
            statement = statement.bind(project.clone());
        }
        for event_type in &query.types {
            statement = statement.bind(event_enum_token(event_type));
        }
        statement = statement.bind(limit as i64);

        let rows = statement
            .fetch_all(&self.pool)
            .await
            .context("Failed to query context events")?;
        let mut out: Vec<ContextEventRow> = rows.iter().filter_map(row_to_context_event).collect();
        out.reverse();
        Ok(out)
    }

    /// Full-text search over `context_events_fts` (summary +
    /// window_title), best matches first. This is the Task C4 read path
    /// (`context_search`); Plan C opens the DB read-only and reuses the
    /// same query shape.
    ///
    /// `limit` is clamped to `1..=`[`EVENT_SEARCH_MAX_LIMIT`] in the store
    /// (spec §5.2), so no caller can widen the cap. The raw string is
    /// normalized by [`fts_match_query`] **in the store** for the same
    /// reason: `context_search`'s argument comes straight from a model, and
    /// FTS5 `MATCH` has its own query syntax where a stray `"` or `NEAR(`
    /// is a hard error rather than a literal. A query that normalizes away
    /// to nothing returns no rows instead of raising.
    pub async fn search_context_events(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ContextEventRow>> {
        let limit = limit.clamp(1, EVENT_SEARCH_MAX_LIMIT);
        let query = fts_match_query(query);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            SELECT ce.id, ce.ts_first, ce.ts_last, ce.count, ce.source, ce.application,
                   ce.window_title, ce.project_id, ce.event_type, ce.summary,
                   ce.importance, ce.confidence, ce.sensitivity, ce.raw_reference,
                   ce.dedupe_key
            FROM context_events_fts f
            JOIN context_events ce ON ce.id = f.rowid
            WHERE context_events_fts MATCH ?1
            ORDER BY rank
            LIMIT ?2
            "#,
        )
        .bind(query)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("Failed to search context events")?;
        Ok(rows.iter().filter_map(row_to_context_event).collect())
    }

    /// Deletes `context_events` rows whose `ts_last` is older than the
    /// `[events]` retention period. The FTS mirror rows are removed by
    /// the delete trigger. Part of the raw-log rotation path (spec
    /// §4.6: "retention rotated with the raw log"); the events-writer
    /// task calls it periodically.
    pub async fn rotate_events(&self, retention_days: u32) -> Result<u64> {
        let cutoff = (Utc::now() - Duration::days(retention_days as i64)).to_rfc3339();
        let result = sqlx::query("DELETE FROM context_events WHERE ts_last < ?1")
            .bind(&cutoff)
            .execute(&self.pool)
            .await
            .context("Failed to rotate old context events")?;
        let deleted = result.rows_affected();
        if deleted > 0 {
            info!(
                layer = "memory",
                component = "raw_log",
                deleted_events = deleted,
                retention_days = retention_days,
                "Rotated old context events"
            );
        }
        Ok(deleted)
    }

    // -----------------------------------------------------------------
    // Targeted deletion — the Context page's Forget cascade and
    // delete-range (spec §4.13, Task C5)
    // -----------------------------------------------------------------

    /// Fetches one `context_events` row by rowid.
    pub async fn context_event(&self, id: i64) -> Result<Option<ContextEventRow>> {
        let row = sqlx::query(
            "SELECT id, ts_first, ts_last, count, source, application, window_title, \
             project_id, event_type, summary, importance, confidence, sensitivity, \
             raw_reference, dedupe_key FROM context_events WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to fetch context event by id")?;
        Ok(row.as_ref().and_then(row_to_context_event))
    }

    /// Deletes one `context_events` row. The FTS mirror row goes with it
    /// via the `context_events_fts_ad` delete trigger — never delete from
    /// `context_events_fts` directly.
    pub async fn delete_context_event(&self, id: i64) -> Result<u64> {
        let result = sqlx::query("DELETE FROM context_events WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("Failed to delete context event")?;
        Ok(result.rows_affected())
    }

    /// Deletes every `context_events` row pointing at one raw reference
    /// (a perception-frame id for screen/audio events). FTS rows follow
    /// via the delete trigger.
    pub async fn delete_context_events_by_raw_reference(&self, raw_reference: &str) -> Result<u64> {
        let result = sqlx::query("DELETE FROM context_events WHERE raw_reference = ?1")
            .bind(raw_reference)
            .execute(&self.pool)
            .await
            .context("Failed to delete context events by raw reference")?;
        Ok(result.rows_affected())
    }

    /// Deletes one perception frame and, when the policy allows it, the
    /// screenshot file it referenced.
    ///
    /// Returns `(rows_deleted, screenshots_deleted)`. A screenshot that is
    /// already gone is not an error — the row still goes.
    pub async fn delete_frame(
        &self,
        frame_id: uuid::Uuid,
        policy: &ScreenshotPolicy,
    ) -> Result<(u64, usize)> {
        let id = frame_id.to_string();
        let paths: Vec<String> = sqlx::query(
            "SELECT screen_screenshot_path FROM perception_frames \
             WHERE id = ?1 AND screen_screenshot_path IS NOT NULL",
        )
        .bind(&id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to query screenshot path before frame delete")?
        .iter()
        .filter_map(|row| row.get::<Option<String>, _>("screen_screenshot_path"))
        .collect();

        let mut screenshots_deleted = 0usize;
        if policy.deletes_referenced_files() {
            for path in &paths {
                match std::fs::remove_file(path) {
                    Ok(()) => screenshots_deleted += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => warn!(
                        layer = "memory",
                        component = "raw_log",
                        path = path,
                        error = %e,
                        "Failed to delete screenshot during forget"
                    ),
                }
            }
        }

        let result = sqlx::query("DELETE FROM perception_frames WHERE id = ?1")
            .bind(&id)
            .execute(&self.pool)
            .await
            .context("Failed to delete perception frame")?;
        Ok((result.rows_affected(), screenshots_deleted))
    }

    /// Purges everything this database holds about a time window
    /// (spec §4.13 delete-range, Continuum.md §13): perception frames plus
    /// their screenshots, and every `context_events` row that overlaps the
    /// window.
    ///
    /// Window semantics match [`EventQuery`]: frames by `ts`, events by
    /// **overlap** (`ts_last >= from AND ts_first <= to`) — a collapsed row
    /// that started before the window but was last seen inside it *did*
    /// happen inside the window, so it goes.
    ///
    /// Bounds are inclusive. An inverted range (`to < from`) deletes
    /// nothing rather than everything.
    pub async fn delete_range(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        policy: &ScreenshotPolicy,
    ) -> Result<DeleteRangeStats> {
        if to < from {
            warn!(
                layer = "memory",
                component = "raw_log",
                from = %from,
                to = %to,
                "Refusing an inverted delete range"
            );
            return Ok(DeleteRangeStats::default());
        }
        let from_str = from.to_rfc3339();
        let to_str = to.to_rfc3339();

        let paths: Vec<String> = sqlx::query(
            "SELECT screen_screenshot_path FROM perception_frames \
             WHERE ts >= ?1 AND ts <= ?2 AND screen_screenshot_path IS NOT NULL",
        )
        .bind(&from_str)
        .bind(&to_str)
        .fetch_all(&self.pool)
        .await
        .context("Failed to query screenshot paths for delete range")?
        .iter()
        .filter_map(|row| row.get::<Option<String>, _>("screen_screenshot_path"))
        .collect();

        let mut screenshots_deleted = 0usize;
        if policy.deletes_referenced_files() {
            for path in &paths {
                match std::fs::remove_file(path) {
                    Ok(()) => screenshots_deleted += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => warn!(
                        layer = "memory",
                        component = "raw_log",
                        path = path,
                        error = %e,
                        "Failed to delete screenshot during range purge"
                    ),
                }
            }
        }

        let frames = sqlx::query("DELETE FROM perception_frames WHERE ts >= ?1 AND ts <= ?2")
            .bind(&from_str)
            .bind(&to_str)
            .execute(&self.pool)
            .await
            .context("Failed to delete frames in range")?
            .rows_affected();

        let events =
            sqlx::query("DELETE FROM context_events WHERE ts_last >= ?1 AND ts_first <= ?2")
                .bind(&from_str)
                .bind(&to_str)
                .execute(&self.pool)
                .await
                .context("Failed to delete context events in range")?
                .rows_affected();

        info!(
            layer = "memory",
            component = "raw_log",
            from = %from,
            to = %to,
            frames = frames,
            events = events,
            screenshots = screenshots_deleted,
            "Purged a user-requested time range"
        );

        Ok(DeleteRangeStats {
            frames_deleted: frames,
            events_deleted: events,
            screenshots_deleted,
        })
    }

    /// The perception-frame ids inside a window — the episodic store needs
    /// them to delete the memories derived from those frames, and it has no
    /// timestamp index of its own that the SQL window can be pushed into.
    /// The delete-range intent also uses them to cascade to vault
    /// candidates keyed on `triage:frame:<id>` (fixwave 2, M7), which is
    /// why it must be called *before* the purge.
    pub async fn frame_ids_in_range(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<String>> {
        if to < from {
            return Ok(Vec::new());
        }
        let rows = sqlx::query("SELECT id FROM perception_frames WHERE ts >= ?1 AND ts <= ?2")
            .bind(from.to_rfc3339())
            .bind(to.to_rfc3339())
            .fetch_all(&self.pool)
            .await
            .context("Failed to list frame ids in range")?;
        Ok(rows.iter().map(|row| row.get::<String, _>("id")).collect())
    }

    /// Test-only single-event insert.
    ///
    /// Production writes go through the events-writer task's batched,
    /// deduped transaction (`memory::events`); this helper exists so
    /// sibling test modules (the Task C5 Forget cascade, for one) can seed
    /// a row without standing up a writer. `pub(crate)` and `#[cfg(test)]`
    /// so it can never become a second production write path.
    #[cfg(test)]
    pub(crate) async fn insert_event_for_test(
        &self,
        event: &crate::memory::events::ContextEvent,
        dedupe_key: &str,
    ) -> i64 {
        let mut conn = self.immediate_conn().await.expect("immediate conn");
        let result = insert_context_event(&mut conn, event, dedupe_key).await;
        RawLog::finish_write(conn, result)
            .await
            .expect("insert test event")
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

/// One `projects` row: the resolver entry plus its persistence-only stats
/// (context engine spec §4.3).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredProject {
    /// The resolver-facing identity fields.
    pub entry: ProjectEntry,
    /// When the row was first created (RFC 3339).
    pub created_ts: String,
    /// Last frame the resolver attributed to this project (RFC 3339).
    pub last_active_ts: Option<String>,
    /// Number of frames attributed to this project.
    pub frames_count: i64,
}

fn row_to_project(row: &sqlx::sqlite::SqliteRow) -> StoredProject {
    let root_paths: Vec<String> = row
        .get::<Option<String>, _>("root_paths")
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    let keywords: Vec<String> = row
        .get::<Option<String>, _>("keywords")
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    let zone = row
        .get::<Option<String>, _>("zone")
        .and_then(|z| serde_json::from_value(serde_json::Value::String(z)).ok());
    let status_str: String = row.get("status");
    StoredProject {
        entry: ProjectEntry {
            id: row.get("id"),
            name: row.get("name"),
            root_paths,
            repo: row.get("repo"),
            keywords,
            zone,
            status: crate::context::project::ProjectStatus::parse(&status_str),
        },
        created_ts: row.get("created_ts"),
        last_active_ts: row.get("last_active_ts"),
        frames_count: row.get("frames_count"),
    }
}

/// One stored `context_events` row (context engine spec §4.6): a deduped
/// event with its collapse bookkeeping (`ts_first`/`ts_last`/`count`) and
/// the persisted dedupe key.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextEventRow {
    /// Rowid (also the FTS external-content rowid).
    pub id: i64,
    /// When the first collapsed occurrence happened.
    pub ts_first: DateTime<Utc>,
    /// When the most recent collapsed occurrence happened (the dedupe
    /// window anchor).
    pub ts_last: DateTime<Utc>,
    /// Number of occurrences collapsed into this row.
    pub count: i64,
    /// Originating collector family.
    pub source: EventSource,
    /// Application the event is about.
    pub application: String,
    /// Window title at first occurrence (scrubbed upstream).
    pub window_title: String,
    /// Resolved project id, when known.
    pub project_id: Option<String>,
    /// What happened.
    pub event_type: EventType,
    /// The *first* occurrence's summary (kept as display text across
    /// collapses, spec §4.6).
    pub summary: String,
    /// Importance in \[0.0, 1.0\].
    pub importance: f32,
    /// Confidence in \[0.0, 1.0\].
    pub confidence: f32,
    /// Zone-derived sensitivity tag.
    pub sensitivity: EventSensitivity,
    /// Optional pointer into the raw log.
    pub raw_reference: Option<String>,
    /// The spec §4.6 dedupe key this row collapses under.
    pub dedupe_key: String,
}

/// The dedupe bookkeeping of one open collapse row — what the writer needs
/// to decide collapse-vs-fresh without re-reading the full row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DedupeCandidate {
    /// Rowid of the open row.
    pub id: i64,
    /// `ts_first` of the open row (span-cap check).
    pub ts_first: DateTime<Utc>,
    /// `ts_last` of the open row (window anchor).
    pub ts_last: DateTime<Utc>,
    /// Current collapsed count (count-cap check).
    pub count: i64,
}

fn parse_ts(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn row_to_context_event(row: &sqlx::sqlite::SqliteRow) -> Option<ContextEventRow> {
    let source_str: String = row.get("source");
    let type_str: String = row.get("event_type");
    let sensitivity_str: String = row.get("sensitivity");
    let (Some(source), Some(event_type), Some(sensitivity)) = (
        parse_event_enum::<EventSource>(&source_str),
        parse_event_enum::<EventType>(&type_str),
        parse_event_enum::<EventSensitivity>(&sensitivity_str),
    ) else {
        warn!(
            layer = "memory",
            component = "raw_log",
            source = %source_str,
            event_type = %type_str,
            sensitivity = %sensitivity_str,
            "context_events row has unknown registry strings; skipping"
        );
        return None;
    };
    Some(ContextEventRow {
        id: row.get("id"),
        ts_first: parse_ts(&row.get::<String, _>("ts_first")),
        ts_last: parse_ts(&row.get::<String, _>("ts_last")),
        count: row.get("count"),
        source,
        application: row.get("application"),
        window_title: row.get("window_title"),
        project_id: row.get("project_id"),
        event_type,
        summary: row.get("summary"),
        importance: row.get::<f64, _>("importance") as f32,
        confidence: row.get::<f64, _>("confidence") as f32,
        sensitivity,
        raw_reference: row.get("raw_reference"),
        dedupe_key: row.get("dedupe_key"),
    })
}

/// Inserts a fresh dedupe row for `event` inside the writer's batch
/// transaction and returns its rowid. `ts_first = ts_last = event.ts`,
/// `count = 1`.
pub(crate) async fn insert_context_event(
    conn: &mut SqliteConnection,
    event: &ContextEvent,
    dedupe_key: &str,
) -> Result<i64> {
    let result = sqlx::query(
        r#"
        INSERT INTO context_events (
            ts_first, ts_last, count, source, application, window_title,
            project_id, event_type, summary, importance, confidence,
            sensitivity, raw_reference, dedupe_key
        ) VALUES (?1, ?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        "#,
    )
    .bind(event.ts.to_rfc3339())
    .bind(event_enum_token(&event.source))
    .bind(&event.application)
    .bind(&event.window_title)
    .bind(event.project_id.as_deref())
    .bind(event_enum_token(&event.event_type))
    .bind(&event.summary)
    .bind(event.importance as f64)
    .bind(event.confidence as f64)
    .bind(event_enum_token(&event.sensitivity))
    .bind(event.raw_reference.as_deref())
    .bind(dedupe_key)
    .execute(&mut *conn)
    .await
    .context("Failed to insert context event")?;
    Ok(result.last_insert_rowid())
}

/// Collapses a repeat occurrence into an open row: `count = ?2`,
/// `ts_last = ?3`, first summary kept (spec §4.6). Returns `false` when
/// the row no longer exists (rotated away under a stale LRU entry) so the
/// writer can fall back to a fresh insert.
pub(crate) async fn collapse_context_event(
    conn: &mut SqliteConnection,
    id: i64,
    count: i64,
    ts_last: DateTime<Utc>,
) -> Result<bool> {
    let result = sqlx::query("UPDATE context_events SET count = ?2, ts_last = ?3 WHERE id = ?1")
        .bind(id)
        .bind(count)
        .bind(ts_last.to_rfc3339())
        .execute(&mut *conn)
        .await
        .context("Failed to collapse context event")?;
    Ok(result.rows_affected() > 0)
}

/// Stamps `perception_frames.context_event_at` for the frame a
/// classified event points at (fixwave 3a, C3), inside the writer's batch
/// transaction.
///
/// Called for **every** occurrence — the fresh insert and each collapse —
/// because a collapsed occurrence leaves no per-frame trace anywhere else
/// and would otherwise be distilled a second time through the raw-frame
/// fallback. Idempotent: the first stamp wins (`context_event_at IS
/// NULL`), so the column records when the moment first became an event.
///
/// A frame id that is not a UUID (a git sha, a file path — other sources
/// put non-frame pointers in `raw_reference`) never reaches here; see the
/// caller in `memory::events`.
pub(crate) async fn mark_frame_context_event(
    conn: &mut SqliteConnection,
    frame_id: &uuid::Uuid,
    ts: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "UPDATE perception_frames SET context_event_at = ?2 \
         WHERE id = ?1 AND context_event_at IS NULL",
    )
    .bind(frame_id.to_string())
    .bind(ts.to_rfc3339())
    .execute(&mut *conn)
    .await
    .context("Failed to stamp perception_frames.context_event_at")?;
    Ok(())
}

/// Fetches the most recent row for a dedupe key (the `(dedupe_key,
/// ts_last)` index path), for LRU misses.
pub(crate) async fn latest_context_event_for_key(
    conn: &mut SqliteConnection,
    dedupe_key: &str,
) -> Result<Option<DedupeCandidate>> {
    let row = sqlx::query(
        "SELECT id, ts_first, ts_last, count FROM context_events \
         WHERE dedupe_key = ?1 ORDER BY ts_last DESC LIMIT 1",
    )
    .bind(dedupe_key)
    .fetch_optional(&mut *conn)
    .await
    .context("Failed to query context event by dedupe key")?;
    Ok(row.map(|row| DedupeCandidate {
        id: row.get("id"),
        ts_first: parse_ts(&row.get::<String, _>("ts_first")),
        ts_last: parse_ts(&row.get::<String, _>("ts_last")),
        count: row.get("count"),
    }))
}

fn row_to_frame(row: &sqlx::sqlite::SqliteRow) -> PerceptionFrame {
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
            duration_ms: row.get::<Option<i64>, _>("audio_duration_ms").unwrap_or(0) as u64,
            confidence: row.get::<Option<f64>, _>("audio_confidence").unwrap_or(0.0) as f32,
            ts,
        })
    };

    PerceptionFrame {
        id,
        ts,
        screen: crate::senses::types::ScreenObservation {
            description: row
                .get::<Option<String>, _>("screen_description")
                .unwrap_or_default(),
            world_compact: row.get("screen_world_compact"),
            foreground_app: row
                .get::<Option<String>, _>("screen_foreground_app")
                .unwrap_or_default(),
            has_error_visible: row.get::<Option<i32>, _>("screen_has_error").unwrap_or(0) != 0,
            confidence: row
                .get::<Option<f64>, _>("screen_confidence")
                .unwrap_or(0.0) as f32,
            screenshot_path: row.get("screen_screenshot_path"),
            ts,
        },
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
            in_call: row.get::<Option<i32>, _>("context_in_call").unwrap_or(0) != 0,
            pid: row
                .get::<Option<i64>, _>("context_pid")
                .and_then(|pid| u32::try_from(pid).ok()),
            exe_path: row.get("context_exe_path"),
            active_since_secs: row
                .get::<Option<i64>, _>("context_active_since_secs")
                .unwrap_or(0) as u64,
            monitor_id: row.get("context_monitor_id"),
            // NULL (pre-fixwave-2 row) or an unrecognised token both mean
            // "untagged": consumers fall back to the sentinel-literal gate.
            privacy: row
                .get::<Option<String>, _>("context_privacy")
                .as_deref()
                .and_then(crate::senses::live_context::PrivacyDisposition::from_token),
            ts,
        },
        audio,
        salience_hint: row.get::<Option<f64>, _>("salience").unwrap_or(0.0) as f32,
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
                world_compact: None,
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

    #[tokio::test]
    async fn test_schema_creation() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();
        // Schema should be created without error.
        let count = log.frame_count().await.unwrap();
        assert_eq!(count, 0);
        log.close().await;
    }

    /// The writer pool must carry an explicit, short lock wait — sqlx's
    /// 5 s default let a frame write stall the runtime's main select loop
    /// behind the events writer's batch transaction.
    #[tokio::test]
    async fn test_writer_pool_sets_an_explicit_busy_timeout() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();
        let row = sqlx::query("PRAGMA busy_timeout")
            .fetch_one(&log.pool)
            .await
            .unwrap();
        let timeout_ms: i64 = row.get(0);
        assert_eq!(timeout_ms, WRITER_BUSY_TIMEOUT.as_millis() as i64);
        assert!(
            timeout_ms <= 1000,
            "the loop must never wait a second on a lock"
        );
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
        assert_eq!(frames[0].screen.world_compact, None);
        assert!(frames[0].audio.is_none());

        log.close().await;
    }

    #[tokio::test]
    async fn query_recent_frames_is_bounded_and_chronological() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();
        let base = Utc::now();
        for (index, description) in ["old", "middle", "new"].into_iter().enumerate() {
            let mut frame = test_frame(description, false);
            frame.ts = base + Duration::seconds(index as i64);
            frame.screen.ts = frame.ts;
            frame.context.ts = frame.ts;
            log.write_frame(&frame).await.unwrap();
        }

        let frames = log
            .query_recent_frames(base - Duration::seconds(1), base + Duration::seconds(5), 2)
            .await
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].screen.description, "middle");
        assert_eq!(frames[1].screen.description, "new");
        assert!(frames[0].ts < frames[1].ts);

        log.close().await;
    }

    #[tokio::test]
    async fn test_caption_and_world_compact_are_stored_separately() {
        // Field routing (context engine spec §4.10, Task B4): the caption
        // lands in screen_description, the compact world-state blob in
        // screen_world_compact, and neither leaks into the other across a
        // write/read cycle. The blob is deliberately multi-line and long —
        // exactly the payload that used to sit in `description`.
        let log = RawLog::open("sqlite::memory:").await.unwrap();

        let blob = format!(
            "live-context/v2 seq=42 generated=2026-08-05T12:00:00Z\n\
             [monitor:display-1] Primary primary event=7 capture=9 change=0.812 \
             privacy=Visible vision=\"a code editor with a terminal\"\n\
             [window:foreground] process=Code.exe title=\"main.rs\" privacy=Visible\n{}",
            "x".repeat(600)
        );
        let mut frame = test_frame("a code editor with a terminal", false);
        frame.screen.world_compact = Some(blob.clone());
        log.write_frame(&frame).await.unwrap();

        let frames = log
            .query_frames(
                frame.ts - Duration::seconds(1),
                frame.ts + Duration::seconds(1),
            )
            .await
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].screen.description,
            "a code editor with a terminal"
        );
        assert_eq!(
            frames[0].screen.world_compact.as_deref(),
            Some(blob.as_str())
        );

        // The undistilled query reads through the same projection.
        let undistilled = log
            .query_undistilled_frames(
                frame.ts - Duration::seconds(1),
                frame.ts + Duration::seconds(1),
                0.0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(undistilled.len(), 1);
        assert_eq!(
            undistilled[0].screen.description,
            "a code editor with a terminal"
        );
        assert_eq!(
            undistilled[0].screen.world_compact.as_deref(),
            Some(blob.as_str())
        );

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
    async fn test_enrichment_columns_roundtrip() {
        // Context engine spec §4.2: pid, exe_path, active_since_secs and
        // monitor_id persist through the additive raw-log columns.
        let log = RawLog::open("sqlite::memory:").await.unwrap();

        let mut frame = test_frame("enriched frame", false);
        frame.context.pid = Some(1234);
        frame.context.exe_path = Some("~\\AppData\\Code\\Code.exe".to_string());
        frame.context.active_since_secs = 42;
        frame.context.monitor_id = Some("display-2".to_string());
        log.write_frame(&frame).await.unwrap();

        // A legacy-shaped frame (no enrichment) must roundtrip as None/0.
        let bare = test_frame("bare frame", false);
        log.write_frame(&bare).await.unwrap();

        let since = Utc::now() - Duration::seconds(5);
        let until = Utc::now() + Duration::seconds(5);
        let frames = log.query_frames(since, until).await.unwrap();
        assert_eq!(frames.len(), 2);

        let enriched = frames.iter().find(|f| f.id == frame.id).unwrap();
        assert_eq!(enriched.context.pid, Some(1234));
        assert_eq!(
            enriched.context.exe_path.as_deref(),
            Some("~\\AppData\\Code\\Code.exe")
        );
        assert_eq!(enriched.context.active_since_secs, 42);
        assert_eq!(enriched.context.monitor_id.as_deref(), Some("display-2"));

        let plain = frames.iter().find(|f| f.id == bare.id).unwrap();
        assert_eq!(plain.context.pid, None);
        assert_eq!(plain.context.exe_path, None);
        assert_eq!(plain.context.active_since_secs, 0);
        assert_eq!(plain.context.monitor_id, None);

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

    // --- Projects table (context engine spec §4.3, Task A4) ---

    use crate::context::project::{ProjectStatus, RuleAction};

    fn project_entry(id: &str, name: &str, status: ProjectStatus) -> ProjectEntry {
        ProjectEntry {
            id: id.to_string(),
            name: name.to_string(),
            root_paths: vec![format!("D:\\Dev\\{id}")],
            repo: None,
            keywords: vec![id.to_string()],
            zone: None,
            status,
        }
    }

    #[tokio::test]
    async fn projects_upsert_confirm_and_stats_roundtrip() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();

        let mut entry = project_entry("continuum", "Continuum", ProjectStatus::Configured);
        entry.zone = Some(crate::senses::privacy::Zone::LocalOnly);
        entry.repo = Some("https://example.com/x.git".to_string());
        log.upsert_project(&entry).await.unwrap();

        let rows = log.list_projects().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry, entry);
        assert_eq!(rows[0].frames_count, 0);
        assert!(rows[0].last_active_ts.is_none());

        // Stats bump.
        let ts = Utc::now();
        log.bump_project_stats("continuum", ts).await.unwrap();
        log.bump_project_stats("continuum", ts).await.unwrap();
        let rows = log.list_projects().await.unwrap();
        assert_eq!(rows[0].frames_count, 2);
        assert_eq!(
            rows[0].last_active_ts.as_deref(),
            Some(ts.to_rfc3339().as_str())
        );

        // Confirm flips status; unknown ids report false.
        let discovered = project_entry("newthing", "NewThing", ProjectStatus::Discovered);
        log.upsert_project(&discovered).await.unwrap();
        assert!(log.confirm_project("newthing").await.unwrap());
        assert!(!log.confirm_project("ghost").await.unwrap());
        let rows = log.list_projects().await.unwrap();
        let confirmed = rows.iter().find(|r| r.entry.id == "newthing").unwrap();
        assert_eq!(confirmed.entry.status, ProjectStatus::Confirmed);

        log.close().await;
    }

    #[tokio::test]
    async fn projects_reconciliation_config_wins_by_id_and_preserves_rows() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();

        // Pre-existing rows: a discovered candidate colliding with a
        // config id (with accumulated stats) and an unrelated confirmed row.
        let stale = project_entry("continuum", "continuum-guess", ProjectStatus::Discovered);
        log.upsert_project(&stale).await.unwrap();
        log.bump_project_stats("continuum", Utc::now())
            .await
            .unwrap();
        let confirmed = project_entry("sideproject", "SideProject", ProjectStatus::Confirmed);
        log.upsert_project(&confirmed).await.unwrap();

        // Boot reconciliation: config wins by id.
        let config = vec![project_entry(
            "continuum",
            "Continuum",
            ProjectStatus::Configured,
        )];
        let rows = log.reconcile_projects(&config).await.unwrap();
        assert_eq!(rows.len(), 2);
        let reconciled = rows.iter().find(|r| r.entry.id == "continuum").unwrap();
        assert_eq!(reconciled.entry.name, "Continuum", "config name wins");
        assert_eq!(reconciled.entry.status, ProjectStatus::Configured);
        assert_eq!(
            reconciled.frames_count, 1,
            "stats preserved across reconcile"
        );
        // The non-config row persists untouched.
        let kept = rows.iter().find(|r| r.entry.id == "sideproject").unwrap();
        assert_eq!(kept.entry.status, ProjectStatus::Confirmed);

        // Reconciliation is idempotent.
        let again = log.reconcile_projects(&config).await.unwrap();
        assert_eq!(again.len(), 2);

        log.close().await;
    }

    #[tokio::test]
    async fn project_rules_and_session_pins_roundtrip() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();

        let force = OverrideRule {
            match_process: Some("obsidian.exe".to_string()),
            match_title_substring: None,
            action: RuleAction::ForceProject,
            project_id: "continuum".to_string(),
        };
        let exclude = OverrideRule {
            match_process: None,
            match_title_substring: Some("blog".to_string()),
            action: RuleAction::ExcludeProject,
            project_id: "continuum".to_string(),
        };
        let first_id = log.add_project_rule(&force).await.unwrap();
        let second_id = log.add_project_rule(&exclude).await.unwrap();
        assert!(second_id > first_id);
        let rules = log.list_project_rules().await.unwrap();
        assert_eq!(rules, vec![force, exclude]);

        // Session pins upsert-by-key and clear.
        log.set_session_pin("active_project", Some("continuum"), Some("continuum"))
            .await
            .unwrap();
        log.set_session_pin("active_project", Some("simcharts"), Some("simcharts"))
            .await
            .unwrap();
        let pins = log.list_session_pins().await.unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].1.as_deref(), Some("simcharts"));
        assert!(log.clear_session_pin("active_project").await.unwrap());
        assert!(!log.clear_session_pin("active_project").await.unwrap());
        assert!(log.list_session_pins().await.unwrap().is_empty());

        log.close().await;
    }

    // --- Context events table (context engine spec §4.6, Task A6) ---

    fn context_event(summary: &str, ts: DateTime<Utc>) -> ContextEvent {
        ContextEvent {
            ts,
            source: EventSource::System,
            application: String::new(),
            window_title: "editor window".to_string(),
            project_id: None,
            event_type: EventType::ToggleChange,
            summary: summary.to_string(),
            importance: 0.2,
            confidence: 1.0,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: None,
        }
    }

    async fn insert_event(log: &RawLog, event: &ContextEvent, key: &str) -> i64 {
        let mut conn = log.immediate_conn().await.unwrap();
        let result = insert_context_event(&mut conn, event, key).await;
        RawLog::finish_write(conn, result).await.unwrap()
    }

    #[tokio::test]
    async fn context_events_fts_triggers_follow_text_changes_only() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();
        let id = insert_event(&log, &context_event("alpha build failed", Utc::now()), "k1").await;

        // Insert trigger: text is searchable.
        assert_eq!(
            log.search_context_events("alpha", 10).await.unwrap().len(),
            1
        );
        // window_title is indexed too.
        assert_eq!(
            log.search_context_events("editor", 10).await.unwrap().len(),
            1
        );

        // Count/ts_last-only UPDATE (the hot dedupe path) must NOT touch
        // the FTS index: still exactly one hit, no duplicates.
        sqlx::query("UPDATE context_events SET count = 5, ts_last = ?2 WHERE id = ?1")
            .bind(id)
            .bind(Utc::now().to_rfc3339())
            .execute(&log.pool)
            .await
            .unwrap();
        let hits = log.search_context_events("alpha", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].count, 5);

        // A text-bearing UPDATE rewrites the index row.
        sqlx::query("UPDATE context_events SET summary = 'replacement text' WHERE id = ?1")
            .bind(id)
            .execute(&log.pool)
            .await
            .unwrap();
        assert!(log
            .search_context_events("alpha", 10)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            log.search_context_events("replacement", 10)
                .await
                .unwrap()
                .len(),
            1
        );

        // Delete trigger removes the index row.
        sqlx::query("DELETE FROM context_events WHERE id = ?1")
            .bind(id)
            .execute(&log.pool)
            .await
            .unwrap();
        assert!(log
            .search_context_events("replacement", 10)
            .await
            .unwrap()
            .is_empty());
        log.close().await;
    }

    #[test]
    fn fts_match_query_normalizes_and_neutralizes_operators() {
        assert_eq!(fts_match_query("build failed"), "\"build\"* \"failed\"*");
        // FTS5 operators are stripped, never forwarded. Note that
        // stripping happens *inside* a whitespace token, so `NEAR(a`
        // glues into one term rather than splitting into two.
        assert_eq!(
            fts_match_query("\"quoted\" NEAR(a b)"),
            "\"quoted\"* \"NEARa\"* \"b\"*"
        );
        assert_eq!(fts_match_query("cargo-test"), "\"cargotest\"*");
        assert_eq!(fts_match_query("   "), "");
        assert_eq!(fts_match_query("*^:-"), "");
    }

    /// A model-supplied query with FTS syntax in it must return rows (or
    /// nothing), never an error — `context_search` may not kill a wake.
    #[tokio::test]
    async fn context_events_search_survives_hostile_query_strings() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();
        insert_event(&log, &context_event("alpha build failed", Utc::now()), "k1").await;

        // Unbalanced quote + operator soup: no error, still finds the row.
        // Surviving terms are ANDed, so both words must be present.
        let hits = log
            .search_context_events("\"alpha (build*", 10)
            .await
            .expect("hostile query must not error");
        assert_eq!(hits.len(), 1);

        // Prefix matching survives normalization.
        assert_eq!(
            log.search_context_events("alph", 10).await.unwrap().len(),
            1
        );

        // A query that normalizes to nothing is "no rows", not an error.
        assert!(log
            .search_context_events("***", 10)
            .await
            .unwrap()
            .is_empty());
        log.close().await;
    }

    /// Task B5 (spec §4.8 boot rehydration): `recent_context_events`
    /// returns only rows at/after `since`, **oldest first**, and when more
    /// than `limit` match it keeps the NEWEST ones (not the first ones the
    /// table happens to hold).
    #[tokio::test]
    async fn recent_context_events_windows_orders_and_keeps_the_newest() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();
        let now = Utc::now();

        // Oldest first in insertion order so "keeps newest" is a real
        // assertion, not an artifact of rowid ordering.
        for (i, ago_min) in [180i64, 45, 30, 20, 10, 1].iter().enumerate() {
            insert_event(
                &log,
                &context_event(&format!("event {i}"), now - Duration::minutes(*ago_min)),
                &format!("k{i}"),
            )
            .await;
        }

        // 60-minute window drops the 180-minute-old row.
        let rows = log
            .recent_context_events(now - Duration::minutes(60), 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows.first().unwrap().summary, "event 1");
        assert_eq!(rows.last().unwrap().summary, "event 5");
        for pair in rows.windows(2) {
            assert!(pair[0].ts_last <= pair[1].ts_last, "must be oldest-first");
        }

        // Limit keeps the newest 2, still oldest-first.
        let rows = log
            .recent_context_events(now - Duration::minutes(60), 2)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].summary, "event 4");
        assert_eq!(rows[1].summary, "event 5");

        // A window with nothing in it is empty, not an error.
        assert!(log
            .recent_context_events(now + Duration::minutes(1), 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn context_events_migration_is_idempotent() {
        // Opening the same DB twice re-runs create_schema over the
        // existing table, index, FTS table, and triggers — must be a
        // no-op, preserving rows.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw.db");
        let path_str = path.to_string_lossy().to_string();

        let log = RawLog::open(&path_str).await.unwrap();
        insert_event(
            &log,
            &context_event("persisted across reopen", Utc::now()),
            "k1",
        )
        .await;
        assert_eq!(log.context_event_count().await.unwrap(), 1);
        log.close().await;

        let reopened = RawLog::open(&path_str).await.unwrap();
        assert_eq!(reopened.context_event_count().await.unwrap(), 1);
        assert_eq!(
            reopened
                .search_context_events("persisted", 10)
                .await
                .unwrap()
                .len(),
            1
        );
        // Writes still work after the second migration pass.
        insert_event(&reopened, &context_event("second row", Utc::now()), "k2").await;
        assert_eq!(reopened.context_event_count().await.unwrap(), 2);
        reopened.close().await;
    }

    #[tokio::test]
    async fn rotate_events_deletes_old_rows_and_their_fts_entries() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();
        insert_event(
            &log,
            &context_event("ancient history", Utc::now() - Duration::days(40)),
            "k-old",
        )
        .await;
        insert_event(&log, &context_event("fresh news", Utc::now()), "k-new").await;

        let deleted = log.rotate_events(30).await.unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(log.context_event_count().await.unwrap(), 1);
        assert!(log
            .search_context_events("ancient", 10)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            log.search_context_events("fresh", 10).await.unwrap().len(),
            1
        );

        // Idempotent: nothing left to rotate.
        assert_eq!(log.rotate_events(30).await.unwrap(), 0);
        log.close().await;
    }

    #[tokio::test]
    async fn test_wipe_all_deletes_every_frame_regardless_of_age() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();

        let mut old_frame = test_frame("old frame", false);
        old_frame.ts = Utc::now() - Duration::days(400);
        log.write_frame(&old_frame).await.unwrap();

        let recent = test_frame("recent frame", true);
        log.write_frame(&recent).await.unwrap();

        assert_eq!(log.frame_count().await.unwrap(), 2);

        let deleted = log.wipe_all().await.unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(log.frame_count().await.unwrap(), 0);

        // Calling again on an already-empty log is a harmless no-op.
        let deleted_again = log.wipe_all().await.unwrap();
        assert_eq!(deleted_again, 0);

        log.close().await;
    }

    // --- Distiller input selection (Task B6, spec §4.11) ---

    /// A classified screen event pointing at `frame_id`, the shape B3
    /// writes.
    fn classified_event(
        summary: &str,
        importance: f32,
        frame_id: Option<Uuid>,
        project: Option<&str>,
    ) -> ContextEvent {
        ContextEvent {
            ts: Utc::now(),
            source: EventSource::Screen,
            application: "Code.exe".to_string(),
            window_title: "main.rs - continuum".to_string(),
            project_id: project.map(str::to_string),
            event_type: EventType::Error,
            summary: summary.to_string(),
            importance,
            confidence: 0.8,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: frame_id.map(|id| id.to_string()),
        }
    }

    #[tokio::test]
    async fn undistilled_events_respect_importance_window_and_marking() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();
        let since = Utc::now() - Duration::minutes(30);
        let until = Utc::now() + Duration::minutes(1);

        let important = insert_event(
            &log,
            &classified_event("build failed", 0.7, None, Some("continuum")),
            "k-important",
        )
        .await;
        insert_event(
            &log,
            &classified_event("mouse wiggled", 0.05, None, None),
            "k-trivial",
        )
        .await;
        let mut stale = classified_event("ancient failure", 0.9, None, None);
        stale.ts = Utc::now() - Duration::days(3);
        insert_event(&log, &stale, "k-stale").await;

        let rows = log
            .query_undistilled_events(since, until, 0.35, 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "importance + window filter, got {rows:?}");
        assert_eq!(rows[0].id, important);
        assert_eq!(rows[0].project_id.as_deref(), Some("continuum"));

        // Marking is idempotent: the first call stamps, the second is a
        // no-op, and the row never comes back.
        assert_eq!(log.mark_events_distilled(&[important]).await.unwrap(), 1);
        assert_eq!(log.mark_events_distilled(&[important]).await.unwrap(), 0);
        assert!(log
            .query_undistilled_events(since, until, 0.35, 100)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(log.mark_events_distilled(&[]).await.unwrap(), 0);

        log.close().await;
    }

    #[tokio::test]
    async fn fallback_frame_query_skips_frames_that_produced_an_event() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();
        let since = Utc::now() - Duration::minutes(30);
        let until = Utc::now() + Duration::minutes(1);

        // Two qualifying frames (both have audio); only one was classified.
        let classified = test_frame("classified frame", true);
        let unclassified = test_frame("unclassified frame", true);
        log.write_frame(&classified).await.unwrap();
        log.write_frame(&unclassified).await.unwrap();
        insert_event(
            &log,
            &classified_event("build failed", 0.7, Some(classified.id), None),
            "k-classified",
        )
        .await;

        // The legacy predicate still sees both (it stays as-is).
        assert_eq!(
            log.query_undistilled_frames(since, until, 0.35, 100)
                .await
                .unwrap()
                .len(),
            2
        );

        // The distiller's fallback predicate sees only the unclassified one.
        let fallback = log
            .query_undistilled_unclassified_frames(since, until, 0.35, 100)
            .await
            .unwrap();
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].id, unclassified.id);

        log.close().await;
    }

    // --- Screenshot policy on rotation (Task B6, spec §4.11) ---

    #[tokio::test]
    async fn rotation_deletes_referenced_screenshots_and_sweeps_the_directory() {
        use crate::senses::screenshots::ScreenshotPolicy;
        use std::time::SystemTime;

        let shots = tempfile::tempdir().unwrap();
        let referenced = shots.path().join("monitor-0/2026-01-01/referenced.jpg");
        let orphan = shots.path().join("monitor-0/2026-01-01/orphan.jpg");
        let recent_orphan = shots.path().join("monitor-0/2026-08-05/recent.jpg");
        for path in [&referenced, &orphan, &recent_orphan] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"jpeg").unwrap();
        }
        // Age both old files past the 720 h backstop.
        for path in [&referenced, &orphan] {
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(SystemTime::now() - std::time::Duration::from_secs(800 * 3600))
                .unwrap();
        }

        let log = RawLog::open("sqlite::memory:").await.unwrap();
        let mut old_frame = test_frame("old frame", false);
        old_frame.ts = Utc::now() - Duration::days(40);
        old_frame.screen.screenshot_path = Some(referenced.to_string_lossy().into_owned());
        log.write_frame(&old_frame).await.unwrap();

        let stats = log
            .rotate_with_policy(30, Some(shots.path()), &ScreenshotPolicy::default())
            .await
            .unwrap();

        assert_eq!(stats.frames_deleted, 1);
        assert_eq!(stats.screenshots_deleted, 1, "row-referenced file deleted");
        assert_eq!(stats.swept.deleted, 1, "the orphan needs the mtime sweep");
        assert!(!referenced.exists());
        assert!(!orphan.exists());
        assert!(recent_orphan.exists(), "fresh files are untouched");

        log.close().await;
    }

    #[tokio::test]
    async fn rotation_keeps_screenshots_when_the_policy_says_so() {
        use crate::senses::screenshots::ScreenshotPolicy;

        let shots = tempfile::tempdir().unwrap();
        let referenced = shots.path().join("monitor-0/old/keep.jpg");
        std::fs::create_dir_all(referenced.parent().unwrap()).unwrap();
        std::fs::write(&referenced, b"jpeg").unwrap();

        let log = RawLog::open("sqlite::memory:").await.unwrap();
        let mut old_frame = test_frame("old frame", false);
        old_frame.ts = Utc::now() - Duration::days(40);
        old_frame.screen.screenshot_path = Some(referenced.to_string_lossy().into_owned());
        log.write_frame(&old_frame).await.unwrap();

        let stats = log
            .rotate_with_policy(30, Some(shots.path()), &ScreenshotPolicy::KEEP_EVERYTHING)
            .await
            .unwrap();

        assert_eq!(stats.frames_deleted, 1, "rows rotate regardless");
        assert_eq!(stats.screenshots_deleted, 0);
        assert_eq!(stats.swept.deleted, 0);
        assert!(referenced.exists(), "policy off keeps the image");

        log.close().await;
    }

    // -----------------------------------------------------------------------
    // Read-only open path (context engine spec §5.2 preamble, Task C2)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_only_open_of_a_missing_database_is_not_yet_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw_log.sqlite");
        let opened = RawLog::open_read_only(&path.to_string_lossy()).await;
        assert!(
            matches!(opened, Err(RawLogError::NotYetCreated { .. })),
            "expected NotYetCreated for a missing database, got {:?}",
            opened.err()
        );
        // The probe must not create the file as a side effect — the MCP
        // process is not allowed to author the runtime's database.
        assert!(!path.exists(), "read-only open created the database file");
    }

    #[tokio::test]
    async fn read_only_open_of_a_schemaless_database_is_not_yet_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.sqlite");
        // A real, valid SQLite file with no Continuum tables in it.
        {
            let opts = SqliteConnectOptions::from_str(&path.to_string_lossy())
                .unwrap()
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .unwrap();
            sqlx::query("CREATE TABLE unrelated (x INTEGER)")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }
        let opened = RawLog::open_read_only(&path.to_string_lossy()).await;
        assert!(
            matches!(opened, Err(RawLogError::NotYetCreated { .. })),
            "expected NotYetCreated for a schemaless database, got {:?}",
            opened.err()
        );
    }

    /// The spec's WAL requirement: the reader opens while the runtime's
    /// writer still holds the database with a live `-wal` file, reads what
    /// the writer committed, and cannot write through its own handle.
    #[tokio::test]
    async fn read_only_open_reads_a_live_wal_database_and_refuses_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw_log.sqlite");
        let db = path.to_string_lossy().into_owned();

        // Writer stays open for the whole test — this is the live-WAL case.
        let writer = RawLog::open(&db).await.unwrap();
        insert_event(&writer, &context_event("build failed", Utc::now()), "wal-1").await;
        writer
            .write_frame(&test_frame("a frame", false))
            .await
            .unwrap();
        assert!(
            path.with_extension("sqlite-wal").exists(),
            "expected a live -wal file next to the database"
        );

        let reader = RawLog::open_read_only(&db).await.expect("read-only open");

        // Reads see the writer's committed rows.
        assert_eq!(reader.context_event_count().await.unwrap(), 1);
        assert_eq!(reader.frame_count().await.unwrap(), 1);
        assert_eq!(
            reader
                .search_context_events("build", 10)
                .await
                .unwrap()
                .len(),
            1,
            "FTS search must work over the read-only handle"
        );

        // Writes are refused by PRAGMA query_only, not merely discouraged.
        let write = sqlx::query("DELETE FROM context_events")
            .execute(&reader.pool)
            .await;
        assert!(write.is_err(), "query_only must reject writes");
        // ... and nothing changed.
        assert_eq!(writer.context_event_count().await.unwrap(), 1);

        // No DDL ran: the reader did not add or alter anything.
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(&reader.pool)
                .await
                .unwrap();
        assert!(tables.iter().any(|t| t == "context_events"));

        reader.close().await;
        writer.close().await;
    }

    // -----------------------------------------------------------------------
    // Filtered event query (Task C4 read path)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn query_context_events_filters_clamps_and_orders() {
        let log = RawLog::open("sqlite::memory:").await.unwrap();
        let now = Utc::now();

        let mk = |summary: &str,
                  ts: DateTime<Utc>,
                  event_type: EventType,
                  source: EventSource,
                  project: Option<&str>,
                  key: &str| {
            let mut event = context_event(summary, ts);
            event.event_type = event_type;
            event.source = source;
            event.project_id = project.map(str::to_owned);
            (event, key.to_string())
        };

        let rows = vec![
            mk(
                "old toggle",
                now - Duration::hours(5),
                EventType::ToggleChange,
                EventSource::System,
                None,
                "a",
            ),
            mk(
                "file saved",
                now - Duration::minutes(30),
                EventType::FileModified,
                EventSource::File,
                Some("continuum"),
                "b",
            ),
            mk(
                "other project file",
                now - Duration::minutes(20),
                EventType::FileModified,
                EventSource::File,
                Some("simcharts"),
                "c",
            ),
            mk(
                "recent toggle",
                now - Duration::minutes(5),
                EventType::ToggleChange,
                EventSource::System,
                Some("continuum"),
                "d",
            ),
        ];
        for (event, key) in &rows {
            insert_event(&log, event, key).await;
        }

        // No filters: everything, oldest first.
        let all = log
            .query_context_events(&EventQuery {
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(all.len(), 4);
        assert!(all[0].ts_last < all[3].ts_last, "must be oldest first");

        // `since` drops the 5-hour-old row.
        let since = log
            .query_context_events(&EventQuery {
                since: Some(now - Duration::hours(1)),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(since.len(), 3);

        // `until` keeps only what started before the cutoff.
        let until = log
            .query_context_events(&EventQuery {
                until: Some(now - Duration::minutes(25)),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(until.len(), 2);

        // Type filter.
        let files = log
            .query_context_events(&EventQuery {
                types: vec![EventType::FileModified],
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(files.len(), 2);
        assert!(files
            .iter()
            .all(|r| r.event_type == EventType::FileModified));

        // Project filter.
        let project = log
            .query_context_events(&EventQuery {
                project: Some("continuum".to_string()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(project.len(), 2);

        // Source filter.
        let system = log
            .query_context_events(&EventQuery {
                source: Some(EventSource::System),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(system.len(), 2);

        // Combined filters AND together.
        let combined = log
            .query_context_events(&EventQuery {
                types: vec![EventType::FileModified],
                project: Some("continuum".to_string()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].summary, "file saved");

        // limit keeps the NEWEST rows, still returned oldest first.
        let limited = log
            .query_context_events(&EventQuery {
                limit: 2,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[1].summary, "recent toggle");

        // limit 0 clamps up to 1; an absurd limit clamps down to the cap.
        assert_eq!(
            log.query_context_events(&EventQuery::default())
                .await
                .unwrap()
                .len(),
            1,
            "limit 0 must clamp to 1, not return nothing"
        );
        let capped = log
            .query_context_events(&EventQuery {
                limit: usize::MAX,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(capped.len(), 4, "cap is an upper bound, not a floor");

        log.close().await;
    }

    #[tokio::test]
    async fn query_context_events_works_over_a_read_only_handle() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir
            .path()
            .join("raw_log.sqlite")
            .to_string_lossy()
            .into_owned();
        let writer = RawLog::open(&db).await.unwrap();
        insert_event(&writer, &context_event("readable", Utc::now()), "ro").await;

        let reader = RawLog::open_read_only(&db).await.expect("read-only open");
        let events = reader
            .query_context_events(&EventQuery {
                limit: 10,
                ..EventQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "readable");

        reader.close().await;
        writer.close().await;
    }
}
