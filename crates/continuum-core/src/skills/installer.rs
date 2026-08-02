//! # Skill installer (create / save / delete on disk)
//!
//! Dashboard-visible CRUD operations on skill directories. Kept separate
//! from the [`super::loader`] so the read and write halves don't tangle and
//! we can unit-test each independently.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use super::frontmatter::parse_skill_file;
use super::types::{Skill, SkillFrontmatter};

/// Create a brand-new skill. Fails if a skill with that name already exists.
pub fn create_skill(
    skills_root: &Path,
    frontmatter: SkillFrontmatter,
    body: &str,
) -> Result<Skill> {
    validate_name(&frontmatter.name)?;
    let dir = skills_root.join(&frontmatter.name);
    if dir.exists() {
        return Err(anyhow!(
            "Skill '{}' already exists at {}",
            frontmatter.name,
            dir.display()
        ));
    }
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create skill dir at {}", dir.display()))?;
    write_skill_md(&dir, &frontmatter, body)?;
    parse_skill_file(&dir.join("SKILL.md"))
}

/// Overwrite (or create) a skill's SKILL.md. The name must match the
/// directory — renaming is a delete+create operation by the caller.
pub fn save_skill(skills_root: &Path, frontmatter: SkillFrontmatter, body: &str) -> Result<Skill> {
    validate_name(&frontmatter.name)?;
    let dir = skills_root.join(&frontmatter.name);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to ensure skill dir {}", dir.display()))?;
    write_skill_md(&dir, &frontmatter, body)?;
    parse_skill_file(&dir.join("SKILL.md"))
}

/// Delete a skill directory (entire folder). Returns `Ok(())` if the skill
/// was already gone.
pub fn delete_skill(skills_root: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let dir = skills_root.join(name);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("Failed to delete skill dir {}", dir.display())),
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("Skill name may not be empty"));
    }
    if name.len() > 64 {
        return Err(anyhow!("Skill name is too long (64 chars max)"));
    }
    for ch in name.chars() {
        if !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
            return Err(anyhow!(
                "Skill name '{name}' contains illegal character '{ch}' — use [a-zA-Z0-9_-]"
            ));
        }
    }
    Ok(())
}

fn write_skill_md(dir: &Path, fm: &SkillFrontmatter, body: &str) -> Result<PathBuf> {
    let mut buf = String::from("---\n");
    buf.push_str(&format!("name: {}\n", fm.name));
    buf.push_str(&format!(
        "description: {}\n",
        if fm.description.contains(':') || fm.description.contains('#') {
            format!("\"{}\"", fm.description.replace('"', "\\\""))
        } else {
            fm.description.clone()
        }
    ));
    if !fm.triggers.is_empty() {
        buf.push_str("triggers:\n");
        for t in &fm.triggers {
            if t.contains(' ') || t.contains(':') {
                buf.push_str(&format!("  - \"{}\"\n", t.replace('"', "\\\"")));
            } else {
                buf.push_str(&format!("  - {}\n", t));
            }
        }
    }
    if let Some(src) = &fm.source {
        buf.push_str(&format!("source: {}\n", src));
    }
    if fm.manual_only {
        buf.push_str("manual_only: true\n");
    }
    buf.push_str("---\n");
    let body_trimmed = body.trim_start();
    buf.push_str(body_trimmed);
    if !body_trimmed.ends_with('\n') {
        buf.push('\n');
    }
    let path = dir.join("SKILL.md");
    std::fs::write(&path, buf)
        .with_context(|| format!("Failed to write SKILL.md at {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fm(name: &str) -> SkillFrontmatter {
        SkillFrontmatter {
            name: name.into(),
            description: "hello".into(),
            triggers: vec!["one".into(), "another trigger".into()],
            source: Some("user".into()),
            manual_only: false,
        }
    }

    #[test]
    fn create_then_parse_back_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let skill = create_skill(tmp.path(), fm("demo"), "Hello body.").unwrap();
        assert_eq!(skill.frontmatter.name, "demo");
        assert_eq!(skill.frontmatter.triggers.len(), 2);
        assert!(skill.body.contains("Hello body"));
    }

    #[test]
    fn create_rejects_duplicate() {
        let tmp = TempDir::new().unwrap();
        create_skill(tmp.path(), fm("demo"), "a").unwrap();
        let err = create_skill(tmp.path(), fm("demo"), "b");
        assert!(err.is_err());
    }

    #[test]
    fn save_overwrites_existing_body() {
        let tmp = TempDir::new().unwrap();
        create_skill(tmp.path(), fm("demo"), "v1").unwrap();
        save_skill(tmp.path(), fm("demo"), "v2").unwrap();
        let reparsed = parse_skill_file(&tmp.path().join("demo/SKILL.md")).unwrap();
        assert!(reparsed.body.contains("v2"));
    }

    #[test]
    fn delete_removes_directory() {
        let tmp = TempDir::new().unwrap();
        create_skill(tmp.path(), fm("demo"), "body").unwrap();
        assert!(tmp.path().join("demo").exists());
        delete_skill(tmp.path(), "demo").unwrap();
        assert!(!tmp.path().join("demo").exists());
    }

    #[test]
    fn delete_missing_is_ok() {
        let tmp = TempDir::new().unwrap();
        delete_skill(tmp.path(), "never-was-here").unwrap();
    }

    #[test]
    fn rejects_invalid_names() {
        let tmp = TempDir::new().unwrap();
        let err = create_skill(tmp.path(), fm("../escape"), "x");
        assert!(err.is_err());
    }
}
