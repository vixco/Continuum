//! # Extraction
//!
//! Turns recent activity (perception events, session transcripts) into
//! candidate vault memories: prompt construction for the curator LLM,
//! strict parsing of its JSON reply, near-duplicate detection against the
//! vault's full-text index, and threshold-based routing into candidate vs.
//! confirmed vault notes.

use continuum_memory::{NodeStatus, NodeType, NoteDraft, Relation, Source, Vault};

use crate::config::CuratorConfig;

fn default_importance() -> f32 {
    0.5
}

fn default_rel_conf() -> f32 {
    1.0
}

/// One candidate memory as parsed from the curator LLM's JSON reply.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CandidateJson {
    /// Vault node type, snake_case (see [`NodeType::parse`]).
    pub r#type: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub project: Option<String>,
    pub confidence: f32,
    #[serde(default = "default_importance")]
    pub importance: f32,
    /// `user_statement` | `observed` | `inferred` (free text; unrecognized
    /// values fall back to `observed` in [`candidate_to_draft`]).
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub relations: Vec<RelationJson>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A relation edge as parsed from the curator LLM's JSON reply.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RelationJson {
    pub to: String,
    pub rel: String,
    #[serde(default = "default_rel_conf")]
    pub confidence: f32,
}

/// From a `[` at byte offset `start` in `raw`, scans forward tracking
/// bracket depth (starting at 1 for the opening `[`) while respecting JSON
/// string literals — `[`/`]` bytes inside a quoted string don't perturb the
/// count, and a backslash escapes the following byte so `\"` doesn't
/// prematurely end the string. Returns the byte offset of the `]` where
/// depth first returns to 0 (the bracket matching `start`), or `None` if
/// the text ends before the brackets balance.
///
/// Byte-level comparison against the ASCII bytes for `"`, `\`, `[`, `]` is
/// safe on non-ASCII UTF-8 input: every continuation/lead byte of a
/// multi-byte codepoint is >= 0x80, so it can never be mistaken for one of
/// these single-byte ASCII structural characters.
fn matching_bracket_end(raw: &str, start: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => in_string = true,
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Parse the curator LLM's raw reply into candidates.
///
/// LLMs like to wrap JSON in prose ("Sure! Here are the memories: [...]
/// Done.") — and that prose can itself contain stray `[`/`]` characters
/// (footnote markers, `[[Wiki-Links]]`, trailing asides), so naive
/// first-`[`/last-`]` slicing breaks the moment prose brackets appear on
/// either side of the real array. Instead this walks every `[` in the text
/// left to right, uses [`matching_bracket_end`] to find each one's balanced
/// closing `]` (respecting string literals, so nested `"relations": [...]`
/// arrays inside a candidate object don't confuse the scan), and returns
/// the first slice that both balances *and* deserializes as
/// `Vec<CandidateJson>`. A syntactically-valid-but-wrong-shaped bracket
/// pair (e.g. a footnote `[1]`) is skipped in favor of the next `[` rather
/// than treated as a hard failure.
///
/// Once a slice deserializes structurally, every candidate's `type` must
/// parse via [`NodeType::parse`] and its `title` must be non-blank, or the
/// whole call errors immediately (naming the offending candidate) rather
/// than silently dropping it or trying yet another bracket — a
/// successfully-parsed-but-semantically-invalid candidate is a real
/// problem the caller should see, not something to paper over.
pub fn parse_candidates(raw: &str) -> anyhow::Result<Vec<CandidateJson>> {
    let mut last_err: Option<String> = None;
    let mut search_from = 0usize;

    while let Some(rel_start) = raw[search_from..].find('[') {
        let start = search_from + rel_start;
        search_from = start + 1; // always advance past this '[' before the next attempt

        let Some(end) = matching_bracket_end(raw, start) else {
            last_err = Some(format!("no matching ']' for '[' at byte offset {start}"));
            continue;
        };

        match serde_json::from_str::<Vec<CandidateJson>>(&raw[start..=end]) {
            Ok(candidates) => {
                for c in &candidates {
                    if NodeType::parse(&c.r#type).is_none() {
                        anyhow::bail!(
                            "candidate {:?} has unknown node type {:?}",
                            c.title,
                            c.r#type
                        );
                    }
                    if c.title.trim().is_empty() {
                        anyhow::bail!("candidate of type {:?} has a blank title", c.r#type);
                    }
                }
                return Ok(candidates);
            }
            Err(e) => {
                last_err = Some(e.to_string());
            }
        }
    }

    Err(match last_err {
        Some(e) => anyhow::anyhow!("failed to parse curator candidates JSON: {e} (raw: {raw:?})"),
        None => anyhow::anyhow!("no JSON array found in curator LLM output: {raw:?}"),
    })
}

/// Normalize a title for duplicate comparison: lowercase, strip punctuation,
/// collapse whitespace.
pub fn normalize_title(t: &str) -> String {
    let cleaned: String = t
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether a near-duplicate of `title` already exists in the vault.
///
/// Checks notes of every status, including rejected ones — the curator
/// should not keep re-proposing something a human already declined. Plan
/// A's full-text index has no status filter, so `Vault::search` already
/// covers rejected notes; this pairs the FTS top-5 with a normalized-title
/// equality check to avoid false positives from loose FTS matches.
pub async fn is_duplicate(vault: &Vault, title: &str) -> anyhow::Result<bool> {
    let results = vault.search(title, 5).await?;
    let target = normalize_title(title);
    Ok(results.iter().any(|n| normalize_title(&n.title) == target))
}

/// Threshold routing (Plan B, Stage 3): decide whether a candidate is
/// written straight to `Confirmed`, held as `Candidate` for human review,
/// or discarded outright.
///
/// - confidence < `discard_floor` → discard (`None`)
/// - confidence >= `auto_confirm_threshold` AND source is `"user_statement"`
///   → `Confirmed` (the user said it directly; no review needed)
/// - confidence >= `auto_confirm_threshold` otherwise, or anywhere in
///   between the floor and the threshold → `Candidate`
pub fn route_candidate(c: &CandidateJson, cfg: &CuratorConfig) -> Option<NodeStatus> {
    if c.confidence < cfg.discard_floor {
        return None;
    }
    if c.confidence >= cfg.auto_confirm_threshold && c.source.as_deref() == Some("user_statement") {
        return Some(NodeStatus::Confirmed);
    }
    Some(NodeStatus::Candidate)
}

/// Map a validated [`CandidateJson`] into a [`NoteDraft`] ready for
/// `Vault::create`, tagged with the curator's provenance.
pub fn candidate_to_draft(c: &CandidateJson, status: NodeStatus) -> NoteDraft {
    let node_type =
        NodeType::parse(&c.r#type).expect("candidate type validated by parse_candidates");
    let source = match c.source.as_deref() {
        Some("user_statement") => Source::UserStatement,
        Some("observed") => Source::Observed,
        Some("inferred") => Source::Inferred,
        Some("agent_run") => Source::AgentRun,
        Some("chat") => Source::Chat,
        Some("manual") => Source::Manual,
        _ => Source::Observed,
    };
    let relations = c
        .relations
        .iter()
        .map(|r| Relation {
            to: r.to.clone(),
            rel: r.rel.clone(),
            confidence: r.confidence,
        })
        .collect();

    NoteDraft {
        node_type,
        title: c.title.clone(),
        body: c.body.clone(),
        project: c.project.clone(),
        status,
        confidence: c.confidence,
        importance: c.importance,
        source,
        source_ref: Some("curator:extract".to_string()),
        sensitivity: Default::default(),
        relations,
        tags: c.tags.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_candidates_accepts_wrapped_json() {
        let raw = "Sure! Here are the memories:\n[{\"type\":\"preference\",\"title\":\"Prefers pnpm\",\"body\":\"uses pnpm\",\"confidence\":0.7}]\nDone.";
        let c = parse_candidates(raw).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].title, "Prefers pnpm");
        assert_eq!(c[0].importance, 0.5); // default backfill
    }

    #[test]
    fn parse_candidates_rejects_garbage() {
        assert!(parse_candidates("no json here").is_err());
        assert!(parse_candidates("[{\"title\":\"missing type\"}]").is_err());
    }

    #[test]
    fn parse_candidates_skips_prose_bracket_before_array() {
        // "[1]" is a syntactically-valid JSON array too (of one integer) —
        // the balanced-bracket scan must reject it as the wrong shape and
        // keep looking rather than erroring out on the first match.
        let raw = "Here's a note [1]:\n[{\"type\":\"fact\",\"title\":\"T\",\"body\":\"b\",\"confidence\":0.5}]";
        let c = parse_candidates(raw).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].title, "T");
    }

    #[test]
    fn parse_candidates_skips_prose_bracket_after_array() {
        // Naive first-'['/last-']' slicing would span all the way to the
        // ']' in "[Note Two]", swallowing trailing prose into the JSON
        // slice and breaking the parse. The balanced scan must stop at the
        // real array's own closing bracket.
        let raw = "[{\"type\":\"fact\",\"title\":\"T\",\"body\":\"b\",\"confidence\":0.5}]\nSee [Note Two].";
        let c = parse_candidates(raw).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].title, "T");
    }

    #[test]
    fn parse_candidates_rejects_unknown_type() {
        let raw = r#"[{"type":"bogus","title":"T","body":"b","confidence":0.5}]"#;
        let err = parse_candidates(raw).unwrap_err();
        assert!(err.to_string().contains("unknown node type"));
    }

    #[test]
    fn parse_candidates_rejects_blank_title() {
        let raw = r#"[{"type":"fact","title":"   ","body":"b","confidence":0.5}]"#;
        let err = parse_candidates(raw).unwrap_err();
        assert!(err.to_string().contains("blank title"));
    }

    #[test]
    fn route_candidate_threshold_table() {
        let cfg = CuratorConfig::default(); // auto_confirm 0.85, floor 0.4
        let mk = |conf: f32, source: &str| CandidateJson {
            r#type: "fact".into(),
            title: "T".into(),
            body: "b".into(),
            project: None,
            confidence: conf,
            importance: 0.5,
            source: Some(source.into()),
            relations: vec![],
            tags: vec![],
        };
        // >= threshold AND user_statement -> Confirmed
        assert_eq!(
            route_candidate(&mk(0.9, "user_statement"), &cfg),
            Some(NodeStatus::Confirmed)
        );
        // >= threshold but NOT user_statement -> stays candidate
        assert_eq!(
            route_candidate(&mk(0.9, "observed"), &cfg),
            Some(NodeStatus::Candidate)
        );
        // below floor -> discard
        assert_eq!(route_candidate(&mk(0.3, "observed"), &cfg), None);
        // in between -> candidate
        assert_eq!(
            route_candidate(&mk(0.6, "inferred"), &cfg),
            Some(NodeStatus::Candidate)
        );
    }

    #[tokio::test]
    async fn is_duplicate_matches_rejected_notes_too() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = continuum_memory::Vault::open(tmp.path()).await.unwrap();
        let mut d = NoteDraft {
            node_type: NodeType::Fact,
            title: "Prefers PNPM".into(),
            body: "uses pnpm over npm".into(),
            project: None,
            status: NodeStatus::Confirmed,
            confidence: 0.5,
            importance: 0.5,
            source: Default::default(),
            source_ref: None,
            sensitivity: Default::default(),
            relations: vec![],
            tags: vec![],
        };
        d.status = NodeStatus::Rejected;
        vault.create(d).await.unwrap();
        assert!(is_duplicate(&vault, "prefers pnpm").await.unwrap());
        assert!(!is_duplicate(&vault, "totally new idea").await.unwrap());
    }
}
