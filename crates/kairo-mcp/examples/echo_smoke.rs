//! Phase 4 smoke test: a minimal rmcp stdio MCP server with a single `echo` tool.
//!
//! Used to verify the claude CLI ↔ rmcp stdio handshake end-to-end before writing
//! any real Kairo tools. If this example runs and claude can invoke its `echo` tool
//! via --mcp-config, the protocol wiring is sound.
//!
//! Run manually:
//!   cargo build --example echo_smoke -p kairo-mcp
//!   # then invoke via claude CLI with --mcp-config pointing at the built binary.

use anyhow::Result;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoRequest {
    text: String,
}

#[derive(Clone)]
struct EchoServer {
    #[allow(dead_code)] // populated by the #[tool_router] macro's dispatch table
    tool_router: ToolRouter<EchoServer>,
}

#[tool_router]
impl EchoServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Echo the input text back verbatim. Smoke-test tool only.")]
    fn echo(
        &self,
        Parameters(EchoRequest { text }): Parameters<EchoRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_handler]
impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions("Kairo Phase 4 echo smoke-test server.")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("kairo-echo-smoke: starting stdio server");

    let service = EchoServer::new().serve(stdio()).await.inspect_err(|e| {
        tracing::error!("serve error: {e:?}");
    })?;

    service.waiting().await?;
    Ok(())
}
