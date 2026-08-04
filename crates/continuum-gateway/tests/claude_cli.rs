use std::time::Duration;

use continuum_gateway::providers::ClaudeCliAdapter;
use continuum_gateway::{ChatMessage, ChatProvider, ChatRequest, ChatRole, GatewayError};
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
