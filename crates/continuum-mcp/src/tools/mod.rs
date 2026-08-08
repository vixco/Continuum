//! # MCP tool modules
//!
//! Each submodule implements one functional group of Continuum's MCP tool suite.
//! Tool methods are mounted on [`crate::server::ContinuumMcpServer`] via rmcp's
//! `#[tool_router]` macro; these modules hold supporting types, helpers, and
//! (eventually) extracted implementations kept out of the server file for
//! readability.

pub mod context;
pub mod file_actions;
pub mod fs;
pub mod git;
pub mod memory;
pub mod repair;
pub mod system;
pub mod web;
pub mod workers;
