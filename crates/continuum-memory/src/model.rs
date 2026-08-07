//! Data types for the memory vault. All wire types serialize snake_case to
//! match the rest of the Continuum IPC surface.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

fn default_score() -> f32 {
    0.5
}
fn default_rel_confidence() -> f32 {
    1.0
}

/// The kind of memory a node represents (Continuum.md §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Project,
    Goal,
    Task,
    Decision,
    Person,
    Preference,
    Fact,
    Error,
    Session,
    Note,
}

impl NodeType {
    /// Vault subdirectory for this type.
    pub fn folder(&self) -> &'static str {
        match self {
            Self::Project => "projects",
            Self::Goal => "goals",
            Self::Task => "tasks",
            Self::Decision => "decisions",
            Self::Person => "people",
            Self::Preference => "preferences",
            Self::Fact => "facts",
            Self::Error => "errors",
            Self::Session => "sessions",
            Self::Note => "notes",
        }
    }

    /// All types, used to create vault folders and validate filters.
    pub const ALL: [NodeType; 10] = [
        Self::Project,
        Self::Goal,
        Self::Task,
        Self::Decision,
        Self::Person,
        Self::Preference,
        Self::Fact,
        Self::Error,
        Self::Session,
        Self::Note,
    ];

    /// snake_case string form (matches serde).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Goal => "goal",
            Self::Task => "task",
            Self::Decision => "decision",
            Self::Person => "person",
            Self::Preference => "preference",
            Self::Fact => "fact",
            Self::Error => "error",
            Self::Session => "session",
            Self::Note => "note",
        }
    }

    /// Parse the snake_case form; `None` for unknown strings.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == s)
    }
}

/// Lifecycle status of a node (spec: candidate → confirmed | rejected |
/// superseded | archived).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Candidate,
    #[default]
    Confirmed,
    Rejected,
    Superseded,
    Archived,
}

impl NodeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Confirmed => "confirmed",
            Self::Rejected => "rejected",
            Self::Superseded => "superseded",
            Self::Archived => "archived",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Candidate,
            Self::Confirmed,
            Self::Rejected,
            Self::Superseded,
            Self::Archived,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }
}

/// How Continuum learned a memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    UserStatement,
    Observed,
    Inferred,
    AgentRun,
    Chat,
    #[default]
    Manual,
}

impl Source {
    /// snake_case string form (matches serde). Mirrors [`NodeType::as_str`]
    /// above — lets callers get a label string without round-tripping
    /// through `serde_json::to_value` (previously duplicated as
    /// `source_label` in `continuum-core`'s `orchestrator::wake_context`
    /// and `source_to_string` in `continuum-mcp`'s `tools::memory`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserStatement => "user_statement",
            Self::Observed => "observed",
            Self::Inferred => "inferred",
            Self::AgentRun => "agent_run",
            Self::Chat => "chat",
            Self::Manual => "manual",
        }
    }
}

/// Who may see this memory in generated context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    #[default]
    Internal,
    Sensitive,
}

/// A typed edge declared in frontmatter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    /// Target: slug, title, or node id (resolved by the index).
    pub to: String,
    /// Relation kind (free text; recommended vocabulary in docs/memory.md).
    pub rel: String,
    #[serde(default = "default_rel_confidence")]
    pub confidence: f32,
}

/// Parsed YAML frontmatter of a vault note. Unknown keys are preserved in
/// `extra` and written back verbatim (round-trip safety).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frontmatter {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    pub title: String,
    #[serde(default)]
    pub status: NodeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default = "default_score")]
    pub confidence: f32,
    #[serde(default = "default_score")]
    pub importance: f32,
    #[serde(default)]
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default)]
    pub sensitivity: Sensitivity,
    pub created: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<Relation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Unknown frontmatter keys, preserved on rewrite.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

/// A full note: frontmatter + body + location. `backlinks` is filled by
/// `Vault::get`, empty elsewhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub frontmatter: Frontmatter,
    pub body: String,
    /// Absolute path of the markdown file.
    pub path: PathBuf,
    /// File stem; stable identifier for wiki-links.
    pub slug: String,
    #[serde(default)]
    pub backlinks: Vec<NodeSummary>,
}

/// Input for `Vault::create`. Everything except type + title is optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteDraft {
    #[serde(rename = "type")]
    pub node_type: NodeType,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub status: NodeStatus,
    #[serde(default = "default_score")]
    pub confidence: f32,
    #[serde(default = "default_score")]
    pub importance: f32,
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub source_ref: Option<String>,
    #[serde(default)]
    pub sensitivity: Sensitivity,
    /// Optional expiry timestamp copied verbatim into the note's
    /// frontmatter. `None` (the default) means the note never expires;
    /// `Some(ts)` makes `Vault::sweep_expired` archive it once `ts` has
    /// passed. Additive and serde-defaulted — drafts written by older
    /// callers still deserialize.
    #[serde(default)]
    pub expires: Option<DateTime<Utc>>,
    #[serde(default)]
    pub relations: Vec<Relation>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Compact node row used in lists, search results, and backlinks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSummary {
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    pub status: NodeStatus,
    pub project: Option<String>,
    pub confidence: f32,
    pub importance: f32,
    pub source: Source,
    pub sensitivity: Sensitivity,
    pub created: String,
    pub updated: String,
    pub tags: Vec<String>,
    /// First ~160 chars of the body (search/pending lists).
    pub snippet: Option<String>,
}

/// Filter for `Vault::graph`. `statuses: None` means confirmed + candidate
/// (the spec's default view).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphFilter {
    #[serde(default)]
    pub types: Option<Vec<NodeType>>,
    #[serde(default)]
    pub statuses: Option<Vec<NodeStatus>>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub updated_since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// A node in the memory graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    pub status: NodeStatus,
    pub project: Option<String>,
    pub confidence: f32,
    pub importance: f32,
    pub created: String,
    pub updated: String,
}

/// A directed edge between nodes in the memory graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub rel: String,
    pub confidence: f32,
    /// "frontmatter" (typed) or "body" (wiki-link mention).
    pub origin: String,
}

/// An unresolved wiki-link target, rendered hollow in the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhostNode {
    pub target: String,
    pub ref_count: u32,
}

/// Complete graph snapshot for UI rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub ghosts: Vec<GhostNode>,
    /// True if the result was truncated due to query limit.
    pub truncated: bool,
}

/// How to resolve a candidate note.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Resolution {
    Confirm,
    Reject,
    /// Confirm the candidate and mark `replaces` (a node id) superseded.
    Supersede {
        replaces: String,
    },
}

/// Timeline event to append (id assigned by the index).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEvent {
    /// Event time; `None` = now.
    #[serde(default)]
    pub ts: Option<DateTime<Utc>>,
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    /// Zone-derived sensitivity of `text` (context engine spec §4.1
    /// propagation rule). `true` means the text came from a `local_only`
    /// window/project and must never be rendered into a cloud-bound
    /// context package. Defaults to `false` so existing producers and
    /// deserialized payloads keep today's meaning.
    #[serde(default)]
    pub local_only: bool,
}

/// Stored timeline event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub ts: String,
    pub kind: String,
    pub text: String,
    pub project: Option<String>,
    pub node_id: Option<String>,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    /// Whether this timeline entry is `local_only` (see
    /// [`NewEvent::local_only`]). Rows written before the flag existed
    /// read `false`.
    #[serde(default)]
    pub local_only: bool,
}

/// Query range for events. `limit` defaults to 500.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventRange {
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    /// Exclusive lower bound by event id (the `events` table's
    /// `AUTOINCREMENT` primary key, returned as [`Event::id`]) — matches
    /// events with `id > since_id` only. An id watermark, not a
    /// `since`/`until` timestamp filter: `ts` on a stored event can be
    /// backdated well before the row is actually written (see
    /// `continuum-core`'s memory distiller, which stamps `ts` as the
    /// original frame-capture time but only inserts the row on its own
    /// periodic pass), so a caller polling by `since`/`until` on a
    /// steadily-advancing clock can permanently skip an event whose row
    /// lands after the window has already moved past its `ts`. `since_id`
    /// has no such race — it only ever depends on what rows exist in the
    /// table at query time. Combinable with `since`/`until` (all three
    /// filters `AND` together); the curator's own extraction pass uses
    /// `since_id` exclusively for this reason (see
    /// `curator::run::extract_pass`).
    ///
    /// Ordering guarantee: whenever `since_id` is set, [`crate::Vault::events`]
    /// orders the result by `id` ascending (not `ts` ascending, its
    /// default) — under a `limit`-bounded fetch, this is what keeps the
    /// returned batch *contiguous by id* (the lowest ids past the
    /// watermark). Ordering by `ts` instead could return a higher id while
    /// skipping a lower one whose `ts` happens to sort later (e.g. a
    /// backdated row), and a watermark caller that then advances past the
    /// batch's max id would permanently lose the skipped one.
    #[serde(default)]
    pub since_id: Option<i64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// A markdown file that failed to parse and was quarantined.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineEntry {
    pub path: String,
    pub error: String,
}

/// Vault health/summary for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultInfo {
    pub path: String,
    pub note_count: u64,
    pub counts_by_status: BTreeMap<String, u64>,
    pub quarantined: Vec<QuarantineEntry>,
    pub last_full_index_at: Option<String>,
}

/// Statistics from an index operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexStats {
    /// Number of notes successfully indexed.
    pub indexed: u64,
    /// Number of files quarantined due to parse errors.
    pub quarantined: u64,
    /// Number of previously-indexed notes removed from the vault.
    pub removed: u64,
}

/// Result of a legacy data migration operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MigrationReport {
    /// Number of records successfully migrated.
    pub migrated: u64,
    /// Number of records skipped (already migrated or invalid).
    pub skipped: u64,
    /// Error messages encountered during migration.
    pub errors: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_as_str_matches_serde_repr() {
        assert_eq!(Source::AgentRun.as_str(), "agent_run");
    }

    #[test]
    fn node_type_roundtrip_and_folders() {
        for t in NodeType::ALL {
            assert_eq!(NodeType::parse(t.as_str()), Some(t));
            assert!(!t.folder().is_empty());
        }
        let json = serde_json::to_string(&NodeType::Decision).unwrap();
        assert_eq!(json, "\"decision\"");
    }

    #[test]
    fn resolution_serde_shape() {
        let r: Resolution =
            serde_json::from_str(r#"{"action":"supersede","replaces":"mem_x"}"#).unwrap();
        match r {
            Resolution::Supersede { replaces } => assert_eq!(replaces, "mem_x"),
            _ => panic!("wrong variant"),
        }
        assert_eq!(
            serde_json::to_string(&Resolution::Confirm).unwrap(),
            r#"{"action":"confirm"}"#
        );
    }

    #[test]
    fn frontmatter_defaults_apply() {
        let yaml = "id: mem_1\ntype: fact\ntitle: T\ncreated: 2026-08-01T10:00:00Z\n";
        let fm: Frontmatter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(fm.status, NodeStatus::Confirmed);
        assert_eq!(fm.confidence, 0.5);
        assert_eq!(fm.sensitivity, Sensitivity::Internal);
        assert!(fm.extra.is_empty());
    }

    #[test]
    fn frontmatter_preserves_unknown_keys() {
        let yaml =
            "id: mem_1\ntype: fact\ntitle: T\ncreated: 2026-08-01T10:00:00Z\ncustom_field: hello\n";
        let fm: Frontmatter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            fm.extra.get("custom_field").and_then(|v| v.as_str()),
            Some("hello")
        );
        let back = serde_yaml::to_string(&fm).unwrap();
        assert!(back.contains("custom_field: hello"));
    }
}
