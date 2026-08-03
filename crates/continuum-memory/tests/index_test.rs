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
async fn rebuild_quarantines_slug_collision_deterministically() {
    let tmp = tempfile::tempdir().unwrap();
    // Same file stem ("dup") in two different folders.
    write(
        tmp.path(),
        "facts/dup.md",
        &note("mem_1", "fact", "First", "body"),
    );
    write(
        tmp.path(),
        "notes/dup.md",
        &note("mem_2", "note", "Second", "body"),
    );
    let idx = open_index(tmp.path()).await;

    let stats = idx.rebuild(tmp.path()).await.unwrap();
    assert_eq!(stats.indexed, 1);
    assert_eq!(stats.quarantined, 1);

    // "facts/dup.md" sorts before "notes/dup.md" lexicographically, so it
    // must be the winner; "notes/dup.md" is quarantined with an error that
    // names the occupant path.
    let winner: (String,) = sqlx::query_as("SELECT path FROM nodes WHERE slug = 'dup'")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(winner.0, "facts/dup.md");
    let q: (String, String) = sqlx::query_as("SELECT path, error FROM quarantine")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(q.0, "notes/dup.md");
    assert!(
        q.1.contains("facts/dup.md"),
        "quarantine error should name the occupant path: {}",
        q.1
    );

    // Rebuilding again must produce the exact same winner/loser, not
    // whichever direction the filesystem happens to enumerate them in.
    let stats2 = idx.rebuild(tmp.path()).await.unwrap();
    assert_eq!(stats2.indexed, 1);
    assert_eq!(stats2.quarantined, 1);
    let winner2: (String,) = sqlx::query_as("SELECT path FROM nodes WHERE slug = 'dup'")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(winner2.0, "facts/dup.md");
    let q2: (String,) = sqlx::query_as("SELECT path FROM quarantine")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(q2.0, "notes/dup.md");
}

#[tokio::test]
async fn rewriting_id_in_place_purges_old_links_and_fts_rows() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "facts/note.md",
        &note("mem_old", "fact", "Old Title", "see [[Ghost1]]"),
    );
    let idx = open_index(tmp.path()).await;
    idx.rebuild(tmp.path()).await.unwrap();

    // Rewrite the same path with a brand new id (e.g. a user duplicated a
    // note's frontmatter and forgot to change the id, then fixed it).
    write(
        tmp.path(),
        "facts/note.md",
        &note("mem_new", "fact", "New Title", "see [[Ghost2]]"),
    );
    let out = idx
        .index_file(tmp.path(), &tmp.path().join("facts/note.md"))
        .await
        .unwrap();
    assert!(matches!(out, IndexOutcome::Indexed(ref id) if id == "mem_new"));

    let old_node: (i64,) = sqlx::query_as("SELECT count(*) FROM nodes WHERE id = 'mem_old'")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(old_node.0, 0);
    let old_links: (i64,) = sqlx::query_as("SELECT count(*) FROM links WHERE from_id = 'mem_old'")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(old_links.0, 0, "links under the old id must be purged");
    let old_fts: (i64,) =
        sqlx::query_as("SELECT count(*) FROM nodes_fts WHERE node_id = 'mem_old'")
            .fetch_one(idx.pool())
            .await
            .unwrap();
    assert_eq!(
        old_fts.0, 0,
        "nodes_fts rows under the old id must be purged"
    );

    // The incremental state must equal what a full rebuild would produce.
    let nodes_before: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, path, title FROM nodes ORDER BY id")
            .fetch_all(idx.pool())
            .await
            .unwrap();
    let links_before: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT from_id, target, rel, origin FROM links ORDER BY from_id, target, rel, origin",
    )
    .fetch_all(idx.pool())
    .await
    .unwrap();
    let edges_before: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT from_id, to_id, rel, origin FROM edges ORDER BY from_id, to_id, rel, origin",
    )
    .fetch_all(idx.pool())
    .await
    .unwrap();

    idx.rebuild(tmp.path()).await.unwrap();

    let nodes_after: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, path, title FROM nodes ORDER BY id")
            .fetch_all(idx.pool())
            .await
            .unwrap();
    let links_after: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT from_id, target, rel, origin FROM links ORDER BY from_id, target, rel, origin",
    )
    .fetch_all(idx.pool())
    .await
    .unwrap();
    let edges_after: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT from_id, to_id, rel, origin FROM edges ORDER BY from_id, to_id, rel, origin",
    )
    .fetch_all(idx.pool())
    .await
    .unwrap();

    assert_eq!(nodes_before, nodes_after);
    assert_eq!(links_before, links_after);
    assert_eq!(edges_before, edges_after);
}

#[tokio::test]
async fn duplicate_titles_resolve_deterministically_to_smallest_id() {
    let tmp = tempfile::tempdir().unwrap();
    // Two different notes sharing a title (different slugs); written to
    // disk with the higher id first so filesystem/insertion order can't be
    // the thing making this test pass by accident.
    write(
        tmp.path(),
        "facts/x2.md",
        &note("mem_x2", "fact", "Same Title", "body"),
    );
    write(
        tmp.path(),
        "facts/x1.md",
        &note("mem_x1", "fact", "Same Title", "body"),
    );
    write(
        tmp.path(),
        "facts/linker.md",
        &note("mem_link", "fact", "Linker", "see [[Same Title]]"),
    );
    let idx = open_index(tmp.path()).await;

    idx.rebuild(tmp.path()).await.unwrap();
    let e1: (String,) = sqlx::query_as("SELECT to_id FROM edges WHERE from_id = 'mem_link'")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(e1.0, "mem_x1");

    idx.rebuild(tmp.path()).await.unwrap();
    let e2: (String,) = sqlx::query_as("SELECT to_id FROM edges WHERE from_id = 'mem_link'")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_eq!(e2.0, "mem_x1");
}

#[tokio::test]
async fn search_graph_pending_backlinks() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "projects/sidelife.md",
        &note("mem_p", "project", "SideLife", "the game"),
    );
    write(
        tmp.path(),
        "decisions/lobby.md",
        "---\nid: mem_d\ntype: decision\ntitle: Manual lobby\nstatus: confirmed\nimportance: 0.9\ncreated: 2026-08-01T10:00:00Z\nrelations:\n- to: sidelife\n  rel: belongs_to\n---\nLobbies are [[SideLife]] manual.\n",
    );
    write(
        tmp.path(),
        "facts/cand.md",
        "---\nid: mem_c\ntype: fact\ntitle: Pnpm preferred\nstatus: candidate\ncreated: 2026-08-02T10:00:00Z\n---\nuser prefers pnpm\n",
    );
    let idx = open_index(tmp.path()).await;
    idx.rebuild(tmp.path()).await.unwrap();

    // FTS search hits body text
    let hits = idx.search("pnpm", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "mem_c");
    assert!(hits[0]
        .snippet
        .as_deref()
        .unwrap_or_default()
        .contains("pnpm"));

    // graph default statuses = confirmed + candidate
    let g = idx
        .graph(&continuum_memory::GraphFilter::default(), 1500)
        .await
        .unwrap();
    assert_eq!(g.nodes.len(), 3);
    // typed edge + mention edge both present between d and p
    assert_eq!(
        g.edges
            .iter()
            .filter(|e| e.from == "mem_d" && e.to == "mem_p")
            .count(),
        2
    );

    // type filter
    let g2 = idx
        .graph(
            &continuum_memory::GraphFilter {
                types: Some(vec![continuum_memory::NodeType::Decision]),
                ..Default::default()
            },
            1500,
        )
        .await
        .unwrap();
    assert_eq!(g2.nodes.len(), 1);

    // cap + truncated flag: limit 2 keeps the 2 highest-importance nodes
    let g3 = idx
        .graph(
            &continuum_memory::GraphFilter {
                limit: Some(2),
                ..Default::default()
            },
            1500,
        )
        .await
        .unwrap();
    assert_eq!(g3.nodes.len(), 2);
    assert!(g3.truncated);

    let pend = idx.pending().await.unwrap();
    assert_eq!(pend.len(), 1);
    assert_eq!(pend[0].id, "mem_c");

    let back = idx.backlinks("mem_p").await.unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].id, "mem_d");

    assert_eq!(
        idx.find_by_slug_or_title("sidelife").await.unwrap(),
        Some("mem_p".into())
    );
    assert_eq!(
        idx.find_by_slug_or_title("SIDELIFE").await.unwrap(),
        Some("mem_p".into())
    );
    assert_eq!(idx.find_by_slug_or_title("nope").await.unwrap(), None);
}

#[tokio::test]
async fn neighbors_bfs_depth() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "notes/a.md",
        &note("mem_a", "note", "A", "[[B]]"),
    );
    write(
        tmp.path(),
        "notes/b.md",
        &note("mem_b", "note", "B", "[[C]]"),
    );
    write(tmp.path(), "notes/c.md", &note("mem_c", "note", "C", ""));
    let idx = open_index(tmp.path()).await;
    idx.rebuild(tmp.path()).await.unwrap();
    let g1 = idx.neighbors("mem_a", 1, 1500).await.unwrap();
    assert_eq!(g1.nodes.len(), 2); // a + b
    let g2 = idx.neighbors("mem_a", 2, 1500).await.unwrap();
    assert_eq!(g2.nodes.len(), 3);
}

#[test]
fn fts_query_sanitizes() {
    assert_eq!(
        continuum_memory::index::fts_query("hello world"),
        "\"hello\"* \"world\"*"
    );
    assert_eq!(
        continuum_memory::index::fts_query("a-b (c)"),
        "\"ab\"* \"c\"*"
    );
    assert_eq!(continuum_memory::index::fts_query("  "), "");
}

#[tokio::test]
async fn reindex_skips_unchanged_files() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "facts/a.md",
        &note("mem_a", "fact", "A", "body"),
    );
    let idx = open_index(tmp.path()).await;
    idx.rebuild(tmp.path()).await.unwrap();
    let before: (String,) = sqlx::query_as("SELECT value FROM meta WHERE key='reindex_ops'")
        .fetch_optional(idx.pool())
        .await
        .unwrap()
        .unwrap_or(("0".into(),));
    // Re-index the same unchanged file — must be a no-op (mtime+hash short-circuit).
    idx.index_file(tmp.path(), &tmp.path().join("facts/a.md"))
        .await
        .unwrap();
    let after: (String,) = sqlx::query_as("SELECT value FROM meta WHERE key='reindex_ops'")
        .fetch_optional(idx.pool())
        .await
        .unwrap()
        .unwrap_or(("0".into(),));
    assert_eq!(
        before.0, after.0,
        "unchanged file must not bump reindex_ops"
    );
    // Changing the body must reindex (ops bumps).
    write(
        tmp.path(),
        "facts/a.md",
        &note("mem_a", "fact", "A", "body changed"),
    );
    idx.index_file(tmp.path(), &tmp.path().join("facts/a.md"))
        .await
        .unwrap();
    let after2: (String,) = sqlx::query_as("SELECT value FROM meta WHERE key='reindex_ops'")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_ne!(after.0, after2.0);
}

#[tokio::test]
async fn reindex_skips_unchanged_quarantined_files() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "facts/broken.md", "---\nid: [oops\n---\nbody");
    let idx = open_index(tmp.path()).await;
    let stats = idx.rebuild(tmp.path()).await.unwrap();
    assert_eq!(stats.quarantined, 1);

    let before: (String,) = sqlx::query_as("SELECT value FROM meta WHERE key='reindex_ops'")
        .fetch_optional(idx.pool())
        .await
        .unwrap()
        .unwrap_or(("0".into(),));
    let old_mtime: (i64,) =
        sqlx::query_as("SELECT mtime FROM quarantine WHERE path = 'facts/broken.md'")
            .fetch_one(idx.pool())
            .await
            .unwrap();

    // Re-index the same unchanged broken file — must be a no-op (the
    // quarantine table's mtime short-circuits it, same as the nodes table
    // does for successfully-indexed files).
    let out = idx
        .index_file(tmp.path(), &tmp.path().join("facts/broken.md"))
        .await
        .unwrap();
    assert!(matches!(out, IndexOutcome::Skipped));
    let after: (String,) = sqlx::query_as("SELECT value FROM meta WHERE key='reindex_ops'")
        .fetch_optional(idx.pool())
        .await
        .unwrap()
        .unwrap_or(("0".into(),));
    assert_eq!(
        before.0, after.0,
        "unchanged quarantined file must not bump reindex_ops"
    );

    // Touch the file with different (still broken) content — this must be
    // detected as a change and re-quarantined, bumping reindex_ops and
    // storing the file's new mtime in the quarantine row. Sleep past a
    // whole second first: mtime is second-granularity, so two writes
    // inside the same wall-clock second wouldn't be distinguishable.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(
        tmp.path(),
        "facts/broken.md",
        "---\nid: [still broken\n---\nbody two",
    );
    let out2 = idx
        .index_file(tmp.path(), &tmp.path().join("facts/broken.md"))
        .await
        .unwrap();
    assert!(matches!(out2, IndexOutcome::Quarantined));
    let after2: (String,) = sqlx::query_as("SELECT value FROM meta WHERE key='reindex_ops'")
        .fetch_one(idx.pool())
        .await
        .unwrap();
    assert_ne!(
        after.0, after2.0,
        "changed quarantined file must bump reindex_ops"
    );
    let new_mtime: (i64,) =
        sqlx::query_as("SELECT mtime FROM quarantine WHERE path = 'facts/broken.md'")
            .fetch_one(idx.pool())
            .await
            .unwrap();
    assert!(
        new_mtime.0 > old_mtime.0,
        "quarantine row's mtime must be updated on re-quarantine"
    );
}

#[tokio::test]
async fn uppercase_md_extension_is_indexed() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "facts/upper.MD",
        &note("mem_u", "fact", "Upper", ""),
    );
    let idx = open_index(tmp.path()).await;
    let stats = idx.rebuild(tmp.path()).await.unwrap();
    assert_eq!(stats.indexed, 1);
}

#[tokio::test]
async fn rebuild_is_atomic_for_concurrent_readers() {
    // A second connection must never observe an EMPTY nodes table while a
    // rebuild over a non-empty vault is in flight (old-or-new, never empty).
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..300 {
        write(
            tmp.path(),
            &format!("notes/n{i}.md"),
            &note(&format!("mem_{i}"), "note", &format!("N {i}"), "x"),
        );
    }
    let idx = std::sync::Arc::new(open_index(tmp.path()).await);
    idx.rebuild(tmp.path()).await.unwrap();
    let reader = {
        let db = tmp.path().join(".continuum/index.db");
        tokio::spawn(async move {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .connect(&format!("sqlite:{}", db.display()))
                .await
                .unwrap();
            let mut min_seen = i64::MAX;
            for _ in 0..200 {
                let n: (i64,) = sqlx::query_as("SELECT count(*) FROM nodes")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                min_seen = min_seen.min(n.0);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            min_seen
        })
    };
    let idx2 = idx.clone();
    let root = tmp.path().to_path_buf();
    let rebuilder = tokio::spawn(async move {
        for _ in 0..3 {
            idx2.rebuild(&root).await.unwrap();
        }
    });
    rebuilder.await.unwrap();
    let min_seen = reader.await.unwrap();
    assert!(
        min_seen > 0,
        "reader observed an empty nodes table mid-rebuild"
    );
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
