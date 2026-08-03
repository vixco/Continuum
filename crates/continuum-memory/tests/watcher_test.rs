//! Integration tests for the debounced vault file-watcher (Task 7).

use continuum_memory::Vault;

#[tokio::test]
async fn watcher_reports_external_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let mut w = vault.watch().unwrap();

    std::fs::write(
        tmp.path().join("facts/external.md"),
        "---\nid: mem_ext\ntype: fact\ntitle: External\ncreated: 2026-08-01T10:00:00Z\n---\nwritten outside\n",
    )
    .unwrap();

    // Debounce default is 500 ms; wait up to 10 s for the batch.
    let paths = tokio::time::timeout(std::time::Duration::from_secs(10), w.rx.recv())
        .await
        .expect("watcher timed out")
        .expect("channel closed");
    assert!(paths.iter().any(|p| p.ends_with("external.md")));

    let ids = vault.reindex_paths(&paths).await.unwrap();
    assert!(ids.contains(&"mem_ext".to_string()));
    assert!(vault.get("mem_ext").await.is_ok());
}

#[tokio::test]
async fn watcher_ignores_index_db_churn() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let mut w = vault.watch().unwrap();
    // Touch a file inside .continuum — must NOT produce a batch.
    std::fs::write(tmp.path().join(".continuum/scratch.txt"), "x").unwrap();
    let res = tokio::time::timeout(std::time::Duration::from_millis(1500), w.rx.recv()).await;
    assert!(res.is_err(), "expected no event for .continuum writes");
}
