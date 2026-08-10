//! Provenance-aware temporal session synthesis.
//!
//! This module turns already privacy-filtered observations into bounded coherent
//! sessions. It is intentionally pure: collectors remain authoritative for facts,
//! while this layer only groups evidence and emits explicitly-labelled hypotheses.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Default maximum idle gap inside one coherent activity session.
pub const DEFAULT_SESSION_GAP_MINUTES: i64 = 12;
/// Maximum evidence rows retained in one synthesized session.
pub const MAX_SESSION_EVIDENCE: usize = 32;

/// How strongly a conclusion is supported by source evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStrength {
    /// Direct collector observation; no semantic conclusion was added.
    Observed,
    /// Multiple independent/repeated signals support the conclusion.
    StronglyInferred,
    /// Some signals support the conclusion, but alternatives remain plausible.
    WeaklyInferred,
    /// There is not enough consistent evidence to make the claim.
    Unknown,
}

/// Privacy class carried by an observation before synthesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemporalSensitivity {
    CloudAllowed,
    LocalOnly,
}

/// One bounded source observation supplied by an existing collector/store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalObservation {
    /// Stable source reference such as `context_event:42`.
    pub source_reference: String,
    /// Collector family (`window`, `process`, `file`, `vision`, `agent`, ...).
    pub source: String,
    /// Start of the observation span.
    pub started_at: DateTime<Utc>,
    /// Latest occurrence/end of the observation span.
    pub ended_at: DateTime<Utc>,
    /// Resolved project id when known.
    pub project: Option<String>,
    /// Application/process identity when known.
    pub application: Option<String>,
    /// Stable event-kind token when known.
    pub event_type: Option<String>,
    /// Scrubbed, bounded semantic summary from the source.
    pub summary: String,
    /// Source confidence in [0, 1].
    pub confidence: f32,
    /// Read/egress sensitivity inherited from the source.
    pub sensitivity: TemporalSensitivity,
}

/// Optional restrictions applied before temporal grouping.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TemporalScope {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub project: Option<String>,
    /// Cloud-bound consumers must leave this false.
    #[serde(default)]
    pub include_local_only: bool,
}

/// A conclusion plus exactly which observations support it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalConclusion {
    pub text: String,
    pub strength: EvidenceStrength,
    pub confidence: f32,
    pub source_references: Vec<String>,
}

/// One coherent process of activity through time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalSession {
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub project: Option<String>,
    pub applications: Vec<String>,
    pub conclusion: TemporalConclusion,
    pub evidence: Vec<TemporalObservation>,
    /// True when competing projects/signals lowered confidence.
    pub conflicting_signals: bool,
}

/// Result of a bounded temporal synthesis query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalContext {
    pub sessions: Vec<TemporalSession>,
    pub omitted_private: usize,
    pub omitted_out_of_scope: usize,
}

/// Pure deterministic synthesizer shared by chat/MCP/runtime consumers.
#[derive(Debug, Clone, Copy)]
pub struct TemporalSynthesizer {
    session_gap: Duration,
}

impl Default for TemporalSynthesizer {
    fn default() -> Self {
        Self::new(Duration::minutes(DEFAULT_SESSION_GAP_MINUTES))
    }
}

impl TemporalSynthesizer {
    pub fn new(session_gap: Duration) -> Self {
        Self { session_gap }
    }

    /// Filters, orders and groups observations into coherent sessions.
    ///
    /// No raw content is recovered here: callers must pass already-scrubbed
    /// summaries and source references. `local_only` evidence is excluded unless
    /// the caller explicitly selects a local-only consumer profile.
    pub fn synthesize(
        &self,
        observations: impl IntoIterator<Item = TemporalObservation>,
        scope: &TemporalScope,
    ) -> TemporalContext {
        let mut omitted_private = 0usize;
        let mut omitted_out_of_scope = 0usize;
        let mut rows = Vec::new();

        for mut row in observations {
            row.confidence = row.confidence.clamp(0.0, 1.0);
            if row.sensitivity == TemporalSensitivity::LocalOnly && !scope.include_local_only {
                omitted_private += 1;
                continue;
            }
            let overlaps = scope.since.map_or(true, |since| row.ended_at >= since)
                && scope.until.map_or(true, |until| row.started_at <= until);
            let project_matches = scope
                .project
                .as_deref()
                .map_or(true, |project| row.project.as_deref() == Some(project));
            if !overlaps || !project_matches {
                omitted_out_of_scope += 1;
                continue;
            }
            rows.push(row);
        }

        rows.sort_by_key(|row| (row.started_at, row.ended_at));
        let mut grouped: Vec<Vec<TemporalObservation>> = Vec::new();
        for row in rows {
            match grouped.last_mut() {
                None => grouped.push(vec![row]),
                Some(current) if belongs_to_session(current, &row, self.session_gap) => {
                    current.push(row)
                }
                Some(_) => grouped.push(vec![row]),
            }
        }

        TemporalContext {
            sessions: grouped.into_iter().map(synthesize_session).collect(),
            omitted_private,
            omitted_out_of_scope,
        }
    }
}

fn belongs_to_session(
    current: &[TemporalObservation],
    next: &TemporalObservation,
    session_gap: Duration,
) -> bool {
    let Some(last) = current.last() else {
        return true;
    };
    if next.started_at.signed_duration_since(last.ended_at) > session_gap {
        return false;
    }
    match (dominant_project(current), next.project.as_deref()) {
        (Some(current_project), Some(next_project)) if current_project != next_project => false,
        _ => true,
    }
}

fn synthesize_session(mut evidence: Vec<TemporalObservation>) -> TemporalSession {
    if evidence.len() > MAX_SESSION_EVIDENCE {
        let drop = evidence.len() - MAX_SESSION_EVIDENCE;
        evidence.drain(0..drop);
    }
    let started_at = evidence
        .first()
        .map(|e| e.started_at)
        .unwrap_or_else(Utc::now);
    let updated_at = evidence.last().map(|e| e.ended_at).unwrap_or(started_at);
    let project = dominant_project(&evidence).map(str::to_string);
    let project_count = evidence
        .iter()
        .filter_map(|e| e.project.as_deref())
        .collect::<BTreeSet<_>>()
        .len();
    let conflicting_signals = project_count > 1 || has_signal_conflict(&evidence);

    let applications = evidence
        .iter()
        .filter_map(|e| e.application.as_deref())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let conclusion = infer_conclusion(&evidence, conflicting_signals);
    TemporalSession {
        started_at,
        updated_at,
        project,
        applications,
        conclusion,
        evidence,
        conflicting_signals,
    }
}

fn dominant_project(evidence: &[TemporalObservation]) -> Option<&str> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for project in evidence.iter().filter_map(|e| e.project.as_deref()) {
        *counts.entry(project).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|(a_name, a_count), (b_name, b_count)| {
            a_count.cmp(b_count).then_with(|| b_name.cmp(a_name))
        })
        .map(|(project, _)| project)
}

fn has_signal_conflict(evidence: &[TemporalObservation]) -> bool {
    let has_success = evidence
        .iter()
        .any(|e| token_eq(e.event_type.as_deref(), "success"));
    let has_error = evidence
        .iter()
        .any(|e| token_eq(e.event_type.as_deref(), "error"));
    has_success && has_error
}

fn infer_conclusion(evidence: &[TemporalObservation], conflicting: bool) -> TemporalConclusion {
    let mut scores: BTreeMap<&'static str, f32> = BTreeMap::new();
    let mut refs: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();

    for row in evidence {
        let haystack = format!(
            "{} {} {} {}",
            row.source,
            row.application.as_deref().unwrap_or_default(),
            row.event_type.as_deref().unwrap_or_default(),
            row.summary
        )
        .to_ascii_lowercase();
        let weight = row.confidence.max(0.2);

        if contains_any(
            &haystack,
            &[
                "cargo test",
                "pytest",
                "test suite",
                "tests running",
                "test failed",
                "test passed",
            ],
        ) {
            add_signal(&mut scores, &mut refs, "testing", weight * 1.25, row);
        }
        if contains_any(
            &haystack,
            &["bug", "defect", "error", "failed", "failure", "diagnostic"],
        ) {
            add_signal(&mut scores, &mut refs, "debugging", weight, row);
        }
        if row.source.eq_ignore_ascii_case("file")
            || contains_any(
                &haystack,
                &["notes", "note file", "markdown", "document changed"],
            )
        {
            add_signal(&mut scores, &mut refs, "notes", weight * 0.8, row);
        }
        if contains_any(
            &haystack,
            &["browser", "pdf", "research", "documentation", "article"],
        ) {
            add_signal(&mut scores, &mut refs, "research", weight, row);
        }
    }

    let (text, key, base_strength) = if score(&scores, "testing") >= 1.0
        && score(&scores, "debugging") >= 0.8
        && score(&scores, "notes") >= 0.6
    {
        (
            "The user appears to be testing the current project and recording defects.".to_string(),
            vec!["testing", "debugging", "notes"],
            EvidenceStrength::StronglyInferred,
        )
    } else if score(&scores, "research") >= 1.2 && score(&scores, "notes") >= 0.6 {
        (
            "The user appears to be researching a topic and consolidating notes.".to_string(),
            vec!["research", "notes"],
            EvidenceStrength::StronglyInferred,
        )
    } else if let Some(project) = dominant_project(evidence) {
        (
            format!("The observed activity is mainly associated with project {project}."),
            Vec::new(),
            EvidenceStrength::WeaklyInferred,
        )
    } else {
        (
            "The current activity cannot be summarized confidently from the available evidence."
                .to_string(),
            Vec::new(),
            EvidenceStrength::Unknown,
        )
    };

    let mut source_references = if key.is_empty() {
        evidence
            .iter()
            .take(8)
            .map(|e| e.source_reference.clone())
            .collect::<Vec<_>>()
    } else {
        key.into_iter()
            .flat_map(|kind| refs.get(kind).into_iter().flatten().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .take(12)
            .collect::<Vec<_>>()
    };
    source_references.sort();

    let mean_confidence = if evidence.is_empty() {
        0.0
    } else {
        evidence.iter().map(|e| e.confidence).sum::<f32>() / evidence.len() as f32
    };
    let diversity = evidence
        .iter()
        .map(|e| e.source.to_ascii_lowercase())
        .collect::<BTreeSet<_>>()
        .len();
    let corroboration = (diversity.saturating_sub(1) as f32 * 0.08).min(0.18);
    let conflict_penalty = if conflicting { 0.25 } else { 0.0 };
    let confidence = (mean_confidence + corroboration - conflict_penalty).clamp(0.0, 0.95);
    let strength = match (base_strength, confidence) {
        (_, c) if c < 0.35 => EvidenceStrength::Unknown,
        (EvidenceStrength::StronglyInferred, c) if c < 0.68 => EvidenceStrength::WeaklyInferred,
        (other, _) => other,
    };

    TemporalConclusion {
        text,
        strength,
        confidence,
        source_references,
    }
}

fn token_eq(actual: Option<&str>, expected: &str) -> bool {
    actual.is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn add_signal(
    scores: &mut BTreeMap<&'static str, f32>,
    refs: &mut BTreeMap<&'static str, Vec<String>>,
    key: &'static str,
    amount: f32,
    row: &TemporalObservation,
) {
    *scores.entry(key).or_default() += amount;
    refs.entry(key)
        .or_default()
        .push(row.source_reference.clone());
}

fn score(scores: &BTreeMap<&'static str, f32>, key: &'static str) -> f32 {
    scores.get(key).copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::minutes(minutes)
    }

    fn obs(
        id: &str,
        minute: i64,
        source: &str,
        project: Option<&str>,
        app: Option<&str>,
        event_type: Option<&str>,
        summary: &str,
    ) -> TemporalObservation {
        TemporalObservation {
            source_reference: id.into(),
            source: source.into(),
            started_at: at(minute),
            ended_at: at(minute + 1),
            project: project.map(str::to_string),
            application: app.map(str::to_string),
            event_type: event_type.map(str::to_string),
            summary: summary.into(),
            confidence: 0.9,
            sensitivity: TemporalSensitivity::CloudAllowed,
        }
    }

    #[test]
    fn continuum_testing_is_inferred_from_corroborating_sources() {
        let rows = vec![
            obs(
                "window:1",
                0,
                "window",
                Some("continuum"),
                Some("code"),
                None,
                "Editor open on Continuum",
            ),
            obs(
                "process:2",
                2,
                "process",
                Some("continuum"),
                Some("cargo"),
                None,
                "cargo test suite running",
            ),
            obs(
                "event:3",
                4,
                "triage",
                Some("continuum"),
                Some("continuum"),
                Some("error"),
                "test failed in desktop integration",
            ),
            obs(
                "file:4",
                5,
                "file",
                Some("continuum"),
                Some("code"),
                None,
                "bug notes document changed",
            ),
        ];
        let out = TemporalSynthesizer::default().synthesize(rows, &TemporalScope::default());
        assert_eq!(out.sessions.len(), 1);
        let session = &out.sessions[0];
        assert_eq!(session.project.as_deref(), Some("continuum"));
        assert_eq!(
            session.conclusion.strength,
            EvidenceStrength::StronglyInferred
        );
        assert!(session.conclusion.text.contains("testing"));
        assert!(session.conclusion.source_references.len() >= 3);
    }

    #[test]
    fn research_windows_and_notes_form_one_session() {
        let rows = vec![
            obs(
                "w:1",
                0,
                "window",
                Some("school"),
                Some("browser"),
                None,
                "browser research on network protocols",
            ),
            obs(
                "v:2",
                5,
                "vision",
                Some("school"),
                Some("browser"),
                None,
                "PDF documentation about network protocols",
            ),
            obs(
                "f:3",
                9,
                "file",
                Some("school"),
                Some("editor"),
                None,
                "research notes document changed",
            ),
        ];
        let out = TemporalSynthesizer::default().synthesize(rows, &TemporalScope::default());
        assert_eq!(out.sessions.len(), 1);
        assert!(out.sessions[0].conclusion.text.contains("researching"));
    }

    #[test]
    fn time_gap_or_project_switch_starts_new_session() {
        let rows = vec![
            obs("a", 0, "window", Some("one"), Some("code"), None, "editing"),
            obs(
                "b",
                30,
                "window",
                Some("one"),
                Some("code"),
                None,
                "editing",
            ),
            obs(
                "c",
                31,
                "window",
                Some("two"),
                Some("browser"),
                None,
                "research",
            ),
        ];
        let out = TemporalSynthesizer::default().synthesize(rows, &TemporalScope::default());
        assert_eq!(out.sessions.len(), 3);
    }

    #[test]
    fn conflicting_signals_reduce_confidence() {
        let rows = vec![
            obs(
                "e",
                0,
                "triage",
                Some("p"),
                Some("cargo"),
                Some("error"),
                "test failed",
            ),
            obs(
                "s",
                1,
                "triage",
                Some("p"),
                Some("cargo"),
                Some("success"),
                "test passed",
            ),
            obs(
                "n",
                2,
                "file",
                Some("p"),
                Some("code"),
                None,
                "bug notes changed",
            ),
        ];
        let out = TemporalSynthesizer::default().synthesize(rows, &TemporalScope::default());
        assert!(out.sessions[0].conflicting_signals);
        assert!(out.sessions[0].conclusion.confidence < 0.9);
    }

    #[test]
    fn cloud_scope_withholds_local_only_and_counts_it() {
        let mut private = obs(
            "private",
            0,
            "window",
            Some("p"),
            Some("browser"),
            None,
            "private research",
        );
        private.sensitivity = TemporalSensitivity::LocalOnly;
        let public = obs(
            "public",
            1,
            "window",
            Some("p"),
            Some("code"),
            None,
            "editor open",
        );
        let out = TemporalSynthesizer::default()
            .synthesize(vec![private, public], &TemporalScope::default());
        assert_eq!(out.omitted_private, 1);
        assert_eq!(out.sessions[0].evidence.len(), 1);
        assert_eq!(out.sessions[0].evidence[0].source_reference, "public");
    }

    #[test]
    fn bounded_time_and_project_scope_never_leaks_other_rows() {
        let rows = vec![
            obs("old", 0, "window", Some("p"), Some("code"), None, "old"),
            obs(
                "wanted",
                20,
                "window",
                Some("p"),
                Some("code"),
                None,
                "wanted",
            ),
            obs(
                "other",
                21,
                "window",
                Some("q"),
                Some("code"),
                None,
                "other project",
            ),
        ];
        let scope = TemporalScope {
            since: Some(at(15)),
            until: Some(at(25)),
            project: Some("p".into()),
            include_local_only: false,
        };
        let out = TemporalSynthesizer::default().synthesize(rows, &scope);
        assert_eq!(out.omitted_out_of_scope, 2);
        assert_eq!(out.sessions.len(), 1);
        assert_eq!(out.sessions[0].evidence[0].source_reference, "wanted");
    }
}
