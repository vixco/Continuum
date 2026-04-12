//! # MCP protocol integration test
//!
//! Spawns the kairo-mcp binary as a child process and drives it through the
//! core MCP handshake over stdio:
//!
//! 1. `initialize` request → verify server capabilities advertise tools
//! 2. `notifications/initialized` → enter ready state
//! 3. `tools/list` → verify all 11 Kairo tools are registered
//! 4. `tools/call` on `system_current_time` → verify a valid ISO-8601
//!    timestamp is returned
//!
//! Per-tool behavior is covered by unit tests in the respective modules
//! (`audit`, `allowlist`, `tools::*`). This test only proves the JSON-RPC
//! wiring and the set of exposed tool names.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdout, Command};
use tokio::time::timeout;

const EXPECTED_TOOLS: &[&str] = &[
    "memory_query_episodic",
    "memory_list_facts",
    "memory_get_fact",
    "memory_set_fact",
    "system_current_time",
    "system_active_window",
    "system_clipboard_get",
    "fs_read_file",
    "fs_list_dir",
    "web_fetch",
    "system_notification",
];

fn mcp_bin() -> std::path::PathBuf {
    // tests are run by cargo; the binary lives in target/debug or target/release
    // one directory up from CARGO_MANIFEST_DIR.
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("cannot locate workspace root");
    // Honor profile — tests run in debug by default.
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    workspace
        .join("target")
        .join(profile)
        .join(if cfg!(windows) {
            "kairo-mcp.exe"
        } else {
            "kairo-mcp"
        })
}

async fn read_response(reader: &mut BufReader<ChildStdout>) -> Value {
    let mut line = String::new();
    let n = timeout(Duration::from_secs(10), reader.read_line(&mut line))
        .await
        .expect("response timeout")
        .expect("read error");
    assert!(n > 0, "unexpected EOF from MCP server");
    serde_json::from_str(line.trim_end()).expect("server emitted non-JSON line")
}

#[tokio::test]
async fn protocol_handshake_and_one_tool_call() {
    let bin = mcp_bin();
    assert!(
        bin.exists(),
        "kairo-mcp binary not found at {}; run `cargo build -p kairo-mcp` first",
        bin.display()
    );

    let data_dir = tempdir().expect("tempdir");

    let mut child = Command::new(&bin)
        .env("KAIRO_DATA_DIR", data_dir.path())
        .env("RUST_LOG", "warn") // silence info logs during the test
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kairo-mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    // ---- 1. initialize ----
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "kairo-mcp-test",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    });
    stdin
        .write_all(format!("{init}\n").as_bytes())
        .await
        .expect("write init");
    stdin.flush().await.unwrap();

    let init_resp = read_response(&mut reader).await;
    assert_eq!(init_resp["jsonrpc"], "2.0");
    assert_eq!(init_resp["id"], 1);
    let caps = &init_resp["result"]["capabilities"];
    assert!(
        caps.get("tools").is_some(),
        "server did not advertise tools capability: {init_resp}"
    );

    // ---- 2. initialized notification ----
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    stdin
        .write_all(format!("{initialized}\n").as_bytes())
        .await
        .expect("write initialized");
    stdin.flush().await.unwrap();

    // ---- 3. tools/list ----
    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    });
    stdin
        .write_all(format!("{list_req}\n").as_bytes())
        .await
        .expect("write tools/list");
    stdin.flush().await.unwrap();

    let list_resp = read_response(&mut reader).await;
    assert_eq!(list_resp["id"], 2);
    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools is not an array");

    let mut tool_names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool.name is not a string"))
        .collect();
    tool_names.sort();

    let mut expected_sorted: Vec<&str> = EXPECTED_TOOLS.to_vec();
    expected_sorted.sort();
    assert_eq!(
        tool_names, expected_sorted,
        "tools/list returned unexpected set"
    );

    // ---- 4. tools/call: system_current_time ----
    let call_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "system_current_time",
            "arguments": {}
        }
    });
    stdin
        .write_all(format!("{call_req}\n").as_bytes())
        .await
        .expect("write tools/call");
    stdin.flush().await.unwrap();

    let call_resp = read_response(&mut reader).await;
    assert_eq!(call_resp["id"], 3);
    assert!(
        !call_resp["result"]["isError"].as_bool().unwrap_or(false),
        "tools/call returned error: {call_resp}"
    );
    let content_text = call_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text missing");
    let parsed: Value = serde_json::from_str(content_text).expect("tool output is not JSON");
    let iso = parsed["iso8601"]
        .as_str()
        .expect("iso8601 field missing from system_current_time response");
    assert!(
        iso.contains('T') && iso.contains(':'),
        "iso8601 doesn't look like ISO-8601: {iso}"
    );

    // ---- Clean shutdown ----
    drop(stdin);
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}
