use std::time::Duration;

use continuum_gateway::providers::AnthropicAdapter;
use continuum_gateway::{ChatMessage, ChatProvider, ChatRequest, ChatRole};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn req() -> ChatRequest {
    ChatRequest {
        model: "claude-opus-4-6".into(),
        system: "You are Continuum.".into(),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hi".into(),
        }],
        max_tokens: 64,
        temperature: Some(0.7), // must NOT be forwarded
    }
}

fn adapter(server: &MockServer) -> AnthropicAdapter {
    AnthropicAdapter::new(
        server.uri(),
        "sk-ant-test".into(),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .expect("adapter")
}

#[tokio::test]
async fn stream_parses_anthropic_events() {
    let server = MockServer::start().await;
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hoi\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut stream = adapter(&server)
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    let mut text = String::new();
    let mut usage = None;
    while let Some(ev) = stream.next().await {
        match ev {
            continuum_gateway::ChatEvent::Delta { text: t } => text.push_str(&t),
            continuum_gateway::ChatEvent::Done {
                usage: u,
                stop_reason,
            } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                usage = Some(u);
            }
            continuum_gateway::ChatEvent::Error { message, .. } => panic!("{message}"),
        }
    }
    assert_eq!(text, "Hoi");
    let u = usage.expect("usage");
    assert_eq!(u.input_tokens, Some(9));
    assert_eq!(u.output_tokens, Some(5));
}

#[tokio::test]
async fn request_body_omits_temperature() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::body_string_contains(
            "\"max_tokens\":64",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                    "text/event-stream",
                ),
        )
        .mount(&server)
        .await;
    // Guard: any body containing "temperature" would not match a second mock and 404 — assert via received requests.
    let mut s = adapter(&server)
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    while s.next().await.is_some() {}
    let reqs = server.received_requests().await.expect("requests");
    assert!(!String::from_utf8_lossy(&reqs[0].body).contains("temperature"));
}

#[tokio::test]
async fn refusal_stop_reason_surfaces_error() {
    let server = MockServer::start().await;
    let body = concat!(
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"},\"usage\":{\"output_tokens\":0}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;
    let mut s = adapter(&server)
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    let mut saw_refusal_error = false;
    while let Some(ev) = s.next().await {
        if let continuum_gateway::ChatEvent::Error { message, .. } = ev {
            assert!(message.to_lowercase().contains("declined"), "{message}");
            saw_refusal_error = true;
        }
    }
    assert!(saw_refusal_error);
}

#[tokio::test]
async fn list_models_works() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "sk-ant-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "claude-opus-4-6"}, {"id": "claude-sonnet-4-6"}]
        })))
        .mount(&server)
        .await;
    let models = adapter(&server).list_models().await.expect("models");
    assert_eq!(models, vec!["claude-opus-4-6", "claude-sonnet-4-6"]);
}
