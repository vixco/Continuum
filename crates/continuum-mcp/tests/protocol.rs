//! # MCP protocol integration test
//!
//! Spawns the continuum-mcp binary as a child process and drives it through the
//! core MCP handshake over stdio:
//!
//! 1. `initialize` request → verify server capabilities advertise tools
//! 2. `notifications/initialized` → enter ready state
//! 3. `tools/list` → verify the complete Continuum tool registry
//! 4. `tools/call` on `system_current_time` → verify a valid ISO-8601
//!    timestamp is returned
//!
//! Per-tool behavior is covered by unit tests in the respective modules
//! (`audit`, `allowlist`, `tools::*`). This test only proves the JSON-RPC
//! wiring and the set of exposed tool names.

use std::process::Stdio;
use std::time::Duration;

use chrono::Utc;
use continuum_core::memory::semantic::{Fact, FactSource, SemanticStore};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

const EXPECTED_TOOLS: &[&str] = &[
    // Phase 4 — memory / system / fs / web
    "memory_query_episodic",
    "memory_list_facts",
    "memory_get_fact",
    "memory_set_fact",
    "system_current_time",
    "system_active_window",
    "system_live_context",
    "system_clipboard_get",
    "fs_read_file",
    "fs_list_dir",
    "web_fetch",
    "system_notification",
    // Phase 7 — repair
    "repair_restart_component",
    "repair_reinstall_model",
    "repair_rollback_config",
    "repair_test_component",
    "repair_escalate",
    // Phase 8 — workers
    "workers_spawn_worker",
    "workers_worker_status",
    "workers_worker_cancel",
    "workers_worker_wait",
    "workers_worker_list",
    // Plan B, Task 8 — memory vault
    "memory_vault_search",
    "memory_vault_get",
    "memory_vault_save",
    "memory_vault_resolve",
    "memory_vault_delete",
    "memory_wipe_all",
    // Plan C, Task C3 — context tool family, part 1 (published state)
    "context_session",
    "context_window",
    "context_screen",
    "context_audio",
    "context_projects",
    // Plan C, Task C4 — context tool family, part 2 (events / git / package)
    "context_timeline",
    "context_search",
    "context_files",
    "context_git",
    "context_package",
];

fn mcp_bin() -> std::path::PathBuf {
    // Cargo exposes the exact binary path to integration tests. This also
    // honors custom CARGO_TARGET_DIR values used by CI and remote test hosts.
    if let Some(path) = option_env!("CARGO_BIN_EXE_continuum-mcp") {
        return std::path::PathBuf::from(path);
    }

    // Fallback for direct execution outside Cargo.
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
            "continuum-mcp.exe"
        } else {
            "continuum-mcp"
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
        "continuum-mcp binary not found at {}; run `cargo build -p continuum-mcp` first",
        bin.display()
    );

    let data_dir = tempdir().expect("tempdir");

    let mut child = Command::new(&bin)
        .env("CONTINUUM_DATA_DIR", data_dir.path())
        .env("RUST_LOG", "warn") // silence info logs during the test
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn continuum-mcp");

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
                "name": "continuum-mcp-test",
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

// ---------------------------------------------------------------------------
// Vault tool round trip (Plan B, Task 8)
// ---------------------------------------------------------------------------

/// Writes `msg` as a single newline-delimited JSON-RPC frame to `stdin`.
async fn send_json(stdin: &mut ChildStdin, msg: &Value) {
    stdin
        .write_all(format!("{msg}\n").as_bytes())
        .await
        .expect("write request");
    stdin.flush().await.unwrap();
}

/// Sends a `tools/call` request for `name` with `arguments` and returns the
/// raw JSON-RPC response.
async fn call_tool(
    stdin: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    id: i64,
    name: &str,
    arguments: Value,
) -> Value {
    send_json(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }),
    )
    .await;
    read_response(reader).await
}

/// Parses a successful tool call's `content[0].text` (itself a JSON string)
/// into a [`Value`]. Panics with the full response if that shape isn't met
/// — every tool in this suite returns pretty-printed JSON text content.
fn tool_output(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tools/call response missing content[0].text: {resp}"));
    serde_json::from_str(text).unwrap_or_else(|_| panic!("tool output is not JSON: {text}"))
}

/// True if `resp` represents a failed tool call. A tool handler returning
/// `Err(McpError)` (this server's `run_tool` never converts an `Err` into a
/// `CallToolResult{isError: true}`) surfaces as a top-level JSON-RPC
/// `error` field; checking `result.isError` too keeps this robust against
/// either MCP error-reporting convention.
fn is_tool_error(resp: &Value) -> bool {
    resp.get("error").is_some() || resp["result"]["isError"].as_bool().unwrap_or(false)
}

#[tokio::test]
async fn vault_tools_round_trip() {
    let bin = mcp_bin();
    assert!(
        bin.exists(),
        "continuum-mcp binary not found at {}; run `cargo build -p continuum-mcp` first",
        bin.display()
    );

    let data_dir = tempdir().expect("tempdir");

    // Plant a `candidate` note directly on disk, before the server (and
    // therefore the vault) ever starts — the MCP server has no live
    // file-watcher, so the vault only ever learns about this file via the
    // full-vault reindex that `Vault::open` performs the first time any
    // memory_vault_* tool is called. Frontmatter shape per docs/memory.md.
    let candidate_id = "mem_01j8test0000000000candidate";
    let candidate_dir = data_dir.path().join("vault").join("notes");
    std::fs::create_dir_all(&candidate_dir).expect("create candidate note dir");
    std::fs::write(
        candidate_dir.join("candidate-test.md"),
        format!(
            "---\nid: {candidate_id}\ntype: note\ntitle: Candidate Under Review\n\
             status: candidate\ncreated: 2026-08-01T00:00:00Z\n---\n\
             This candidate note is awaiting resolution.\n"
        ),
    )
    .expect("write candidate note");

    let mut child = Command::new(&bin)
        .env("CONTINUUM_DATA_DIR", data_dir.path())
        .env("RUST_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn continuum-mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    // ---- handshake (tools/list is already exercised by the other test) ----
    send_json(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "continuum-mcp-test", "version": env!("CARGO_PKG_VERSION") }
            }
        }),
    )
    .await;
    let init_resp = read_response(&mut reader).await;
    assert_eq!(init_resp["id"], 1);

    send_json(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    // ---- memory_vault_save: create a new confirmed note ----
    let save_resp = call_tool(
        &mut stdin,
        &mut reader,
        2,
        "memory_vault_save",
        json!({
            "type": "note",
            "title": "Round Trip Vault Note",
            "body": "Hello from the vault tool round-trip test.",
            "tags": ["roundtrip"]
        }),
    )
    .await;
    assert!(!is_tool_error(&save_resp), "save failed: {save_resp}");
    let saved = tool_output(&save_resp);
    let saved_id = saved["id"]
        .as_str()
        .expect("save response missing id")
        .to_string();
    assert_eq!(
        saved["updated"], false,
        "expected a fresh create, not an update"
    );

    // ---- memory_vault_get: fetch it back ----
    let get_resp = call_tool(
        &mut stdin,
        &mut reader,
        3,
        "memory_vault_get",
        json!({ "id": saved_id }),
    )
    .await;
    assert!(!is_tool_error(&get_resp), "get failed: {get_resp}");
    let fetched = tool_output(&get_resp);
    assert_eq!(fetched["frontmatter"]["title"], "Round Trip Vault Note");
    assert_eq!(
        fetched["body"],
        "Hello from the vault tool round-trip test."
    );

    // ---- memory_vault_search: find it by title ----
    let search_resp = call_tool(
        &mut stdin,
        &mut reader,
        4,
        "memory_vault_search",
        json!({ "query": "Round Trip Vault Note", "limit": 5 }),
    )
    .await;
    assert!(!is_tool_error(&search_resp), "search failed: {search_resp}");
    let hits = tool_output(&search_resp);
    let hits = hits.as_array().expect("search result is not an array");
    assert!(
        hits.iter().any(|h| h["id"] == saved_id),
        "expected {saved_id} in search hits: {hits:?}"
    );

    // ---- memory_vault_resolve: confirm the pre-planted candidate ----
    let resolve_resp = call_tool(
        &mut stdin,
        &mut reader,
        5,
        "memory_vault_resolve",
        json!({ "id": candidate_id, "action": "confirm" }),
    )
    .await;
    assert!(
        !is_tool_error(&resolve_resp),
        "resolve failed: {resolve_resp}"
    );
    let resolved = tool_output(&resolve_resp);
    assert_eq!(resolved["ok"], true);

    let candidate_after = call_tool(
        &mut stdin,
        &mut reader,
        6,
        "memory_vault_get",
        json!({ "id": candidate_id }),
    )
    .await;
    assert!(!is_tool_error(&candidate_after));
    assert_eq!(
        tool_output(&candidate_after)["frontmatter"]["status"],
        "confirmed"
    );

    // ---- memory_vault_resolve: "supersede" without `replaces` errors ----
    let bad_resolve = call_tool(
        &mut stdin,
        &mut reader,
        7,
        "memory_vault_resolve",
        json!({ "id": saved_id, "action": "supersede" }),
    )
    .await;
    assert!(
        is_tool_error(&bad_resolve),
        "expected an error for supersede without replaces: {bad_resolve}"
    );

    // ---- memory_vault_delete: unknown id maps to a graceful tool error, not a crash ----
    let bad_delete = call_tool(
        &mut stdin,
        &mut reader,
        8,
        "memory_vault_delete",
        json!({ "id": "mem_does_not_exist" }),
    )
    .await;
    assert!(
        is_tool_error(&bad_delete),
        "expected an error deleting an unknown id: {bad_delete}"
    );

    // ---- memory_vault_delete: removes the note's markdown file for real ----
    let delete_resp = call_tool(
        &mut stdin,
        &mut reader,
        9,
        "memory_vault_delete",
        json!({ "id": saved_id }),
    )
    .await;
    assert!(!is_tool_error(&delete_resp), "delete failed: {delete_resp}");
    let deleted = tool_output(&delete_resp);
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["id"], saved_id);

    // ---- memory_vault_get: the deleted note is really gone ----
    let get_after_delete = call_tool(
        &mut stdin,
        &mut reader,
        10,
        "memory_vault_get",
        json!({ "id": saved_id }),
    )
    .await;
    assert!(
        is_tool_error(&get_after_delete),
        "expected the deleted note to be gone: {get_after_delete}"
    );

    // ---- memory_wipe_all: wrong confirm string errors, no file written ----
    let bad_wipe = call_tool(
        &mut stdin,
        &mut reader,
        11,
        "memory_wipe_all",
        json!({ "confirm": "please" }),
    )
    .await;
    assert!(
        is_tool_error(&bad_wipe),
        "expected an error for the wrong confirm string: {bad_wipe}"
    );
    assert!(!data_dir.path().join("wipe-request.json").exists());

    // ---- memory_wipe_all: correct confirm writes the request file ----
    let wipe_resp = call_tool(
        &mut stdin,
        &mut reader,
        12,
        "memory_wipe_all",
        json!({ "confirm": "WIPE" }),
    )
    .await;
    assert!(!is_tool_error(&wipe_resp), "wipe failed: {wipe_resp}");
    let wipe_out = tool_output(&wipe_resp);
    let wipe_path = wipe_out["path"]
        .as_str()
        .expect("wipe response missing path");
    assert!(
        wipe_path.ends_with("wipe-request.json"),
        "unexpected wipe path: {wipe_path}"
    );
    let expected_path = data_dir.path().join("wipe-request.json");
    assert!(expected_path.exists(), "wipe-request.json was not written");
    let contents = std::fs::read_to_string(&expected_path).expect("read wipe-request.json");
    assert!(contents.contains("\"raw_log\""));
    assert!(contents.contains("\"episodic\""));
    assert!(contents.contains("\"events\""));

    // ---- Clean shutdown ----
    drop(stdin);
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

/// Fix round 1 (review finding, HIGH): `memory_get_fact`/`memory_list_facts`
/// must fall back to the legacy semantic store not only when the vault has
/// no matching note, but also when the vault itself is unusable (unopenable
/// directory, disk full, unrecoverable index corruption, ...) — the
/// contract is vault-FIRST, legacy-FALLBACK on any vault miss, not just "no
/// match". This is the deterministic injection the review asked for:
/// `CONTINUUM_VAULT_DIR` is pointed at a plain FILE (not a directory), so
/// `Vault::open`'s `create_dir_all` fails every time the server tries to
/// lazily open the vault, while a legacy `semantic.sqlite` seeded directly
/// (via `continuum_core::memory::semantic::SemanticStore`, a regular
/// dependency of this crate and therefore usable directly from an
/// integration test) still answers the query.
#[tokio::test]
async fn memory_get_fact_falls_back_when_vault_is_unopenable() {
    let bin = mcp_bin();
    assert!(
        bin.exists(),
        "continuum-mcp binary not found at {}; run `cargo build -p continuum-mcp` first",
        bin.display()
    );

    let data_dir = tempdir().expect("tempdir");

    // Seed the legacy store directly, the way this fact would have landed
    // there before the vault redirect (or via a pre-vault install).
    let semantic_path = data_dir.path().join("semantic.sqlite");
    {
        let store = SemanticStore::open(&semantic_path.to_string_lossy())
            .await
            .expect("open legacy semantic store");
        store
            .upsert_fact(&Fact {
                key: "user.name".to_string(),
                value: "Toshan".to_string(),
                confidence: 0.9,
                source: FactSource::UserStated,
                source_frame_id: None,
                updated_at: Utc::now(),
            })
            .await
            .expect("seed legacy fact");
    }

    // A regular file where the vault dir is expected: `Vault::open`'s
    // `create_dir_all(dir)` errors on this (path exists and is not a
    // directory), so every `self.vault().await` call in the server fails.
    let vault_file = data_dir.path().join("vault-is-actually-a-file");
    std::fs::write(&vault_file, b"not a directory").expect("write vault placeholder file");

    let mut child = Command::new(&bin)
        .env("CONTINUUM_DATA_DIR", data_dir.path())
        .env("CONTINUUM_VAULT_DIR", &vault_file)
        .env("RUST_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn continuum-mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);

    send_json(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "continuum-mcp-test", "version": env!("CARGO_PKG_VERSION") }
            }
        }),
    )
    .await;
    let init_resp = read_response(&mut reader).await;
    assert_eq!(init_resp["id"], 1);

    send_json(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    let get_resp = call_tool(
        &mut stdin,
        &mut reader,
        2,
        "memory_get_fact",
        json!({ "key": "user.name" }),
    )
    .await;
    assert!(
        !is_tool_error(&get_resp),
        "memory_get_fact should still succeed via the legacy fallback when the vault is \
         unopenable: {get_resp}"
    );
    let fact = tool_output(&get_resp);
    assert_eq!(fact["key"], "user.name");
    assert_eq!(fact["value"], "Toshan");

    // memory_list_facts (no prefix) must fall back the same way.
    let list_resp = call_tool(&mut stdin, &mut reader, 3, "memory_list_facts", json!({})).await;
    assert!(
        !is_tool_error(&list_resp),
        "memory_list_facts should still succeed via the legacy fallback when the vault is \
         unopenable: {list_resp}"
    );
    let facts = tool_output(&list_resp);
    let facts = facts.as_array().expect("list result is not an array");
    assert!(
        facts.iter().any(|f| f["key"] == "user.name"),
        "expected the legacy-seeded fact in memory_list_facts output: {facts:?}"
    );

    // ---- Clean shutdown ----
    drop(stdin);
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}
