//! Privacy admission boundary between ambient/session observations and memory candidates.
//!
//! The senses/context layers use a three-state privacy vocabulary while the legacy vault
//! has a separate `Sensitivity` enum. This adapter is intentionally explicit so callers
//! cannot accidentally treat "persistable recent history" as permission to create a
//! durable-memory candidate.

use serde::{Deserialize, Serialize};

use crate::model::Sensitivity;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
