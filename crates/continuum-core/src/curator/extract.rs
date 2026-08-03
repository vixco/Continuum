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

/// Build the extraction prompt sent to the curator LLM: recent activity
/// plus already-known related notes (so the model doesn't re-propose what
/// the vault already has), capped to `max_candidates` results.
pub fn build_extract_prompt(
    events_block: &str,
    related_notes_block: &str,
    max_candidates: u32,
) -> String {
    format!(
        "You are Continuum's memory curator. Review the recent activity below and \
         extract up to {max_candidates} durable memories worth recording in the vault. \
         Only extract genuinely reusable facts, preferences, decisions, goals, or \
         errors — skip anything trivial or already covered by the related notes shown \
         below.\n\n\
         Valid node types: project, goal, task, decision, person, preference, fact, \
         error, session, note.\n\n\
         Respond with ONLY a JSON array, no prose before or after, matching this shape:\n\
         [{{\"type\":\"fact\",\"title\":\"...\",\"body\":\"...\",\"project\":null,\
         \"confidence\":0.0,\"importance\":0.5,\"source\":\"observed\",\
         \"relations\":[{{\"to\":\"...\",\"rel\":\"...\",\"confidence\":1.0}}],\"tags\":[]}}]\n\n\
         Recent activity:\n{events_block}\n\n\
         Related existing notes:\n{related_notes_block}\n"
    )
}

/// Parse the curator LLM's raw reply into candidates.
///
/// LLMs like to wrap JSON in prose ("Sure! Here are the memories: [...] Done.")
/// so this finds the first `[` and the last `]` in the text and strictly
/// deserializes that slice. Every candidate's `type` must parse via
/// [`NodeType::parse`] and its `title` must be non-blank, or the whole call
/// errors (naming the offending candidate) rather than silently dropping it.
pub fn parse_candidates(raw: &str) -> anyhow::Result<Vec<CandidateJson>> {
    let start = raw
        .find('[')
        .ok_or_else(|| anyhow::anyhow!("no JSON array found in curator LLM output: {raw:?}"))?;
    let end = raw
        .rfind(']')
        .ok_or_else(|| anyhow::anyhow!("no closing ']' found in curator LLM output: {raw:?}"))?;
    if end < start {
        anyhow::bail!("malformed JSON array bounds in curator LLM output: {raw:?}");
    }
    let slice = &raw[start..=end];
    let candidates: Vec<CandidateJson> = serde_json::from_str(slice)
        .map_err(|e| anyhow::anyhow!("failed to parse curator candidates JSON: {e}"))?;

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

    Ok(candidates)
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
