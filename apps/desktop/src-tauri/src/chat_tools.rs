//! Chat memory tools: the bridge between the chat tab's provider adapters
//! and the memory vault. Layer: desktop, component: chat_tools.
//!
//! Three pieces, all consumed by `chat.rs`'s `chat_send_message` wiring:
//!
//! - [`VaultToolExecutor`] + [`memory_tool_defs`] — the in-process tool
//!   loop for HTTP providers (OpenAI-compatible, Anthropic). All four
//!   tools run against the same lazily-opened [`Vault`] the Memory tab
//!   uses; no subprocess is involved.
//! - [`mcp_spec`] — the Claude CLI path. The CLI cannot call back into
//!   this process, so it gets the `continuum-mcp` server attached instead,
//!   pointed at the same vault directory via env vars.
//! - [`memory_context_section`] — formats retrieved vault notes into the
//!   "## Memory context" system-prompt section injected per turn.
//!
//! Every vault failure inside the executor becomes an `Err(String)`
//! delivered to the model as an error tool result — a broken vault must
//! degrade a tool call, never abort the chat turn (see
//! [`continuum_gateway::ToolExecutor`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use continuum_gateway::{McpSpec, ToolDef, ToolExecutor};
use continuum_memory::{NodeStatus, NodeSummary, NodeType, NoteDraft, Sensitivity, Source, Vault};
use serde_json::json;

/// Hard cap on `memory_search` results, mirroring the candidate-pool size
/// used by the same-title upsert lookup.
const SEARCH_LIMIT_MAX: u64 = 25;
/// Default `memory_search` result count when the model omits `limit`.
const SEARCH_LIMIT_DEFAULT: u64 = 10;
/// Per-note snippet cap (chars) in the "## Memory context" section.
const CONTEXT_SNIPPET_MAX_CHARS: usize = 300;

/// Platform-correct file name of the MCP server binary.
const MCP_BIN_NAME: &str = if cfg!(windows) {
    "continuum-mcp.exe"
} else {
    "continuum-mcp"
};

/// In-process vault executor for chat tool calls (all layers stay in the
/// desktop process; no extra subprocess for HTTP providers).
pub struct VaultToolExecutor {
    /// The shared, already-opened vault handle (see `MemoryState::vault`).
    pub vault: Arc<Vault>,
    /// Whether `sensitivity: sensitive` notes appear in search results.
    pub include_sensitive: bool,
}

#[async_trait::async_trait]
impl ToolExecutor for VaultToolExecutor {
    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<String, String> {
        let result = match name {
            "memory_search" => self.search(input).await,
            "memory_get" => self.get(input).await,
            "memory_save" => self.save(input).await,
            "memory_delete" => self.delete(input).await,
            other => Err(format!(
                "unknown tool \"{other}\" (expected memory_search|memory_get|memory_save|memory_delete)"
            )),
        };
        if let Err(e) = &result {
            tracing::warn!(
                layer = "desktop",
                component = "chat_tools",
                tool = name,
                error = %e,
                "chat memory tool call failed"
            );
        }
        result
    }
}

impl VaultToolExecutor {
    /// `memory_search {query, limit?}` → JSON array of compact hit rows.
    /// Rejected/superseded notes are always dropped; sensitive notes only
    /// pass when `include_sensitive` is set.
    async fn search(&self, input: &serde_json::Value) -> Result<String, String> {
        let query = str_field(input, "query")?;
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(SEARCH_LIMIT_DEFAULT)
            .min(SEARCH_LIMIT_MAX) as u32;
        let hits = self
            .vault
            .search(query, limit)
            .await
            .map_err(|e| e.user_message())?;
        let rows: Vec<serde_json::Value> = hits
            .into_iter()
            .filter(|h| !matches!(h.status, NodeStatus::Rejected | NodeStatus::Superseded))
            .filter(|h| self.include_sensitive || h.sensitivity != Sensitivity::Sensitive)
            .map(|h| {
                json!({
                    "id": h.id,
                    "title": h.title,
                    "type": h.node_type.as_str(),
                    "status": h.status.as_str(),
                    "snippet": h.snippet,
                    "tags": h.tags,
                    "project": h.project,
                    "updated": h.updated,
                })
            })
            .collect();
        serde_json::to_string(&rows).map_err(|e| e.to_string())
    }

    /// `memory_get {id}` → one note with its full body.
    async fn get(&self, input: &serde_json::Value) -> Result<String, String> {
        let id = str_field(input, "id")?;
        let note = self.vault.get(id).await.map_err(|e| e.user_message())?;
        serde_json::to_string(&json!({
            "id": note.frontmatter.id,
            "title": note.frontmatter.title,
            "type": note.frontmatter.node_type.as_str(),
            "status": note.frontmatter.status.as_str(),
            "tags": note.frontmatter.tags,
            "project": note.frontmatter.project,
            "body": note.body,
        }))
        .map_err(|e| e.to_string())
    }

    /// `memory_save {title, content, type?, tags?, project?}` — same-title
    /// upsert. A case-insensitive exact title match (trimmed) updates the
    /// existing note's body (and tags/project when provided); otherwise a
    /// new confirmed note is created with `source: chat`. An unknown `type`
    /// string falls back to `note` rather than erroring.
    async fn save(&self, input: &serde_json::Value) -> Result<String, String> {
        let title = str_field(input, "title")?;
        let content = str_field(input, "content")?;
        let node_type = input
            .get("type")
            .and_then(|v| v.as_str())
            .and_then(NodeType::parse)
            .unwrap_or(NodeType::Note);
        let tags: Option<Vec<String>> = input.get("tags").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        });
        let project = input
            .get("project")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        if let Some(id) = self.find_existing_note_id(title).await? {
            let mut note = self.vault.get(&id).await.map_err(|e| e.user_message())?;
            note.body = content.to_string();
            if let Some(tags) = tags {
                note.frontmatter.tags = tags;
            }
            if let Some(project) = project {
                note.frontmatter.project = Some(project);
            }
            self.vault.save(&note).await.map_err(|e| e.user_message())?;
            Ok(json!({"id": id, "updated": true}).to_string())
        } else {
            let note = self
                .vault
                .create(NoteDraft {
                    node_type,
                    title: title.trim().to_string(),
                    body: content.to_string(),
                    project,
                    status: NodeStatus::Confirmed,
                    confidence: 0.5,
                    importance: 0.5,
                    source: Source::Chat,
                    source_ref: None,
                    sensitivity: Sensitivity::default(),
                    expires: None,
                    relations: Vec::new(),
                    tags: tags.unwrap_or_default(),
                })
                .await
                .map_err(|e| e.user_message())?;
            Ok(json!({"id": note.frontmatter.id, "updated": false}).to_string())
        }
    }

    /// `memory_delete {id}` — permanently removes the note.
    async fn delete(&self, input: &serde_json::Value) -> Result<String, String> {
        let id = str_field(input, "id")?;
        self.vault.delete(id).await.map_err(|e| e.user_message())?;
        Ok(json!({"deleted": true}).to_string())
    }

    /// Id of an existing (any-status) note whose trimmed title equals
    /// `title` case-insensitively, if any. Mirrors
    /// `find_existing_note_id` in `continuum-mcp`'s memory tools: FTS
    /// search as the candidate pool, exact-title verification here.
    async fn find_existing_note_id(&self, title: &str) -> Result<Option<String>, String> {
        let target = title.trim().to_lowercase();
        if target.is_empty() {
            return Ok(None);
        }
        let hits = self
            .vault
            .search(title, SEARCH_LIMIT_MAX as u32)
            .await
            .map_err(|e| e.user_message())?;
        Ok(hits
            .into_iter()
            .find(|h| h.title.trim().to_lowercase() == target)
            .map(|h| h.id))
    }
}

/// Extracts a required non-empty string field from a tool input object.
fn str_field<'a>(input: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("missing required string field `{key}`"))
}

/// The four provider-neutral tool definitions offered to HTTP providers.
pub fn memory_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "memory_search".into(),
            description: "Full-text search over the user's saved memories (titles, bodies, \
                          tags). Returns a JSON array of matches with id, title, type, status, \
                          snippet, tags, project, and updated timestamp."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search terms."},
                    "limit": {"type": "integer", "description": "Max results (default 10, max 25)."}
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "memory_get".into(),
            description:
                "Fetch one saved memory by id (from memory_search), including its full body.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Memory id, e.g. from memory_search."}
                },
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "memory_save".into(),
            description: "Save a memory. If a memory with the same title already exists \
                          (case-insensitive), it is UPDATED in place instead of duplicated — \
                          reuse an existing title to update it. Keep titles short and stable."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Short, stable title."},
                    "content": {"type": "string", "description": "The memory body."},
                    "type": {"type": "string", "description": "One of project|goal|task|decision|person|preference|fact|error|session|note (default: note)."},
                    "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional tags."},
                    "project": {"type": "string", "description": "Optional project slug."}
                },
                "required": ["title", "content"]
            }),
        },
        ToolDef {
            name: "memory_delete".into(),
            description: "Permanently delete a saved memory by id. Irreversible.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Memory id to delete."}
                },
                "required": ["id"]
            }),
        },
    ]
}

/// MCP attachment for the Claude CLI path: the `continuum-mcp` server
/// pointed at the chat's vault dir + dev dir, with the memory tool family
/// allowed. Returns `None` (log once) when the binary cannot be found —
/// the chat then degrades to running without memory tools.
pub fn mcp_spec(vault_dir: &Path, dev_dir: &Path, session_id: &str) -> Option<McpSpec> {
    let server_command = resolve_mcp_binary()?;
    Some(McpSpec {
        server_command,
        env: vec![
            (
                "CONTINUUM_VAULT_DIR".into(),
                vault_dir.to_string_lossy().into_owned(),
            ),
            (
                "CONTINUUM_DATA_DIR".into(),
                dev_dir.to_string_lossy().into_owned(),
            ),
            ("CONTINUUM_SESSION_ID".into(), format!("chat-{session_id}")),
        ],
        allowed_tools: [
            "mcp__continuum__memory_vault_search",
            "mcp__continuum__memory_vault_get",
            "mcp__continuum__memory_vault_save",
            "mcp__continuum__memory_vault_resolve",
            "mcp__continuum__memory_vault_delete",
            "mcp__continuum__memory_get_fact",
            "mcp__continuum__memory_list_facts",
            "mcp__continuum__memory_query_episodic",
            // Context family (spec §5.2: "the chat CLI provider's
            // allowed-tools list gains the context family"). Listed
            // explicitly rather than wildcarded so adding a tool to the
            // MCP server is never the same act as granting chat access to
            // it. All ten are read-only and privacy-gated server-side.
            "mcp__continuum__context_session",
            "mcp__continuum__context_window",
            "mcp__continuum__context_screen",
            "mcp__continuum__context_audio",
            "mcp__continuum__context_projects",
            "mcp__continuum__context_timeline",
            "mcp__continuum__context_search",
            "mcp__continuum__context_files",
            "mcp__continuum__context_git",
            "mcp__continuum__context_package",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
    })
}

/// Locates the `continuum-mcp` binary: `CONTINUUM_MCP_BIN` env var, then a
/// packaged Tauri resources, then a PATH scan (mirrors
/// `resolve_mcp_binary` in `continuum-core`'s orchestrator spawn path).
/// `None` — with a once-per-process warning — when none of those exist.
fn resolve_mcp_binary() -> Option<PathBuf> {
    if let Ok(env_bin) = std::env::var("CONTINUUM_MCP_BIN") {
        let p = PathBuf::from(env_bin);
        if p.exists() {
            return Some(p);
        }
    }
    if let Some(candidate) = crate::commands::bundled_binary_candidates(MCP_BIN_NAME)
        .into_iter()
        .find(|candidate| candidate.exists())
    {
        return Some(candidate);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(MCP_BIN_NAME);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    static WARN_ONCE: std::sync::Once = std::sync::Once::new();
    WARN_ONCE.call_once(|| {
        tracing::warn!(
            layer = "desktop",
            component = "chat_tools",
            "continuum-mcp binary not found (CONTINUUM_MCP_BIN, exe sibling, PATH) — \
             Claude CLI chat runs without memory tools"
        );
    });
    None
}

/// Formats vault hits into a "## Memory context" system-prompt section:
/// one `- [type] title: snippet` line per note (snippet capped at
/// [`CONTEXT_SNIPPET_MAX_CHARS`] chars, newlines flattened) plus one
/// instruction line. Empty input yields an empty string — no section.
pub fn memory_context_section(notes: &[NodeSummary]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Memory context\n");
    for n in notes {
        let title = n.title.replace(['\r', '\n'], " ");
        let snippet: String = n
            .snippet
            .as_deref()
            .unwrap_or("")
            .trim()
            .chars()
            .take(CONTEXT_SNIPPET_MAX_CHARS)
            .collect();
        let snippet = snippet.replace(['\r', '\n'], " ");
        if snippet.is_empty() {
            out.push_str(&format!("- [{}] {title}\n", n.node_type.as_str()));
        } else {
            out.push_str(&format!(
                "- [{}] {title}: {snippet}\n",
                n.node_type.as_str()
            ));
        }
    }
    out.push_str(
        "\nThese are the user's saved memories, retrieved for the current message. \
         Use them when relevant; call the memory search tool for more.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn executor(include_sensitive: bool) -> (tempfile::TempDir, VaultToolExecutor) {
        let tmp = tempfile::tempdir().expect("tmp");
        let vault = Vault::open(tmp.path()).await.expect("open vault");
        (
            tmp,
            VaultToolExecutor {
                vault: Arc::new(vault),
                include_sensitive,
            },
        )
    }

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("tool output is JSON")
    }

    fn draft(title: &str, status: NodeStatus, sensitivity: Sensitivity) -> NoteDraft {
        NoteDraft {
            node_type: NodeType::Fact,
            title: title.into(),
            body: format!("body of {title}"),
            project: None,
            status,
            confidence: 0.5,
            importance: 0.5,
            source: Source::Chat,
            source_ref: None,
            sensitivity,
            expires: None,
            relations: Vec::new(),
            tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn save_creates_then_updates_case_insensitive() {
        let (_tmp, ex) = executor(false).await;

        let out = ex
            .execute(
                "memory_save",
                &json!({"title": "User Name", "content": "Toshan", "type": "fact"}),
            )
            .await
            .expect("create");
        let v = parse(&out);
        assert_eq!(v["updated"], false);
        let id = v["id"].as_str().expect("id").to_string();

        // Same title, different case + surrounding whitespace → update.
        let out = ex
            .execute(
                "memory_save",
                &json!({"title": "  user name ", "content": "Toshan O.", "tags": ["identity"]}),
            )
            .await
            .expect("update");
        let v = parse(&out);
        assert_eq!(v["updated"], true);
        assert_eq!(v["id"].as_str(), Some(id.as_str()));

        let note = ex.vault.get(&id).await.expect("get updated note");
        assert_eq!(note.body, "Toshan O.");
        assert_eq!(note.frontmatter.tags, vec!["identity".to_string()]);
        assert_eq!(note.frontmatter.node_type, NodeType::Fact);
        assert_eq!(note.frontmatter.source, Source::Chat);
        assert_eq!(note.frontmatter.status, NodeStatus::Confirmed);
    }

    #[tokio::test]
    async fn save_with_unknown_type_defaults_to_note() {
        let (_tmp, ex) = executor(false).await;
        let out = ex
            .execute(
                "memory_save",
                &json!({"title": "Odd type", "content": "b", "type": "banana"}),
            )
            .await
            .expect("save");
        let id = parse(&out)["id"].as_str().expect("id").to_string();
        let note = ex.vault.get(&id).await.expect("get");
        assert_eq!(note.frontmatter.node_type, NodeType::Note);
    }

    #[tokio::test]
    async fn search_hides_sensitive_unless_flagged_and_hides_rejected() {
        let (_tmp, ex) = executor(false).await;
        for d in [
            draft("Alpha normal", NodeStatus::Confirmed, Sensitivity::Internal),
            draft(
                "Alpha secret",
                NodeStatus::Confirmed,
                Sensitivity::Sensitive,
            ),
            draft(
                "Alpha rejected",
                NodeStatus::Rejected,
                Sensitivity::Internal,
            ),
        ] {
            ex.vault.create(d).await.expect("create");
        }

        let out = ex
            .execute("memory_search", &json!({"query": "alpha"}))
            .await
            .expect("search");
        let titles: Vec<String> = parse(&out)
            .as_array()
            .expect("array")
            .iter()
            .map(|r| r["title"].as_str().expect("title").to_string())
            .collect();
        assert_eq!(titles, vec!["Alpha normal".to_string()]);

        let flagged = VaultToolExecutor {
            vault: ex.vault.clone(),
            include_sensitive: true,
        };
        let out = flagged
            .execute("memory_search", &json!({"query": "alpha"}))
            .await
            .expect("search");
        let titles: Vec<String> = parse(&out)
            .as_array()
            .expect("array")
            .iter()
            .map(|r| r["title"].as_str().expect("title").to_string())
            .collect();
        assert!(titles.contains(&"Alpha normal".to_string()));
        assert!(titles.contains(&"Alpha secret".to_string()));
        assert!(
            !titles.contains(&"Alpha rejected".to_string()),
            "rejected notes must never surface, even with include_sensitive"
        );
    }

    #[tokio::test]
    async fn get_returns_body_and_delete_removes() {
        let (_tmp, ex) = executor(false).await;
        let note = ex
            .vault
            .create(draft(
                "To forget",
                NodeStatus::Confirmed,
                Sensitivity::Internal,
            ))
            .await
            .expect("create");
        let id = note.frontmatter.id.clone();

        let out = ex
            .execute("memory_get", &json!({"id": id}))
            .await
            .expect("get");
        let v = parse(&out);
        assert_eq!(v["title"], "To forget");
        assert_eq!(v["body"], "body of To forget");

        let out = ex
            .execute("memory_delete", &json!({"id": id}))
            .await
            .expect("delete");
        assert_eq!(parse(&out)["deleted"], true);
        assert!(
            ex.vault.get(&id).await.is_err(),
            "deleted note must be gone from the vault"
        );
    }

    #[tokio::test]
    async fn unknown_tool_name_is_an_error_not_a_panic() {
        let (_tmp, ex) = executor(false).await;
        let err = ex
            .execute("memory_teleport", &json!({}))
            .await
            .expect_err("unknown tool must err");
        assert!(err.contains("unknown tool"), "got: {err}");
    }

    #[tokio::test]
    async fn missing_required_field_is_an_error() {
        let (_tmp, ex) = executor(false).await;
        let err = ex
            .execute("memory_search", &json!({}))
            .await
            .expect_err("missing query must err");
        assert!(err.contains("query"), "got: {err}");
    }

    #[test]
    fn memory_context_section_formats_and_truncates() {
        let long_snippet = "x".repeat(400);
        let notes = vec![NodeSummary {
            id: "mem_1".into(),
            slug: "user-name".into(),
            title: "User name".into(),
            node_type: NodeType::Fact,
            status: NodeStatus::Confirmed,
            project: None,
            confidence: 0.5,
            importance: 0.5,
            source: Source::Chat,
            sensitivity: Sensitivity::Internal,
            created: "2026-08-01T00:00:00Z".into(),
            updated: "2026-08-01T00:00:00Z".into(),
            tags: Vec::new(),
            snippet: Some(long_snippet),
        }];
        let section = memory_context_section(&notes);
        assert!(section.starts_with("## Memory context"));
        assert!(section.contains("- [fact] User name: x"));
        assert!(section.contains(&"x".repeat(300)));
        assert!(
            !section.contains(&"x".repeat(301)),
            "snippet must cap at 300 chars"
        );
        assert!(section.contains("saved memories"));
    }

    #[test]
    fn memory_context_section_empty_input_yields_no_section() {
        assert!(memory_context_section(&[]).is_empty());
    }

    #[test]
    fn tool_defs_cover_the_four_memory_tools() {
        let names: Vec<String> = memory_tool_defs().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "memory_search",
                "memory_get",
                "memory_save",
                "memory_delete"
            ]
        );
    }
}
