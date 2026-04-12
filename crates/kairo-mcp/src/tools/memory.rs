//! # Memory tools (`mcp__kairo__memory_*`)
//!
//! - `memory_query_episodic` — vector search over episodic memory (LanceDB)
//! - `memory_list_facts` — list semantic facts, optionally filtered by prefix
//! - `memory_get_fact` — fetch a single semantic fact by key
//! - `memory_set_fact` — upsert a semantic fact (reserved prefixes rejected)
//!
//! Request/response types live here. The tool methods themselves are declared
//! on [`crate::KairoMcpServer`] in `server.rs` so rmcp's `#[tool_router]` macro
//! can register them all in one dispatch table.

use chrono::{DateTime, Utc};
use kairo_core::memory::{
    episodic::EventKind,
    semantic::{Fact, FactSource},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// memory_query_episodic
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MemoryQueryEpisodicRequest {
    /// Natural-language query to search for similar past events.
    pub query: String,
    /// Maximum number of matches to return. Default 5, max 25.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct EpisodicHit {
    pub id: String,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub summary: String,
    pub importance: f32,
    pub tags: Vec<String>,
    /// L2 distance from the query vector. Lower = more similar.
    pub distance: f32,
}

// ---------------------------------------------------------------------------
// memory_list_facts
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MemoryListFactsRequest {
    /// Optional key prefix filter (e.g. "project." or "user.preferences.").
    /// When omitted, returns the most recently updated facts.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Maximum number of facts to return. Default 50, max 200.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FactView {
    pub key: String,
    pub value: String,
    pub confidence: f32,
    pub source: String,
    pub updated_at: DateTime<Utc>,
}

impl From<Fact> for FactView {
    fn from(f: Fact) -> Self {
        Self {
            key: f.key,
            value: f.value,
            confidence: f.confidence,
            source: f.source.as_str().to_string(),
            updated_at: f.updated_at,
        }
    }
}

// ---------------------------------------------------------------------------
// memory_get_fact
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MemoryGetFactRequest {
    /// Dotted key (e.g. "user.name", "project.kairo.stack").
    pub key: String,
}

// ---------------------------------------------------------------------------
// memory_set_fact
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MemorySetFactRequest {
    /// Dotted key (e.g. "user.preferred_language"). Keys starting with
    /// "system." or "kairo." are reserved and rejected.
    pub key: String,
    /// Value to store (JSON-encoded string is fine, plain string works too).
    pub value: String,
    /// How Kairo learned this. Defaults to "inferred". "user_stated" is only
    /// accepted when the user explicitly told Kairo this fact during the wake.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SetFactResponse {
    pub key: String,
    pub stored: bool,
    pub confidence: f32,
}

/// Returns true if the key starts with a reserved namespace that the
/// orchestrator is not allowed to overwrite. Kairo's own internal state lives
/// under these prefixes and is managed by the runtime.
pub fn is_reserved_key(key: &str) -> bool {
    key.starts_with("system.") || key.starts_with("kairo.")
}

/// Parses a source string into a [`FactSource`]. Defaults to `Inferred` if
/// unrecognized — we deliberately do NOT accept `UserStated` without an
/// explicit match, so Opus cannot claim user-stated provenance on guesses.
pub fn parse_source(s: Option<&str>) -> FactSource {
    match s {
        Some("user_stated") => FactSource::UserStated,
        Some("observed") => FactSource::Observed,
        _ => FactSource::Inferred,
    }
}

/// Confidence clamp for orchestrator-written facts. User-stated facts in Kairo
/// Core are stored at 1.0; inferred facts from an LLM should never claim that.
pub fn clamp_confidence(source: FactSource, requested: f32) -> f32 {
    let ceiling = match source {
        FactSource::UserStated => 0.9, // orchestrator-claimed "user_stated" is still softer than direct
        FactSource::Observed => 0.8,
        FactSource::Inferred => 0.7,
    };
    requested.clamp(0.0, ceiling)
}

// ---------------------------------------------------------------------------
// Helpers for constructing events
// ---------------------------------------------------------------------------

/// Translates an episodic event kind string into the canonical enum variant.
/// Used by the search result mapper.
pub fn kind_to_string(kind: EventKind) -> String {
    kind.as_str().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_prefixes_rejected() {
        assert!(is_reserved_key("system.version"));
        assert!(is_reserved_key("kairo.config.mode"));
    }

    #[test]
    fn non_reserved_keys_allowed() {
        assert!(!is_reserved_key("user.name"));
        assert!(!is_reserved_key("project.simcharts.dir"));
        assert!(!is_reserved_key("routine.morning_start"));
    }

    #[test]
    fn reserved_check_is_prefix_only() {
        // "kairosystem." should NOT be reserved (the dot is part of the prefix).
        assert!(!is_reserved_key("kairosystem.x"));
        // "systems.foo" should NOT be reserved either.
        assert!(!is_reserved_key("systems.foo"));
    }

    #[test]
    fn source_parse_defaults_to_inferred() {
        assert_eq!(parse_source(None), FactSource::Inferred);
        assert_eq!(parse_source(Some("nonsense")), FactSource::Inferred);
        assert_eq!(parse_source(Some("user_stated")), FactSource::UserStated);
        assert_eq!(parse_source(Some("observed")), FactSource::Observed);
    }

    #[test]
    fn confidence_clamped_by_source() {
        assert_eq!(clamp_confidence(FactSource::Inferred, 0.99), 0.7);
        assert_eq!(clamp_confidence(FactSource::Observed, 0.99), 0.8);
        assert_eq!(clamp_confidence(FactSource::UserStated, 0.99), 0.9);
        assert!((clamp_confidence(FactSource::Inferred, 0.5) - 0.5).abs() < 1e-6);
    }
}
