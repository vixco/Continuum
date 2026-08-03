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
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].kind, "error");

    assert_eq!(vault.prune_events(30).await.unwrap(), 1);
    assert_eq!(vault.events(&Default::default()).await.unwrap().len(), 1);
}
