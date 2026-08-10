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
    /// May exist in bounded local history, but durable memory requires explicit user confirmation.
    LocalOnly,
    /// Must not create a memory candidate or evidence object at all.
    NeverObserve,
}

/// Result of checking an observation before `MemoryCandidate` construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAdmission {
    /// The observation must be dropped at this boundary; do not construct candidate/evidence.
    RejectBeforeCandidate,
    /// Candidate construction is allowed with the given legacy-vault sensitivity.
    Candidate {
        sensitivity: Sensitivity,
        /// Whether recurrence/salience alone may ever auto-promote this candidate.
        auto_promotion_allowed: bool,
    },
}

/// Map canonical observation privacy into the legacy vault boundary.
///
/// `never_observe` is rejected before candidate construction. `local_only` maps to
/// `Sensitivity::Sensitive` but cannot auto-promote; only an explicit user-confirmation
/// path may subsequently make it durable. `cloud_allowed` maps to the normal internal
/// memory class and may be evaluated by the consolidation policy.
pub fn memory_admission(privacy: ObservationPrivacy) -> MemoryAdmission {
    match privacy {
        ObservationPrivacy::NeverObserve => MemoryAdmission::RejectBeforeCandidate,
        ObservationPrivacy::LocalOnly => MemoryAdmission::Candidate {
            sensitivity: Sensitivity::Sensitive,
            auto_promotion_allowed: false,
        },
        ObservationPrivacy::CloudAllowed => MemoryAdmission::Candidate {
            sensitivity: Sensitivity::Internal,
            auto_promotion_allowed: true,
        },
    }
}

/// Evaluate a candidate that originated from ambient/session observation using the
/// canonical privacy state as an unskippable gate.
///
/// `None` means the source was `never_observe` and the caller must not retain a candidate
/// or evidence record. `local_only` can be scored and retained as a candidate, but even a
/// generic policy that allows promotion of sensitive memories cannot turn ambient local-only
/// evidence into a durable memory. Explicit user-confirmation flows should not call this
/// function; they may use the generic consolidation primitives with separately recorded
/// confirmation evidence.
pub fn assess_observation_consolidation(
    candidate: &MemoryCandidate,
    privacy: ObservationPrivacy,
    policy: &ConsolidationPolicy,
    now: DateTime<Utc>,
) -> Option<ConsolidationDecision> {
    match memory_admission(privacy) {
        MemoryAdmission::RejectBeforeCandidate => None,
        MemoryAdmission::Candidate {
            auto_promotion_allowed,
            ..
        } => {
            let decision = assess_consolidation(candidate, policy, now);
            if !auto_promotion_allowed && decision == ConsolidationDecision::PromoteDurable {
                Some(ConsolidationDecision::KeepCandidate)
            } else {
                Some(decision)
            }
        }
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
            sensitivity: Sensitivity::Sensitive,
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
    fn local_only_is_sensitive_and_cannot_auto_promote() {
        assert_eq!(
            memory_admission(ObservationPrivacy::LocalOnly),
            MemoryAdmission::Candidate {
                sensitivity: Sensitivity::Sensitive,
                auto_promotion_allowed: false,
            }
        );
    }

    #[test]
    fn cloud_allowed_can_enter_normal_consolidation() {
        assert_eq!(
            memory_admission(ObservationPrivacy::CloudAllowed),
            MemoryAdmission::Candidate {
                sensitivity: Sensitivity::Internal,
                auto_promotion_allowed: true,
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
    fn local_only_cannot_use_sensitive_policy_as_promotion_escape_hatch() {
        let now = Utc::now();
        let candidate = strong_candidate(now);
        let mut policy = ConsolidationPolicy::default();
        policy.auto_promote_sensitive = true;
        assert_eq!(
            assess_observation_consolidation(
                &candidate,
                ObservationPrivacy::LocalOnly,
                &policy,
                now,
            ),
            Some(ConsolidationDecision::KeepCandidate)
        );
    }

    #[test]
    fn cloud_allowed_strong_recurrence_can_promote() {
        let now = Utc::now();
        let mut candidate = strong_candidate(now);
        candidate.sensitivity = Sensitivity::Internal;
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
