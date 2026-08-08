//! # Native IDE bridge
//!
//! Opens allowlisted files in VS Code-compatible editors by launching the
//! native editor executable directly. Model input never reaches a shell or an
//! extension command surface.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;

use crate::allowlist::{is_path_allowed, AllowlistConfig};
use crate::config::McpIdeConfig;

/// Empty request for IDE bridge health/status.
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct IdeStatusRequest {}

/// Opens one file at an optional source location.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct IdeOpenFileRequest {
    /// Configured editor alias, such as `code`.
    pub editor: String,
    /// Existing allowlisted file.
    pub path: String,
    /// Optional one-based line number.
    pub line: Option<u32>,
    /// Optional one-based column number; requires `line`.
    pub column: Option<u32>,
}

/// Opens the editor's native two-file diff view.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct IdeOpenDiffRequest {
    /// Configured editor alias, such as `code`.
    pub editor: String,
    /// Existing allowlisted left-hand file.
    pub source: String,
    /// Existing allowlisted right-hand file.
    pub destination: String,
}

/// IDE bridge availability.
#[derive(Debug, Serialize, JsonSchema)]
pub struct IdeStatusResponse {
    /// Editors from the configured allowlist that resolve to native executables.
    pub available_editors: Vec<String>,
    /// True when at least one supported editor is available.
    pub healthy: bool,
}

/// Successful native editor handoff.
#[derive(Debug, Serialize, JsonSchema)]
pub struct IdeOpenResponse {
    /// Editor alias used for the request.
    pub editor: String,
    /// Canonical files handed to the editor.
    pub paths: Vec<String>,
    /// Editor CLI exit code.
    pub exit_code: Option<i32>,
}

/// Reports which configured native editor executables are available.
pub fn status(config: &McpIdeConfig) -> IdeStatusResponse {
    let available_editors = config
        .allowed_editors
        .iter()
        .filter(|editor| resolve_editor(editor).is_ok())
        .cloned()
        .collect::<Vec<_>>();
    IdeStatusResponse {
        healthy: !available_editors.is_empty(),
        available_editors,
    }
}

/// Opens one allowlisted file at an optional source location.
pub async fn open_file(
    request: &IdeOpenFileRequest,
    allowlist: &AllowlistConfig,
    config: &McpIdeConfig,
) -> Result<IdeOpenResponse, IdeError> {
    validate_editor(&request.editor, config)?;
    if request.column.is_some() && request.line.is_none() {
        return Err(IdeError::ColumnWithoutLine);
    }
    if request.line == Some(0) || request.column == Some(0) {
        return Err(IdeError::InvalidLocation);
    }
    let path = allowed_file(&request.path, allowlist)?;
    let mut target = path.to_string_lossy().into_owned();
    if let Some(line) = request.line {
        target.push(':');
        target.push_str(&line.to_string());
        if let Some(column) = request.column {
            target.push(':');
            target.push_str(&column.to_string());
        }
    }
    launch(
        &request.editor,
        &["--reuse-window".into(), "--goto".into(), target],
        vec![path],
        config,
    )
    .await
}

/// Opens two allowlisted files in the editor's diff view.
pub async fn open_diff(
    request: &IdeOpenDiffRequest,
    allowlist: &AllowlistConfig,
    config: &McpIdeConfig,
) -> Result<IdeOpenResponse, IdeError> {
    validate_editor(&request.editor, config)?;
    let source = allowed_file(&request.source, allowlist)?;
    let destination = allowed_file(&request.destination, allowlist)?;
    launch(
        &request.editor,
        &[
            "--reuse-window".into(),
            "--diff".into(),
            source.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        ],
        vec![source, destination],
        config,
    )
    .await
}

fn validate_editor(editor: &str, config: &McpIdeConfig) -> Result<(), IdeError> {
    if editor.is_empty()
        || editor.contains(['/', '\\', ':'])
        || editor.chars().any(char::is_whitespace)
        || !config
            .allowed_editors
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(editor))
    {
        return Err(IdeError::EditorNotAllowed);
    }
    Ok(())
}

fn allowed_file(path: &str, allowlist: &AllowlistConfig) -> Result<PathBuf, IdeError> {
    let canonical = is_path_allowed(Path::new(path), allowlist)
        .map_err(|error| IdeError::Denied(error.to_string()))?;
    if !canonical.is_file() {
        return Err(IdeError::NotAFile);
    }
    Ok(canonical)
}

async fn launch(
    editor: &str,
    args: &[String],
    paths: Vec<PathBuf>,
    config: &McpIdeConfig,
) -> Result<IdeOpenResponse, IdeError> {
    let executable = resolve_editor(editor)?;
    let mut command = Command::new(executable);
    command
        .args(args)
        .env_clear()
        .envs(filtered_environment())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = timeout(
        Duration::from_secs(config.launch_timeout_secs.clamp(1, 60)),
        command.output(),
    )
    .await
    .map_err(|_| IdeError::Timeout)?
    .map_err(|error| IdeError::Spawn(error.to_string()))?;
    if !output.status.success() {
        return Err(IdeError::EditorRejected(
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(500)
                .collect(),
        ));
    }
    Ok(IdeOpenResponse {
        editor: editor.to_string(),
        paths: paths
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        exit_code: output.status.code(),
    })
}

#[cfg(windows)]
fn resolve_editor(editor: &str) -> Result<PathBuf, IdeError> {
    let path = std::env::var_os("PATH").ok_or(IdeError::EditorNotFound)?;
    let native_name = match editor.to_ascii_lowercase().as_str() {
        "code-insiders" => "Code - Insiders.exe",
        "codium" => "VSCodium.exe",
        _ => "Code.exe",
    };
    for directory in std::env::split_paths(&path) {
        let direct = directory.join(format!("{editor}.exe"));
        if direct.is_file() {
            return Ok(direct);
        }
        if directory.join(format!("{editor}.cmd")).is_file() {
            if let Some(parent) = directory.parent() {
                let native = parent.join(native_name);
                if native.is_file() {
                    return Ok(native);
                }
            }
        }
    }
    Err(IdeError::EditorNotFound)
}

#[cfg(not(windows))]
fn resolve_editor(editor: &str) -> Result<PathBuf, IdeError> {
    Ok(PathBuf::from(editor))
}

fn filtered_environment() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| {
            let upper = key.to_ascii_uppercase();
            !["TOKEN", "SECRET", "PASSWORD", "API_KEY", "AUTH", "COOKIE"]
                .iter()
                .any(|needle| upper.contains(needle))
        })
        .collect()
}

/// Native IDE bridge failure.
#[derive(Debug, thiserror::Error)]
pub enum IdeError {
    /// File path failed the project allowlist.
    #[error("file denied: {0}")]
    Denied(String),
    /// Target is not a regular file.
    #[error("target is not an existing file")]
    NotAFile,
    /// Editor alias is not configured.
    #[error("editor is not in mcp.ide.allowed_editors")]
    EditorNotAllowed,
    /// Native editor executable could not be found.
    #[error("native editor executable was not found")]
    EditorNotFound,
    /// Column was provided without a line.
    #[error("column requires a line")]
    ColumnWithoutLine,
    /// Source locations are one-based.
    #[error("line and column must be positive")]
    InvalidLocation,
    /// Editor did not acknowledge the handoff in time.
    #[error("editor handoff timed out")]
    Timeout,
    /// Native process failed to start.
    #[error("failed to start editor: {0}")]
    Spawn(String),
    /// Editor CLI rejected the request.
    #[error("editor rejected the request: {0}")]
    EditorRejected(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn validates_editor_and_file_scope() {
        let root = tempdir().unwrap();
        let file = root.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let allowlist = AllowlistConfig::from_roots([root.path()]);
        let config = McpIdeConfig::default();
        assert!(validate_editor("code", &config).is_ok());
        assert!(validate_editor("cmd /c code", &config).is_err());
        let resolved = allowed_file(file.to_str().unwrap(), &allowlist).unwrap();
        assert!(resolved.is_file());
        assert_eq!(
            resolved.file_name().and_then(|name| name.to_str()),
            Some("main.rs")
        );
        assert!(allowed_file(root.path().to_str().unwrap(), &allowlist).is_err());
    }
}
