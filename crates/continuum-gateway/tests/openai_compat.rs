use std::time::Duration;

use continuum_gateway::providers::OpenAiCompatAdapter;
use continuum_gateway::{ChatMessage, ChatProvider, ChatRequest, ChatRole, GatewayError};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn req() -> ChatRequest {
    ChatRequest {
        model: "test-model".into(),
        system: "You are Continuum.".into(),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hi".into(),
        }],
        max_tokens: 64,
        temperature: None,
    }
}

fn adapter(server: &MockServer, key: Option<&str>) -> OpenAiCompatAdapter {
    OpenAiCompatAdapter::new(
        format!("{}/v1", server.uri()),
        key.map(String::from),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .expect("adapter")
}

#[tokio::test]
async fn list_models_parses_ids_and_sends_bearer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer k1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "m1"}, {"id": "m2"}]
        })))
        .mount(&server)
        .await;
    let models = adapter(&server, Some("k1"))
        .list_models()
        .await
        .expect("models");
    assert_eq!(models, vec!["m1", "m2"]);
}

#[tokio::test]
async fn stream_chat_yields_deltas_then_done() {
    let server = MockServer::start().await;
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut stream = adapter(&server, None)
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    let mut text = String::new();
    let mut done = false;
    while let Some(ev) = stream.next().await {
        match ev {
            continuum_gateway::ChatEvent::Delta { text: t } => text.push_str(&t),
            continuum_gateway::ChatEvent::Done { usage, .. } => {
                assert_eq!(usage.output_tokens, Some(2));
                done = true;
            }
            continuum_gateway::ChatEvent::Error { message, .. } => panic!("error: {message}"),
        }
    }
    assert_eq!(text, "Hello");
    assert!(done);
}

#[tokio::test]
async fn maps_401_and_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let err = adapter(&server, Some("bad"))
        .stream_chat(req(), CancellationToken::new())
        .await
        .err()
        .expect("err");
    assert!(matches!(err, GatewayError::Unauthorized));

    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
        .mount(&server2)
        .await;
    let err2 = adapter(&server2, None)
        .stream_chat(req(), CancellationToken::new())
        .await
        .err()
        .expect("err");
    assert!(matches!(
        err2,
        GatewayError::RateLimited {
            retry_after_secs: Some(7)
        }
    ));
}

#[tokio::test]
async fn unreachable_maps_to_unreachable() {
    let a = OpenAiCompatAdapter::new(
        "http://127.0.0.1:9".into(), // port 9 (discard) — nothing listens
        None,
        Duration::from_millis(300),
        Duration::from_millis(300),
    )
    .expect("adapter");
    let err = a.list_models().await.expect_err("err");
    assert!(matches!(
        err,
        GatewayError::Unreachable { .. } | GatewayError::Timeout
    ));
}
