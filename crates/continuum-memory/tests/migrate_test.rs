use continuum_memory::{migrate_legacy_semantic, Vault};

async fn make_legacy(path: &std::path::Path) {
    let url = format!("sqlite:{}?mode=rwc", path.display());
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE semantic_facts(key TEXT PRIMARY KEY, value TEXT NOT NULL, confidence REAL NOT NULL DEFAULT 0.5, source TEXT NOT NULL DEFAULT 'observed', source_frame_id TEXT, updated_at TEXT NOT NULL)")
        .execute(&pool).await.unwrap();
    sqlx::query("CREATE TABLE semantic_edges(from_key TEXT NOT NULL, to_key TEXT NOT NULL, relation TEXT NOT NULL, PRIMARY KEY(from_key,to_key,relation))")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO semantic_facts VALUES ('user.name', '\"Toshan\"', 1.0, 'user_stated', NULL, '2026-07-01T10:00:00Z')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO semantic_facts VALUES ('project.sidelife.stack', '\"Unity\"', 0.8, 'observed', NULL, '2026-07-02T10:00:00Z')")
        .execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO semantic_edges VALUES ('user.name', 'project.sidelife.stack', 'works_on')",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn migrates_facts_and_edges_idempotently() {
    let tmp = tempfile::tempdir().unwrap();
    let legacy = tmp.path().join("semantic.sqlite");
    make_legacy(&legacy).await;
    let vault_dir = tmp.path().join("vault");
    std::fs::create_dir_all(&vault_dir).unwrap();
    let vault = Vault::open(&vault_dir).await.unwrap();

    let report = migrate_legacy_semantic(&vault, &legacy).await.unwrap();
    assert_eq!(report.migrated, 2);
    assert_eq!(report.skipped, 0);
    assert!(report.errors.is_empty());

    let hits = vault.search("Toshan", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "user: name");
    // source mapped user_stated -> user_statement, confidence carried
    assert!(matches!(
        hits[0].source,
        continuum_memory::Source::UserStatement
    ));
    assert_eq!(hits[0].confidence, 1.0);

    // updated is carried over from the legacy updated_at, not stamped to
    // "now" — otherwise every migrated fact would look freshly changed in
    // the timeline/graph filters.
    let note = vault.get(&hits[0].id).await.unwrap();
    let expected_updated = chrono::DateTime::parse_from_rfc3339("2026-07-01T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert_eq!(note.frontmatter.updated, Some(expected_updated));

    // edge became a typed relation
    let g = vault.graph(&Default::default()).await.unwrap();
    assert_eq!(g.edges.iter().filter(|e| e.rel == "works_on").count(), 1);

    // second run skips everything
    let again = migrate_legacy_semantic(&vault, &legacy).await.unwrap();
    assert_eq!(again.migrated, 0);
    assert_eq!(again.skipped, 2);

    // edge pass 2 must not duplicate the relation on rerun
    let g2 = vault.graph(&Default::default()).await.unwrap();
    assert_eq!(g2.edges.iter().filter(|e| e.rel == "works_on").count(), 1);
}

#[tokio::test]
async fn missing_legacy_db_is_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(&tmp.path().join("v")).await.unwrap();
    assert!(
        migrate_legacy_semantic(&vault, &tmp.path().join("nope.sqlite"))
            .await
            .is_err()
    );
}
