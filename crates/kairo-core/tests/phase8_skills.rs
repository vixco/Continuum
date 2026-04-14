//! # Phase 8 — Skills integration tests
//!
//! Load the real bundled skills from `<repo>/skills/` and verify the matcher
//! lights up for the cases they advertise. Tests are read-only on the
//! repository — they do not mutate the skills/ directory.

use kairo_core::skills::{MatchContext, SkillLoader, SkillMatcher};

fn repo_skills_root() -> std::path::PathBuf {
    // The workspace root is the parent of `crates/`.
    let mut p = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    // Walk up until we find a `skills/` sibling.
    for _ in 0..4 {
        if p.join("skills").exists() {
            return p.join("skills");
        }
        if !p.pop() {
            break;
        }
    }
    std::path::PathBuf::from("skills")
}

#[test]
fn bundled_skills_load_without_errors() {
    let root = repo_skills_root();
    let loader = SkillLoader::new(&root);
    loader.reload().unwrap();
    let errors = loader.errors();
    assert!(
        errors.is_empty(),
        "some bundled skills failed to parse: {errors:?}"
    );
    let list = loader.list();
    assert!(!list.is_empty(), "no skills found under {}", root.display());
    for s in &list {
        assert!(!s.frontmatter.name.is_empty());
        assert!(!s.frontmatter.description.is_empty());
        assert!(
            s.body.len() > 50,
            "skill body too short: {}",
            s.frontmatter.name
        );
    }
}

#[test]
fn matcher_picks_code_review_for_pr_request() {
    let loader = SkillLoader::new(repo_skills_root());
    loader.reload().unwrap();
    let ctx = MatchContext::from_wake("user wants a review of the auth PR");
    let matched = SkillMatcher::match_skills(&loader.enabled(), &ctx, 2000);
    let names: Vec<_> = matched
        .iter()
        .map(|(s, _)| s.frontmatter.name.clone())
        .collect();
    assert!(
        names.iter().any(|n| n == "code-review"),
        "expected code-review in {names:?}"
    );
}

#[test]
fn matcher_picks_daily_briefing_for_morning_question() {
    let loader = SkillLoader::new(repo_skills_root());
    loader.reload().unwrap();
    let ctx = MatchContext::from_wake("can you give me the morning briefing");
    let matched = SkillMatcher::match_skills(&loader.enabled(), &ctx, 2000);
    let names: Vec<_> = matched
        .iter()
        .map(|(s, _)| s.frontmatter.name.clone())
        .collect();
    assert!(
        names.iter().any(|n| n == "daily-briefing"),
        "expected daily-briefing in {names:?}"
    );
}

#[test]
fn matcher_picks_project_context_by_project_name() {
    let loader = SkillLoader::new(repo_skills_root());
    loader.reload().unwrap();
    let ctx = MatchContext::from_wake("what's happening with simcharts");
    let matched = SkillMatcher::match_skills(&loader.enabled(), &ctx, 2000);
    let names: Vec<_> = matched
        .iter()
        .map(|(s, _)| s.frontmatter.name.clone())
        .collect();
    assert!(
        names.iter().any(|n| n == "project-context"),
        "expected project-context in {names:?}"
    );
}

#[test]
fn matcher_picks_email_draft_for_reply_request() {
    let loader = SkillLoader::new(repo_skills_root());
    loader.reload().unwrap();
    let ctx = MatchContext::from_wake("draft a reply to this email");
    let matched = SkillMatcher::match_skills(&loader.enabled(), &ctx, 2000);
    let names: Vec<_> = matched
        .iter()
        .map(|(s, _)| s.frontmatter.name.clone())
        .collect();
    assert!(
        names.iter().any(|n| n == "email-draft"),
        "expected email-draft in {names:?}"
    );
}

#[test]
fn matcher_picks_file_organizer_for_cleanup_request() {
    let loader = SkillLoader::new(repo_skills_root());
    loader.reload().unwrap();
    let ctx = MatchContext::from_wake("organize my downloads folder");
    let matched = SkillMatcher::match_skills(&loader.enabled(), &ctx, 2000);
    let names: Vec<_> = matched
        .iter()
        .map(|(s, _)| s.frontmatter.name.clone())
        .collect();
    assert!(
        names.iter().any(|n| n == "file-organizer"),
        "expected file-organizer in {names:?}"
    );
}

#[test]
fn render_prompt_emits_combined_text() {
    let loader = SkillLoader::new(repo_skills_root());
    loader.reload().unwrap();
    let ctx = MatchContext::from_wake("review the PR and summarise what's today");
    let (prompt, names) = SkillMatcher::render_prompt(&loader.enabled(), &ctx, 4000);
    assert!(!prompt.is_empty());
    assert!(!names.is_empty());
    assert!(prompt.starts_with("# Active skills"));
}

#[test]
fn forced_skill_overrides_budget() {
    let loader = SkillLoader::new(repo_skills_root());
    loader.reload().unwrap();
    let ctx = MatchContext {
        wake_reason: Some("something totally unrelated".into()),
        forced: vec!["daily-briefing".into()],
        ..Default::default()
    };
    let matched = SkillMatcher::match_skills(&loader.enabled(), &ctx, 100);
    let names: Vec<_> = matched
        .iter()
        .map(|(s, _)| s.frontmatter.name.clone())
        .collect();
    assert!(names.contains(&"daily-briefing".to_string()));
}
