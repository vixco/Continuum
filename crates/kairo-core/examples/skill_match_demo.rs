//! # skill_match_demo — load skills and match against a wake reason
//!
//! ```bash
//! cargo run --example skill_match_demo -p kairo-core -- "review the PR for the auth module"
//! ```

use std::path::PathBuf;

use kairo_core::skills::{MatchContext, SkillLoader, SkillMatcher};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    let reason = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let reason = if reason.trim().is_empty() {
        "user asked for a morning briefing".to_string()
    } else {
        reason
    };
    println!("Wake reason: {reason}\n");

    let root = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("skills");
    let loader = SkillLoader::new(&root);
    loader.reload()?;
    let skills = loader.enabled();
    println!("Loaded skills ({}):", skills.len());
    for s in &skills {
        println!(
            "  - {} (triggers: {})",
            s.frontmatter.name,
            s.frontmatter.triggers.join(", ")
        );
    }

    let ctx = MatchContext::from_wake(&reason);
    let matched = SkillMatcher::match_skills(&skills, &ctx, 2000);
    println!("\nMatched ({}):", matched.len());
    for (skill, score) in &matched {
        println!(
            "  - {} (score {}, triggers {:?}, ~{} tokens)",
            skill.frontmatter.name, score.score, score.matched_triggers, score.approx_tokens
        );
    }

    let (prompt, names) = SkillMatcher::render_prompt(&skills, &ctx, 2000);
    if !prompt.is_empty() {
        println!(
            "\n---- Prompt block ({} skills: {:?}) ----",
            names.len(),
            names
        );
        println!("{prompt}");
    } else {
        println!("\nNo skills would be injected for this context.");
    }
    Ok(())
}
