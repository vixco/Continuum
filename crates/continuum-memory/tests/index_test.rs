use std::fs;
use std::path::Path;

use continuum_memory::index::{Index, IndexOutcome};

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn note(id: &str, ty: &str, title: &str, body: &str) -> String {
    format!(
        "---\nid: {id}\ntype: {ty}\ntitle: {title}\ncreated: 2026-08-01T10:00:00Z\n---\n{body}\n"
    )
}

async fn open_index(dir: &Path) -> Index {
    fs::create_dir_all(dir.join(".continuum")).unwrap();
    Index::open(&dir.join(".continuum/index.db")).await.unwrap()
}

#[tokio::test]
async fn rebuild_indexes_notes_and_quarantines_broken() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "facts/alpha.md",
        &note("mem_a", "fact", "Alpha", "links [[Beta]]"),
    );
    write(
        tmp.path(),
        "facts/beta.md",
        &note("mem_b", "fact", "Beta", "no links"),
    );
    write(tmp.path(), "facts/broken.md", "---\nid: [oops\n---\nbody");
    write(tmp.path(), "facts/ignore.tmp", "not markdown");
    let idx = open_index(tmp.path()).await;
    let stats = idx.rebuild(tmp.path()).await.unwrap();
    assert_eq!(stats.indexed, 2);
    assert_eq!(stats.quarantined, 1);
    let n: (i64,) = sqlx::query_as("SELECT count(*) FROM nodes")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(n.0, 2);
    // edge alpha -> beta resolved via title
    let e: (i64,) = sqlx::query_as("SELECT count(*) FROM edges WHERE rel='mentions'")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(e.0, 1);
}

#[tokio::test]
async fn rebuild_is_deterministic() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..20 {
        write(
            tmp.path(),
            &format!("notes/n{i}.md"),
            &note(
                &format!("mem_{i}"),
                "note",
                &format!("Note {i}"),
                "links [[Note 1]] [[Ghost]]",
            ),
        );
    }
    let idx = open_index(tmp.path()).await;
    idx.rebuild(tmp.path()).await.unwrap();
    let rows1: Vec<(String, String)> =
        sqlx::query_as("SELECT from_id, to_id FROM edges ORDER BY from_id, to_id")
            .fetch_all(idx.pool())
            .await
            .unwrap();
    idx.rebuild(tmp.path()).await.unwrap();
    let rows2: Vec<(String, String)> =
        sqlx::query_as("SELECT from_id, to_id FROM edges ORDER BY from_id, to_id")
            .fetch_all(idx.pool())
            .await
            .unwrap();
    assert_eq!(rows1, rows2);
    let g: (i64,) = sqlx::query_as("SELECT count(*) FROM unresolved_links WHERE target='Ghost'")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert!(g.0 >= 1);
}

#[tokio::test]
async fn incremental_matches_full_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "facts/a.md",
        &note("mem_a", "fact", "A", "see [[B]]"),
    );
    let idx = open_index(tmp.path()).await;
    idx.rebuild(tmp.path()).await.unwrap();
    // B appears later — incremental index must resolve A's ghost link.
    write(tmp.path(), "facts/b.md", &note("mem_b", "fact", "B", ""));
    let out = idx
        .index_file(tmp.path(), &tmp.path().join("facts/b.md"))
        .await
        .unwrap();
    assert!(matches!(out, IndexOutcome::Indexed(_)));
    let e: (i64,) =
        sqlx::query_as("SELECT count(*) FROM edges WHERE from_id='mem_a' AND to_id='mem_b'")
            .fetch_one(idx.pool())
            .await
            .unwrap();
    assert_eq!(e.0, 1);
    // deleting B turns the edge back into a ghost
    std::fs::remove_file(tmp.path().join("facts/b.md")).unwrap();
    idx.remove_path(tmp.path(), &tmp.path().join("facts/b.md"))
        .await
        .unwrap();
    let e: (i64,) = sqlx::query_as("SELECT count(*) FROM edges")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(e.0, 0);
    let g: (i64,) = sqlx::query_as("SELECT count(*) FROM unresolved_links")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(g.0, 1);
}

#[tokio::test]
async fn perf_smoke_1000_notes() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..1000 {
        write(
            tmp.path(),
            &format!("notes/n{i}.md"),
            &note(
                &format!("mem_{i}"),
                "note",
                &format!("Note {i}"),
                "body [[Note 0]]",
            ),
        );
    }
    let idx = open_index(tmp.path()).await;
    let t0 = std::time::Instant::now();
    let stats = idx.rebuild(tmp.path()).await.unwrap();
    assert_eq!(stats.indexed, 1000);
    assert!(
        t0.elapsed().as_secs() < 5,
        "rebuild took {:?}",
        t0.elapsed()
    );
}
