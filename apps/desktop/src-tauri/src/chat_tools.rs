//! Provider-neutral memory + live-context + settings tools for the desktop chat surface.
//!
//! HTTP providers execute these tools in-process; Claude CLI receives equivalent
//! capabilities through `continuum-mcp`. Both paths obey the effective native
//! permission policy and the same privacy/egress boundaries.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use continuum_core::config::{continuum_dev_dir, load_config};
use continuum_core::senses::live_context::LiveWorldState;
use continuum_core::senses::privacy::{source_enabled, ObservedSource, PrivacyFilter};
use continuum_gateway::{McpSpec, ToolDef, ToolExecutor};
use continuum_memory::{NodeStatus, NodeSummary, NodeType, NoteDraft, Sensitivity, Source, Vault};
use serde_json::json;

#[path = "settings_tools.rs"]
mod settings_tools;

const SEARCH_LIMIT_MAX: u64 = 25;
const SEARCH_LIMIT_DEFAULT: u64 = 10;
const CONTEXT_SNIPPET_MAX_CHARS: usize = 300;
const LIVE_CONTEXT_STALE_SECS: i64 = 10;

const MCP_BIN_NAME: &str = if cfg!(windows) {
    "continuum-mcp.exe"
} else {
    "continuum-mcp"
};

/// In-process chat executor used by OpenAI-compatible and Anthropic adapters.
/// Permission checks happen before dispatch so this path cannot bypass the MCP
/// broker merely because it lives inside the desktop process. The live-context
/// methods read only the runtime-published projection and re-apply the cloud
/// privacy gate before returning anything to a provider.
pub struct VaultToolExecutor {
    pub vault: Arc<Vault>,
}

#[async_trait::async_trait]
impl ToolExecutor for VaultToolExecutor {
    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<String, String> {
        crate::permissions::authorize_in_process_tool(name, input).await?;

        let result = match name {
            "memory_search" => self.search(input).await,
            "memory_get" => self.get(input).await,
            "memory_save" => self.save(input).await,
            "memory_delete" => self.delete(input).await,
            "context_screen" => self.context_screen(),
            "context_window" => self.context_window(),
            "settings_list" => settings_tools::list(input),
            "settings_get" => settings_tools::get(input),
            "settings_set" => settings_tools::set(input),
            other => Err(format!(
                "unknown chat tool {other:?} (expected memory_search|memory_get|memory_save|memory_delete|context_screen|context_window|settings_list|settings_get|settings_set)"
            )),
        };
        if let Err(error) = &result {
            tracing::warn!(
                layer = "desktop",
                component = "chat_tools",
                tool = name,
                error = %error,
                "chat tool call failed"
            );
        }
        result
    }
}

impl VaultToolExecutor {
    async fn search(&self, input: &serde_json::Value) -> Result<String, String> {
        let query = str_field(input, "query")?;
        let limit = input
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(SEARCH_LIMIT_DEFAULT)
            .min(SEARCH_LIMIT_MAX) as u32;
        let hits = self
            .vault
            .search(query, limit)
            .await
            .map_err(|error| error.user_message())?;
        let rows: Vec<serde_json::Value> = hits
            .into_iter()
            .filter(|hit| {
                !matches!(hit.status, NodeStatus::Rejected | NodeStatus::Superseded)
                    && hit.sensitivity != Sensitivity::Sensitive
            })
            .map(|hit| {
                json!({
                    "id": hit.id,
                    "title": hit.title,
                    "type": hit.node_type.as_str(),
                    "status": hit.status.as_str(),
                    "snippet": hit.snippet,
                    "tags": hit.tags,
                    "project": hit.project,
                    "updated": hit.updated,
                })
            })
            .collect();
        serde_json::to_string(&rows).map_err(|error| error.to_string())
    }

    async fn get(&self, input: &serde_json::Value) -> Result<String, String> {
        let id = str_field(input, "id")?;
        let note = self
            .vault
            .get(id)
            .await
            .map_err(|error| error.user_message())?;
        if note.frontmatter.sensitivity == Sensitivity::Sensitive {
            return Err(
                "Sensitive memory body withheld by the chat privacy policy. Open it locally in the Memory tab."
                    .to_string(),
            );
        }
        serde_json::to_string(&json!({
            "id": note.frontmatter.id,
            "title": note.frontmatter.title,
            "type": note.frontmatter.node_type.as_str(),
            "status": note.frontmatter.status.as_str(),
            "tags": note.frontmatter.tags,
            "project": note.frontmatter.project,
            "sensitivity": note.frontmatter.sensitivity.as_str(),
            "body": note.body,
        }))
        .map_err(|error| error.to_string())
    }

    async fn save(&self, input: &serde_json::Value) -> Result<String, String> {
        let title = str_field(input, "title")?;
        let content = str_field(input, "content")?;
        let requested_type = input
            .get("type")
            .and_then(serde_json::Value::as_str)
            .and_then(NodeType::parse);
        let tags = input
            .get("tags")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            });
        let project = input
            .get("project")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        if let Some(id) = self
            .find_existing_note_id(title, project.as_deref())
            .await?
        {
            let mut note = self
                .vault
                .get(&id)
                .await
                .map_err(|error| error.user_message())?;
            note.body = content.to_string();
            note.frontmatter.status = NodeStatus::Confirmed;
            note.frontmatter.source = Source::Chat;
            if let Some(node_type) = requested_type {
                note.frontmatter.node_type = node_type;
            }
            if let Some(tags) = tags {
                note.frontmatter.tags = tags;
            }
            if let Some(project) = project {
                note.frontmatter.project = Some(project);
            }
            self.vault
                .save(&note)
                .await
                .map_err(|error| error.user_message())?;
            Ok(json!({"id": id, "updated": true}).to_string())
        } else {
            let note = self
                .vault
                .create(NoteDraft {
                    node_type: requested_type.unwrap_or(NodeType::Note),
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
                .map_err(|error| error.user_message())?;
            Ok(json!({"id": note.frontmatter.id, "updated": false}).to_string())
        }
    }

    async fn delete(&self, input: &serde_json::Value) -> Result<String, String> {
        let id = str_field(input, "id")?;
        self.vault
            .delete(id)
            .await
            .map_err(|error| error.user_message())?;
        Ok(json!({"deleted": true, "id": id}).to_string())
    }

    fn context_screen(&self) -> Result<String, String> {
        let (state, stale) = read_live_context_for_chat(ObservedSource::Screen)?;
        let monitors = state
            .monitors
            .iter()
            .map(|monitor| {
                json!({
                    "id": monitor.monitor_id,
                    "name": monitor.name,
                    "primary": monitor.is_primary,
                    "caption": monitor.description,
                    "privacy": monitor.privacy.as_str(),
                    "captured_at": monitor.captured_at,
                    "vision_updated_at": monitor.vision_updated_at,
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_string(&json!({
            "available": true,
            "stale": stale,
            "generated_at": state.generated_at,
            "monitors": monitors,
            "world_compact": state.compact_for_agents(4_000),
        }))
        .map_err(|error| error.to_string())
    }

    fn context_window(&self) -> Result<String, String> {
        let (state, stale) = read_live_context_for_chat(ObservedSource::Window)?;
        let active = state.window.map(|window| {
            json!({
                "process": window.process_name,
                "title": window.title,
                "pid": window.pid,
                "exe_path": window.exe_path,
                "monitor_id": window.monitor_id,
                "active_since_secs": window.active_since_secs,
                "observed_at": window.observed_at,
                "in_call": window.in_call,
                "privacy": window.privacy.as_str(),
            })
        });
        serde_json::to_string(&json!({
            "available": true,
            "stale": stale,
            "generated_at": state.generated_at,
            "active": active,
        }))
        .map_err(|error| error.to_string())
    }

    /// Same-title upsert is scoped by project so identical titles in two
    /// projects cannot overwrite each other. A missing project matches only a
    /// note that also has no project.
    async fn find_existing_note_id(
        &self,
        title: &str,
        project: Option<&str>,
    ) -> Result<Option<String>, String> {
        let target = title.trim().to_lowercase();
        if target.is_empty() {
            return Ok(None);
        }
        let hits = self
            .vault
            .search(title, SEARCH_LIMIT_MAX as u32)
            .await
            .map_err(|error| error.user_message())?;
        Ok(hits
            .into_iter()
            .find(|hit| {
                hit.title.trim().to_lowercase() == target && hit.project.as_deref() == project
            })
            .map(|hit| hit.id))
    }
}

/// Read the same runtime projection as the MCP context tools and apply the same
/// cloud egress privacy filter before exposing it to an HTTP model provider.
fn read_live_context_for_chat(source: ObservedSource) -> Result<(LiveWorldState, bool), String> {
    let dev_dir = continuum_dev_dir();
    let config_path = dev_dir.join("config.toml");
    let cfg = load_config(&config_path)
        .map_err(|error| format!("Live context is unavailable: could not load config: {error}"))?;

    if !cfg.context_tools.enabled {
        return Err("Live context is unavailable because [context_tools].enabled is false.".into());
    }
    if matches!(source, ObservedSource::Screen) && !cfg.screen.enabled {
        return Err("Live context is unavailable because [screen].enabled is false.".into());
    }
    if !source_enabled(&cfg.privacy.toggles, source) {
        let label = match source {
            ObservedSource::Screen => "screen observation",
            ObservedSource::Window => "window observation",
            _ => "the requested observation source",
        };
        return Err(format!(
            "Live context is unavailable because {label} is disabled by the current privacy toggles."
        ));
    }

    let path = dev_dir.join("live-context.json");
    let body = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "Live context is unavailable: could not read {}: {error}",
            path.display()
        )
    })?;
    let state: LiveWorldState = serde_json::from_str(&body).map_err(|error| {
        format!(
            "Live context is unavailable: {} contains invalid JSON: {error}",
            path.display()
        )
    })?;
    let stale = Utc::now()
        .signed_duration_since(state.generated_at)
        .num_seconds()
        > LIVE_CONTEXT_STALE_SECS;
    let filter = PrivacyFilter::from_config(&cfg.context, &cfg.privacy);
    Ok((state.cloud_view(&filter), stale))
}

fn str_field<'a>(input: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing required string field `{key}`"))
}

pub fn memory_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "memory_search".into(),
            description: "Search the user's saved memories. Sensitive, rejected and superseded memories are withheld from cloud chat."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search terms."},
                    "limit": {"type": "integer", "description": "Maximum results (default 10, maximum 25)."}
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "memory_get".into(),
            description: "Fetch one saved memory by id. Sensitive bodies are always withheld from cloud chat."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Memory id returned by memory_search."}
                },
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "memory_save".into(),
            description: "Save or update a memory. Identity is case-insensitive title within the same project, preventing same-title notes in separate projects from overwriting each other."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Short stable title."},
                    "content": {"type": "string", "description": "Memory body."},
                    "type": {"type": "string", "description": "project|goal|task|decision|person|preference|fact|error|session|note"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "project": {"type": "string", "description": "Optional project slug."}
                },
                "required": ["title", "content"]
            }),
        },
        ToolDef {
            name: "memory_delete".into(),
            description: "Permanently delete a saved memory by id. This is irreversible and requires an enforced confirmation by default."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Memory id to delete."}
                },
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "context_screen".into(),
            description: "Read Continuum's current privacy-filtered per-monitor visual captions. Use this before answering what is currently visible on a screen, monitor, or display. Monitor ids are display-N; for example screen 3/scherm 3/monitor 3 means display-3."
                .into(),
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "context_window".into(),
            description: "Read Continuum's current privacy-filtered foreground app/window, including which display it is on. Use this for questions about the active app or window."
                .into(),
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "settings_list".into(),
            description: "Discover Continuum runtime settings by typed dotted path. Use this when the user asks to change a setting and you do not already know the exact path. Results include current/default values and never expose secret values."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Optional path keyword such as screen, voice, privacy, resources, chat, github, or memory."},
                    "limit": {"type": "integer", "description": "Maximum matches (default 80, max 250)."}
                }
            }),
        },
        ToolDef {
            name: "settings_get".into(),
            description: "Read one exact Continuum setting by dotted path, including its default and where it appears in the UI. Secret-like values are redacted."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Exact dotted setting path discovered with settings_list."}
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "settings_set".into(),
            description: "Change one existing typed Continuum setting. Use only when the user's current request explicitly asks to change that setting. The candidate config is fully deserialized and validated before being written, and the previous config is backed up."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Exact dotted setting path discovered with settings_list/settings_get."},
                    "value": {"description": "New JSON value. It must match the setting's typed config field."}
                },
                "required": ["path", "value"]
            }),
        },
    ]
}

/// Claude CLI gets an explicit allowlist rather than a wildcard. Adding a new
/// server tool therefore does not implicitly grant chat access to it. The
/// conversation id scopes permission grants to this chat session.
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
            "mcp__continuum__fs_read_file",
            "mcp__continuum__fs_apply_patch",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
    })
}

fn resolve_mcp_binary() -> Option<PathBuf> {
    if let Ok(environment_binary) = std::env::var("CONTINUUM_MCP_BIN") {
        let path = PathBuf::from(environment_binary);
        if path.exists() {
            return Some(path);
        }
    }
    if let Some(candidate) = crate::commands::bundled_binary_candidates(MCP_BIN_NAME)
        .into_iter()
        .find(|candidate| candidate.exists())
    {
        return Some(candidate);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            let candidate = directory.join(MCP_BIN_NAME);
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
            "continuum-mcp binary not found — Claude CLI chat runs without Continuum tools"
        );
    });
    None
}

pub fn memory_context_section(notes: &[NodeSummary]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut output = String::from("## Memory context\n");
    for note in notes {
        if matches!(note.status, NodeStatus::Rejected | NodeStatus::Superseded)
            || note.sensitivity == Sensitivity::Sensitive
        {
            continue;
        }
        let title = note.title.replace(['\r', '\n'], " ");
        let snippet: String = note
            .snippet
            .as_deref()
            .unwrap_or("")
            .trim()
            .chars()
            .take(CONTEXT_SNIPPET_MAX_CHARS)
            .collect::<String>()
            .replace(['\r', '\n'], " ");
        if snippet.is_empty() {
            output.push_str(&format!("- [{}] {title}\n", note.node_type.as_str()));
        } else {
            output.push_str(&format!(
                "- [{}] {title}: {snippet}\n",
                note.node_type.as_str()
            ));
        }
    }
    if output == "## Memory context\n" {
        return String::new();
    }
    output.push_str(
        "\nThese are privacy-filtered saved memories retrieved for the current message. Use them only when relevant.\n",
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn executor() -> (tempfile::TempDir, VaultToolExecutor) {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let vault = Vault::open(temporary.path()).await.expect("open vault");
        (
            temporary,
            VaultToolExecutor {
                vault: Arc::new(vault),
            },
        )
    }

    fn parse(value: &str) -> serde_json::Value {
        serde_json::from_str(value).expect("tool output is JSON")
    }

    fn draft(
        title: &str,
        status: NodeStatus,
        sensitivity: Sensitivity,
        project: Option<&str>,
    ) -> NoteDraft {
        NoteDraft {
            node_type: NodeType::Fact,
            title: title.into(),
            body: format!("body of {title}"),
            project: project.map(str::to_string),
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
    async fn save_creates_then_updates_same_project_case_insensitively() {
        let (_temporary, executor) = executor().await;
        let created = executor
            .execute(
                "memory_save",
                &json!({
                    "title": "User Name",
                    "content": "Toshan",
                    "type": "fact",
                    "project": "continuum"
                }),
            )
            .await
            .expect("create memory");
        let created = parse(&created);
        let id = created["id"].as_str().expect("memory id").to_string();
        assert_eq!(created["updated"], false);

        let updated = executor
            .execute(
                "memory_save",
                &json!({
                    "title": "  user name ",
                    "content": "Toshan O.",
                    "project": "continuum",
                    "tags": ["identity"]
                }),
            )
            .await
            .expect("update memory");
        assert_eq!(parse(&updated)["id"], id);
        let note = executor.vault.get(&id).await.expect("updated note");
        assert_eq!(note.body, "Toshan O.");
        assert_eq!(note.frontmatter.tags, vec!["identity".to_string()]);
    }

    #[tokio::test]
    async fn same_title_in_different_projects_does_not_overwrite() {
        let (_temporary, executor) = executor().await;
        let first = parse(
            &executor
                .execute(
                    "memory_save",
                    &json!({"title":"Deployment", "content":"A", "project":"alpha"}),
                )
                .await
                .expect("first"),
        );
        let second = parse(
            &executor
                .execute(
                    "memory_save",
                    &json!({"title":"Deployment", "content":"B", "project":"beta"}),
                )
                .await
                .expect("second"),
        );
        assert_ne!(first["id"], second["id"]);
    }

    #[tokio::test]
    async fn sensitive_search_and_get_are_always_withheld() {
        let (_temporary, executor) = executor().await;
        let sensitive = executor
            .vault
            .create(draft(
                "Alpha secret",
                NodeStatus::Confirmed,
                Sensitivity::Sensitive,
                None,
            ))
            .await
            .expect("create sensitive note");

        let search = executor
            .execute("memory_search", &json!({"query":"alpha"}))
            .await
            .expect("search");
        assert!(parse(&search).as_array().expect("array").is_empty());

        let error = executor
            .execute("memory_get", &json!({"id":sensitive.frontmatter.id}))
            .await
            .expect_err("sensitive body must be withheld");
        assert!(error.contains("Sensitive memory body withheld"));
    }

    #[tokio::test]
    async fn rejected_notes_never_surface() {
        let (_temporary, executor) = executor().await;
        executor
            .vault
            .create(draft(
                "Rejected",
                NodeStatus::Rejected,
                Sensitivity::Internal,
                None,
            ))
            .await
            .expect("create rejected note");
        let output = executor
            .execute("memory_search", &json!({"query":"rejected"}))
            .await
            .expect("search");
        assert!(parse(&output).as_array().expect("array").is_empty());
    }

    #[tokio::test]
    async fn delete_removes_note() {
        let (_temporary, executor) = executor().await;
        let note = executor
            .vault
            .create(draft(
                "Forget",
                NodeStatus::Confirmed,
                Sensitivity::Internal,
                None,
            ))
            .await
            .expect("create note");
        let id = note.frontmatter.id;
        executor
            .execute("memory_delete", &json!({"id":id}))
            .await
            .expect("delete");
        assert!(executor.vault.get(&id).await.is_err());
    }

    #[test]
    fn memory_context_excludes_sensitive_and_rejected_notes() {
        let notes = vec![
            NodeSummary {
                id: "safe".into(),
                slug: "safe".into(),
                title: "Safe".into(),
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
                snippet: Some("visible".into()),
            },
            NodeSummary {
                id: "private".into(),
                slug: "private".into(),
                title: "Private".into(),
                node_type: NodeType::Fact,
                status: NodeStatus::Confirmed,
                project: None,
                confidence: 0.5,
                importance: 0.5,
                source: Source::Chat,
                sensitivity: Sensitivity::Sensitive,
                created: "2026-08-01T00:00:00Z".into(),
                updated: "2026-08-01T00:00:00Z".into(),
                tags: Vec::new(),
                snippet: Some("secret".into()),
            },
        ];
        let section = memory_context_section(&notes);
        assert!(section.contains("Safe"));
        assert!(!section.contains("Private"));
        assert!(!section.contains("secret"));
    }

    #[test]
    fn tool_definitions_cover_memory_live_context_and_settings() {
        let names = memory_tool_defs()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "memory_search",
                "memory_get",
                "memory_save",
                "memory_delete",
                "context_screen",
                "context_window",
                "settings_list",
                "settings_get",
                "settings_set"
            ]
        );
    }
}
