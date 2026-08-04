use std::time::Duration;

use continuum_gateway::providers::claude_cli::{flatten_tool_result_content, mcp_config_json};
use continuum_gateway::providers::ClaudeCliAdapter;
use continuum_gateway::{
    ChatEvent, ChatMessage, ChatProvider, ChatRequest, ChatRole, GatewayError, McpSpec,
};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

/// Writes a stub "claude" that ignores stdin and prints canned stream-json.
/// Windows: .cmd batch file; unix: sh script.
fn write_stub(dir: &std::path::Path, lines: &[&str]) -> String {
    #[cfg(windows)]
    {
        let path = dir.join("claude-stub.cmd");
        let mut body = String::from("@echo off\r\n");
        for l in lines {
            // ^ escapes special chars in cmd; JSON quotes survive inside echo
            body.push_str(&format!(
                "echo {}\r\n",
                l.replace('"', "\"\"").replace("\"\"", "\"")
            ));
        }
        std::fs::write(&path, body).expect("write stub");
        path.to_string_lossy().into_owned()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("claude-stub.sh");
        let mut body = String::from("#!/bin/sh\n");
        for l in lines {
            body.push_str(&format!("echo '{l}'\n"));
        }
        std::fs::write(&path, &body).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path.to_string_lossy().into_owned()
    }
}

fn req() -> ChatRequest {
    ChatRequest {
        model: "claude-opus-4-6".into(),
        system: "You are Continuum.".into(),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hi".into(),
        }],
        max_tokens: 64,
        temperature: None,
        tools: vec![],
        executor: None,
        mcp: None,
        tool_max_rounds: continuum_gateway::DEFAULT_MAX_TOOL_ROUNDS,
    }
}

/// Like [`req`] but with an [`McpSpec`] attached, exercising the MCP path.
fn req_with_mcp() -> ChatRequest {
    let mut r = req();
    r.mcp = Some(McpSpec {
        server_command: std::path::PathBuf::from("C:/fake/continuum-mcp.exe"),
        env: vec![("CONTINUUM_VAULT_DIR".into(), "C:/fake/vault".into())],
        allowed_tools: vec![
            "mcp__continuum__memory_vault_search".into(),
            "mcp__continuum__memory_vault_get".into(),
        ],
    });
    r
}

/// Writes a stub that records its argv to `argv.txt` next to itself, copies
/// any `--mcp-config` file to `mcp.json`, then prints a success result event.
fn write_argv_stub(dir: &std::path::Path) -> String {
    #[cfg(windows)]
    {
        let path = dir.join("claude-argv-stub.cmd");
        // NOTE: a `shift`/`goto :loop` walk over %1 is a trap here — cmd's
        // plain `shift` also shifts %0, so `%~dp0` stops resolving to the
        // stub's directory mid-loop and the outputs land in the test CWD.
        // A `for` over %* leaves %0 intact. The "1 file(s) copied." chatter
        // on stdout is harmless: the adapter ignores non-JSON lines.
        let body = concat!(
            "@echo off\r\n",
            "echo %* > \"%~dp0argv.txt\"\r\n",
            "set \"grab=\"\r\n",
            "for %%A in (%*) do (\r\n",
            "  if defined grab copy %%A \"%~dp0mcp.json\"\r\n",
            "  if defined grab set \"grab=\"\r\n",
            "  if \"%%~A\"==\"--mcp-config\" set \"grab=1\"\r\n",
            ")\r\n",
            "echo {\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}\r\n",
        );
        std::fs::write(&path, body).expect("write stub");
        path.to_string_lossy().into_owned()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("claude-argv-stub.sh");
        let body = r#"#!/bin/sh
dir="$(cd "$(dirname "$0")" && pwd)"
printf '%s\n' "$@" > "$dir/argv.txt"
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--mcp-config" ]; then cp "$2" "$dir/mcp.json"; fi
  shift
done
echo '{"type":"result","subtype":"success","is_error":false}'
"#;
        std::fs::write(&path, body).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path.to_string_lossy().into_owned()
    }
}

/// Drains a full stream from `stub` for `req`, returning every event.
async fn collect_events(stub: String, req: ChatRequest) -> Vec<ChatEvent> {
    let a = ClaudeCliAdapter::new(stub, Duration::from_secs(10));
    let mut s = a
        .stream_chat(req, CancellationToken::new())
        .await
        .expect("stream");
    let mut out = Vec::new();
    while let Some(ev) = s.next().await {
        out.push(ev);
    }
    out
}

/// Reads the argv the stub recorded, normalized to a single space-separated
/// line (the unix stub prints one arg per line).
fn read_argv(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("argv.txt"))
        .expect("argv.txt written by stub")
        .replace("\r\n", " ")
        .replace('\n', " ")
}

#[tokio::test]
async fn streams_deltas_from_stub() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let stub = write_stub(
        dir.path(),
        &[
            r#"{"type":"system","subtype":"init","session_id":"s"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hallo"}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"usage":{"output_tokens":3}}"#,
        ],
    );
    let a = ClaudeCliAdapter::new(stub, Duration::from_secs(10));
    let mut s = a
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    let mut text = String::new();
    let mut done = false;
    while let Some(ev) = s.next().await {
        match ev {
            continuum_gateway::ChatEvent::Delta { text: t } => text.push_str(&t),
            continuum_gateway::ChatEvent::Done { usage, .. } => {
                assert_eq!(usage.output_tokens, Some(3));
                done = true;
            }
            continuum_gateway::ChatEvent::Error { message, .. } => panic!("{message}"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert_eq!(text, "Hallo");
    assert!(done);
}

#[tokio::test]
async fn missing_binary_maps_to_cli_not_found() {
    let a = ClaudeCliAdapter::new(
        "definitely-not-a-real-binary-xyz".into(),
        Duration::from_secs(2),
    );
    let err = a
        .stream_chat(req(), CancellationToken::new())
        .await
        .err()
        .expect("err");
    assert!(matches!(err, GatewayError::CliNotFound));
}

#[tokio::test]
async fn error_result_maps_to_not_logged_in() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let stub = write_stub(
        dir.path(),
        &[
            r#"{"type":"result","subtype":"error","is_error":true,"result":"Not logged in. Please run claude login"}"#,
        ],
    );
    let a = ClaudeCliAdapter::new(stub, Duration::from_secs(10));
    let mut s = a
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    let mut saw = false;
    while let Some(ev) = s.next().await {
        if let continuum_gateway::ChatEvent::Error { message, .. } = ev {
            assert!(message.contains("claude login"), "{message}");
            saw = true;
        }
    }
    assert!(saw);
}

#[test]
fn mcp_config_json_matches_orchestrator_shape() {
    let spec = McpSpec {
        server_command: std::path::PathBuf::from("C:/fake/continuum-mcp.exe"),
        env: vec![("CONTINUUM_VAULT_DIR".into(), "C:/fake/vault".into())],
        allowed_tools: vec!["mcp__continuum__memory_vault_search".into()],
    };
    let v = mcp_config_json(&spec);
    assert_eq!(
        v,
        serde_json::json!({
            "mcpServers": {
                "continuum": {
                    "type": "stdio",
                    "command": "C:/fake/continuum-mcp.exe",
                    "args": [],
                    "env": {"CONTINUUM_VAULT_DIR": "C:/fake/vault"}
                }
            }
        })
    );
}

#[test]
fn flatten_tool_result_content_variants() {
    use serde_json::json;
    // String passthrough.
    assert_eq!(flatten_tool_result_content(&json!("plain")), "plain");
    // Array of text blocks joins with newline.
    assert_eq!(
        flatten_tool_result_content(&json!([
            {"type": "text", "text": "a"},
            {"type": "text", "text": "b"}
        ])),
        "a\nb"
    );
    // Non-text array items fall back to their JSON serialization.
    let image = json!({"type": "image", "source": "x"});
    assert_eq!(
        flatten_tool_result_content(&json!([image.clone()])),
        image.to_string()
    );
    // Non-string, non-array content falls back to JSON serialization.
    let obj = json!({"weird": true});
    assert_eq!(flatten_tool_result_content(&obj), obj.to_string());
    assert_eq!(
        flatten_tool_result_content(&serde_json::Value::Null),
        "null"
    );
}

#[tokio::test]
async fn mcp_spec_adds_mcp_flags_and_config_file() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let stub = write_argv_stub(dir.path());
    let events = collect_events(stub, req_with_mcp()).await;
    assert!(
        matches!(events.last(), Some(ChatEvent::Done { .. })),
        "{events:?}"
    );
    let argv = read_argv(dir.path());
    assert!(argv.contains("--mcp-config"), "{argv}");
    assert!(argv.contains("--strict-mcp-config"), "{argv}");
    // Windows quotes the comma-containing value when spawning a batch
    // stub; strip quotes so the assertion holds on every platform.
    assert!(
        argv.replace('"', "").contains(
            "--allowedTools mcp__continuum__memory_vault_search,mcp__continuum__memory_vault_get"
        ),
        "{argv}"
    );
    assert!(argv.contains("--permission-mode default"), "{argv}");
    // The stub copied the temp config file while the child was alive —
    // proving both the keep-alive and the JSON shape on disk.
    let cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("mcp.json")).expect("mcp.json copied by stub"),
    )
    .expect("parse mcp.json");
    let server = &cfg["mcpServers"]["continuum"];
    assert_eq!(server["type"], "stdio");
    assert_eq!(server["command"], "C:/fake/continuum-mcp.exe");
    assert_eq!(server["args"], serde_json::json!([]));
    assert_eq!(server["env"]["CONTINUUM_VAULT_DIR"], "C:/fake/vault");
}

#[tokio::test]
async fn no_mcp_keeps_flags_unchanged() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let stub = write_argv_stub(dir.path());
    let events = collect_events(stub, req()).await;
    assert!(
        matches!(events.last(), Some(ChatEvent::Done { .. })),
        "{events:?}"
    );
    let argv = read_argv(dir.path());
    assert!(!argv.contains("--mcp-config"), "{argv}");
    assert!(!argv.contains("--strict-mcp-config"), "{argv}");
    assert!(argv.contains("--permission-mode default"), "{argv}");
    // Windows quoting renders the empty --allowedTools value as "".
    #[cfg(windows)]
    assert!(argv.contains("--allowedTools \"\""), "{argv}");
}

#[tokio::test]
async fn emits_tool_call_and_result_exactly_once_across_snapshots() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let stub = write_stub(
        dir.path(),
        &[
            r#"{"type":"system","subtype":"init","session_id":"s"}"#,
            // Partial snapshot: input not yet streamed in — must NOT emit.
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"tu_1","name":"mcp__continuum__memory_vault_search","input":{}}],"stop_reason":null}}"#,
            // Complete snapshot: emit ToolCall now.
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"tu_1","name":"mcp__continuum__memory_vault_search","input":{"query":"rust"}}],"stop_reason":"tool_use"}}"#,
            // Replayed snapshot: deduped by id.
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"tu_1","name":"mcp__continuum__memory_vault_search","input":{"query":"rust"}}],"stop_reason":"tool_use"}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"found 3"}]}}"#,
            // Replayed tool_result: deduped by tool_use_id.
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"found 3"}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Done"}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"usage":{"output_tokens":5}}"#,
        ],
    );
    let events = collect_events(stub, req_with_mcp()).await;
    assert_eq!(events.len(), 4, "{events:?}");
    match &events[0] {
        ChatEvent::ToolCall { id, name, input } => {
            assert_eq!(id, "tu_1");
            assert_eq!(name, "mcp__continuum__memory_vault_search");
            assert_eq!(input["query"], "rust");
        }
        other => panic!("expected ToolCall first, got {other:?}"),
    }
    match &events[1] {
        ChatEvent::ToolResult {
            id,
            output,
            is_error,
            duration_ms,
        } => {
            assert_eq!(id, "tu_1");
            assert_eq!(output, "found 3");
            assert!(!*is_error);
            assert_eq!(*duration_ms, 0);
        }
        other => panic!("expected ToolResult second, got {other:?}"),
    }
    assert!(
        matches!(&events[2], ChatEvent::Delta { text } if text.as_str() == "Done"),
        "{events:?}"
    );
    assert!(matches!(&events[3], ChatEvent::Done { .. }), "{events:?}");
}

#[tokio::test]
async fn empty_input_tool_call_emitted_late_and_array_content_flattens() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let stub = write_stub(
        dir.path(),
        &[
            // Only ever seen with empty input and no tool_use stop_reason:
            // the ToolCall must still be emitted (with input {}) when the
            // matching tool_result arrives, and before it.
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"tu_9","name":"mcp__continuum__memory_vault_get","input":{}}],"stop_reason":null}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_9","content":[{"type":"text","text":"line1"},{"type":"text","text":"line2"}],"is_error":true}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        ],
    );
    let events = collect_events(stub, req_with_mcp()).await;
    assert_eq!(events.len(), 3, "{events:?}");
    match &events[0] {
        ChatEvent::ToolCall { id, name, input } => {
            assert_eq!(id, "tu_9");
            assert_eq!(name, "mcp__continuum__memory_vault_get");
            assert_eq!(input, &serde_json::json!({}));
        }
        other => panic!("expected ToolCall first, got {other:?}"),
    }
    match &events[1] {
        ChatEvent::ToolResult {
            id,
            output,
            is_error,
            duration_ms,
        } => {
            assert_eq!(id, "tu_9");
            assert_eq!(output, "line1\nline2");
            assert!(*is_error);
            assert_eq!(*duration_ms, 0);
        }
        other => panic!("expected ToolResult second, got {other:?}"),
    }
    assert!(matches!(&events[2], ChatEvent::Done { .. }), "{events:?}");
}

#[tokio::test]
async fn empty_input_with_tool_use_stop_reason_emits_immediately() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let stub = write_stub(
        dir.path(),
        &[
            // stop_reason "tool_use" marks the snapshot final: emit even
            // though input is an empty object (no tool_result ever arrives).
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"tu_5","name":"mcp__continuum__memory_vault_get","input":{}}],"stop_reason":"tool_use"}}"#,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        ],
    );
    let events = collect_events(stub, req_with_mcp()).await;
    assert_eq!(events.len(), 2, "{events:?}");
    match &events[0] {
        ChatEvent::ToolCall { id, name, input } => {
            assert_eq!(id, "tu_5");
            assert_eq!(name, "mcp__continuum__memory_vault_get");
            assert_eq!(input, &serde_json::json!({}));
        }
        other => panic!("expected ToolCall first, got {other:?}"),
    }
    assert!(matches!(&events[1], ChatEvent::Done { .. }), "{events:?}");
}
