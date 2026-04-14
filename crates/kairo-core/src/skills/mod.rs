//! # Skills
//!
//! Skills are `SKILL.md` files stored under `skills/` that extend the
//! orchestrator's (and workers') knowledge for specific workflows.
//!
//! Each skill is a directory with a `SKILL.md` that has YAML frontmatter
//! (`name`, `description`, `triggers`) and a Markdown body. At runtime the
//! loader scans the directory, parses each skill, and exposes a list. The
//! matcher then decides which skills apply to the current context and
//! produces a prompt fragment within a configurable token budget.
//!
//! Skills are plain prompt text — they do **not** run code. Permissions still
//! flow through the orchestrator's tool allowlist. A skill can *instruct* the
//! orchestrator to use certain tools, but cannot grant access the user did
//! not already configure.

pub mod frontmatter;
pub mod installer;
pub mod loader;
pub mod matcher;
pub mod types;

pub use frontmatter::{parse_skill_file, SkillParseError};
pub use installer::{create_skill, delete_skill, save_skill};
pub use loader::{SkillLoader, SkillWatchHandle};
pub use matcher::{MatchContext, MatchScore, SkillMatcher};
pub use types::{Skill, SkillFrontmatter};
