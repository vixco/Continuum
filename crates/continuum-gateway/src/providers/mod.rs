//! Provider adapters.
pub mod anthropic;
pub mod openai_compat;
pub use anthropic::AnthropicAdapter;
pub use openai_compat::OpenAiCompatAdapter;

use futures_util::stream::BoxStream;

use crate::error::GatewayError;
use crate::types::ChatEvent;

/// Adapts an mpsc receiver of [`ChatEvent`]s into the `BoxStream` shape
/// required by [`crate::ChatProvider::stream_chat`]. Shared by every
/// provider adapter that drives its stream from a background task.
pub(crate) fn mpsc_stream(
    mut rx: tokio::sync::mpsc::Receiver<ChatEvent>,
) -> BoxStream<'static, ChatEvent> {
    Box::pin(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)))
}

/// Maps an HTTP error status (plus any `retry-after` header and response
/// body) to a [`GatewayError`]. Shared by every HTTP-based adapter so the
/// 401/403/429/529 mapping stays consistent across providers.
pub(crate) fn map_status(
    status: reqwest::StatusCode,
    retry_after: Option<u64>,
    body: String,
) -> GatewayError {
    match status.as_u16() {
        401 | 403 => GatewayError::Unauthorized,
        429 => GatewayError::RateLimited {
            retry_after_secs: retry_after,
        },
        529 => GatewayError::RateLimited {
            retry_after_secs: None,
        },
        _ => GatewayError::BadResponse {
            detail: format!(
                "HTTP {status}: {}",
                body.chars().take(300).collect::<String>()
            ),
        },
    }
}

/// Extracts the `retry-after` header (seconds) from a response, if present
/// and parseable.
pub(crate) fn retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .parse()
        .ok()
}
