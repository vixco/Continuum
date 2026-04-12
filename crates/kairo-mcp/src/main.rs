//! # kairo-mcp binary
//!
//! Standalone MCP server exposing Kairo tools to Claude Code. Launched by the
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
//! - `--version`: print Kairo version and exit 0.
//! - `--data-dir <path>`: override the Kairo data directory. Falls back to the
//!   `KAIRO_DATA_DIR` env var, then to `~/.kairo-dev/`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use kairo_mcp::KairoMcpServer;
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("kairo-mcp {}", env!("CARGO_PKG_VERSION"));
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
            "kairo-mcp starting"
        );

        let server = KairoMcpServer::new(data_dir).await?;
        let service = server.serve(stdio()).await.inspect_err(|e| {
            tracing::error!(layer = "mcp", component = "main", error = %e, "serve error");
        })?;

        service.waiting().await?;
        tracing::info!(layer = "mcp", component = "main", "kairo-mcp exiting");
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

/// Resolves the Kairo data directory in this order:
/// (1) `--data-dir <path>` flag, (2) `KAIRO_DATA_DIR` env var, (3) `~/.kairo-dev/`.
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

    if let Ok(env) = std::env::var("KAIRO_DATA_DIR") {
        if !env.is_empty() {
            return Ok(PathBuf::from(env));
        }
    }

    let home = dirs::home_dir().context("Cannot locate home directory")?;
    Ok(home.join(".kairo-dev"))
}
