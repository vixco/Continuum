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
async fn stream_end_without_done_still_yields_done() {
    let server = MockServer::start().await;
    // No `data: [DONE]` terminator — the mock server closes the connection
    // after writing this body, so the stream must end on its own.
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n";
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
            continuum_gateway::ChatEvent::Done { .. } => done = true,
            continuum_gateway::ChatEvent::Error { message, .. } => panic!("error: {message}"),
        }
    }
    assert_eq!(text, "Hi");
    assert!(
        done,
        "connection close without [DONE] must still yield Done"
    );
}

#[tokio::test]
async fn cancellation_yields_cancelled_error() {
    let server = MockServer::start().await;
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
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

    let cancel = CancellationToken::new();
    let mut stream = adapter(&server, None)
        .stream_chat(req(), cancel.clone())
        .await
        .expect("stream");
    // Cancel before ever polling the stream: the eager `is_cancelled()`
    // check at the top of the stream task's loop guarantees the Cancelled
    // error wins deterministically, rather than racing an already-cancelled
    // token against a possibly-already-buffered first chunk.
    cancel.cancel();
    let ev = stream.next().await.expect("event");
    match ev {
        continuum_gateway::ChatEvent::Error { message, retryable } => {
            assert_eq!(message, GatewayError::Cancelled.user_message());
            assert!(!retryable);
        }
        other => panic!("expected cancelled error as first event, got {other:?}"),
    }
}

#[tokio::test]
async fn idle_timeout_yields_timeout_error() {
    // Coverage note: wiremock can only delay a response as a whole (there's
    // no way to start writing a body and then stall mid-stream), so this
    // exercises the *outer* request timeout added for the "no overall
    // request timeout" finding — the initial POST's `send()` is bounded by
    // `idle_timeout` — rather than the mid-stream idle-timeout branch inside
    // `stream_events`'s `tokio::select!` loop. A mid-stream stall isn't
    // constructible with wiremock; see `mid_stream_stall_yields_timeout_error`
    // below, which drives that branch via a hand-rolled raw-TCP fixture.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw("data: [DONE]\n\n", "text/event-stream")
                .set_delay(Duration::from_secs(1)),
        )
        .mount(&server)
        .await;

    let a = OpenAiCompatAdapter::new(
        format!("{}/v1", server.uri()),
        None,
        Duration::from_secs(2),
        Duration::from_millis(200),
    )
    .expect("adapter");
    let err = a
        .stream_chat(req(), CancellationToken::new())
        .await
        .err()
        .expect("err");
    assert!(matches!(err, GatewayError::Timeout));
}

#[tokio::test]
async fn mid_stream_stall_yields_timeout_error() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        // Read the request bytes (don't parse; just drain a bit) then answer
        let mut buf = [0u8; 4096];
        use tokio::io::AsyncReadExt;
        let _ = sock.read(&mut buf).await;
        let resp =
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
        sock.write_all(resp.as_bytes()).await.expect("headers");
        // one complete SSE event as a chunked body piece
        let payload = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n";
        let chunk = format!("{:x}\r\n{}\r\n", payload.len(), payload);
        sock.write_all(chunk.as_bytes()).await.expect("chunk");
        sock.flush().await.expect("flush");
        // then stall forever (never send terminating chunk)
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    });
    let a = OpenAiCompatAdapter::new(
        format!("http://{addr}/v1"),
        None,
        Duration::from_secs(2),
        Duration::from_millis(200),
    )
    .expect("adapter");
    let mut s = a
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    let mut got_delta = false;
    let mut got_timeout = false;
    while let Some(ev) = s.next().await {
        match ev {
            continuum_gateway::ChatEvent::Delta { text } => {
                assert_eq!(text, "Hi");
                got_delta = true;
            }
            continuum_gateway::ChatEvent::Error { message, retryable } => {
                assert!(retryable);
                assert_eq!(message, GatewayError::Timeout.user_message());
                got_timeout = true;
            }
            continuum_gateway::ChatEvent::Done { .. } => panic!("should not complete"),
        }
    }
    assert!(got_delta && got_timeout);
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
