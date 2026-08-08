//! # continuum-mcp library
//!
//! Exposes the context MCP server and the provider-neutral Agent OS action
//! server for integration tests and reuse. Binary targets run them over stdio.

pub mod agent_os;
pub mod allowlist;
pub(crate) mod audit;
pub mod config;
/// Mandatory authorization and egress controls for the context MCP server.
pub mod permission_broker;
/// Durable, cross-process-safe plan execution and typed postconditions.
pub mod reliable_agent;
/// Safety-redaction evaluation harness (context engine spec §9, Task C6).
pub mod redaction;
pub mod server;
pub mod tools;

pub use permission_broker::PermissionedMcpServer;
pub use reliable_agent::ReliableAgentOsServer;
pub use server::ContinuumMcpServer;
