//! # Memory tools (`mcp__continuum__memory_*`)
//!
//! - `memory_query_episodic` — vector search over episodic memory (LanceDB)
//! - `memory_list_facts` — list facts, vault-first, falling back to the
//!   legacy semantic store
//! - `memory_get_fact` — fetch a single fact by key, vault-first
//! - `memory_set_fact` — upsert a fact key. Writes a vault `fact` note (see
//!   [`title_from_key`]/[`tag_from_key`]); no longer touches the legacy
//!   `semantic.sqlite` store.
//! - `memory_vault_search` / `memory_vault_get` / `memory_vault_save` /
//!   `memory_vault_resolve` — direct access to the memory vault's full node
//!   model (see `docs/memory.md`).
//! - `memory_wipe_all` — queue a derived-data wipe request for the runtime.
//!
//! Request/response types live here. The tool methods themselves are declared
//! on [`crate::ContinuumMcpServer`] in `server.rs` so rmcp's `#[tool_router]` macro
//! can register them all in one dispatch table.

use chrono::{DateTime, Utc};
use continuum_core::memory::{
    episodic::EventKind,
    semantic::{Fact, FactSource},
};
use continuum_memory::{MemoryError, NodeType, Note, Resolution, Vault};
use rmcp::model::ErrorData as McpError;
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
    /// Dotted key (e.g. "user.name", "project.continuum.stack").
    pub key: String,
}

// ---------------------------------------------------------------------------
// memory_set_fact
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MemorySetFactRequest {
    /// Dotted key (e.g. "user.preferred_language"). Keys starting with
    /// "system." or "continuum." are reserved and rejected.
    pub key: String,
    /// Value to store (JSON-encoded string is fine, plain string works too).
    pub value: String,
    /// How Continuum learned this. Defaults to "inferred". "user_stated" is only
    /// accepted when the user explicitly told Continuum this fact during the wake.
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
/// orchestrator is not allowed to overwrite. Continuum's own internal state lives
/// under these prefixes and is managed by the runtime.
pub fn is_reserved_key(key: &str) -> bool {
    key.starts_with("system.") || key.starts_with("continuum.")
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

/// Confidence clamp for orchestrator-written facts. User-stated facts in Continuum
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
// memory_set_fact <-> vault key/title mapping
//
// Mirrors `continuum_memory::migrate`'s legacy-fact mapping exactly (that
// module's `title_from_key`/`tag_from_key` are crate-private) so a fact
// written via `memory_set_fact` and one migrated from the legacy semantic
// store land on the same vault title and are found by the same lookup.
// ---------------------------------------------------------------------------

/// Title for a `memory_set_fact` key: only the *first* `.`-separated
/// segment boundary becomes `": "` (`user.name` -> `user: name`,
/// `project.sidelife.stack` -> `project: sidelife.stack`).
pub fn title_from_key(key: &str) -> String {
    key.replacen('.', ": ", 1)
}

/// Tag for a `memory_set_fact` key: its first `.`-separated segment
/// (`user.name` -> `user`). Keys with no dot become their own tag.
pub fn tag_from_key(key: &str) -> String {
    key.split('.').next().unwrap_or(key).to_string()
}

/// Inverse of [`title_from_key`]: reconstructs the dotted key from a vault
/// note's title. Only the first `": "` is turned back into `.`, matching
/// `title_from_key`'s first-segment-only mapping. Not a perfect inverse for
/// a title that happens to contain `": "` of its own accord (e.g. a vault
/// note created by other means with that literal substring in its title),
/// but that's an acceptable edge case for the `memory_set_fact` compat path.
pub fn key_from_title(title: &str) -> String {
    title.replacen(": ", ".", 1)
}

/// Maps a [`MemoryError`] onto an [`McpError`]. Caller-input problems
/// (missing note, invalid state transition, bad title) become
/// `invalid_params`; everything else (disk/db/parse failures) is
/// `internal_error`.
pub fn vault_err_to_mcp(e: &MemoryError) -> McpError {
    match e {
        MemoryError::NotFound(_) | MemoryError::Invalid(_) => {
            McpError::invalid_params(e.to_string(), None)
        }
        MemoryError::Io { .. }
        | MemoryError::Parse(_)
        | MemoryError::Db(_)
        | MemoryError::Watch(_) => McpError::internal_error(e.to_string(), None),
    }
}

/// Find the id of an existing (any-status) vault note whose title matches
/// `title`, case-insensitively (after trimming). Backs the "same title ->
/// update, don't duplicate" semantics of `memory_vault_save` and the
/// `memory_set_fact` redirect.
///
/// `Vault` has no direct exact-title lookup in its public API — the
/// crate-internal one backing `migrate`'s idempotency check
/// (`find_by_slug_or_title`) is `pub(crate)` to `continuum-memory`. This
/// uses [`Vault::search`] as a candidate pool instead and verifies an exact
/// title match itself: an exact-title note is guaranteed to be present in
/// the FTS hit set whenever `title` tokenizes to at least one alphanumeric
/// token (the common case for any real title).
pub async fn find_existing_note_id(vault: &Vault, title: &str) -> Result<Option<String>, McpError> {
    let target = title.trim().to_lowercase();
    if target.is_empty() {
        return Ok(None);
    }
    let hits = vault
        .search(title, 25)
        .await
        .map_err(|e| vault_err_to_mcp(&e))?;
    Ok(hits
        .into_iter()
        .find(|h| h.title.trim().to_lowercase() == target)
        .map(|h| h.id))
}

/// Maps a `memory_vault_resolve` action string + optional `replaces` into a
/// [`Resolution`]. `"supersede"` without `replaces` is a caller input error
/// (`invalid_params`), not a panic.
pub fn parse_resolution(action: &str, replaces: Option<String>) -> Result<Resolution, McpError> {
    match action {
        "confirm" => Ok(Resolution::Confirm),
        "reject" => Ok(Resolution::Reject),
        "supersede" => match replaces {
            Some(replaces) => Ok(Resolution::Supersede { replaces }),
            None => Err(McpError::invalid_params(
                "action \"supersede\" requires \"replaces\" (the id of the node being superseded)",
                None,
            )),
        },
        other => Err(McpError::invalid_params(
            format!("unknown resolve action \"{other}\" (expected confirm|reject|supersede)"),
            None,
        )),
    }
}

/// Builds a [`FactView`] from a vault note, using `key` (the caller's
/// original dotted key, not the note's title) as the returned `key` field
/// so the output schema matches what the legacy semantic-store path
/// returns.
fn fact_view_from_note(key: &str, note: &Note) -> FactView {
    let updated_at = note.frontmatter.updated.unwrap_or(note.frontmatter.created);
    FactView {
        key: key.to_string(),
        value: note.body.clone(),
        confidence: note.frontmatter.confidence,
        source: note.frontmatter.source.as_str().to_string(),
        updated_at,
    }
}

/// `memory_get_fact`'s vault-first lookup: maps `key` to its vault title,
/// searches for an existing note, and returns a [`FactView`] built from the
/// full note when found. `Ok(None)` means "no vault note for that key" —
/// the caller falls back to the legacy semantic store.
pub async fn vault_get_fact(vault: &Vault, key: &str) -> Result<Option<FactView>, McpError> {
    let title = title_from_key(key);
    let Some(id) = find_existing_note_id(vault, &title).await? else {
        return Ok(None);
    };
    let note = vault.get(&id).await.map_err(|e| vault_err_to_mcp(&e))?;
    Ok(Some(fact_view_from_note(key, &note)))
}

/// `memory_list_facts`'s vault-first lookup. With `prefix`, casts a
/// candidate net via [`Vault::search`] on the prefix's first `.`-segment
/// (the tag every `memory_set_fact`-written note carries) and then filters
/// to notes whose reconstructed key ([`key_from_title`]) literally starts
/// with `prefix` — this is the FTS-search-plus-verify pattern also used by
/// [`find_existing_note_id`]. Without `prefix`, pulls `type: fact` notes via
/// [`Vault::graph`] instead (search requires a non-empty query). Either way,
/// the candidate pool is over-fetched relative to `limit` and then sorted
/// by `updated_at` descending before truncating, since neither underlying
/// query is update-ordered (FTS rank / importance DESC respectively) —
/// this only approximates true global recency when the vault holds more
/// matching facts than the over-fetched pool size.
pub async fn vault_list_facts(
    vault: &Vault,
    prefix: Option<&str>,
    limit: u32,
) -> Result<Vec<FactView>, McpError> {
    struct Candidate {
        id: String,
        title: String,
        node_type: NodeType,
    }

    let pool_size = limit.saturating_mul(4).clamp(50, 500);

    let candidates: Vec<Candidate> = match prefix {
        Some(p) => {
            let tag_hint = p.trim_end_matches('.').split('.').next().unwrap_or(p);
            if tag_hint.is_empty() {
                vec![]
            } else {
                vault
                    .search(tag_hint, pool_size)
                    .await
                    .map_err(|e| vault_err_to_mcp(&e))?
                    .into_iter()
                    .map(|n| Candidate {
                        id: n.id,
                        title: n.title,
                        node_type: n.node_type,
                    })
                    .collect()
            }
        }
        None => {
            let filter = continuum_memory::GraphFilter {
                types: Some(vec![NodeType::Fact]),
                limit: Some(pool_size),
                ..Default::default()
            };
            vault
                .graph(&filter)
                .await
                .map_err(|e| vault_err_to_mcp(&e))?
                .nodes
                .into_iter()
                .map(|n| Candidate {
                    id: n.id,
                    title: n.title,
                    node_type: n.node_type,
                })
                .collect()
        }
    };

    let mut out: Vec<FactView> = Vec::new();
    for c in candidates {
        if c.node_type != NodeType::Fact {
            continue;
        }
        let key = key_from_title(&c.title);
        if let Some(p) = prefix {
            if !key.starts_with(p) {
                continue;
            }
        }
        let note = vault.get(&c.id).await.map_err(|e| vault_err_to_mcp(&e))?;
        out.push(fact_view_from_note(&key, &note));
    }

    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    out.truncate(limit as usize);
    Ok(out)
}

/// Atomically writes `<data_dir>/wipe-request.json` — the exact same
/// contract as Task 7's runtime `WipeRequest` / desktop
/// `write_wipe_request_file`: `{ requested_at, scopes: ["raw_log",
/// "episodic", "events"] }`, written to a `.tmp` sibling then renamed over
/// the target. Returns the path written. The MCP tool only *queues* the
/// request — the running `continuum` runtime drains it at its next boot or
/// daily hygiene tick (`curator::run::process_wipe_request`); this never
/// touches vault markdown notes.
pub fn write_wipe_request(data_dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    std::fs::create_dir_all(data_dir)?;
    let payload = serde_json::json!({
        "requested_at": Utc::now().to_rfc3339(),
        "scopes": ["raw_log", "episodic", "events"],
    });
    let final_path = data_dir.join("wipe-request.json");
    let tmp_path = data_dir.join("wipe-request.json.tmp");
    std::fs::write(&tmp_path, payload.to_string())?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

// ---------------------------------------------------------------------------
// memory_vault_search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MemoryVaultSearchRequest {
    /// Full-text query over vault note titles/bodies/tags.
    pub query: String,
    /// Optional node-type filter (e.g. ["fact", "decision"]), matched
    /// case-insensitively. Unrecognized type strings simply never match —
    /// no error (contrast `memory_vault_save`'s strict `type` validation).
    #[serde(default)]
    pub types: Option<Vec<String>>,
    /// Optional exact `project` slug filter.
    #[serde(default)]
    pub project: Option<String>,
    /// Maximum number of results. Default 10, max 100.
    #[serde(default)]
    pub limit: Option<u32>,
}

// ---------------------------------------------------------------------------
// memory_vault_get
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MemoryVaultGetRequest {
    /// Node id, e.g. "mem_01j8f3a6k2...".
    pub id: String,
}

// ---------------------------------------------------------------------------
// memory_vault_save
// ---------------------------------------------------------------------------

/// One relation entry in a `memory_vault_save` call. Mirrors
/// [`continuum_memory::Relation`] but is defined locally so it can derive
/// [`JsonSchema`] for the tool's advertised input schema.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct RelationInput {
    /// Target: slug, title, or node id (resolved by the vault index).
    pub to: String,
    /// Relation kind (free text; see docs/memory.md for the recommended vocabulary).
    pub rel: String,
    /// Edge confidence, 0.0-1.0. Defaults to 1.0.
    #[serde(default)]
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryVaultSaveRequest {
    /// One of: project, goal, task, decision, person, preference, fact,
    /// error, session, note.
    #[serde(rename = "type")]
    pub r#type: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub project: Option<String>,
    /// 0.0-1.0. On create, defaults to 0.5; on an update (same title
    /// already exists), omitting this leaves the existing note's
    /// confidence unchanged.
    #[serde(default)]
    pub confidence: Option<f32>,
    /// 0.0-1.0. Same create/update default behavior as `confidence`.
    #[serde(default)]
    pub importance: Option<f32>,
    /// Replaces the note's relations entirely when present; omitted on an
    /// update leaves existing relations unchanged (omitted on create just
    /// means "no relations").
    #[serde(default)]
    pub relations: Option<Vec<RelationInput>>,
    /// Replaces the note's tags entirely when present; same omission rule
    /// as `relations`.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub source_ref: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct VaultSaveResponse {
    pub id: String,
    /// True if an existing note with the same (case-insensitive) title was
    /// updated in place instead of a new one being created.
    pub updated: bool,
}

// ---------------------------------------------------------------------------
// memory_vault_resolve
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MemoryVaultResolveRequest {
    pub id: String,
    /// One of "confirm" | "reject" | "supersede".
    pub action: String,
    /// Required when `action` is "supersede": the id of the node this one replaces.
    #[serde(default)]
    pub replaces: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct VaultResolveResponse {
    pub id: String,
    pub ok: bool,
}

// ---------------------------------------------------------------------------
// memory_wipe_all
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MemoryWipeAllRequest {
    /// Must equal the literal string "WIPE".
    pub confirm: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WipeAllResponse {
    /// Path of the wipe-request file written.
    pub path: String,
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
        assert!(is_reserved_key("continuum.config.mode"));
    }

    #[test]
    fn non_reserved_keys_allowed() {
        assert!(!is_reserved_key("user.name"));
        assert!(!is_reserved_key("project.simcharts.dir"));
        assert!(!is_reserved_key("routine.morning_start"));
    }

    #[test]
    fn reserved_check_is_prefix_only() {
        // "continuumsystem." should NOT be reserved (the dot is part of the prefix).
        assert!(!is_reserved_key("continuumsystem.x"));
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

    #[test]
    fn title_from_key_replaces_first_dot_only() {
        assert_eq!(title_from_key("user.name"), "user: name");
        assert_eq!(
            title_from_key("project.sidelife.stack"),
            "project: sidelife.stack"
        );
        assert_eq!(title_from_key("singleword"), "singleword");
    }

    #[test]
    fn tag_from_key_is_first_segment() {
        assert_eq!(tag_from_key("user.name"), "user");
        assert_eq!(tag_from_key("project.sidelife.stack"), "project");
        assert_eq!(tag_from_key("singleword"), "singleword");
    }

    #[test]
    fn key_from_title_is_inverse_of_title_from_key() {
        for key in ["user.name", "project.sidelife.stack", "singleword"] {
            assert_eq!(key_from_title(&title_from_key(key)), key);
        }
    }

    #[test]
    fn parse_resolution_confirm_and_reject() {
        assert!(matches!(
            parse_resolution("confirm", None).unwrap(),
            Resolution::Confirm
        ));
        assert!(matches!(
            parse_resolution("reject", None).unwrap(),
            Resolution::Reject
        ));
    }

    #[test]
    fn parse_resolution_supersede_requires_replaces() {
        let err = parse_resolution("supersede", None).unwrap_err();
        assert!(err.message.contains("replaces"));

        match parse_resolution("supersede", Some("mem_x".to_string())).unwrap() {
            Resolution::Supersede { replaces } => assert_eq!(replaces, "mem_x"),
            other => panic!("expected Supersede, got {other:?}"),
        }
    }

    #[test]
    fn parse_resolution_rejects_unknown_action() {
        let err = parse_resolution("delete", None).unwrap_err();
        assert!(err.message.contains("delete"));
    }

    #[test]
    fn vault_err_to_mcp_maps_not_found_and_invalid_to_invalid_params() {
        use rmcp::model::ErrorCode;
        let not_found = vault_err_to_mcp(&MemoryError::NotFound("mem_x".into()));
        assert_eq!(not_found.code, ErrorCode::INVALID_PARAMS);
        let invalid = vault_err_to_mcp(&MemoryError::Invalid("bad state".into()));
        assert_eq!(invalid.code, ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn vault_err_to_mcp_maps_parse_to_internal_error() {
        use rmcp::model::ErrorCode;
        let parse = vault_err_to_mcp(&MemoryError::Parse("bad yaml".into()));
        assert_eq!(parse.code, ErrorCode::INTERNAL_ERROR);
    }
}
