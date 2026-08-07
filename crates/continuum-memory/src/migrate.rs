//! One-shot migration of the legacy `semantic.sqlite` key/value fact store
//! into the memory vault. Layer: memory.
//!
//! Before the vault existed, Continuum's semantic memory was a flat
//! key/value fact table (`semantic_facts`, e.g. `user.name` ->
//! `"Toshan"`) plus a typed-edge table (`semantic_edges`, e.g.
//! `user.name -> project.sidelife.stack` via `works_on`). This module reads
//! both tables **read-only** and re-creates each fact as a `fact` node in
//! the vault, then each edge as a frontmatter [`Relation`] on the migrated
//! `from` note.
//!
//! The migration is idempotent: a fact whose mapped title already exists
//! in the vault is left untouched and counted as `skipped` rather than
//! `migrated`. Re-running the migration after a partial run (or after the
//! vault has since acquired the same facts through normal use) is always
//! safe. The legacy database itself is never written to — every query
//! against it is a `SELECT`, and the connection is opened
//! [`SqliteConnectOptions::read_only`].

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::error::{MemoryError, Result};
use crate::model::{MigrationReport, NodeStatus, NodeType, NoteDraft, Relation, Source};
use crate::vault::Vault;

/// Title mapping for a legacy fact key: only the *first* `.`-separated
/// segment boundary becomes `": "` (`user.name` -> `user: name`,
/// `project.sidelife.stack` -> `project: sidelife.stack`). Using
/// [`str::replacen`] with a count of 1 rather than replacing every dot
/// keeps the mapping stable and reversible-by-eye even for keys with three
/// or more segments.
fn title_from_key(key: &str) -> String {
    key.replacen('.', ": ", 1)
}

/// Tag for a legacy fact key: its first `.`-separated segment
/// (`user.name` -> `user`, `project.sidelife.stack` -> `project`). Keys
/// with no dot at all become their own single-word tag.
fn tag_from_key(key: &str) -> String {
    key.split('.').next().unwrap_or(key).to_string()
}

/// A legacy fact's `value` column is a JSON-encoded scalar (almost always
/// a JSON string literal, e.g. `"Toshan"`). If it parses as a JSON string,
/// the note body becomes the *unwrapped* inner string; otherwise (a JSON
/// number, object, or plain non-JSON text) the raw column value is used
/// verbatim as the body.
fn body_from_value(value: &str) -> String {
    serde_json::from_str::<String>(value).unwrap_or_else(|_| value.to_string())
}

/// Map a legacy `semantic_facts.source` string onto the vault's [`Source`]
/// enum. Unrecognized values (including future/unknown legacy sources)
/// default to [`Source::Observed`] rather than failing the migration.
fn map_source(s: &str) -> Source {
    match s {
        "user_stated" => Source::UserStatement,
        "observed" => Source::Observed,
        "inferred" => Source::Inferred,
        _ => Source::Observed,
    }
}

/// Migrate every row of the legacy `semantic.sqlite` database at
/// `semantic_db` into `vault`.
///
/// # Errors
///
/// Returns [`MemoryError::Invalid`] if `semantic_db` does not exist.
/// Propagates any [`MemoryError::Db`] from the legacy (read-only) or vault
/// index connections, and any error from [`Vault::create`]/[`Vault::save`].
/// A single unparsable `updated_at` timestamp does *not* fail the
/// migration — that fact still migrates, using the current time as its
/// `created`/`updated` and recording a message in
/// [`MigrationReport::errors`].
pub async fn migrate_legacy_semantic(vault: &Vault, semantic_db: &Path) -> Result<MigrationReport> {
    if !semantic_db.exists() {
        return Err(MemoryError::Invalid(format!(
            "legacy semantic db not found at {}",
            semantic_db.display()
        )));
    }

    let opts = SqliteConnectOptions::new()
        .filename(semantic_db)
        .read_only(true);
    let pool = SqlitePoolOptions::new().connect_with(opts).await?;

    let mut report = MigrationReport::default();
    // Legacy key -> migrated (or already-present) vault node id. Populated
    // for every fact that is either migrated this run or found to already
    // exist by title; edges pass 2 only resolves against this map, so an
    // edge touching a key that failed to migrate (impossible today, since
    // fact migration cannot itself fail short of a hard db error) or was
    // simply never present in `semantic_facts` correctly falls through to
    // the "target not migrated" error path.
    let mut key_to_id: HashMap<String, String> = HashMap::new();

    let facts: Vec<(String, String, f64, String, String)> = sqlx::query_as(
        "SELECT key, value, confidence, source, updated_at FROM semantic_facts ORDER BY key",
    )
    .fetch_all(&pool)
    .await?;

    for (key, value, confidence, source, updated_at) in facts {
        let title = title_from_key(&key);

        if let Some(existing_id) = vault.find_by_slug_or_title(&title).await? {
            report.skipped += 1;
            key_to_id.insert(key, existing_id);
            continue;
        }

        let ts = match DateTime::parse_from_rfc3339(&updated_at) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(e) => {
                report.errors.push(format!(
                    "fact {key}: unparsable updated_at '{updated_at}' ({e}), using current time"
                ));
                Utc::now()
            }
        };

        let draft = NoteDraft {
            node_type: NodeType::Fact,
            title,
            body: body_from_value(&value),
            project: None,
            status: NodeStatus::Confirmed,
            confidence: confidence as f32,
            importance: 0.5,
            source: map_source(&source),
            source_ref: Some(format!("legacy:semantic:{key}")),
            sensitivity: Default::default(),
            expires: None,
            relations: vec![],
            tags: vec![tag_from_key(&key)],
        };

        // `NoteDraft` has no `created`/`updated` override, so `create`
        // always stamps `created = now`. Patch the frontmatter with the
        // legacy `updated_at` and save it back — a second, small write
        // under the vault's own write lock — so migrated facts keep their
        // original timestamp instead of "the moment the migration ran".
        // `Vault::save` unconditionally re-stamps `updated = now`, which
        // would make every migrated fact look freshly changed in the
        // timeline/graph filters; `save_preserving_updated` is the
        // migration-only escape hatch that writes `updated` verbatim
        // instead.
        let mut note = vault.create(draft).await?;
        note.frontmatter.created = ts;
        note.frontmatter.updated = Some(ts);
        vault.save_preserving_updated(&note).await?;

        key_to_id.insert(key, note.frontmatter.id.clone());
        report.migrated += 1;
    }

    let edges: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT from_key, to_key, relation FROM semantic_edges \
         ORDER BY from_key, to_key, relation",
    )
    .fetch_all(&pool)
    .await?;

    for (from_key, to_key, relation) in edges {
        match (key_to_id.get(&from_key), key_to_id.get(&to_key)) {
            (Some(from_id), Some(to_id)) => {
                let mut from_note = vault.get(from_id).await?;
                let already_present = from_note
                    .frontmatter
                    .relations
                    .iter()
                    .any(|r| r.to == *to_id && r.rel == relation);
                if !already_present {
                    from_note.frontmatter.relations.push(Relation {
                        to: to_id.clone(),
                        rel: relation,
                        confidence: 0.5,
                    });
                    // Same reasoning as the created/updated patch above:
                    // this write is reconstructing a legacy edge, not
                    // performing a fresh update, so it must not bump
                    // `updated` away from the fact's original timestamp
                    // (the regular `save` would stamp `updated = now` and
                    // clobber the patch already applied in pass 1).
                    vault.save_preserving_updated(&from_note).await?;
                }
            }
            _ => {
                report.errors.push(format!(
                    "edge {from_key}\u{2192}{to_key} skipped: target not migrated"
                ));
            }
        }
    }

    pool.close().await;

    tracing::info!(
        layer = "memory",
        component = "migrate",
        migrated = report.migrated,
        skipped = report.skipped,
        errors = report.errors.len(),
        "legacy semantic migration complete"
    );

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_from_key_replaces_first_dot_only() {
        assert_eq!(title_from_key("user.name"), "user: name");
        assert_eq!(
            title_from_key("project.sidelife.stack"),
            "project: sidelife.stack"
        );
        assert_eq!(title_from_key("singleword"), "singleword");
    }

    #[test]
    fn tag_from_key_is_first_segment() {
        assert_eq!(tag_from_key("user.name"), "user");
        assert_eq!(tag_from_key("project.sidelife.stack"), "project");
        assert_eq!(tag_from_key("singleword"), "singleword");
    }

    #[test]
    fn body_from_value_unwraps_json_string_else_raw() {
        assert_eq!(body_from_value("\"Toshan\""), "Toshan");
        assert_eq!(body_from_value("42"), "42");
        assert_eq!(body_from_value("not json at all"), "not json at all");
    }

    #[test]
    fn map_source_known_and_unknown() {
        assert!(matches!(map_source("user_stated"), Source::UserStatement));
        assert!(matches!(map_source("observed"), Source::Observed));
        assert!(matches!(map_source("inferred"), Source::Inferred));
        assert!(matches!(map_source("something_else"), Source::Observed));
    }
}
