use continuum_memory::{NodeStatus, NodeType, NoteDraft, Resolution, Vault};

fn draft(ty: NodeType, title: &str, body: &str) -> NoteDraft {
    NoteDraft {
        node_type: ty,
        title: title.into(),
        body: body.into(),
        project: None,
        status: NodeStatus::Confirmed,
        confidence: 0.5,
        importance: 0.5,
        source: Default::default(),
        source_ref: None,
        sensitivity: Default::default(),
        expires: None,
        relations: vec![],
        tags: vec![],
    }
}

#[tokio::test]
async fn create_get_save_delete_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let note = vault
        .create(draft(NodeType::Decision, "Manual lobby", "Body [[Ghost]]"))
        .await
        .unwrap();
    assert!(note.frontmatter.id.starts_with("mem_"));
    assert_eq!(note.slug, "manual-lobby");
    assert!(tmp.path().join("decisions/manual-lobby.md").exists());
    // no stray tmp files
    assert!(std::fs::read_dir(tmp.path().join("decisions"))
        .unwrap()
        .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));

    let mut got = vault.get(&note.frontmatter.id).await.unwrap();
    assert_eq!(got.frontmatter.title, "Manual lobby");
    got.body = "New body".into();
    got.frontmatter.importance = 0.9;
    vault.save(&got).await.unwrap();
    let again = vault.get(&note.frontmatter.id).await.unwrap();
    assert_eq!(again.body.trim(), "New body");
    assert!(again.frontmatter.updated.is_some());

    vault.delete(&note.frontmatter.id).await.unwrap();
    assert!(!tmp.path().join("decisions/manual-lobby.md").exists());
    assert!(vault.get(&note.frontmatter.id).await.is_err());
}

#[tokio::test]
async fn slug_collision_appends_counter() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let a = vault
        .create(draft(NodeType::Fact, "Same Title", ""))
        .await
        .unwrap();
    let b = vault
        .create(draft(NodeType::Fact, "Same Title", ""))
        .await
        .unwrap();
    assert_eq!(a.slug, "same-title");
    assert_eq!(b.slug, "same-title-2");
}

#[tokio::test]
async fn backlinks_are_populated_on_get() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let target = vault
        .create(draft(NodeType::Project, "SideLife", ""))
        .await
        .unwrap();
    vault
        .create(draft(NodeType::Decision, "D1", "about [[SideLife]]"))
        .await
        .unwrap();
    let got = vault.get(&target.frontmatter.id).await.unwrap();
    assert_eq!(got.backlinks.len(), 1);
    assert_eq!(got.backlinks[0].title, "D1");
}

#[tokio::test]
async fn resolve_candidate_confirm_reject_supersede() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let old = vault
        .create(draft(NodeType::Decision, "Use MongoDB", ""))
        .await
        .unwrap();
    let mut cand = draft(NodeType::Decision, "Use PostgreSQL", "");
    cand.status = NodeStatus::Candidate;
    let new = vault.create(cand).await.unwrap();

    vault
        .resolve_candidate(
            &new.frontmatter.id,
            Resolution::Supersede {
                replaces: old.frontmatter.id.clone(),
            },
        )
        .await
        .unwrap();
    let new2 = vault.get(&new.frontmatter.id).await.unwrap();
    let old2 = vault.get(&old.frontmatter.id).await.unwrap();
    assert_eq!(new2.frontmatter.status, NodeStatus::Confirmed);
    assert_eq!(
        new2.frontmatter.supersedes.as_deref(),
        Some(old.frontmatter.id.as_str())
    );
    assert_eq!(old2.frontmatter.status, NodeStatus::Superseded);
    assert_eq!(
        old2.frontmatter.superseded_by.as_deref(),
        Some(new.frontmatter.id.as_str())
    );

    // reject path
    let mut c2 = draft(NodeType::Fact, "Maybe wrong", "");
    c2.status = NodeStatus::Candidate;
    let c2 = vault.create(c2).await.unwrap();
    vault
        .resolve_candidate(&c2.frontmatter.id, Resolution::Reject)
        .await
        .unwrap();
    assert_eq!(
        vault
            .get(&c2.frontmatter.id)
            .await
            .unwrap()
            .frontmatter
            .status,
        NodeStatus::Rejected
    );
    // resolving a non-candidate errors
    assert!(vault
        .resolve_candidate(&old.frontmatter.id, Resolution::Confirm)
        .await
        .is_err());
}

#[tokio::test]
async fn sweep_expired_archives() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let n = vault
        .create(draft(NodeType::Fact, "Old", ""))
        .await
        .unwrap();
    let mut note = vault.get(&n.frontmatter.id).await.unwrap();
    note.frontmatter.expires = Some(chrono::Utc::now() - chrono::Duration::days(1));
    vault.save(&note).await.unwrap();
    assert_eq!(vault.sweep_expired().await.unwrap(), 1);
    assert_eq!(
        vault
            .get(&n.frontmatter.id)
            .await
            .unwrap()
            .frontmatter
            .status,
        NodeStatus::Archived
    );
}

#[tokio::test]
async fn traversal_and_bad_input_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let mut d = draft(NodeType::Fact, "..", "");
    d.title = "../../escape".into();
    let n = vault.create(d).await.unwrap();
    // slugify strips the dots — file stays inside the vault
    assert!(n.path.starts_with(tmp.path()));
    assert!(vault.get("mem_does_not_exist").await.is_err());
    let mut e = draft(NodeType::Fact, "", "");
    e.title = "   ".into();
    assert!(vault.create(e).await.is_err());
}

#[tokio::test]
async fn info_reports_counts_and_quarantine() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    vault.create(draft(NodeType::Fact, "Ok", "")).await.unwrap();
    std::fs::write(tmp.path().join("facts/broken.md"), "---\nid: [x\n---\n").unwrap();
    vault.rebuild_index().await.unwrap();
    let info = vault.info().await.unwrap();
    assert_eq!(info.note_count, 1);
    assert_eq!(info.quarantined.len(), 1);
    assert_eq!(info.counts_by_status.get("confirmed"), Some(&1));
}

/// Regression test for the concurrent-create data-destruction bug: two
/// callers racing to create a note with the same title must never both
/// compute the same slug and overwrite each other's file. Before
/// `Vault::write_lock` was added, both calls could pass the
/// slug-uniqueness check before either had written, then race to
/// atomically write the same path — the second write silently destroyed
/// the first note (the index read it back as an id-rewrite of the same
/// path and purged the first note's rows, with no error surfaced
/// anywhere).
///
/// Uses a real multi-threaded runtime plus a barrier so the two `create`
/// calls actually start at the same instant on different OS threads —
/// `tokio::join!` on the default single-threaded runtime was observed
/// *not* to reproduce the race (the local sqlx/SQLite driver resolves
/// quickly enough that the two futures never truly interleave under
/// cooperative single-thread polling), so this is the version that
/// meaningfully exercises `write_lock`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_same_title_does_not_collide() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = std::sync::Arc::new(Vault::open(tmp.path()).await.unwrap());
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

    let mut handles = Vec::new();
    for _ in 0..2 {
        let vault = vault.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            vault.create(draft(NodeType::Fact, "Same Title", "")).await
        }));
    }
    let mut notes = Vec::new();
    for h in handles {
        notes.push(h.await.unwrap().unwrap());
    }
    let (a, b) = (notes.remove(0), notes.remove(0));

    assert_ne!(a.frontmatter.id, b.frontmatter.id);
    let mut slugs = vec![a.slug.clone(), b.slug.clone()];
    slugs.sort();
    assert_eq!(
        slugs,
        vec!["same-title".to_string(), "same-title-2".to_string()]
    );

    // Both files must exist on disk — neither write clobbered the other.
    assert!(tmp.path().join("facts/same-title.md").exists());
    assert!(tmp.path().join("facts/same-title-2.md").exists());

    // Both notes must be independently readable...
    assert!(vault.get(&a.frontmatter.id).await.is_ok());
    assert!(vault.get(&b.frontmatter.id).await.is_ok());

    // ...and both present in the graph (not one purged as a stale id).
    let g = vault
        .graph(&continuum_memory::GraphFilter::default())
        .await
        .unwrap();
    assert_eq!(
        g.nodes.iter().filter(|n| n.title == "Same Title").count(),
        2
    );
}

#[tokio::test]
async fn resolve_candidate_supersede_rejects_self_reference() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let mut cand = draft(NodeType::Decision, "Self Superseding", "");
    cand.status = NodeStatus::Candidate;
    let note = vault.create(cand).await.unwrap();

    let err = vault
        .resolve_candidate(
            &note.frontmatter.id,
            Resolution::Supersede {
                replaces: note.frontmatter.id.clone(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, continuum_memory::MemoryError::Invalid(_)));

    // No write happened: the candidate is untouched.
    let again = vault.get(&note.frontmatter.id).await.unwrap();
    assert_eq!(again.frontmatter.status, NodeStatus::Candidate);
    assert!(again.frontmatter.supersedes.is_none());
}

/// Regression test for the supersede write ordering fix: the partner is
/// now written first, then the candidate, so a crash/error between the two
/// leaves the candidate still pending (visible, retriable) rather than
/// confirmed-but-orphaned. This simulates exactly that partial state (as
/// if a prior `resolve_candidate(Supersede)` call had completed only the
/// partner write) and asserts the retried call completes cleanly — in
/// particular, re-writing an already-superseded partner must not error.
#[tokio::test]
async fn resolve_candidate_supersede_retry_after_partial_failure_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let old = vault
        .create(draft(NodeType::Decision, "Use MongoDB", ""))
        .await
        .unwrap();
    let mut cand = draft(NodeType::Decision, "Use PostgreSQL", "");
    cand.status = NodeStatus::Candidate;
    let new = vault.create(cand).await.unwrap();

    // Simulate a crash between the two saves of a prior resolve_candidate
    // call: the partner write landed, the candidate write did not.
    let mut partner = vault.get(&old.frontmatter.id).await.unwrap();
    partner.frontmatter.status = NodeStatus::Superseded;
    partner.frontmatter.superseded_by = Some(new.frontmatter.id.clone());
    vault.save(&partner).await.unwrap();

    // The candidate is still pending (status untouched by the simulated
    // partial failure) — a retried call must complete cleanly.
    vault
        .resolve_candidate(
            &new.frontmatter.id,
            Resolution::Supersede {
                replaces: old.frontmatter.id.clone(),
            },
        )
        .await
        .unwrap();

    let new2 = vault.get(&new.frontmatter.id).await.unwrap();
    let old2 = vault.get(&old.frontmatter.id).await.unwrap();
    assert_eq!(new2.frontmatter.status, NodeStatus::Confirmed);
    assert_eq!(
        new2.frontmatter.supersedes.as_deref(),
        Some(old.frontmatter.id.as_str())
    );
    assert_eq!(old2.frontmatter.status, NodeStatus::Superseded);
    assert_eq!(
        old2.frontmatter.superseded_by.as_deref(),
        Some(new.frontmatter.id.as_str())
    );
}

#[tokio::test]
async fn events_append_query_prune() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    let old = chrono::Utc::now() - chrono::Duration::days(40);
    vault
        .append_event(continuum_memory::NewEvent {
            ts: Some(old),
            kind: "build".into(),
            text: "old build".into(),
            project: None,
            node_id: None,
            reference: None,
            local_only: false,
        })
        .await
        .unwrap();
    vault
        .append_event(continuum_memory::NewEvent {
            ts: None,
            kind: "error".into(),
            text: "fresh error".into(),
            project: Some("sidelife".into()),
            node_id: None,
            reference: None,
            local_only: false,
        })
        .await
        .unwrap();

    let all = vault
        .events(&continuum_memory::EventRange::default())
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].text, "old build"); // ascending

    let recent = vault
        .events(&continuum_memory::EventRange {
            since: Some(chrono::Utc::now() - chrono::Duration::days(1)),
            until: None,
            since_id: None,
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].kind, "error");

    assert_eq!(vault.prune_events(30).await.unwrap(), 1);
    assert_eq!(vault.events(&Default::default()).await.unwrap().len(), 1);
}

/// Regression for the C1 fix: `since_id` is an id watermark, independent
/// of `ts` — an event's (possibly backdated) timestamp must never affect
/// whether `since_id` includes it, only its `id` (insertion order) does.
#[tokio::test]
async fn events_since_id_watermark_is_id_based_not_ts_based() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();

    // Inserted first (lowest id) but with a *newer* ts than the second
    // event — proves the filter keys off id, not ts.
    vault
        .append_event(continuum_memory::NewEvent {
            ts: Some(chrono::Utc::now()),
            kind: "first".into(),
            text: "first inserted, newest ts".into(),
            project: None,
            node_id: None,
            reference: None,
            local_only: false,
        })
        .await
        .unwrap();
    vault
        .append_event(continuum_memory::NewEvent {
            ts: Some(chrono::Utc::now() - chrono::Duration::days(10)),
            kind: "second".into(),
            text: "second inserted, backdated ts".into(),
            project: None,
            node_id: None,
            reference: None,
            local_only: false,
        })
        .await
        .unwrap();

    let all = vault
        .events(&continuum_memory::EventRange::default())
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    let first_id = all.iter().find(|e| e.kind == "first").unwrap().id;

    // since_id excludes the first (id == first_id) but includes the
    // second (id > first_id), even though the second's ts is far older —
    // a ts-based `since` filter would have excluded it instead.
    let watermarked = vault
        .events(&continuum_memory::EventRange {
            since: None,
            until: None,
            since_id: Some(first_id),
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(watermarked.len(), 1);
    assert_eq!(watermarked[0].kind, "second");
}

/// Regression for the "ts-ordered LIMIT under since_id skips a lower id"
/// data-loss bug found in the scoped re-review of the watermark fix:
/// `Vault::events()` used to always `ORDER BY ts ASC`, even when
/// `since_id` was set. Under a `LIMIT`, that let a *higher* id sort ahead
/// of a *lower* one whenever the lower id's `ts` was backdated later than
/// the higher id's — so a `since_id`-polling caller (the curator's
/// watermark) that advances to the fetched batch's max id would
/// permanently skip the excluded lower id, never seeing it again on any
/// future poll (every future `since_id` is now past it too).
///
/// Exact repro: three events inserted in id order 1, 2, 3, but event 3's
/// `ts` is backdated *before* event 1's `ts` (1: ts=T, 2: ts=T+1m, 3:
/// ts=T-1h). Under the old `ORDER BY ts ASC LIMIT 2`, the two
/// earliest-by-ts rows are 3 (T-1h) and 1 (T) — id 2 (the true "second
/// oldest by id past the watermark") is silently excluded. With the fix
/// (`ORDER BY id ASC` whenever `since_id` is set), `since_id=0, limit=2`
/// must return exactly ids `[1, 2]`, not `[3, 1]`.
#[tokio::test]
async fn events_since_id_orders_by_id_not_ts_under_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();

    let base = chrono::Utc::now();

    // id 1: ts = T
    vault
        .append_event(continuum_memory::NewEvent {
            ts: Some(base),
            kind: "one".into(),
            text: "id 1, ts=T".into(),
            project: None,
            node_id: None,
            reference: None,
            local_only: false,
        })
        .await
        .unwrap();
    // id 2: ts = T + 1m
    vault
        .append_event(continuum_memory::NewEvent {
            ts: Some(base + chrono::Duration::minutes(1)),
            kind: "two".into(),
            text: "id 2, ts=T+1m".into(),
            project: None,
            node_id: None,
            reference: None,
            local_only: false,
        })
        .await
        .unwrap();
    // id 3: ts = T - 1h (backdated behind both 1 and 2, e.g. a late
    // distiller write).
    vault
        .append_event(continuum_memory::NewEvent {
            ts: Some(base - chrono::Duration::hours(1)),
            kind: "three".into(),
            text: "id 3, ts=T-1h".into(),
            project: None,
            node_id: None,
            reference: None,
            local_only: false,
        })
        .await
        .unwrap();

    let all = vault
        .events(&continuum_memory::EventRange::default())
        .await
        .unwrap();
    assert_eq!(all.len(), 3);
    let id1 = all.iter().find(|e| e.kind == "one").unwrap().id;
    let id2 = all.iter().find(|e| e.kind == "two").unwrap().id;
    let id3 = all.iter().find(|e| e.kind == "three").unwrap().id;
    assert!(id1 < id2 && id2 < id3, "ids assigned in insertion order");

    let batch = vault
        .events(&continuum_memory::EventRange {
            since: None,
            until: None,
            since_id: Some(0),
            limit: Some(2),
        })
        .await
        .unwrap();

    let batch_ids: Vec<i64> = batch.iter().map(|e| e.id).collect();
    assert_eq!(
        batch_ids,
        vec![id1, id2],
        "must fetch the two lowest ids past the watermark (contiguous by id), \
         not the two ts-earliest rows (which would wrongly skip id 2)"
    );
}

/// Regression test for the corrupt-index fail-safe in `Vault::open_with`:
/// a note is created via a first vault instance, `.continuum/index.db` is
/// then overwritten with garbage bytes (simulating a crash mid-write, a
/// full-disk truncation, or any other on-disk corruption), and reopening
/// the vault must recover by deleting and rebuilding the index rather than
/// propagating the open error and stranding the user out of their own
/// vault — the markdown files are the real source of truth, so the index
/// must always be disposable.
#[tokio::test]
async fn open_recovers_from_corrupt_index_db() {
    let tmp = tempfile::tempdir().unwrap();
    let id = {
        let vault = Vault::open(tmp.path()).await.unwrap();
        let note = vault
            .create(draft(NodeType::Fact, "Survives Corruption", "body text"))
            .await
            .unwrap();
        note.frontmatter.id
    };

    let index_path = tmp.path().join(".continuum/index.db");
    assert!(index_path.exists());
    std::fs::write(&index_path, b"not a sqlite database, just garbage bytes").unwrap();

    // Reopening must not propagate the corruption.
    let vault = Vault::open(tmp.path()).await.unwrap();
    let note = vault.get(&id).await.unwrap();
    assert_eq!(note.frontmatter.title, "Survives Corruption");
    // And the note must be reachable through the rebuilt index's search
    // path too, not just direct id lookup by path.
    let found = vault.search("Survives Corruption", 10).await.unwrap();
    assert_eq!(found.len(), 1);
}

/// Regression test for the `reindex_paths` batch-recompute fix: indexing a
/// multi-file batch through `reindex_paths` (one `recompute_edges` call
/// for the whole batch) must yield the exact same resolved edge set as a
/// full `rebuild_index` over the same files (one `recompute_edges` call
/// per file during rebuild's own indexing, then a final one) — the batch
/// path only changes *how many times* edges are recomputed, never the
/// resulting graph.
#[tokio::test]
async fn reindex_paths_batch_matches_full_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();

    let a_path = tmp.path().join("facts/a.md");
    let b_path = tmp.path().join("facts/b.md");
    let c_path = tmp.path().join("facts/c.md");
    std::fs::write(
        &a_path,
        "---\nid: mem_a\ntype: fact\ntitle: A\ncreated: 2026-08-01T10:00:00Z\n---\nlinks to [[B]]\n",
    )
    .unwrap();
    std::fs::write(
        &b_path,
        "---\nid: mem_b\ntype: fact\ntitle: B\ncreated: 2026-08-01T10:00:00Z\n---\nlinks to [[C]]\n",
    )
    .unwrap();
    std::fs::write(
        &c_path,
        "---\nid: mem_c\ntype: fact\ntitle: C\ncreated: 2026-08-01T10:00:00Z\n---\nlinks to [[A]]\n",
    )
    .unwrap();

    let mut ids = vault
        .reindex_paths(&[a_path, b_path, c_path])
        .await
        .unwrap();
    ids.sort();
    assert_eq!(ids, vec!["mem_a", "mem_b", "mem_c"]);

    let batch_graph = vault
        .graph(&continuum_memory::GraphFilter::default())
        .await
        .unwrap();

    // A full rebuild from the same files on disk must resolve to the same
    // edges.
    vault.rebuild_index().await.unwrap();
    let rebuilt_graph = vault
        .graph(&continuum_memory::GraphFilter::default())
        .await
        .unwrap();

    let edge_key = |e: &continuum_memory::GraphEdge| (e.from.clone(), e.to.clone(), e.rel.clone());
    let mut batch_edges: Vec<_> = batch_graph.edges.iter().map(edge_key).collect();
    let mut rebuilt_edges: Vec<_> = rebuilt_graph.edges.iter().map(edge_key).collect();
    batch_edges.sort();
    rebuilt_edges.sort();

    assert_eq!(batch_edges.len(), 3);
    assert_eq!(batch_edges, rebuilt_edges);
}

/// Fixwave 3b (I6). "The newest session note" must be found by a targeted
/// query, not by paging `graph()`.
///
/// `graph()` orders by `importance DESC, id ASC` and caps at its limit, and
/// every curator session note carries the same `importance: 0.5` — so the
/// page is simply the N lowest-id (oldest) notes. Past that many sessions
/// the newest note was never in it, and the §4.12 continuation resolver
/// kept recommending a months-old open task forever.
#[tokio::test]
async fn newest_session_note_is_found_past_the_graph_page_size() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();

    let mut last_id = String::new();
    for i in 0..60 {
        let note = vault
            .create(draft(
                NodeType::Session,
                &format!("Session {i:03}"),
                &format!("open_task: task {i}"),
            ))
            .await
            .unwrap();
        last_id = note.frontmatter.id;
    }

    let newest = vault
        .newest_node(NodeType::Session, NodeStatus::Confirmed)
        .await
        .unwrap()
        .expect("a session note exists");
    assert_eq!(
        newest.id, last_id,
        "the newest note must be the last created"
    );
    assert_eq!(newest.title, "Session 059");

    // The old path: a 50-node importance-ordered page cannot see it.
    let page = vault
        .graph(&continuum_memory::GraphFilter {
            types: Some(vec![NodeType::Session]),
            statuses: Some(vec![NodeStatus::Confirmed]),
            limit: Some(50),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.nodes.len(), 50);
    assert!(
        !page.nodes.iter().any(|n| n.id == last_id),
        "the regression: the newest note is not in the page at all"
    );
}

/// An empty vault yields `None`, never an error.
#[tokio::test]
async fn newest_node_on_an_empty_vault_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    let vault = Vault::open(tmp.path()).await.unwrap();
    assert!(vault
        .newest_node(NodeType::Session, NodeStatus::Confirmed)
        .await
        .unwrap()
        .is_none());
}
