//! Persistent registrations for user-managed MCP servers.
//!
//! The desktop writes one validated JSON file per server under the Continuum
//! data directory. The orchestrator reads those files when it builds the
//! per-run MCP configuration, so a successful registration is never merely a
//! dashboard-only state change.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Subdirectory containing user-managed MCP server registrations.
pub const MCP_SERVERS_DIR: &str = "mcp-servers";

/// A local stdio MCP server that Continuum may launch for an agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerRegistration {
    /// Stable server identifier used in MCP tool names.
    pub name: String,
    /// Absolute path to the already-installed local executable.
    pub command: String,
    /// Arguments passed directly to the executable without a shell.
    #[serde(default)]
    pub args: Vec<String>,
}

/// Validate and persist a new local MCP server registration.
///
/// This does not download packages or execute the server. It resolves the
/// executable now and records it for the next agent run.
pub fn install_server(
    data_dir: &Path,
    name: &str,
    command: &str,
    args: Vec<String>,
) -> Result<McpServerRegistration> {
    validate_name(name)?;
    validate_args(&args)?;
    let command = resolve_executable(command)?;
    let registration = McpServerRegistration {
        name: name.to_string(),
        command: command.to_string_lossy().into_owned(),
        args,
    };

    let dir = data_dir.join(MCP_SERVERS_DIR);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create MCP server directory at {}", dir.display()))?;
    let destination = registration_path(data_dir, name);
    if destination.exists() {
        bail!("An MCP server named '{name}' is already installed");
    }

    let unique_suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = dir.join(format!(
        ".{name}-{}-{unique_suffix}.tmp",
        std::process::id()
    ));
    let payload = serde_json::to_vec_pretty(&registration)
        .context("Failed to serialize MCP server registration")?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("Failed to create temporary file at {}", temporary.display()))?;
    if let Err(error) = (|| -> std::io::Result<()> {
        file.write_all(&payload)?;
        file.sync_all()
    })() {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).context("Failed to persist MCP server registration");
    }
    drop(file);
    // A hard link publishes the fully-synced file without replacing an
    // existing registration. This keeps duplicate installs preservation-first
    // on Windows and Unix alike; a plain rename can overwrite on Unix.
    if let Err(error) = std::fs::hard_link(&temporary, &destination) {
        let _ = std::fs::remove_file(&temporary);
        if destination.exists() {
            bail!("An MCP server named '{name}' is already installed");
        }
        return Err(error).with_context(|| {
            format!(
                "Failed to activate MCP server registration at {}",
                destination.display()
            )
        });
    }
    let _ = std::fs::remove_file(&temporary);

    Ok(registration)
}

/// Load all installed MCP server registrations in stable name order.
pub fn list_servers(data_dir: &Path) -> Result<Vec<McpServerRegistration>> {
    let dir = data_dir.join(MCP_SERVERS_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Failed to read MCP server directory at {}", dir.display())
            })
        }
    };

    let mut servers = Vec::new();
    for entry in entries {
        let entry = entry.context("Failed to read an MCP server directory entry")?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let body = std::fs::read(&path).with_context(|| {
            format!("Failed to read MCP server registration {}", path.display())
        })?;
        let server: McpServerRegistration = serde_json::from_slice(&body)
            .with_context(|| format!("Invalid MCP server registration at {}", path.display()))?;
        validate_name(&server.name).with_context(|| {
            format!("Invalid MCP server name in registration {}", path.display())
        })?;
        if path.file_stem().and_then(|value| value.to_str()) != Some(server.name.as_str()) {
            bail!(
                "MCP server registration name '{}' does not match its file {}",
                server.name,
                path.display()
            );
        }
        validate_args(&server.args).with_context(|| {
            format!(
                "Invalid MCP server arguments in registration {}",
                path.display()
            )
        })?;
        canonical_executable(Path::new(&server.command)).with_context(|| {
            format!(
                "Installed MCP server '{}' is unavailable at {}; reinstall it or repair the registration",
                server.name, server.command
            )
        })?;
        servers.push(server);
    }
    servers.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(servers)
}

/// Add installed external servers to an MCP config map and return their tool
/// allowlist patterns.
pub fn append_server_configs(
    data_dir: &Path,
    config: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<String>> {
    let servers = list_servers(data_dir)?;
    let mut allowed_tools = Vec::with_capacity(servers.len());
    for server in servers {
        allowed_tools.push(format!("mcp__{}__*", server.name));
        config.insert(
            server.name,
            serde_json::json!({
                "type": "stdio",
                "command": server.command,
                "args": server.args,
                "env": {},
            }),
        );
    }
    Ok(allowed_tools)
}

fn registration_path(data_dir: &Path, name: &str) -> PathBuf {
    data_dir.join(MCP_SERVERS_DIR).join(format!("{name}.json"))
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Server name is required");
    }
    if name.len() > 64 {
        bail!("Server name is too long (maximum 64 characters)");
    }
    if name == "continuum" {
        bail!("The name 'continuum' is reserved for the built-in server");
    }
    if !name.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '-' | '_')
    }) {
        bail!("Server name may contain only lowercase letters, numbers, '-' and '_'");
    }
    Ok(())
}

fn validate_args(args: &[String]) -> Result<()> {
    if args.len() > 64 {
        bail!("Too many server arguments (maximum 64)");
    }
    for argument in args {
        if argument.len() > 4096 {
            bail!("A server argument is too long (maximum 4096 characters)");
        }
        if argument.contains('\0') {
            bail!("Server arguments cannot contain NUL characters");
        }
    }
    Ok(())
}

fn resolve_executable(command: &str) -> Result<PathBuf> {
    let command = command.trim();
    if command.is_empty() {
        bail!("Executable path is required");
    }
    if command.contains('\0') {
        bail!("Executable path cannot contain NUL characters");
    }

    let supplied = Path::new(command);
    if supplied.is_absolute() || command.contains('/') || command.contains('\\') {
        return canonical_executable(supplied).with_context(|| {
            format!(
                "MCP server executable was not found at {}",
                supplied.display()
            )
        });
    }

    let path = std::env::var_os("PATH")
        .ok_or_else(|| anyhow::anyhow!("PATH is not available; enter the full executable path"))?;
    for directory in std::env::split_paths(&path) {
        for candidate in executable_candidates(&directory, command) {
            if let Ok(resolved) = canonical_executable(&candidate) {
                return Ok(resolved);
            }
        }
    }
    bail!("Executable '{command}' was not found on PATH; install the server first or enter its full path")
}

fn canonical_executable(path: &Path) -> Result<PathBuf> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        bail!("Path is not a file");
    }
    std::fs::canonicalize(path).context("Failed to resolve executable path")
}

fn executable_candidates(directory: &Path, command: &str) -> Vec<PathBuf> {
    let direct = directory.join(command);
    #[cfg(windows)]
    {
        if Path::new(command).extension().is_some() {
            return vec![direct];
        }
        let extensions =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| directory.join(format!("{command}{extension}")))
            .chain(std::iter::once(direct))
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![direct]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn install_and_list_roundtrip_uses_resolved_executable() {
        let temp = TempDir::new().expect("temp dir");
        let executable = temp.path().join(if cfg!(windows) {
            "server.exe"
        } else {
            "server"
        });
        std::fs::write(&executable, b"test").expect("write executable fixture");

        let installed = install_server(
            temp.path(),
            "local-server",
            executable.to_str().expect("utf8 path"),
            vec!["--stdio".to_string()],
        )
        .expect("install registration");

        assert!(Path::new(&installed.command).is_absolute());
        assert_eq!(list_servers(temp.path()).expect("list"), vec![installed]);
    }

    #[test]
    fn duplicate_name_preserves_first_registration() {
        let temp = TempDir::new().expect("temp dir");
        let executable = temp.path().join(if cfg!(windows) {
            "server.exe"
        } else {
            "server"
        });
        std::fs::write(&executable, b"test").expect("write executable fixture");
        let command = executable.to_str().expect("utf8 path");

        install_server(temp.path(), "duplicate", command, vec![]).expect("first install");
        let error = install_server(temp.path(), "duplicate", command, vec!["changed".into()])
            .expect_err("duplicate must fail");

        assert!(error.to_string().contains("already installed"));
        assert!(list_servers(temp.path()).expect("list")[0].args.is_empty());
    }

    #[test]
    fn rejects_reserved_or_unsafe_names() {
        assert!(validate_name("continuum").is_err());
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("Uppercase").is_err());
        assert!(validate_name("valid_server-2").is_ok());
    }

    #[test]
    fn missing_executable_returns_actionable_error_without_writing_registration() {
        let temp = TempDir::new().expect("temp dir");
        let missing = temp.path().join("missing-server.exe");

        let error = install_server(
            temp.path(),
            "missing",
            missing.to_str().expect("utf8 path"),
            vec![],
        )
        .expect_err("missing executable must fail");

        assert!(error.to_string().contains("was not found at"));
        assert!(list_servers(temp.path()).expect("list").is_empty());
    }

    #[test]
    fn installed_server_is_added_to_runtime_config_and_allowlist() {
        let temp = TempDir::new().expect("temp dir");
        let executable = temp.path().join(if cfg!(windows) {
            "server.exe"
        } else {
            "server"
        });
        std::fs::write(&executable, b"test").expect("write executable fixture");
        install_server(
            temp.path(),
            "configured",
            executable.to_str().expect("utf8 path"),
            vec!["--stdio".into()],
        )
        .expect("install registration");

        let mut config = serde_json::Map::new();
        let allowlist = append_server_configs(temp.path(), &mut config).expect("append config");

        assert_eq!(allowlist, vec!["mcp__configured__*"]);
        assert_eq!(config["configured"]["args"], serde_json::json!(["--stdio"]));
        assert_eq!(
            config["configured"]["command"],
            serde_json::json!(executable
                .canonicalize()
                .expect("canonical executable")
                .to_string_lossy())
        );
    }
}
