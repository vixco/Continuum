//! Connected-memory and world-model policies layered over the existing vault graph.
//!
//! The vault markdown remains the source of truth and SQLite remains a rebuildable
//! projection. This module deliberately does not introduce a second graph store.
//! It provides pure, deterministic building blocks for deciding when repeated
//! observations are durable enough to promote and for ranking/traversing connected
//! memory without treating stale or weak evidence as current truth.

use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{GraphData, GraphEdge, NodeStatus, Sensitivity, Source};

fn default_one() -> f32 {
    1.0
}

/// A source reference supporting a candidate durable memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRef {
    /// Stable event/frame/session/evidence identifier. Never raw private content.
    pub reference: String,
    /// How Continuum obtained the evidence.
    pub source: Source,
    /// Session identifier used to distinguish recurrence from repetition in one session.
    pub session_id: String,
    /// When this evidence was observed.
    pub observed_at: DateTime<Utc>,
    /// Epistemic confidence in this individual observation.
    #[serde(default = "default_one")]
    pub confidence: f32,
}

/// A normalized candidate proposed by session/context processing before it becomes durable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryCandidate {
    /// Stable semantic key chosen by the producer, e.g. `project:continuum:active-development`.
    pub key: String,
    /// Human-readable summary; callers should already have redacted sensitive material.
    pub summary: String,
    /// Optional project scope used to prevent unrelated evidence from being merged.
    #[serde(default)]
    pub project: Option<String>,
    /// Salience in `[0, 1]`.
    pub salience: f32,
    /// Candidate-level confidence in `[0, 1]`.
    pub confidence: f32,
    /// Sensitivity inherited from the strongest input evidence.
    pub sensitivity: Sensitivity,
    /// Evidence references supporting this candidate.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// Configurable promotion policy. Values belong in config at integration time rather than
/// being treated as immutable product defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationPolicy {
    pub min_observations: usize,
    pub min_distinct_sessions: usize,
    pub min_confidence: f32,
    pub min_salience: f32,
    pub evidence_horizon_days: i64,
    /// Sensitive observations remain short-lived unless a higher layer gets explicit
    /// user confirmation; this module never auto-promotes them.
    pub auto_promote_sensitive: bool,
}

impl Default for ConsolidationPolicy {
    fn default() -> Self {
        Self {
            min_observations: 3,
            min_distinct_sessions: 2,
            min_confidence: 0.72,
            min_salience: 0.65,
            evidence_horizon_days: 30,
            auto_promote_sensitive: false,
        }
    }
}

/// Result of evaluating a candidate for durable storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationDecision {
    /// The candidate is too weak/transient to retain beyond the session layer.
    RejectTransient,
    /// Keep as a candidate; more evidence or explicit confirmation is required.
    KeepCandidate,
    /// Enough independent, recent evidence exists for durable promotion.
    PromoteDurable,
}

/// Evaluate recurrence, salience, confidence, sensitivity and evidence freshness.
///
/// Evidence is deduplicated by reference. Repeated observations from one session count
/// toward recurrence but cannot satisfy the distinct-session requirement on their own.
pub fn assess_consolidation(
    candidate: &MemoryCandidate,
    policy: &ConsolidationPolicy,
    now: DateTime<Utc>,
) -> ConsolidationDecision {
    if candidate.summary.trim().is_empty()
        || candidate.salience < policy.min_salience
        || candidate.confidence < policy.min_confidence
    {
        return ConsolidationDecision::RejectTransient;
    }

    if candidate.sensitivity == Sensitivity::Sensitive && !policy.auto_promote_sensitive {
        return ConsolidationDecision::KeepCandidate;
    }

    let cutoff = now - Duration::days(policy.evidence_horizon_days.max(0));
    let mut references = HashSet::new();
    let mut sessions = HashSet::new();
    let mut observations = 0usize;

    for evidence in &candidate.evidence {
        if evidence.observed_at < cutoff || evidence.confidence < policy.min_confidence {
            continue;
        }
        if references.insert(evidence.reference.as_str()) {
            observations += 1;
            sessions.insert(evidence.session_id.as_str());
        }
    }

    if observations >= policy.min_observations && sessions.len() >= policy.min_distinct_sessions {
        ConsolidationDecision::PromoteDurable
    } else {
        ConsolidationDecision::KeepCandidate
    }
}

/// Lifecycle state for a relation or derived belief.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeState {
    Observed,
    Inferred,
    Confirmed,
    Disputed,
    Superseded,
    Expired,
}

impl KnowledgeState {
    /// Whether this state is eligible for current-context retrieval by default.
    pub fn is_current(self) -> bool {
        matches!(self, Self::Observed | Self::Inferred | Self::Confirmed)
    }
}

/// Evidence-backed relation contract for context/session producers and the Memory UI.
///
/// This is an additive wire contract. Existing `Relation` frontmatter remains valid; a
/// migration can populate these fields gradually because this structure is not required
/// to parse legacy notes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectedRelation {
    pub from: String,
    pub to: String,
    pub rel: String,
    pub confidence: f32,
    pub state: KnowledgeState,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default)]
    pub valid_from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub valid_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub supersedes: Option<String>,
    #[serde(default)]
    pub contradicted_by: Option<String>,
}

impl ConnectedRelation {
    /// True only when lifecycle, temporal validity and contradiction state all allow the
    /// relation to be treated as current.
    pub fn is_current_at(&self, now: DateTime<Utc>) -> bool {
        self.state.is_current()
            && self.contradicted_by.is_none()
            && self.valid_from.is_none_or(|from| from <= now)
            && self.valid_until.is_none_or(|until| now < until)
    }
}

/// One candidate result supplied by text/vector retrieval and enriched with graph signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HybridCandidate {
    pub id: String,
    /// Cosine/vector similarity normalized to `[0, 1]` by the caller.
    pub semantic_similarity: f32,
    /// Relationship affinity to seeds/project, normalized to `[0, 1]`.
    pub relation_affinity: f32,
    /// Recency score normalized to `[0, 1]`.
    pub recency: f32,
    pub confidence: f32,
    /// Evidence quality/quantity normalized to `[0, 1]`.
    pub evidence_strength: f32,
    /// 1.0 exact project match, 0.5 unscoped, 0.0 conflicting project.
    pub project_affinity: f32,
    pub status: NodeStatus,
}

/// Configurable weights for hybrid retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HybridWeights {
    pub semantic: f32,
    pub relation: f32,
    pub recency: f32,
    pub confidence: f32,
    pub evidence: f32,
    pub project: f32,
}

impl Default for HybridWeights {
    fn default() -> Self {
        Self {
            semantic: 0.34,
            relation: 0.22,
            recency: 0.12,
            confidence: 0.12,
            evidence: 0.12,
            project: 0.08,
        }
    }
}

/// Ranked connected-memory hit with an explainable score breakdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedHit {
    pub id: String,
    pub score: f32,
    pub semantic_component: f32,
    pub relation_component: f32,
    pub recency_component: f32,
    pub confidence_component: f32,
    pub evidence_component: f32,
    pub project_component: f32,
}

fn unit(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

/// Rank already-bounded vector/FTS candidates with relationship, lifecycle and evidence
/// signals. Rejected, superseded and archived nodes are omitted by default.
pub fn rank_hybrid(
    candidates: impl IntoIterator<Item = HybridCandidate>,
    weights: HybridWeights,
    limit: usize,
) -> Vec<RankedHit> {
    let mut ranked: Vec<RankedHit> = candidates
        .into_iter()
        .filter(|candidate| {
            matches!(candidate.status, NodeStatus::Candidate | NodeStatus::Confirmed)
        })
        .map(|candidate| {
            let semantic_component = unit(candidate.semantic_similarity) * weights.semantic;
            let relation_component = unit(candidate.relation_affinity) * weights.relation;
            let recency_component = unit(candidate.recency) * weights.recency;
            let confidence_component = unit(candidate.confidence) * weights.confidence;
            let evidence_component = unit(candidate.evidence_strength) * weights.evidence;
            let project_component = unit(candidate.project_affinity) * weights.project;
            let score = semantic_component
                + relation_component
                + recency_component
                + confidence_component
                + evidence_component
                + project_component;
            RankedHit {
                id: candidate.id,
                score,
                semantic_component,
                relation_component,
                recency_component,
                confidence_component,
                evidence_component,
                project_component,
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.id.cmp(&b.id))
    });
    ranked.truncate(limit);
    ranked
}

/// Bounded breadth-first expansion over one already-fetched graph snapshot.
///
/// This intentionally avoids per-node database queries (no N+1). Callers fetch one
/// bounded `GraphData` snapshot from the existing SQLite index, then traverse locally.
pub fn bounded_related_nodes(
    graph: &GraphData,
    seed_ids: &[String],
    max_depth: usize,
    max_nodes: usize,
    min_edge_confidence: f32,
) -> Vec<String> {
    if max_nodes == 0 || seed_ids.is_empty() {
        return Vec::new();
    }

    let current: HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|node| matches!(node.status, NodeStatus::Candidate | NodeStatus::Confirmed))
        .map(|node| node.id.as_str())
        .collect();

    let mut adjacency: HashMap<&str, Vec<&GraphEdge>> = HashMap::new();
    for edge in &graph.edges {
        if edge.confidence < min_edge_confidence
            || !current.contains(edge.from.as_str())
            || !current.contains(edge.to.as_str())
        {
            continue;
        }
        adjacency.entry(edge.from.as_str()).or_default().push(edge);
        adjacency.entry(edge.to.as_str()).or_default().push(edge);
    }

    let mut queue = VecDeque::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for seed in seed_ids {
        if current.contains(seed.as_str()) && seen.insert(seed.as_str()) {
            queue.push_back((seed.as_str(), 0usize));
        }
    }

    let mut result = Vec::new();
    while let Some((node, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        if let Some(edges) = adjacency.get(node) {
            for edge in edges {
                let other = if edge.from == node { edge.to.as_str() } else { edge.from.as_str() };
                if seen.insert(other) {
                    result.push(other.to_string());
                    if result.len() >= max_nodes {
                        return result;
                    }
                    queue.push_back((other, depth + 1));
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GraphNode, NodeType};

    fn evidence(reference: &str, session: &str, at: DateTime<Utc>) -> EvidenceRef {
        EvidenceRef {
            reference: reference.to_string(),
            source: Source::Observed,
            session_id: session.to_string(),
            observed_at: at,
            confidence: 0.9,
        }
    }

    #[test]
    fn repeated_continuum_activity_across_sessions_promotes() {
        let now = Utc::now();
        let candidate = MemoryCandidate {
            key: "project:continuum:active-development".into(),
            summary: "User is actively developing and testing Continuum".into(),
            project: Some("continuum".into()),
            salience: 0.9,
            confidence: 0.88,
            sensitivity: Sensitivity::Internal,
            evidence: vec![
                evidence("event:edit-1", "session-a", now - Duration::days(2)),
                evidence("event:test-1", "session-a", now - Duration::days(2)),
                evidence("event:docs-1", "session-b", now - Duration::days(1)),
            ],
        };

        assert_eq!(
            assess_consolidation(&candidate, &ConsolidationPolicy::default(), now),
            ConsolidationDecision::PromoteDurable
        );
    }

    #[test]
    fn one_unrelated_observation_stays_candidate() {
        let now = Utc::now();
        let candidate = MemoryCandidate {
            key: "concept:unrelated".into(),
            summary: "One unrelated app was visible".into(),
            project: None,
            salience: 0.8,
            confidence: 0.8,
            sensitivity: Sensitivity::Internal,
            evidence: vec![evidence("event:1", "session-a", now)],
        };
        assert_eq!(
            assess_consolidation(&candidate, &ConsolidationPolicy::default(), now),
            ConsolidationDecision::KeepCandidate
        );
    }

    #[test]
    fn low_salience_transient_data_is_rejected() {
        let now = Utc::now();
        let candidate = MemoryCandidate {
            key: "transient:error".into(),
            summary: "Transient spinner".into(),
            project: Some("continuum".into()),
            salience: 0.2,
            confidence: 0.95,
            sensitivity: Sensitivity::Internal,
            evidence: vec![evidence("event:1", "session-a", now)],
        };
        assert_eq!(
            assess_consolidation(&candidate, &ConsolidationPolicy::default(), now),
            ConsolidationDecision::RejectTransient
        );
    }

    #[test]
    fn sensitive_data_never_auto_promotes_by_default() {
        let now = Utc::now();
        let candidate = MemoryCandidate {
            key: "sensitive:test".into(),
            summary: "Redacted sensitive observation".into(),
            project: Some("continuum".into()),
            salience: 1.0,
            confidence: 1.0,
            sensitivity: Sensitivity::Sensitive,
            evidence: vec![
                evidence("event:1", "a", now),
                evidence("event:2", "b", now),
                evidence("event:3", "c", now),
            ],
        };
        assert_eq!(
            assess_consolidation(&candidate, &ConsolidationPolicy::default(), now),
            ConsolidationDecision::KeepCandidate
        );
    }

    #[test]
    fn contradicted_and_expired_relations_are_not_current() {
        let now = Utc::now();
        let relation = ConnectedRelation {
            from: "goal".into(),
            to: "blocker".into(),
            rel: "blocked_by".into(),
            confidence: 0.9,
            state: KnowledgeState::Confirmed,
            evidence: vec![],
            valid_from: Some(now - Duration::days(1)),
            valid_until: Some(now + Duration::days(1)),
            supersedes: None,
            contradicted_by: Some("relation:newer".into()),
        };
        assert!(!relation.is_current_at(now));

        let expired = ConnectedRelation {
            contradicted_by: None,
            valid_until: Some(now - Duration::seconds(1)),
            ..relation
        };
        assert!(!expired.is_current_at(now));
    }

    #[test]
    fn hybrid_ranking_rewards_project_and_relationship_evidence() {
        let candidates = vec![
            HybridCandidate {
                id: "semantic-only".into(),
                semantic_similarity: 0.95,
                relation_affinity: 0.05,
                recency: 0.5,
                confidence: 0.6,
                evidence_strength: 0.4,
                project_affinity: 0.0,
                status: NodeStatus::Confirmed,
            },
            HybridCandidate {
                id: "connected".into(),
                semantic_similarity: 0.78,
                relation_affinity: 1.0,
                recency: 0.9,
                confidence: 0.9,
                evidence_strength: 0.9,
                project_affinity: 1.0,
                status: NodeStatus::Confirmed,
            },
            HybridCandidate {
                id: "stale".into(),
                semantic_similarity: 1.0,
                relation_affinity: 1.0,
                recency: 1.0,
                confidence: 1.0,
                evidence_strength: 1.0,
                project_affinity: 1.0,
                status: NodeStatus::Superseded,
            },
        ];
        let ranked = rank_hybrid(candidates, HybridWeights::default(), 10);
        assert_eq!(ranked[0].id, "connected");
        assert_eq!(ranked.len(), 2);
    }

    fn graph_node(id: &str, status: NodeStatus) -> GraphNode {
        GraphNode {
            id: id.into(),
            slug: id.into(),
            title: id.into(),
            node_type: NodeType::Note,
            status,
            project: Some("continuum".into()),
            confidence: 1.0,
            importance: 1.0,
            created: "2026-08-10T00:00:00Z".into(),
            updated: "2026-08-10T00:00:00Z".into(),
        }
    }

    #[test]
    fn traversal_is_bounded_and_skips_superseded_nodes() {
        let graph = GraphData {
            nodes: vec![
                graph_node("project", NodeStatus::Confirmed),
                graph_node("goal", NodeStatus::Confirmed),
                graph_node("task", NodeStatus::Confirmed),
                graph_node("old", NodeStatus::Superseded),
            ],
            edges: vec![
                GraphEdge { from: "project".into(), to: "goal".into(), rel: "has_goal".into(), confidence: 1.0, origin: "frontmatter".into() },
                GraphEdge { from: "goal".into(), to: "task".into(), rel: "depends_on".into(), confidence: 0.9, origin: "frontmatter".into() },
                GraphEdge { from: "task".into(), to: "old".into(), rel: "supersedes".into(), confidence: 1.0, origin: "frontmatter".into() },
            ],
            ghosts: vec![],
            truncated: false,
        };
        let related = bounded_related_nodes(&graph, &["project".into()], 2, 10, 0.5);
        assert_eq!(related, vec!["goal".to_string(), "task".to_string()]);
    }
}
