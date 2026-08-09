//! Standalone stdio MCP server for Continuum's action plane.
//!
//! Register this binary as the `agent-os` user-managed MCP server. It shares
//! the Continuum data directory but keeps policy, evidence and reliable run
//! journals under `<data-dir>/agent-os/`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use continuum_core::config::{continuum_dev_dir, env_or_legacy};
use continuum_mcp::{agent_os::AgentOsServer, ReliableAgentOsServer};
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args
        .iter()
        .any(|argument| argument == "--version" || argument == "-V")
    {
        println!("continuum-agent-os {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|argument| argument == "--headless") {
        std::env::set_var("CONTINUUM_AGENT_OS_HEADLESS", "1");
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
            component = "agent_os",
            data_dir = %data_dir.display(),
            version = env!("CARGO_PKG_VERSION"),
            "continuum-agent-os starting with durable execution governance"
        );
        let server = AgentOsServer::new(data_dir.clone())?;
        let server = ReliableAgentOsServer::new(server, &data_dir)?;
        let service = server.serve(stdio()).await.inspect_err(|error| {
            tracing::error!(
                layer = "mcp",
                component = "agent_os",
                error = %error,
                "Agent OS MCP serve error"
            );
        })?;
        service.waiting().await?;
        tracing::info!(
            layer = "mcp",
            component = "agent_os",
            "continuum-agent-os exiting"
        );
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

fn resolve_data_dir(args: &[String]) -> Result<PathBuf> {
    let mut iter = args.iter();
    while let Some(argument) = iter.next() {
        if argument == "--data-dir" {
            if let Some(path) = iter.next() {
                return Ok(PathBuf::from(path));
            }
        } else if let Some(path) = argument.strip_prefix("--data-dir=") {
            return Ok(PathBuf::from(path));
        }
    }
    if let Some(path) = env_or_legacy("CONTINUUM_DATA_DIR", "KAIRO_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(continuum_dev_dir())
}
