//! # Conflict / supersede detection
//!
//! After an [`crate::curator::run::extract_pass`] writes new notes into the
//! vault, this module asks the curator LLM whether each new note
//! contradicts or supersedes an existing, confirmed note on the same topic.
//! It never edits or reclassifies the OLD note itself — that is a human (or
//! orchestrator-reviewed) decision made through `Vault::resolve_candidate`'s
//! `Resolution::Supersede`. All this module does is attach a
//! `proposes_supersede` relation onto the NEW note so the dashboard/graph
//! can surface the suggested link for review. Layer: memory (curator).

use continuum_memory::{NodeStatus, NodeSummary, Note, Relation, Vault};

use crate::curator::CuratorLlm;

/// The conflict-detection prompt template, loaded at compile time from the
/// repo-root `prompts/curator-conflict.md`. `{{OLD}}`/`{{NEW}}` are
/// substituted per-pair in [`build_conflict_prompt`].
pub const CONFLICT_PROMPT: &str = include_str!("../../../../prompts/curator-conflict.md");

/// Relation kind written onto a NEW note when the curator LLM judges it
/// supersedes or contradicts an existing CONFIRMED note. A proposal, not an
/// automatic resolution — see the module doc comment.
const PROPOSES_SUPERSEDE: &str = "proposes_supersede";

/// Confidence floor below which a "supersedes"/"contradicts" verdict is
/// treated as too weak to propose. Matches the fixed value in
/// `prompts/curator-conflict.md`'s design (Plan B Task 5); unlike
/// `CuratorConfig`'s thresholds this isn't yet plumbed through config
/// because [`detect_conflicts`]'s public signature (Plan B Task 5's
/// contract, consumed by [`crate::curator::run`]) doesn't thread a config
/// handle through — see the doc comment on [`detect_conflicts`].
const SUPERSEDE_CONFIDENCE_FLOOR: f32 = 0.5;

/// The curator LLM's verdict on one OLD/NEW note pair, as parsed from its
/// JSON reply (see `prompts/curator-conflict.md`).
#[derive(Debug, Clone, serde::Deserialize)]
struct Verdict {
    verdict: String,
    confidence: f32,
    /// Not surfaced anywhere yet (no UI consumes it), but kept off the raw
    /// LLM reply for future dashboard/tracing use rather than discarded at
    /// parse time.
    #[serde(default)]
    #[allow(dead_code)]
    reason: String,
}

/// From a `{` at byte offset `start` in `raw`, scans forward tracking brace
/// depth (starting at 1 for the opening `{`) while respecting JSON string
/// literals, exactly like
/// [`crate::curator::extract::matching_bracket_end`] does for `[`/`]` — see
/// that function's doc comment for the string/escape-handling rationale,
/// which applies unchanged here for `{`/`}`. Returns the byte offset of the
/// `}` where depth first returns to 0, or `None` if the text ends before
/// the braces balance.
fn matching_brace_end(raw: &str, start: usize) -> Option<usize> {
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
                b'{' => depth += 1,
                b'}' => {
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

/// Parse the curator LLM's raw reply into a single [`Verdict`] object.
/// Mirrors [`crate::curator::extract::parse_candidates`]'s approach: walk
/// every `{` in the text left to right, use [`matching_brace_end`] to find
/// its balanced closing `}`, and return the first slice that both balances
/// *and* deserializes as [`Verdict`] — so prose wrapped around the object
/// ("Sure, here's my verdict: {...} Let me know if you need more.") doesn't
/// break the parse.
fn parse_verdict(raw: &str) -> anyhow::Result<Verdict> {
    let mut last_err: Option<String> = None;
    let mut search_from = 0usize;

    while let Some(rel_start) = raw[search_from..].find('{') {
        let start = search_from + rel_start;
        search_from = start + 1; // always advance past this '{' before the next attempt

        let Some(end) = matching_brace_end(raw, start) else {
            last_err = Some(format!("no matching '}}' for '{{' at byte offset {start}"));
            continue;
        };

        match serde_json::from_str::<Verdict>(&raw[start..=end]) {
            Ok(v) => return Ok(v),
            Err(e) => last_err = Some(e.to_string()),
        }
    }

    Err(match last_err {
        Some(e) => anyhow::anyhow!("failed to parse curator verdict JSON: {e} (raw: {raw:?})"),
        None => anyhow::anyhow!("no JSON object found in curator verdict output: {raw:?}"),
    })
}

/// `true` unless both notes carry a project and they differ — i.e. an
/// unscoped note (either side) is always a candidate partner, matching the
/// brief's "same project when both have one" rule.
fn same_project(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

/// Comparison partners for `new`: full-text search on `new`'s own title,
/// filtered to CONFIRMED notes of the same [`continuum_memory::NodeType`]
/// (excluding `new` itself) that share `new`'s project whenever both have
/// one set, capped to the top 2 hits (in the search's relevance order).
async fn find_partners(vault: &Vault, new: &Note) -> anyhow::Result<Vec<NodeSummary>> {
    let hits = vault.search(&new.frontmatter.title, 5).await?;
    Ok(hits
        .into_iter()
        .filter(|h| {
            h.id != new.frontmatter.id
                && h.node_type == new.frontmatter.node_type
                && h.status == NodeStatus::Confirmed
                && same_project(&new.frontmatter.project, &h.project)
        })
        .take(2)
        .collect())
}

/// Renders one note (OLD or NEW) as `"Title: ...\nCreated: ...\nBody:
/// ..."` for the `{{OLD}}`/`{{NEW}}` prompt slots.
fn render_note_block(note: &Note) -> String {
    format!(
        "Title: {}\nCreated: {}\nBody: {}",
        note.frontmatter.title,
        note.frontmatter.created.to_rfc3339(),
        note.body
    )
}

/// Fills `{{OLD}}`/`{{NEW}}` in [`CONFLICT_PROMPT`] for one comparison pair.
fn build_conflict_prompt(old: &Note, new: &Note) -> String {
    CONFLICT_PROMPT
        .replace("{{OLD}}", &render_note_block(old))
        .replace("{{NEW}}", &render_note_block(new))
}

/// Runs `prompt` through `llm` and parses the reply as a [`Verdict`], with
/// exactly one retry (parse error appended to the prompt) if the first
/// reply doesn't parse — mirroring
/// [`crate::curator::run::extract_pass`]'s retry-once policy for the
/// extraction array. Any completion error, or a parse failure that
/// survives the retry, is returned as `Err` for the caller to log and skip
/// (this one pair never blocks the rest of the batch).
async fn evaluate_pair(llm: &dyn CuratorLlm, prompt: &str) -> anyhow::Result<Verdict> {
    let raw = llm.complete(prompt, 256).await?;
    match parse_verdict(&raw) {
        Ok(v) => Ok(v),
        Err(first_err) => {
            let retry_prompt = format!(
                "{prompt}\n\nYour previous reply was invalid: {first_err}. Reply with ONLY the JSON object."
            );
            let retry_raw = llm.complete(&retry_prompt, 256).await?;
            parse_verdict(&retry_raw)
        }
    }
}

/// For each id in `new_note_ids` (freshly-written notes from an extraction
/// pass), searches the vault for up to 2 existing CONFIRMED notes on the
/// same topic (see [`find_partners`]) and asks the curator LLM whether the
/// new note supersedes or contradicts each one. A `"supersedes"` or
/// `"contradicts"` verdict with confidence >=
/// [`SUPERSEDE_CONFIDENCE_FLOOR`] appends a [`Relation`] (`rel:
/// "proposes_supersede"`) onto the **new** note pointing at the partner,
/// then saves it — skipped if that relation is already present (idempotent
/// against a re-run over the same ids). The OLD note is never read for
/// writing and never touched: this is a proposal for a human (or the
/// orchestrator) to confirm via `Vault::resolve_candidate`'s
/// `Resolution::Supersede`, not an automatic supersede.
///
/// Every failure — loading the new note, the partner search, an LLM
/// completion/parse failure that survives [`evaluate_pair`]'s one retry, or
/// the relation save — is logged and skipped at the smallest possible
/// scope (one id or one pair) rather than aborting the whole batch, mirroring
/// [`crate::curator::run::write_candidate`]'s per-candidate containment.
///
/// Returns the number of `proposes_supersede` relations actually created
/// (saved) across every id in `new_note_ids`.
///
/// The confidence floor is intentionally not threaded through
/// `CuratorConfig` yet: this function's signature is Plan B Task 5's fixed
/// contract (`vault`, `llm`, `new_note_ids` only) so it composes directly
/// with [`crate::curator::run::extract_pass`]'s output. A follow-up task
/// can widen the signature to take `&CuratorConfig` if the floor needs to
/// be dashboard-tunable.
pub async fn detect_conflicts(
    vault: &Vault,
    llm: &dyn CuratorLlm,
    new_note_ids: &[String],
) -> anyhow::Result<usize> {
    let mut created = 0usize;

    for new_id in new_note_ids {
        let mut note = match vault.get(new_id).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    layer = "memory",
                    component = "curator",
                    id = %new_id,
                    error = %e,
                    "detect_conflicts: failed to load new note; skipping"
                );
                continue;
            }
        };

        let partners = match find_partners(vault, &note).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    layer = "memory",
                    component = "curator",
                    id = %new_id,
                    error = %e,
                    "detect_conflicts: partner search failed; skipping note"
                );
                continue;
            }
        };

        for partner in partners {
            if note
                .frontmatter
                .relations
                .iter()
                .any(|r| r.rel == PROPOSES_SUPERSEDE && r.to == partner.id)
            {
                continue; // already proposed against this partner
            }

            let old_note = match vault.get(&partner.id).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(
                        layer = "memory",
                        component = "curator",
                        old = %partner.id,
                        new = %new_id,
                        error = %e,
                        "detect_conflicts: failed to load partner note; skipping pair"
                    );
                    continue;
                }
            };

            let prompt = build_conflict_prompt(&old_note, &note);
            let verdict = match evaluate_pair(llm, &prompt).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        layer = "memory",
                        component = "curator",
                        old = %partner.id,
                        new = %new_id,
                        error = %e,
                        "detect_conflicts: LLM verdict failed; skipping pair"
                    );
                    continue;
                }
            };

            let is_conflict = matches!(verdict.verdict.as_str(), "supersedes" | "contradicts");
            if !is_conflict || verdict.confidence < SUPERSEDE_CONFIDENCE_FLOOR {
                continue;
            }

            note.frontmatter.relations.push(Relation {
                to: partner.id.clone(),
                rel: PROPOSES_SUPERSEDE.to_string(),
                confidence: verdict.confidence,
            });

            match vault.save(&note).await {
                Ok(()) => created += 1,
                Err(e) => {
                    tracing::warn!(
                        layer = "memory",
                        component = "curator",
                        old = %partner.id,
                        new = %new_id,
                        error = %e,
                        "detect_conflicts: failed to save proposed relation"
                    );
                }
            }
        }
    }

    tracing::info!(
        layer = "memory",
        component = "curator",
        new_notes = new_note_ids.len(),
        proposals = created,
        "Curator conflict-detection pass complete"
    );

    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curator::MockLlm;
    use continuum_memory::{NodeType, NoteDraft, Source, Vault};

    /// Minimal `Decision` draft for conflict-detection fixtures.
    fn decision_draft(title: &str, body: &str, status: NodeStatus) -> NoteDraft {
        NoteDraft {
            node_type: NodeType::Decision,
            title: title.to_string(),
            body: body.to_string(),
            project: None,
            status,
            confidence: 0.9,
            importance: 0.5,
            source: Source::default(),
            source_ref: None,
            sensitivity: Default::default(),
            relations: vec![],
            tags: vec![],
        }
    }

    #[tokio::test]
    async fn supersede_verdict_creates_proposal_relation() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        let old = vault
            .create(decision_draft(
                "Use MongoDB",
                "We use MongoDB for the primary datastore; PostgreSQL was considered too.",
                NodeStatus::Confirmed,
            ))
            .await
            .unwrap();

        let cand = decision_draft(
            "Use PostgreSQL",
            "switching db to PostgreSQL for better relational guarantees",
            NodeStatus::Candidate,
        );
        let new = vault.create(cand).await.unwrap();

        let llm = MockLlm::scripted(vec![
            r#"{"verdict":"supersedes","confidence":0.9,"reason":"newer db decision"}"#.into(),
        ]);

        let n = detect_conflicts(&vault, &llm, std::slice::from_ref(&new.frontmatter.id))
            .await
            .unwrap();
        assert_eq!(n, 1);

        let refreshed = vault.get(&new.frontmatter.id).await.unwrap();
        assert!(refreshed
            .frontmatter
            .relations
            .iter()
            .any(|r| r.rel == "proposes_supersede" && r.to == old.frontmatter.id));

        // OLD note untouched (never auto-superseded by Qwen alone).
        assert_eq!(
            vault
                .get(&old.frontmatter.id)
                .await
                .unwrap()
                .frontmatter
                .status,
            NodeStatus::Confirmed
        );
    }

    #[tokio::test]
    async fn unrelated_verdict_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        let old = vault
            .create(decision_draft(
                "Use MongoDB",
                "We use MongoDB for the primary datastore; PostgreSQL was considered too.",
                NodeStatus::Confirmed,
            ))
            .await
            .unwrap();

        let cand = decision_draft(
            "Use PostgreSQL",
            "switching db to PostgreSQL for better relational guarantees",
            NodeStatus::Candidate,
        );
        let new = vault.create(cand).await.unwrap();

        let llm = MockLlm::scripted(vec![
            r#"{"verdict":"unrelated","confidence":0.9,"reason":"different databases, no overlap"}"#
                .into(),
        ]);

        let n = detect_conflicts(&vault, &llm, std::slice::from_ref(&new.frontmatter.id))
            .await
            .unwrap();
        assert_eq!(n, 0);

        let refreshed = vault.get(&new.frontmatter.id).await.unwrap();
        assert!(refreshed.frontmatter.relations.is_empty());

        // OLD note untouched either way.
        assert_eq!(
            vault
                .get(&old.frontmatter.id)
                .await
                .unwrap()
                .frontmatter
                .status,
            NodeStatus::Confirmed
        );
    }

    #[tokio::test]
    async fn parse_error_retries_once_then_still_creates_proposal() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = Vault::open(tmp.path()).await.unwrap();

        let old = vault
            .create(decision_draft(
                "Use MongoDB",
                "We use MongoDB for the primary datastore; PostgreSQL was considered too.",
                NodeStatus::Confirmed,
            ))
            .await
            .unwrap();

        let cand = decision_draft(
            "Use PostgreSQL",
            "switching db to PostgreSQL for better relational guarantees",
            NodeStatus::Candidate,
        );
        let new = vault.create(cand).await.unwrap();

        // First reply is not valid JSON at all; the retry (with the parse
        // error appended to the prompt) is a valid supersede verdict.
        let llm = MockLlm::scripted(vec![
            "not json".into(),
            r#"{"verdict":"supersedes","confidence":0.8,"reason":"newer db decision"}"#.into(),
        ]);

        let n = detect_conflicts(&vault, &llm, std::slice::from_ref(&new.frontmatter.id))
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(llm.calls(), 2); // initial + one retry, for this single pair

        let refreshed = vault.get(&new.frontmatter.id).await.unwrap();
        assert!(refreshed
            .frontmatter
            .relations
            .iter()
            .any(|r| r.rel == "proposes_supersede" && r.to == old.frontmatter.id));
    }

    #[test]
    fn parse_verdict_accepts_wrapped_json() {
        let raw = r#"Sure, here's my verdict: {"verdict":"contradicts","confidence":0.7,"reason":"conflicting facts"} Let me know if you need more."#;
        let v = parse_verdict(raw).unwrap();
        assert_eq!(v.verdict, "contradicts");
        assert_eq!(v.confidence, 0.7);
    }

    #[test]
    fn parse_verdict_rejects_garbage() {
        assert!(parse_verdict("no json here").is_err());
        assert!(parse_verdict(r#"{"confidence": 0.5}"#).is_err()); // missing required "verdict"
    }

    #[test]
    fn same_project_rules() {
        assert!(same_project(&None, &None));
        assert!(same_project(&Some("simcharts".into()), &None));
        assert!(same_project(&None, &Some("simcharts".into())));
        assert!(same_project(
            &Some("simcharts".into()),
            &Some("simcharts".into())
        ));
        assert!(!same_project(
            &Some("simcharts".into()),
            &Some("continuum".into())
        ));
    }
}
