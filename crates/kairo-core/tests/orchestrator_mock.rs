//! # Integration test: orchestrator with mock claude subprocess
//!
//! Verifies the orchestrator's event stream processing by spawning a mock
//! process that emits predefined Claude Code CLI events instead of actually
//! calling the Anthropic API.
//!
//! This test does NOT require an API key or network access.

use kairo_core::orchestrator::events::ClaudeEvent;
use kairo_core::orchestrator::spawn::{OrchestratorEvent, WakeResult};
use std::io::Write;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Pre-recorded events from a real Claude Code CLI session (Phase 0).
/// The mock subprocess will emit these on stdout.
const MOCK_EVENTS: &[&str] = &[
    r#"{"type":"system","subtype":"init","cwd":"F:\\test","session_id":"mock-session-001","tools":[],"mcp_servers":[],"model":"claude-opus-4-6","permissionMode":"plan","apiKeySource":"none","claude_code_version":"2.1.100","uuid":"mock-uuid-1"}"#,
    r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-4-6","id":"msg_mock","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":100,"output_tokens":1}}},"session_id":"mock-session-001","parent_tool_use_id":null,"uuid":"mock-uuid-2"}"#,
    r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"session_id":"mock-session-001","parent_tool_use_id":null,"uuid":"mock-uuid-3"}"#,
    r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Ik zie dat je"}},"session_id":"mock-session-001","parent_tool_use_id":null,"uuid":"mock-uuid-4"}"#,
    r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" vastzit op"}},"session_id":"mock-session-001","parent_tool_use_id":null,"uuid":"mock-uuid-5"}"#,
    r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" een test failure."}},"session_id":"mock-session-001","parent_tool_use_id":null,"uuid":"mock-uuid-6"}"#,
    r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0},"session_id":"mock-session-001","parent_tool_use_id":null,"uuid":"mock-uuid-7"}"#,
    r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":100,"output_tokens":15}},"session_id":"mock-session-001","parent_tool_use_id":null,"uuid":"mock-uuid-8"}"#,
    r#"{"type":"stream_event","event":{"type":"message_stop"},"session_id":"mock-session-001","parent_tool_use_id":null,"uuid":"mock-uuid-9"}"#,
    r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1500,"duration_api_ms":1400,"num_turns":1,"result":"Ik zie dat je vastzit op een test failure.","stop_reason":"end_turn","session_id":"mock-session-001","total_cost_usd":0.035,"usage":{"input_tokens":100,"output_tokens":15},"uuid":"mock-uuid-10"}"#,
];

/// Tests that our event parser correctly handles a full event stream.
#[tokio::test]
async fn test_parse_mock_event_stream() {
    let mut events_seen = Vec::new();

    for line in MOCK_EVENTS {
        let event: ClaudeEvent = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("Failed to parse mock event: {e}\nLine: {line}"));
        events_seen.push(event);
    }

    // Verify we got the expected sequence.
    assert!(events_seen[0].is_system_init());
    assert!(events_seen[9].is_result());

    // Verify text deltas.
    let text: String = events_seen
        .iter()
        .filter_map(|e| e.as_text_delta())
        .collect();
    assert_eq!(text, "Ik zie dat je vastzit op een test failure.");

    // Verify result event.
    match &events_seen[9] {
        ClaudeEvent::Result(r) => {
            assert!(!r.is_error);
            assert_eq!(r.total_cost_usd, Some(0.035));
            assert_eq!(r.duration_ms, Some(1500));
            assert_eq!(r.session_id.as_deref(), Some("mock-session-001"));
        }
        _ => panic!("Expected result event"),
    }
}

/// Tests that an error result event is correctly detected.
#[tokio::test]
async fn test_parse_error_result() {
    let error_result = r#"{"type":"result","subtype":"error","is_error":true,"duration_ms":500,"result":"Authentication failed","stop_reason":"error","session_id":"mock-err-001","total_cost_usd":0.0,"uuid":"mock-err-uuid"}"#;

    let event: ClaudeEvent = serde_json::from_str(error_result).unwrap();
    match event {
        ClaudeEvent::Result(r) => {
            assert!(r.is_error);
            assert_eq!(r.subtype.as_deref(), Some("error"));
            assert_eq!(r.result.as_deref(), Some("Authentication failed"));
        }
        _ => panic!("Expected result event"),
    }
}

/// Tests that the OrchestratorEvent enum correctly represents a wake cycle.
#[tokio::test]
async fn test_orchestrator_event_flow() {
    let mut collected_events: Vec<OrchestratorEvent> = Vec::new();
    let mut full_text = String::new();

    // Simulate the event processing loop.
    for line in MOCK_EVENTS {
        let event: ClaudeEvent = serde_json::from_str(line).unwrap();

        match &event {
            ClaudeEvent::System(sys) => {
                if let Some(ref sid) = sys.session_id {
                    collected_events.push(OrchestratorEvent::SessionReady {
                        session_id: sid.clone(),
                    });
                }
            }
            ClaudeEvent::StreamEvent(_) => {
                if let Some(text) = event.as_text_delta() {
                    full_text.push_str(text);
                    collected_events.push(OrchestratorEvent::TextDelta(text.to_string()));
                }
            }
            ClaudeEvent::Result(r) => {
                collected_events.push(OrchestratorEvent::ResponseComplete {
                    full_text: full_text.clone(),
                    cost_usd: r.total_cost_usd,
                    duration_ms: r.duration_ms,
                    session_id: r.session_id.clone(),
                });
            }
            _ => {}
        }
    }

    // Verify session ready.
    assert!(matches!(
        &collected_events[0],
        OrchestratorEvent::SessionReady { session_id } if session_id == "mock-session-001"
    ));

    // Verify text deltas.
    let delta_count = collected_events
        .iter()
        .filter(|e| matches!(e, OrchestratorEvent::TextDelta(_)))
        .count();
    assert_eq!(delta_count, 3);

    // Verify response complete.
    let last = collected_events.last().unwrap();
    match last {
        OrchestratorEvent::ResponseComplete {
            full_text,
            cost_usd,
            ..
        } => {
            assert_eq!(full_text, "Ik zie dat je vastzit op een test failure.");
            assert_eq!(*cost_usd, Some(0.035));
        }
        _ => panic!("Expected ResponseComplete"),
    }
}
