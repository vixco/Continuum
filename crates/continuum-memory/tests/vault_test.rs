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
