//! # kairo-mcp library
//!
//! Exposes the MCP server struct for integration tests and reuse. The binary
//! target (`src/main.rs`) re-imports these to run the server over stdio.

pub mod allowlist;
pub(crate) mod audit;
pub mod config;
pub mod server;
pub mod tools;

pub use server::KairoMcpServer;
