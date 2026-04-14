//! # Skill types

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The YAML frontmatter block at the top of every `SKILL.md`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillFrontmatter {
    /// Canonical name — must be filesystem-safe.
    pub name: String,
    /// One-sentence human-readable description of when the skill applies.
    pub description: String,
    /// Keywords or phrases that trigger the skill. Case-insensitive contains-match.
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Optional source label: `"bundled"`, `"user"`, or `"third-party"`.
    /// Used by the dashboard to show a badge; defaults to `"user"`.
    #[serde(default)]
    pub source: Option<String>,
    /// When true, Kairo will never auto-apply this skill — it's only available
    /// when explicitly invoked (via `/skills <name>` in the future).
    #[serde(default)]
    pub manual_only: bool,
}

/// A loaded skill. The content is the full Markdown body after the
/// frontmatter, ready to be appended to the orchestrator's system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub frontmatter: SkillFrontmatter,
    pub body: String,
    /// Path the skill was loaded from (the `SKILL.md` file itself).
    pub path: PathBuf,
    /// File modification time — used for hot-reload change detection.
    pub modified_at: Option<DateTime<Utc>>,
    /// Raw byte length of `body` — a cheap approximation for the token
    /// budget (we treat ~4 bytes ≈ 1 token).
    pub body_len: usize,
    /// Whether the skill is currently enabled.
    pub enabled: bool,
}

impl Skill {
    /// Approximate token count of the body. Deliberate over-estimate so we
    /// err on the side of a tighter budget.
    pub fn approx_tokens(&self) -> usize {
        // 3.5 bytes/token is a tight English estimate; round up.
        self.body_len.div_ceil(4) + 16
    }

    /// Full prompt-ready text — the skill body prefixed with its heading.
    pub fn prompt_block(&self) -> String {
        format!(
            "## Skill: {}\n\n{}\n",
            self.frontmatter.name,
            self.body.trim()
        )
    }
}
