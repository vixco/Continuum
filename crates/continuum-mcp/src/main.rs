//! # continuum-mcp binary
//!
//! Standalone MCP server exposing Continuum tools to Claude Code. Launched by the
//! orchestrator subprocess via `--mcp-config` at wake time.
//!
//! ## Runtime contract
//!
//! - stdio is the MCP protocol channel. NEVER write to stdout outside rmcp.
//! - stderr carries structured tracing logs.
//! - The process should exit cleanly on SIGINT / EOF-on-stdin (the client shuts
//!   it down by closing the pipe).
//!
//! ## Flags
//!
//! - `--version`: print Continuum version and exit 0.
//! - `--data-dir <path>`: override the Continuum data directory. Falls back to the
//!   `CONTINUUM_DATA_DIR` env var, then to `~/.continuum-dev/`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use continuum_mcp::{ContinuumMcpServer, PermissionedMcpServer};
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

use continuum_core::config::{continuum_dev_dir, env_or_legacy};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("continuum-mcp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let data_dir = resolve_data_dir(&args)?;

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to build Tokio runtime")?;

    runtime.block_on(async move {
        tracing::info!(
            layer = "mcp",
            component = "main",
            data_dir = %data_dir.display(),
            version = env!("CARGO_PKG_VERSION"),
            "continuum-mcp starting with enforced permissions"
        );

        let server = ContinuumMcpServer::new(data_dir.clone()).await?;
        let server = PermissionedMcpServer::new(server, &data_dir)?;
        let service = server.serve(stdio()).await.inspect_err(|e| {
            tracing::error!(layer = "mcp", component = "main", error = %e, "serve error");
        })?;

        service.waiting().await?;
        tracing::info!(layer = "mcp", component = "main", "continuum-mcp exiting");
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

/// Resolves the Continuum data directory in this order:
/// (1) `--data-dir <path>` flag, (2) `CONTINUUM_DATA_DIR` env var, (3) `~/.continuum-dev/`.
fn resolve_data_dir(args: &[String]) -> Result<PathBuf> {
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        if a == "--data-dir" {
            if let Some(p) = iter.next() {
                return Ok(PathBuf::from(p));
            }
        } else if let Some(rest) = a.strip_prefix("--data-dir=") {
            return Ok(PathBuf::from(rest));
        }
    }

    if let Some(env) = env_or_legacy("CONTINUUM_DATA_DIR", "KAIRO_DATA_DIR") {
        return Ok(PathBuf::from(env));
    }

    Ok(continuum_dev_dir())
}
