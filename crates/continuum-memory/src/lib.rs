//! # continuum-memory
//!
//! The Continuum memory vault. Markdown files on disk are the source of
//! truth (Obsidian-compatible, user-editable); a derived SQLite index
//! provides fast graph/search queries and the event timeline. Both the
//! runtime (layer 2/memory) and the desktop dashboard link this crate and
//! open the same vault directory — cross-process change propagation is
//! the file-watcher, and every write is atomic (tmp + rename).
//!
//! The index is disposable by design: delete `vault/.continuum/index.db`
//! and it is rebuilt from the markdown on next open.

pub mod error;
pub mod frontmatter; // Task 2
pub mod index; // Task 3
pub mod migrate; // Task 8
pub mod model;
pub mod slug; // Task 2
pub mod vault; // Task 5
pub mod watcher; // Task 7

pub use error::{MemoryError, Result};
pub use migrate::migrate_legacy_semantic;
pub use model::*;
pub use vault::{Vault, VaultOptions};
pub use watcher::VaultWatcher;

#[cfg(test)]
mod fts_gate {
    /// The whole design depends on FTS5 being present in sqlx's bundled
    /// SQLite. If this test fails, STOP and escalate — do not work around
    /// it with LIKE queries (spec: "FTS5 must be verified in the first
    /// implementation task").
    #[tokio::test]
    async fn bundled_sqlite_has_fts5() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory sqlite");
        sqlx::query("CREATE VIRTUAL TABLE t USING fts5(body)")
            .execute(&pool)
            .await
            .expect("FTS5 unavailable in bundled SQLite — escalate, do not fall back");
        sqlx::query("INSERT INTO t(body) VALUES ('hello vault world')")
            .execute(&pool)
            .await
            .unwrap();
        let n: (i64,) = sqlx::query_as("SELECT count(*) FROM t WHERE t MATCH 'vault'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n.0, 1);
    }
}
