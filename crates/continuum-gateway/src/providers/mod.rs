//! Provider adapters.
pub mod openai_compat;
pub use openai_compat::OpenAiCompatAdapter;

use futures_util::stream::BoxStream;

use crate::types::ChatEvent;

/// Adapts an mpsc receiver of [`ChatEvent`]s into the `BoxStream` shape
/// required by [`crate::ChatProvider::stream_chat`]. Shared by every
/// provider adapter that drives its stream from a background task.
pub(crate) fn mpsc_stream(
    mut rx: tokio::sync::mpsc::Receiver<ChatEvent>,
) -> BoxStream<'static, ChatEvent> {
    Box::pin(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)))
}
