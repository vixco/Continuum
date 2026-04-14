//! # Skill matcher
//!
//! Given a context (wake reason, task description, active project, audio
//! transcript, foreground app, frame tags) and a set of loaded skills, pick
//! the subset whose triggers match and stay within the configured token
//! budget. Deterministic: no ML, no randomness.

use serde::{Deserialize, Serialize};

use super::types::Skill;

/// Inputs used to match skills. Every field is optional so callers can fill
/// in whatever they have.
#[derive(Debug, Clone, Default)]
pub struct MatchContext {
    pub wake_reason: Option<String>,
    pub task: Option<String>,
    pub project: Option<String>,
    pub audio_transcript: Option<String>,
    pub foreground_app: Option<String>,
    pub tags: Vec<String>,
    /// Names explicitly forced on by the user (e.g. via `/skills code-review`).
    pub forced: Vec<String>,
}

impl MatchContext {
    pub fn from_wake(reason: impl Into<String>) -> Self {
        Self {
            wake_reason: Some(reason.into()),
            ..Default::default()
        }
    }

    pub fn from_task(task: impl Into<String>) -> Self {
        Self {
            task: Some(task.into()),
            ..Default::default()
        }
    }

    /// Concatenate every searchable field into a single lowercase haystack.
    fn haystack(&self) -> String {
        let mut s = String::new();
        for opt in [
            self.wake_reason.as_deref(),
            self.task.as_deref(),
            self.project.as_deref(),
            self.audio_transcript.as_deref(),
            self.foreground_app.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(opt);
        }
        for t in &self.tags {
            s.push(' ');
            s.push_str(t);
        }
        s.to_ascii_lowercase()
    }
}

/// A single skill + its match score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchScore {
    pub name: String,
    pub score: u32,
    pub matched_triggers: Vec<String>,
    /// Token estimate the skill would contribute if included.
    pub approx_tokens: usize,
    pub forced: bool,
}

/// The matcher itself. Stateless; build on demand per wake.
pub struct SkillMatcher;

impl SkillMatcher {
    /// Return the subset of skills that match the context, ranked by score.
    ///
    /// `token_budget` caps the total approximate token cost of the returned
    /// skills — additional matches beyond the budget are dropped.
    pub fn match_skills(
        skills: &[Skill],
        ctx: &MatchContext,
        token_budget: usize,
    ) -> Vec<(Skill, MatchScore)> {
        let haystack = ctx.haystack();
        let mut scored: Vec<(Skill, MatchScore)> = skills
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| score_one(s, &haystack, ctx).map(|sc| (s.clone(), sc)))
            .collect();

        // Forced skills always come first.
        scored.sort_by(|a, b| {
            b.1.forced
                .cmp(&a.1.forced)
                .then(b.1.score.cmp(&a.1.score))
                .then(a.1.name.cmp(&b.1.name))
        });

        let mut used_tokens = 0usize;
        let mut out = Vec::new();
        for (skill, score) in scored {
            let cost = score.approx_tokens;
            if used_tokens + cost > token_budget && !score.forced {
                continue;
            }
            used_tokens += cost;
            out.push((skill, score));
            if used_tokens >= token_budget {
                break;
            }
        }
        out
    }

    /// Convenience: build the combined prompt fragment from matched skills.
    /// Returns `(prompt_text, matched_names)`.
    pub fn render_prompt(
        skills: &[Skill],
        ctx: &MatchContext,
        token_budget: usize,
    ) -> (String, Vec<String>) {
        let matched = Self::match_skills(skills, ctx, token_budget);
        if matched.is_empty() {
            return (String::new(), Vec::new());
        }
        let names: Vec<String> = matched
            .iter()
            .map(|(s, _)| s.frontmatter.name.clone())
            .collect();
        let mut prompt = String::from(
            "# Active skills\n\nThe following skills match the current context. \
                         Follow their instructions when they apply, but do not invent steps \
                         outside them.\n\n",
        );
        for (skill, _) in &matched {
            prompt.push_str(&skill.prompt_block());
            prompt.push('\n');
        }
        (prompt, names)
    }
}

fn score_one(skill: &Skill, haystack: &str, ctx: &MatchContext) -> Option<MatchScore> {
    let forced = ctx.forced.iter().any(|f| f == &skill.frontmatter.name);
    let mut score = if forced { 10 } else { 0 };
    let mut matched = Vec::new();
    for trigger in &skill.frontmatter.triggers {
        let needle = trigger.to_ascii_lowercase();
        if needle.is_empty() {
            continue;
        }
        if haystack.contains(&needle) {
            score += 1;
            matched.push(trigger.clone());
        }
    }
    if score == 0 {
        return None;
    }
    Some(MatchScore {
        name: skill.frontmatter.name.clone(),
        score,
        matched_triggers: matched,
        approx_tokens: skill.approx_tokens(),
        forced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::types::{Skill, SkillFrontmatter};
    use std::path::PathBuf;

    fn make_skill(name: &str, triggers: &[&str], body: &str) -> Skill {
        let body_len = body.len();
        Skill {
            frontmatter: SkillFrontmatter {
                name: name.into(),
                description: "d".into(),
                triggers: triggers.iter().map(|s| s.to_string()).collect(),
                source: None,
                manual_only: false,
            },
            body: body.into(),
            path: PathBuf::from(format!("/tmp/{name}/SKILL.md")),
            modified_at: None,
            body_len,
            enabled: true,
        }
    }

    #[test]
    fn keyword_match_returns_skill() {
        let skills = vec![make_skill("code-review", &["review", "PR"], "Do review")];
        let ctx = MatchContext::from_wake("user wants a review of the auth PR");
        let matched = SkillMatcher::match_skills(&skills, &ctx, 1000);
        assert_eq!(matched.len(), 1);
        assert!(matched[0].1.matched_triggers.contains(&"review".into()));
    }

    #[test]
    fn no_match_returns_empty() {
        let skills = vec![make_skill("code-review", &["review"], "body")];
        let ctx = MatchContext::from_wake("user opened notepad");
        let matched = SkillMatcher::match_skills(&skills, &ctx, 1000);
        assert!(matched.is_empty());
    }

    #[test]
    fn multi_match_sorted_by_score() {
        let skills = vec![
            make_skill("a", &["foo"], "body"),
            make_skill("b", &["foo", "bar"], "body"),
        ];
        let ctx = MatchContext {
            wake_reason: Some("foo and bar".into()),
            ..Default::default()
        };
        let matched = SkillMatcher::match_skills(&skills, &ctx, 1000);
        assert_eq!(matched[0].0.frontmatter.name, "b");
        assert_eq!(matched[1].0.frontmatter.name, "a");
    }

    #[test]
    fn token_budget_drops_later_skills() {
        let long_body = "x".repeat(4000); // ~1000 tokens
        let skills = vec![
            make_skill("big1", &["foo"], &long_body),
            make_skill("big2", &["foo"], &long_body),
        ];
        let ctx = MatchContext::from_wake("foo");
        let matched = SkillMatcher::match_skills(&skills, &ctx, 1100);
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn forced_skills_always_included_and_first() {
        let long_body = "x".repeat(4000);
        let skills = vec![
            make_skill("fa", &["never-matches"], &long_body),
            make_skill("other", &["foo"], "small"),
        ];
        let ctx = MatchContext {
            wake_reason: Some("foo".into()),
            forced: vec!["fa".into()],
            ..Default::default()
        };
        let matched = SkillMatcher::match_skills(&skills, &ctx, 200);
        // Forced comes first, and bypasses the budget.
        assert_eq!(matched[0].0.frontmatter.name, "fa");
        assert!(matched[0].1.forced);
    }

    #[test]
    fn render_prompt_produces_combined_text() {
        let skills = vec![make_skill("x", &["foo"], "do x")];
        let ctx = MatchContext::from_wake("user asked foo");
        let (prompt, names) = SkillMatcher::render_prompt(&skills, &ctx, 1000);
        assert!(prompt.contains("## Skill: x"));
        assert!(prompt.contains("do x"));
        assert_eq!(names, vec!["x"]);
    }

    #[test]
    fn empty_skills_returns_empty_prompt() {
        let (prompt, names) = SkillMatcher::render_prompt(&[], &MatchContext::default(), 1000);
        assert!(prompt.is_empty());
        assert!(names.is_empty());
    }

    #[test]
    fn case_insensitive_match() {
        let skills = vec![make_skill("x", &["Review"], "body")];
        let ctx = MatchContext::from_wake("USER WANTS a REVIEW");
        let matched = SkillMatcher::match_skills(&skills, &ctx, 1000);
        assert_eq!(matched.len(), 1);
    }
}
