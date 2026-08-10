//! Privacy admission boundary between ambient/session observations and memory candidates.
//!
//! The senses/context layers use a three-state privacy vocabulary while the legacy vault
//! has a separate `Sensitivity` enum. This adapter is intentionally explicit so callers
//! cannot accidentally treat "persistable recent history" as permission to create a
//! durable-memory candidate.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::Sensitivity;
use crate::world_model::{
    assess_consolidation, ConsolidationDecision, ConsolidationPolicy, MemoryCandidate,
};

/// Canonical observation privacy state supplied by A2/A3-style producers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationPrivacy {
    /// Eligible for normal candidate-memory processing after ordinary salience checks.
    CloudAllowed,
    /// May exist in bounded local history, but ambient/session evidence may not create
    /// a memory candidate without a separate explicit-confirmation path.
    LocalOnly,
    /// Must not create a memory candidate or evidence object at all.
    NeverObserve,
}

/// Result of checking an observation before `MemoryCandidate` construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAdmission {
    /// The observation must be dropped at this boundary; do not construct candidate/evidence.
    RejectBeforeCandidate,
    /// The observation may remain in bounded local history, but automatic memory-candidate
    /// construction is forbidden. A separately recorded explicit user confirmation may
    /// create a manual/sensitive candidate through a non-observation flow.
    RequireExplicitConfirmation,
    /// Candidate construction is allowed with the given legacy-vault sensitivity.
    Candidate {
        sensitivity: Sensitivity,
    },
}

/// Map canonical observation privacy into the legacy vault boundary.
///
/// `never_observe` is rejected before candidate construction. `local_only` is not admitted
/// to automatic memory-candidate construction at all; retaining it in bounded local history
/// is a different permission. Only `cloud_allowed` enters normal consolidation.
pub fn memory_admission(privacy: ObservationPrivacy) -> MemoryAdmission {
    match privacy {
        ObservationPrivacy::NeverObserve => MemoryAdmission::RejectBeforeCandidate,
        ObservationPrivacy::LocalOnly => MemoryAdmission::RequireExplicitConfirmation,
        ObservationPrivacy::CloudAllowed => MemoryAdmission::Candidate {
            sensitivity: Sensitivity::Internal,
        },
    }
}

/// Evaluate a candidate that originated from ambient/session observation using the
/// canonical privacy state as an unskippable gate.
///
/// `None` means the source is not eligible for automatic memory-candidate processing.
/// This covers both `never_observe` (which must not leave a candidate/evidence object at
/// all) and `local_only` (which may exist only in bounded local history until a separate
/// explicit-confirmation/manual flow creates memory). Explicit confirmation flows should
/// not call this function; they may use the generic consolidation primitives with separately
/// recorded confirmation provenance.
pub fn assess_observation_consolidation(
    candidate: &MemoryCandidate,
    privacy: ObservationPrivacy,
    policy: &ConsolidationPolicy,
    now: DateTime<Utc>,
) -> Option<ConsolidationDecision> {
    match memory_admission(privacy) {
        MemoryAdmission::RejectBeforeCandidate | MemoryAdmission::RequireExplicitConfirmation => {
            None
        }
        MemoryAdmission::Candidate { .. } => Some(assess_consolidation(candidate, policy, now)),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::model::Source;
    use crate::world_model::EvidenceRef;

    fn strong_candidate(now: DateTime<Utc>) -> MemoryCandidate {
        MemoryCandidate {
            key: "project:continuum:active".into(),
            summary: "Repeated synthetic Continuum work".into(),
            project: Some("continuum".into()),
            salience: 1.0,
            confidence: 1.0,
            sensitivity: Sensitivity::Internal,
            evidence: vec![
                EvidenceRef {
                    reference: "observation:hash-a".into(),
                    source: Source::Observed,
                    session_id: "session-a".into(),
                    observed_at: now - Duration::days(2),
                    confidence: 1.0,
                },
                EvidenceRef {
                    reference: "observation:hash-b".into(),
                    source: Source::Observed,
                    session_id: "session-a".into(),
                    observed_at: now - Duration::days(1),
                    confidence: 1.0,
                },
                EvidenceRef {
                    reference: "observation:hash-c".into(),
                    source: Source::Observed,
                    session_id: "session-b".into(),
                    observed_at: now,
                    confidence: 1.0,
                },
            ],
        }
    }

    #[test]
    fn never_observe_is_rejected_before_candidate_construction() {
        assert_eq!(
            memory_admission(ObservationPrivacy::NeverObserve),
            MemoryAdmission::RejectBeforeCandidate
        );
    }

    #[test]
    fn local_only_requires_explicit_confirmation_before_candidate_construction() {
        assert_eq!(
            memory_admission(ObservationPrivacy::LocalOnly),
            MemoryAdmission::RequireExplicitConfirmation
        );
    }

    #[test]
    fn cloud_allowed_can_enter_normal_consolidation() {
        assert_eq!(
            memory_admission(ObservationPrivacy::CloudAllowed),
            MemoryAdmission::Candidate {
                sensitivity: Sensitivity::Internal,
            }
        );
    }

    #[test]
    fn never_observe_has_no_consolidation_result() {
        let now = Utc::now();
        let candidate = strong_candidate(now);
        assert_eq!(
            assess_observation_consolidation(
                &candidate,
                ObservationPrivacy::NeverObserve,
                &ConsolidationPolicy::default(),
                now,
            ),
            None
        );
    }

    #[test]
    fn local_only_never_enters_automatic_candidate_consolidation() {
        let now = Utc::now();
        let mut candidate = strong_candidate(now);
        candidate.sensitivity = Sensitivity::Sensitive;
        let mut policy = ConsolidationPolicy::default();
        policy.auto_promote_sensitive = true;
        assert_eq!(
            assess_observation_consolidation(
                &candidate,
                ObservationPrivacy::LocalOnly,
                &policy,
                now,
            ),
            None
        );
    }

    #[test]
    fn cloud_allowed_strong_recurrence_can_promote() {
        let now = Utc::now();
        let candidate = strong_candidate(now);
        assert_eq!(
            assess_observation_consolidation(
                &candidate,
                ObservationPrivacy::CloudAllowed,
                &ConsolidationPolicy::default(),
                now,
            ),
            Some(ConsolidationDecision::PromoteDurable)
        );
    }
}
