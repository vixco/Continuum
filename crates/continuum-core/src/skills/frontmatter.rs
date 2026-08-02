//! # YAML frontmatter parser (narrow, purpose-built)
//!
//! Every `SKILL.md` uses a tiny subset of YAML that we parse by hand rather
//! than pull in a full YAML crate. Accepted shape:
//!
//! ```yaml
//! ---
//! name: skill-name
//! description: One sentence description (may contain colons, commas)
//! triggers:
//!   - keyword one
//!   - "quoted keyword"
//! source: bundled            # optional
//! manual_only: false         # optional
//! ---
//! ```
//!
//! Lines outside `---` fences are treated as the skill body. Unknown keys in
//! the frontmatter are ignored so Markdown authors can extend it without
//! breaking the loader.

use std::path::Path;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use thiserror::Error;

use super::types::{Skill, SkillFrontmatter};

/// Errors returned by [`parse_skill_file`].
#[derive(Debug, Error)]
pub enum SkillParseError {
    #[error("file has no `---` frontmatter opening fence")]
    NoOpeningFence,
    #[error("frontmatter is missing the required `name:` field")]
    MissingName,
    #[error("frontmatter value for `{0}` is not a string")]
    NotAString(String),
    #[error("unterminated quoted string in triggers list")]
    UnterminatedQuote,
}

/// Parse a `SKILL.md` from disk. Returns a loaded [`Skill`].
pub fn parse_skill_file(path: &Path) -> Result<Skill> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("Failed to read skill file {}: {e}", path.display()))?;
    let modified_at = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| DateTime::<Utc>::from(t).into());
    let (fm, body) = parse_markdown_with_frontmatter(&contents)?;
    let body_len = body.len();
    Ok(Skill {
        frontmatter: fm,
        body,
        path: path.to_path_buf(),
        modified_at,
        body_len,
        enabled: true,
    })
}

/// Split a Markdown file into frontmatter + body. Public for tests.
pub fn parse_markdown_with_frontmatter(
    raw: &str,
) -> Result<(SkillFrontmatter, String), SkillParseError> {
    let raw = raw.trim_start_matches('\u{FEFF}'); // strip BOM

    let Some(rest) = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
    else {
        return Err(SkillParseError::NoOpeningFence);
    };

    // Find the closing fence. Accept "\n---\n" or "\n---" at EOF.
    let (fm_str, body) = if let Some(pos) = rest.find("\n---\n") {
        (&rest[..pos], &rest[pos + 5..])
    } else if let Some(pos) = rest.find("\n---\r\n") {
        (&rest[..pos], &rest[pos + 6..])
    } else if let Some(pos) = rest.rfind("\n---") {
        (&rest[..pos], &rest[pos + 4..])
    } else {
        return Err(SkillParseError::NoOpeningFence);
    };

    let fm = parse_frontmatter_body(fm_str)?;
    if fm.name.is_empty() {
        return Err(SkillParseError::MissingName);
    }
    Ok((fm, body.trim_start_matches('\n').to_string()))
}

fn parse_frontmatter_body(text: &str) -> Result<SkillFrontmatter, SkillParseError> {
    let mut fm = SkillFrontmatter::default();
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Top-level key: value
        let (key, remainder) = match split_key(trimmed) {
            Some(pair) => pair,
            None => continue,
        };
        let key_norm = key.trim().to_ascii_lowercase();

        match key_norm.as_str() {
            "name" => {
                fm.name = parse_scalar(remainder)?;
            }
            "description" => {
                fm.description = parse_scalar(remainder)?;
            }
            "source" => {
                fm.source = Some(parse_scalar(remainder)?);
            }
            "manual_only" => {
                let raw = parse_scalar(remainder)?;
                fm.manual_only = matches!(raw.to_ascii_lowercase().as_str(), "true" | "yes" | "1");
            }
            "triggers" => {
                // Two acceptable styles:
                //   triggers: ["a", "b"]        -- inline
                //   triggers:
                //     - a
                //     - b
                let remainder_trim = remainder.trim();
                if remainder_trim.starts_with('[') {
                    fm.triggers = parse_inline_list(remainder_trim)?;
                } else {
                    // Consume the following indented list items.
                    while let Some(next) = lines.peek() {
                        let ltrim = next.trim_start();
                        if ltrim.starts_with("- ") || ltrim == "-" {
                            let value = ltrim.trim_start_matches('-').trim();
                            if !value.is_empty() {
                                let parsed = unquote(value)?;
                                fm.triggers.push(parsed);
                            }
                            lines.next();
                        } else if next.trim().is_empty() {
                            lines.next();
                            break;
                        } else if !next.starts_with(' ') && !next.starts_with('\t') {
                            break;
                        } else {
                            lines.next();
                        }
                    }
                }
            }
            _ => { /* ignore unknown keys for forward compat */ }
        }
    }

    Ok(fm)
}

fn split_key(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let (k, v) = line.split_at(colon);
    Some((k, v.trim_start_matches(':').trim_start()))
}

fn parse_scalar(raw: &str) -> Result<String, SkillParseError> {
    let trimmed = raw.trim();
    unquote(trimmed)
}

fn unquote(raw: &str) -> Result<String, SkillParseError> {
    let r = raw.trim();
    if r.starts_with('"') {
        if !r.ends_with('"') || r.len() < 2 {
            return Err(SkillParseError::UnterminatedQuote);
        }
        Ok(r[1..r.len() - 1].replace("\\\"", "\""))
    } else if r.starts_with('\'') {
        if !r.ends_with('\'') || r.len() < 2 {
            return Err(SkillParseError::UnterminatedQuote);
        }
        Ok(r[1..r.len() - 1].to_string())
    } else {
        Ok(r.to_string())
    }
}

fn parse_inline_list(raw: &str) -> Result<Vec<String>, SkillParseError> {
    let r = raw.trim();
    if !r.starts_with('[') || !r.ends_with(']') {
        return Err(SkillParseError::NotAString("triggers".into()));
    }
    let inner = &r[1..r.len() - 1];
    let mut out = Vec::new();
    for item in inner.split(',') {
        let piece = item.trim();
        if piece.is_empty() {
            continue;
        }
        out.push(unquote(piece)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_frontmatter() {
        let raw =
            "---\nname: demo\ndescription: say hi\ntriggers:\n  - hello\n  - hi\n---\nbody here\n";
        let (fm, body) = parse_markdown_with_frontmatter(raw).unwrap();
        assert_eq!(fm.name, "demo");
        assert_eq!(fm.description, "say hi");
        assert_eq!(fm.triggers, vec!["hello".to_string(), "hi".into()]);
        assert!(body.contains("body here"));
    }

    #[test]
    fn parses_inline_triggers_list() {
        let raw = "---\nname: x\ndescription: d\ntriggers: [\"a\", \"b\", c]\n---\nbody\n";
        let (fm, _) = parse_markdown_with_frontmatter(raw).unwrap();
        assert_eq!(fm.triggers, vec!["a", "b", "c"]);
    }

    #[test]
    fn missing_opening_fence_errors() {
        let raw = "name: demo\n---\nbody";
        assert!(matches!(
            parse_markdown_with_frontmatter(raw),
            Err(SkillParseError::NoOpeningFence)
        ));
    }

    #[test]
    fn missing_name_errors() {
        let raw = "---\ndescription: x\n---\nbody";
        assert!(matches!(
            parse_markdown_with_frontmatter(raw),
            Err(SkillParseError::MissingName)
        ));
    }

    #[test]
    fn ignores_unknown_keys() {
        let raw = "---\nname: a\ndescription: b\nunknown_field: whatever\n---\nbody";
        let (fm, _) = parse_markdown_with_frontmatter(raw).unwrap();
        assert_eq!(fm.name, "a");
    }

    #[test]
    fn handles_crlf() {
        let raw = "---\r\nname: a\r\ndescription: d\r\n---\r\nbody\r\n";
        let (fm, _) = parse_markdown_with_frontmatter(raw).unwrap();
        assert_eq!(fm.name, "a");
    }

    #[test]
    fn manual_only_flag_is_parsed() {
        let raw = "---\nname: a\ndescription: d\nmanual_only: true\n---\nbody";
        let (fm, _) = parse_markdown_with_frontmatter(raw).unwrap();
        assert!(fm.manual_only);
    }

    #[test]
    fn unterminated_quote_errors() {
        let raw = "---\nname: \"oops\ndescription: d\n---\nbody";
        assert!(matches!(
            parse_markdown_with_frontmatter(raw),
            Err(SkillParseError::UnterminatedQuote)
        ));
    }
}
