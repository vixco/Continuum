//! # continuum-gateway — the Model Gateway
//!
//! Provider adapters for chat: OpenAI-compatible endpoints, the Anthropic
//! Messages API, and the official `claude` CLI. This crate is pure Rust
//! (no Tauri, no llama.cpp) so it can be reused by the runtime and the
//! Phase 3 model router. See `CONTINUUM_ARCHITECTURE.md` and
//! `docs/superpowers/specs/2026-08-02-chat-tab-design.md`.

pub mod catalog;
pub mod error;
pub mod types;

pub mod providers;
mod sse;

pub use error::GatewayError;
pub use types::*;

use futures_util::stream::BoxStream;
use tokio_util::sync::CancellationToken;

/// A chat-capable provider. Implemented by each adapter.
#[async_trait::async_trait]
pub trait ChatProvider: Send + Sync {
    /// Cheap reachability + auth check. Returns models when listable.
    async fn test_connection(&self) -> Result<ConnectionTestReport, GatewayError>;
    /// List model ids offered by this provider.
    async fn list_models(&self) -> Result<Vec<String>, GatewayError>;
    /// Stream a chat completion. The stream itself yields [`ChatEvent`]s;
    /// a returned `Err` means the request could not be started at all.
    async fn stream_chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ChatEvent>, GatewayError>;
}
