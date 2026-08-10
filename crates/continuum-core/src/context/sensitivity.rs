//! Canonical sensitivity vocabulary for privacy-filtered context evidence.
//!
//! This type is deliberately feature-light: context synthesis is linked by the
//! runtime, MCP server and desktop application, including builds that disable
//! heavy runtime dependencies. Runtime event code re-exports this exact type so
//! persisted schemas and downstream imports keep one additive-only vocabulary.

use serde::{Deserialize, Serialize};

/// Sensitivity inherited from the strictest privacy zone of an observation.
/// `never_observe` has no representable event value because excluded content
/// must not create an observation, event, content hash, cache record or memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSensitivity {
    /// May remain in bounded local-only processing and storage, but never in a
    /// cloud-bound context or reusable cross-scope cache.
    LocalOnly,
    /// Eligible for policy-controlled cloud context after redaction.
    CloudAllowed,
}
