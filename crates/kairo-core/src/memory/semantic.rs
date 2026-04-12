//! # Semantic memory
//!
//! SQLite-backed store for stable facts about the user, their projects,
//! relationships, and preferences. Uses a key-value table with confidence
//! scores and a graph structure for relationships between facts.
//!
//! Examples: `user.name`, `project.simcharts.stack`, `routine.morning_start_time`.
//!
//! Part of the three-layer memory system (raw log → episodic → semantic).
//! Semantic memory represents things Kairo "just knows" — stable knowledge
//! that doesn't need to be retrieved from vector search on each wake.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite};
use std::path::Path;
use std::str::FromStr;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The source of a semantic fact — how Kairo learned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactSource {
    /// The user explicitly stated this fact.
    UserStated,
    /// Kairo observed this from perception frames.
    Observed,
    /// Kairo inferred this from patterns in episodic memory.
    Inferred,
}

impl FactSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserStated => "user_stated",
            Self::Observed => "observed",
            Self::Inferred => "inferred",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "user_stated" => Self::UserStated,
            "observed" => Self::Observed,
            "inferred" => Self::Inferred,
            _ => Self::Observed,
        }
    }
}

/// A single semantic fact stored in Kairo's knowledge base.
///
/// Facts are keyed by a dotted path like `user.name` or `project.kairo.stack`.
/// Values are stored as JSON-encoded strings to support any data type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    /// Dotted key path (e.g. `user.name`, `project.simcharts.stack`).
    pub key: String,
    /// JSON-encoded value.
    pub value: String,
    /// Confidence score (0.0 to 1.0). User-stated facts are 1.0, inferred
    /// facts start lower and can be promoted over time.
    pub confidence: f32,
    /// How this fact was learned.
    pub source: FactSource,
    /// The perception frame that triggered this fact (if applicable).
    pub source_frame_id: Option<String>,
    /// When this fact was last updated.
    pub updated_at: DateTime<Utc>,
}

/// A relationship edge between two facts in the knowledge graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    /// Source fact key.
    pub from_key: String,
    /// Target fact key.
    pub to_key: String,
    /// Relationship type (e.g. "owns", "works_on", "prefers", "dislikes").
    pub relation: String,
}

// ---------------------------------------------------------------------------
// Semantic store
// ---------------------------------------------------------------------------

/// Manages the SQLite semantic memory database.
///
/// Provides CRUD operations for facts and edges. Facts are the primary
/// unit — each has a dotted key, a JSON value, a confidence score, and
/// a provenance record (source + source_frame_id).
pub struct SemanticStore {
    pool: Pool<Sqlite>,
}

impl SemanticStore {
    /// Opens (or creates) the semantic memory database at the given path.
    ///
    /// Creates the schema if it doesn't exist.
    pub async fn open(db_path: &str) -> Result<Self> {
        if let Some(parent) = Path::new(db_path).parent() {
            if !db_path.starts_with("sqlite:") || !db_path.contains(":memory:") {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create database directory: {}", parent.display())
                })?;
            }
        }

        let connect_opts = SqliteConnectOptions::from_str(db_path)
            .with_context(|| format!("Invalid database path: {db_path}"))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(connect_opts)
            .await
            .with_context(|| format!("Failed to open semantic database at {db_path}"))?;

        info!(
            layer = "memory",
            component = "semantic",
            db_path = db_path,
            "Semantic memory database opened"
        );

        let store = Self { pool };
        store.create_schema().await?;
        Ok(store)
    }

    /// Creates the database schema if it doesn't already exist.
    async fn create_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS semantic_facts (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                confidence REAL NOT NULL DEFAULT 0.5,
                source TEXT NOT NULL DEFAULT 'observed',
                source_frame_id TEXT,
                updated_at TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create semantic_facts table")?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS semantic_edges (
                from_key TEXT NOT NULL,
                to_key TEXT NOT NULL,
                relation TEXT NOT NULL,
                PRIMARY KEY (from_key, to_key, relation)
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create semantic_edges table")?;

        // Index for querying facts by prefix (e.g. all "project.simcharts.*" facts).
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_facts_key ON semantic_facts(key)")
            .execute(&self.pool)
            .await
            .context("Failed to create key index")?;

        // Index for querying edges by source.
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_edges_from ON semantic_edges(from_key)")
            .execute(&self.pool)
            .await
            .context("Failed to create edge from_key index")?;

        debug!(layer = "memory", component = "semantic", "Schema verified");

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Fact operations
    // -----------------------------------------------------------------------

    /// Inserts or updates a fact. If the key already exists, the value,
    /// confidence, source, and timestamp are overwritten.
    pub async fn upsert_fact(&self, fact: &Fact) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO semantic_facts (key, value, confidence, source, source_frame_id, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                confidence = excluded.confidence,
                source = excluded.source,
                source_frame_id = excluded.source_frame_id,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(&fact.key)
        .bind(&fact.value)
        .bind(fact.confidence as f64)
        .bind(fact.source.as_str())
        .bind(fact.source_frame_id.as_deref())
        .bind(fact.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .context("Failed to upsert fact")?;

        debug!(
            layer = "memory",
            component = "semantic",
            key = %fact.key,
            source = fact.source.as_str(),
            "Upserted fact"
        );

        Ok(())
    }

    /// Retrieves a single fact by exact key.
    pub async fn get_fact(&self, key: &str) -> Result<Option<Fact>> {
        let row = sqlx::query(
            "SELECT key, value, confidence, source, source_frame_id, updated_at FROM semantic_facts WHERE key = ?1",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to query fact")?;

        Ok(row.map(|r| self.row_to_fact(&r)))
    }

    /// Deletes a fact by key. Returns true if a fact was deleted.
    pub async fn delete_fact(&self, key: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM semantic_facts WHERE key = ?1")
            .bind(key)
            .execute(&self.pool)
            .await
            .context("Failed to delete fact")?;

        Ok(result.rows_affected() > 0)
    }

    /// Queries all facts whose key starts with the given prefix.
    ///
    /// For example, `query_facts_by_prefix("project.simcharts")` returns
    /// all facts about the SimCharts project.
    pub async fn query_facts_by_prefix(&self, prefix: &str) -> Result<Vec<Fact>> {
        let like_pattern = format!("{prefix}%");
        let rows = sqlx::query(
            r#"
            SELECT key, value, confidence, source, source_frame_id, updated_at
            FROM semantic_facts
            WHERE key LIKE ?1
            ORDER BY key ASC
            "#,
        )
        .bind(&like_pattern)
        .fetch_all(&self.pool)
        .await
        .context("Failed to query facts by prefix")?;

        Ok(rows.iter().map(|r| self.row_to_fact(r)).collect())
    }

    /// Returns the N most recently updated facts.
    pub async fn list_recent_facts(&self, limit: u32) -> Result<Vec<Fact>> {
        let rows = sqlx::query(
            r#"
            SELECT key, value, confidence, source, source_frame_id, updated_at
            FROM semantic_facts
            ORDER BY updated_at DESC
            LIMIT ?1
            "#,
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list recent facts")?;

        Ok(rows.iter().map(|r| self.row_to_fact(r)).collect())
    }

    /// Returns all facts with confidence above the threshold.
    pub async fn query_facts_by_confidence(&self, min_confidence: f32) -> Result<Vec<Fact>> {
        let rows = sqlx::query(
            r#"
            SELECT key, value, confidence, source, source_frame_id, updated_at
            FROM semantic_facts
            WHERE confidence >= ?1
            ORDER BY confidence DESC
            "#,
        )
        .bind(min_confidence as f64)
        .fetch_all(&self.pool)
        .await
        .context("Failed to query facts by confidence")?;

        Ok(rows.iter().map(|r| self.row_to_fact(r)).collect())
    }

    /// Returns the total number of facts stored.
    pub async fn fact_count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM semantic_facts")
            .fetch_one(&self.pool)
            .await
            .context("Failed to count facts")?;
        Ok(row.get::<i64, _>("cnt"))
    }

    // -----------------------------------------------------------------------
    // Edge operations
    // -----------------------------------------------------------------------

    /// Inserts a relationship edge. Ignored if it already exists.
    pub async fn add_edge(&self, edge: &Edge) -> Result<()> {
        sqlx::query(
            r#"
            INSERT OR IGNORE INTO semantic_edges (from_key, to_key, relation)
            VALUES (?1, ?2, ?3)
            "#,
        )
        .bind(&edge.from_key)
        .bind(&edge.to_key)
        .bind(&edge.relation)
        .execute(&self.pool)
        .await
        .context("Failed to add edge")?;

        Ok(())
    }

    /// Removes a relationship edge. Returns true if an edge was deleted.
    pub async fn remove_edge(&self, from_key: &str, to_key: &str, relation: &str) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM semantic_edges WHERE from_key = ?1 AND to_key = ?2 AND relation = ?3",
        )
        .bind(from_key)
        .bind(to_key)
        .bind(relation)
        .execute(&self.pool)
        .await
        .context("Failed to remove edge")?;

        Ok(result.rows_affected() > 0)
    }

    /// Returns all edges originating from the given key.
    pub async fn edges_from(&self, from_key: &str) -> Result<Vec<Edge>> {
        let rows = sqlx::query(
            "SELECT from_key, to_key, relation FROM semantic_edges WHERE from_key = ?1",
        )
        .bind(from_key)
        .fetch_all(&self.pool)
        .await
        .context("Failed to query edges from key")?;

        Ok(rows
            .iter()
            .map(|r| Edge {
                from_key: r.get("from_key"),
                to_key: r.get("to_key"),
                relation: r.get("relation"),
            })
            .collect())
    }

    /// Returns all edges pointing to the given key.
    pub async fn edges_to(&self, to_key: &str) -> Result<Vec<Edge>> {
        let rows =
            sqlx::query("SELECT from_key, to_key, relation FROM semantic_edges WHERE to_key = ?1")
                .bind(to_key)
                .fetch_all(&self.pool)
                .await
                .context("Failed to query edges to key")?;

        Ok(rows
            .iter()
            .map(|r| Edge {
                from_key: r.get("from_key"),
                to_key: r.get("to_key"),
                relation: r.get("relation"),
            })
            .collect())
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// Closes the database connection pool.
    pub async fn close(&self) {
        self.pool.close().await;
        debug!(
            layer = "memory",
            component = "semantic",
            "Semantic memory database closed"
        );
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn row_to_fact(&self, row: &sqlx::sqlite::SqliteRow) -> Fact {
        let ts_str: String = row.get("updated_at");
        let updated_at = DateTime::parse_from_rfc3339(&ts_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        let source_str: String = row.get("source");

        Fact {
            key: row.get("key"),
            value: row.get("value"),
            confidence: row.get::<f64, _>("confidence") as f32,
            source: FactSource::from_str_lossy(&source_str),
            source_frame_id: row.get("source_frame_id"),
            updated_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fact(key: &str, value: &str, confidence: f32, source: FactSource) -> Fact {
        Fact {
            key: key.to_string(),
            value: value.to_string(),
            confidence,
            source,
            source_frame_id: None,
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_schema_creation() {
        let store = SemanticStore::open("sqlite::memory:").await.unwrap();
        let count = store.fact_count().await.unwrap();
        assert_eq!(count, 0);
        store.close().await;
    }

    #[tokio::test]
    async fn test_upsert_and_get_fact() {
        let store = SemanticStore::open("sqlite::memory:").await.unwrap();

        let fact = make_fact("user.name", "\"Toshan\"", 1.0, FactSource::UserStated);
        store.upsert_fact(&fact).await.unwrap();

        let retrieved = store.get_fact("user.name").await.unwrap().unwrap();
        assert_eq!(retrieved.key, "user.name");
        assert_eq!(retrieved.value, "\"Toshan\"");
        assert_eq!(retrieved.confidence, 1.0);
        assert_eq!(retrieved.source, FactSource::UserStated);

        store.close().await;
    }

    #[tokio::test]
    async fn test_upsert_overwrites_existing() {
        let store = SemanticStore::open("sqlite::memory:").await.unwrap();

        let fact1 = make_fact("user.location", "\"Amsterdam\"", 0.7, FactSource::Inferred);
        store.upsert_fact(&fact1).await.unwrap();

        let fact2 = make_fact("user.location", "\"Breda\"", 1.0, FactSource::UserStated);
        store.upsert_fact(&fact2).await.unwrap();

        let retrieved = store.get_fact("user.location").await.unwrap().unwrap();
        assert_eq!(retrieved.value, "\"Breda\"");
        assert_eq!(retrieved.confidence, 1.0);
        assert_eq!(retrieved.source, FactSource::UserStated);

        // Only one fact should exist.
        assert_eq!(store.fact_count().await.unwrap(), 1);

        store.close().await;
    }

    #[tokio::test]
    async fn test_get_nonexistent_returns_none() {
        let store = SemanticStore::open("sqlite::memory:").await.unwrap();
        let result = store.get_fact("nonexistent.key").await.unwrap();
        assert!(result.is_none());
        store.close().await;
    }

    #[tokio::test]
    async fn test_delete_fact() {
        let store = SemanticStore::open("sqlite::memory:").await.unwrap();

        let fact = make_fact("temp.key", "\"value\"", 0.5, FactSource::Observed);
        store.upsert_fact(&fact).await.unwrap();
        assert_eq!(store.fact_count().await.unwrap(), 1);

        let deleted = store.delete_fact("temp.key").await.unwrap();
        assert!(deleted);
        assert_eq!(store.fact_count().await.unwrap(), 0);

        // Deleting again returns false.
        let deleted_again = store.delete_fact("temp.key").await.unwrap();
        assert!(!deleted_again);

        store.close().await;
    }

    #[tokio::test]
    async fn test_query_by_prefix() {
        let store = SemanticStore::open("sqlite::memory:").await.unwrap();

        store
            .upsert_fact(&make_fact(
                "project.kairo.stack",
                "\"Rust\"",
                1.0,
                FactSource::UserStated,
            ))
            .await
            .unwrap();
        store
            .upsert_fact(&make_fact(
                "project.kairo.repo",
                "\"/kairo-ai\"",
                1.0,
                FactSource::UserStated,
            ))
            .await
            .unwrap();
        store
            .upsert_fact(&make_fact(
                "project.simcharts.stack",
                "\"React\"",
                0.9,
                FactSource::Observed,
            ))
            .await
            .unwrap();
        store
            .upsert_fact(&make_fact(
                "user.name",
                "\"Toshan\"",
                1.0,
                FactSource::UserStated,
            ))
            .await
            .unwrap();

        let kairo_facts = store.query_facts_by_prefix("project.kairo").await.unwrap();
        assert_eq!(kairo_facts.len(), 2);

        let all_project = store.query_facts_by_prefix("project.").await.unwrap();
        assert_eq!(all_project.len(), 3);

        let user_facts = store.query_facts_by_prefix("user.").await.unwrap();
        assert_eq!(user_facts.len(), 1);

        store.close().await;
    }

    #[tokio::test]
    async fn test_list_recent_facts() {
        let store = SemanticStore::open("sqlite::memory:").await.unwrap();

        for i in 0..5 {
            let mut fact = make_fact(
                &format!("test.key{i}"),
                &format!("\"{i}\""),
                0.5,
                FactSource::Observed,
            );
            // Stagger timestamps so ordering is deterministic.
            fact.updated_at = Utc::now() + chrono::Duration::seconds(i as i64);
            store.upsert_fact(&fact).await.unwrap();
        }

        let recent = store.list_recent_facts(3).await.unwrap();
        assert_eq!(recent.len(), 3);
        // Most recent first.
        assert!(recent[0].updated_at >= recent[1].updated_at);
        assert!(recent[1].updated_at >= recent[2].updated_at);

        store.close().await;
    }

    #[tokio::test]
    async fn test_query_by_confidence() {
        let store = SemanticStore::open("sqlite::memory:").await.unwrap();

        store
            .upsert_fact(&make_fact(
                "high.conf",
                "\"yes\"",
                0.95,
                FactSource::UserStated,
            ))
            .await
            .unwrap();
        store
            .upsert_fact(&make_fact(
                "mid.conf",
                "\"maybe\"",
                0.6,
                FactSource::Observed,
            ))
            .await
            .unwrap();
        store
            .upsert_fact(&make_fact(
                "low.conf",
                "\"uncertain\"",
                0.3,
                FactSource::Inferred,
            ))
            .await
            .unwrap();

        let high = store.query_facts_by_confidence(0.8).await.unwrap();
        assert_eq!(high.len(), 1);
        assert_eq!(high[0].key, "high.conf");

        let mid_and_up = store.query_facts_by_confidence(0.5).await.unwrap();
        assert_eq!(mid_and_up.len(), 2);

        store.close().await;
    }

    #[tokio::test]
    async fn test_edge_operations() {
        let store = SemanticStore::open("sqlite::memory:").await.unwrap();

        let edge = Edge {
            from_key: "user.toshan".to_string(),
            to_key: "project.kairo".to_string(),
            relation: "works_on".to_string(),
        };
        store.add_edge(&edge).await.unwrap();

        let edge2 = Edge {
            from_key: "user.toshan".to_string(),
            to_key: "project.simcharts".to_string(),
            relation: "works_on".to_string(),
        };
        store.add_edge(&edge2).await.unwrap();

        // Query edges from user.
        let edges = store.edges_from("user.toshan").await.unwrap();
        assert_eq!(edges.len(), 2);

        // Query edges to project.
        let edges_to = store.edges_to("project.kairo").await.unwrap();
        assert_eq!(edges_to.len(), 1);
        assert_eq!(edges_to[0].from_key, "user.toshan");

        // Remove one edge.
        let removed = store
            .remove_edge("user.toshan", "project.kairo", "works_on")
            .await
            .unwrap();
        assert!(removed);

        let edges_after = store.edges_from("user.toshan").await.unwrap();
        assert_eq!(edges_after.len(), 1);

        store.close().await;
    }

    #[tokio::test]
    async fn test_duplicate_edge_is_ignored() {
        let store = SemanticStore::open("sqlite::memory:").await.unwrap();

        let edge = Edge {
            from_key: "a".to_string(),
            to_key: "b".to_string(),
            relation: "related".to_string(),
        };

        // Insert twice — should not error.
        store.add_edge(&edge).await.unwrap();
        store.add_edge(&edge).await.unwrap();

        let edges = store.edges_from("a").await.unwrap();
        assert_eq!(edges.len(), 1);

        store.close().await;
    }

    #[tokio::test]
    async fn test_fact_with_source_frame_id() {
        let store = SemanticStore::open("sqlite::memory:").await.unwrap();

        let fact = Fact {
            key: "observed.fact".to_string(),
            value: "\"something\"".to_string(),
            confidence: 0.7,
            source: FactSource::Observed,
            source_frame_id: Some("abc-123-def".to_string()),
            updated_at: Utc::now(),
        };
        store.upsert_fact(&fact).await.unwrap();

        let retrieved = store.get_fact("observed.fact").await.unwrap().unwrap();
        assert_eq!(retrieved.source_frame_id.as_deref(), Some("abc-123-def"));

        store.close().await;
    }
}
