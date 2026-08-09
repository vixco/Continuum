use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
/// while the orchestrator consumes MCP servers from Continuum's user registry.
/// Re-running this at every desktop start repairs a stale absolute path after an
/// application update or install-directory move.
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
    let data_dir = state.runtime.dev_dir();
    let args = vec![
        "--data-dir".to_string(),
        data_dir.to_string_lossy().into_owned(),
    ];
    let destination = data_dir
        .join(mcp_registry::MCP_SERVERS_DIR)
        .join(format!("{SERVER_NAME}.json"));

    let existing = load_registration(&destination)?;
    if existing
        .as_ref()
        .is_some_and(|registration| registration_is_current(registration, &command, &args))
    {
        return Ok(BootstrapOutcome::AlreadyCurrent { binary });
    }

    install_or_replace(&data_dir, &destination, &command, args)
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

fn load_registration(path: &Path) -> Result<Option<McpServerRegistration>> {
    match std::fs::read(path) {
        Ok(body) => match serde_json::from_slice(&body) {
            Ok(registration) => Ok(Some(registration)),
            Err(error) => {
                tracing::warn!(
                    layer = "desktop",
                    component = "agent_os_bootstrap",
                    path = %path.display(),
                    error = %error,
                    "Invalid Agent OS registration will be replaced"
                );
                Ok(None)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("Failed to read Agent OS registration {}", path.display())),
    }
}

fn install_or_replace(
    data_dir: &Path,
    destination: &Path,
    command: &str,
    args: Vec<String>,
) -> Result<()> {
    if !destination.exists() {
        mcp_registry::install_server(data_dir, SERVER_NAME, command, args)?;
        return Ok(());
    }

    let parent = destination
        .parent()
        .context("Agent OS registration has no parent directory")?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let backup = parent.join(format!(
        ".{SERVER_NAME}-repair-{}-{suffix}.backup",
        std::process::id()
    ));
    std::fs::rename(destination, &backup).with_context(|| {
        format!(
            "Failed to preserve existing Agent OS registration {}",
            destination.display()
        )
    })?;

    match mcp_registry::install_server(data_dir, SERVER_NAME, command, args) {
        Ok(_) => {
            if let Err(error) = std::fs::remove_file(&backup) {
                tracing::warn!(
                    layer = "desktop",
                    component = "agent_os_bootstrap",
                    path = %backup.display(),
                    error = %error,
                    "Agent OS registration was repaired but its recovery backup remains"
                );
            }
            Ok(())
        }
        Err(error) => {
            if let Err(restore_error) = std::fs::rename(&backup, destination) {
                return Err(error).context(format!(
                    "Agent OS registration failed and restoring {} also failed: {restore_error}",
                    destination.display()
                ));
            }
            Err(error)
        }
    }
}

fn registration_is_current(
    registration: &McpServerRegistration,
    command: &str,
    args: &[String],
) -> bool {
    registration.name == SERVER_NAME && registration.command == command && registration.args == args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_binary(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::write(&path, b"binary").expect("binary fixture");
        path.canonicalize().expect("canonical binary")
    }

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
    fn registration_requires_exact_name_binary_and_args() {
        let args = vec!["--data-dir".to_string(), "C:/data".to_string()];
        let registration = McpServerRegistration {
            name: SERVER_NAME.to_string(),
            command: "C:/Continuum/continuum-agent-os.exe".to_string(),
            args: args.clone(),
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
    }

    #[test]
    fn stale_registration_is_replaced_without_losing_recovery_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let old = fixture_binary(temp.path(), if cfg!(windows) { "old.exe" } else { "old" });
        let current = fixture_binary(
            temp.path(),
            if cfg!(windows) {
                "current.exe"
            } else {
                "current"
            },
        );
        mcp_registry::install_server(
            temp.path(),
            SERVER_NAME,
            old.to_str().expect("old utf8"),
            vec!["--old".to_string()],
        )
        .expect("old registration");
        let destination = temp
            .path()
            .join(mcp_registry::MCP_SERVERS_DIR)
            .join(format!("{SERVER_NAME}.json"));

        install_or_replace(
            temp.path(),
            &destination,
            current.to_str().expect("current utf8"),
            vec!["--data-dir".to_string(), "data".to_string()],
        )
        .expect("replace registration");

        let registrations = mcp_registry::list_servers(temp.path()).expect("list registrations");
        assert_eq!(registrations.len(), 1);
        assert_eq!(registrations[0].command, current.to_string_lossy());
        assert_eq!(registrations[0].args, vec!["--data-dir", "data"]);
    }
}
