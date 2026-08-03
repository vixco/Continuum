//! Markdown-document parsing: `---` YAML fence + body, plus wiki-link
//! extraction. Round-trip safe: unknown frontmatter keys survive rewrite.

use regex::Regex;
use std::sync::OnceLock;

use crate::error::{MemoryError, Result};
use crate::model::Frontmatter;

/// A parsed vault document.
pub struct ParsedDoc {
    pub frontmatter: Frontmatter,
    pub body: String,
}

/// Parse a full markdown document (frontmatter fence required).
pub fn parse_document(text: &str) -> Result<ParsedDoc> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    // Find the opening fence: first line must be "---" (tolerating trailing whitespace).
    let rest = {
        let first_line = text
            .split_inclusive('\n')
            .next()
            .ok_or_else(|| MemoryError::Parse("missing opening --- fence".into()))?;
        if first_line.trim_end() == "---" {
            let consumed = first_line.len();
            text[consumed..].to_string()
        } else {
            return Err(MemoryError::Parse("missing opening --- fence".into()));
        }
    };

    // Find the closing fence on its own line.
    let mut yaml_end = None;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            yaml_end = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }

    let (yaml_to, body_from) =
        yaml_end.ok_or_else(|| MemoryError::Parse("missing closing --- fence".into()))?;
    let yaml = &rest[..yaml_to];
    let body = rest[body_from..].to_string();
    let frontmatter: Frontmatter =
        serde_yaml::from_str(yaml).map_err(|e| MemoryError::Parse(e.to_string()))?;
    Ok(ParsedDoc { frontmatter, body })
}

/// Render frontmatter + body back to a document string.
pub fn render_document(fm: &Frontmatter, body: &str) -> Result<String> {
    let yaml = serde_yaml::to_string(fm).map_err(|e| MemoryError::Parse(e.to_string()))?;
    Ok(format!("---\n{yaml}---\n{body}"))
}

/// Extract `[[wiki-link]]` targets from a body: order-preserving, deduped,
/// `|alias` stripped, whitespace trimmed.
pub fn extract_wiki_links(body: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\[\[([^\[\]\n]+)\]\]").expect("static regex"));
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for cap in re.captures_iter(body) {
        let raw = &cap[1];
        let target = raw.split('|').next().unwrap_or(raw).trim();
        if !target.is_empty() && seen.insert(target.to_lowercase()) {
            out.push(target.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\nid: mem_1\ntype: decision\ntitle: Lobby creation must be manual\ncreated: 2026-08-01T21:45:00Z\nrelations:\n- to: sidelife\n  rel: belongs_to\n  confidence: 1.0\n---\nBody with [[SideLife]] and [[ReadyZone.cs|the zone]].\n";

    #[test]
    fn parse_extracts_frontmatter_and_body() {
        let doc = parse_document(DOC).unwrap();
        assert_eq!(doc.frontmatter.id, "mem_1");
        assert_eq!(doc.frontmatter.relations.len(), 1);
        assert!(doc.body.starts_with("Body with"));
    }

    #[test]
    fn roundtrip_preserves_unknown_keys_and_body() {
        let src = "---\nid: mem_2\ntype: fact\ntitle: T\ncreated: 2026-08-01T10:00:00Z\nmystery: 42\n---\nBody €🧠 stays.\n";
        let doc = parse_document(src).unwrap();
        let out = render_document(&doc.frontmatter, &doc.body).unwrap();
        let again = parse_document(&out).unwrap();
        assert_eq!(
            again.frontmatter.extra.get("mystery").unwrap().as_u64(),
            Some(42)
        );
        assert_eq!(again.body, doc.body);
    }

    #[test]
    fn parse_accepts_crlf() {
        let src = DOC.replace('\n', "\r\n");
        let doc = parse_document(&src).unwrap();
        assert_eq!(doc.frontmatter.node_type, crate::model::NodeType::Decision);
    }

    #[test]
    fn parse_rejects_missing_fence() {
        assert!(matches!(
            parse_document("no frontmatter here"),
            Err(crate::MemoryError::Parse(_))
        ));
    }

    #[test]
    fn parse_rejects_broken_yaml() {
        let src = "---\nid: [unclosed\n---\nbody\n";
        assert!(matches!(
            parse_document(src),
            Err(crate::MemoryError::Parse(_))
        ));
    }

    #[test]
    fn wiki_links_dedup_and_strip_alias() {
        let links = extract_wiki_links("See [[A]] then [[B|alias]] then [[A]] and [[ C ]].");
        assert_eq!(links, vec!["A".to_string(), "B".into(), "C".into()]);
    }

    #[test]
    fn parse_tolerates_trailing_spaces_on_fences() {
        let src = "--- \nid: mem_1\ntype: fact\ntitle: T\ncreated: 2026-08-01T10:00:00Z\n--- \t\nBody here.\n";
        let doc = parse_document(src).unwrap();
        assert_eq!(doc.frontmatter.id, "mem_1");
        assert_eq!(doc.body, "Body here.\n");
    }

    #[test]
    fn parse_closing_fence_at_eof_with_no_trailing_newline() {
        let src = "---\nid: mem_1\ntype: fact\ntitle: T\ncreated: 2026-08-01T10:00:00Z\n---";
        let doc = parse_document(src).unwrap();
        assert_eq!(doc.frontmatter.id, "mem_1");
        assert_eq!(doc.body, "");
    }

    #[test]
    fn parse_with_only_frontmatter_no_body_line() {
        let src = "---\nid: mem_1\ntype: fact\ntitle: T\ncreated: 2026-08-01T10:00:00Z\n---\n";
        let doc = parse_document(src).unwrap();
        assert_eq!(doc.frontmatter.id, "mem_1");
        assert_eq!(doc.body, "");
    }
}
