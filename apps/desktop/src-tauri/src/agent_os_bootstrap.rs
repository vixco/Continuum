use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use continuum_core::mcp_registry::{self, McpServerRegistration};

use crate::{commands, AppState};

const SERVER_NAME: &str = "agent-os";
const WINDOWS_BINARY: &str = "continuum-agent-os.exe";
const UNIX_BINARY: &str = "continuum-agent-os";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BootstrapOutcome {
    AlreadyCurrent { binary: PathBuf },
    Registered { binary: PathBuf },
    Missing { searched: Vec<PathBuf> },
}

/// Keep the first-party Agent OS registration aligned with the packaged binary.
///
/// The release bundles `continuum-agent-os` under the Tauri resources directory,
/// but the orchestrator consumes MCP servers from Continuum's user registry. A
/// package that contains the binary without this registration would therefore
/// look complete while never exposing computer use or Composio to an agent.
/// Re-running this at every desktop start also repairs a stale absolute path
/// after an application update or install-directory move.
pub(crate) fn ensure_registered(state: &AppState) -> Result<BootstrapOutcome> {
    let binary_name = if cfg!(windows) {
        WINDOWS_BINARY
    } else {
        UNIX_BINARY
    };
    let candidates = commands::bundled_binary_candidates(binary_name);
    let Some(binary) = first_regular_file(&candidates) else {
        return Ok(BootstrapOutcome::Missing {
            searched: candidates,
        });
    };
    let binary = std::fs::canonicalize(&binary).with_context(|| {
        format!(
            "Failed to resolve bundled Agent OS binary {}",
            binary.display()
        )
    })?;
    let command = binary.to_string_lossy().into_owned();
    let args = vec![
        "--data-dir".to_string(),
        state.runtime.dev_dir().to_string_lossy().into_owned(),
    ];

    let existing = mcp_registry::list_servers(state.runtime.dev_dir())?
        .into_iter()
        .find(|registration| registration.name == SERVER_NAME);
    if existing
        .as_ref()
        .is_some_and(|registration| registration_is_current(registration, &command, &args))
    {
        return Ok(BootstrapOutcome::AlreadyCurrent { binary });
    }

    mcp_registry::install_server(state.runtime.dev_dir(), SERVER_NAME, &command, &args, &[])
        .context("Failed to register the bundled Agent OS MCP server")?;
    Ok(BootstrapOutcome::Registered { binary })
}

fn first_regular_file(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|path| is_regular_file(path))
        .cloned()
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

fn registration_is_current(
    registration: &McpServerRegistration,
    command: &str,
    args: &[String],
) -> bool {
    registration.command == command
        && registration.args == args
        && registration.env.is_empty()
        && registration.enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_regular_file_skips_missing_and_directories() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("missing");
        let directory = temp.path().join("directory");
        let binary = temp.path().join("continuum-agent-os");
        std::fs::create_dir(&directory).expect("directory");
        std::fs::write(&binary, b"binary").expect("binary");

        assert_eq!(
            first_regular_file(&[missing, directory, binary.clone()]),
            Some(binary)
        );
    }

    #[test]
    fn registration_requires_exact_binary_args_and_enabled_state() {
        let args = vec!["--data-dir".to_string(), "C:/data".to_string()];
        let registration = McpServerRegistration {
            name: SERVER_NAME.to_string(),
            command: "C:/Continuum/continuum-agent-os.exe".to_string(),
            args: args.clone(),
            env: Default::default(),
            enabled: true,
            installed_at: "2026-08-09T00:00:00Z".to_string(),
        };
        assert!(registration_is_current(
            &registration,
            "C:/Continuum/continuum-agent-os.exe",
            &args
        ));
        assert!(!registration_is_current(
            &registration,
            "C:/Other/continuum-agent-os.exe",
            &args
        ));

        let mut disabled = registration;
        disabled.enabled = false;
        assert!(!registration_is_current(
            &disabled,
            "C:/Continuum/continuum-agent-os.exe",
            &args
        ));
    }
}
