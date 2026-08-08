//! # Restricted terminal broker
//!
//! Commands are launched directly as an executable plus argument vector. No
//! shell parses the input, executable paths are forbidden, cwd is allowlisted,
//! stdin is closed, and sensitive environment variables are removed.

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

use crate::allowlist::{is_path_allowed, AllowlistConfig};
use crate::config::McpTerminalConfig;

/// Input for both terminal broker tools.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TerminalRunRequest {
    /// Allowlisted working directory.
    pub cwd: String,
    /// Configured executable basename, never a path or shell expression.
    pub program: String,
    /// Literal process arguments. Shell metacharacters have no special meaning.
    #[serde(default)]
    pub args: Vec<String>,
    /// Requested timeout, clamped by `mcp.terminal.max_timeout_secs`.
    pub timeout_secs: Option<u64>,
    /// Optional short purpose shown in audit/evidence.
    pub label: Option<String>,
}

/// Captured process result.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TerminalRunResponse {
    /// Evidence record id for verifier calls; absent for ordinary execution.
    pub evidence_id: Option<String>,
    /// Path of the durable verifier evidence record.
    pub evidence_path: Option<String>,
    /// Sanitized human-readable purpose.
    pub label: Option<String>,
    /// Executable basename.
    pub program: String,
    /// Literal arguments.
    pub args: Vec<String>,
    /// Canonical working directory.
    pub cwd: String,
    /// Process exit code, or null if terminated by timeout/signal.
    pub exit_code: Option<i32>,
    /// Whether the configured deadline elapsed.
    pub timed_out: bool,
    /// Wall-clock duration.
    pub duration_ms: u64,
    /// Captured UTF-8-lossy stdout.
    pub stdout: String,
    /// Captured UTF-8-lossy stderr.
    pub stderr: String,
    /// Whether output exceeded the configured cap.
    pub truncated: bool,
    /// UTC completion time.
    pub completed_at: String,
}

/// Runs a restricted command without persisting verifier evidence.
pub async fn run(
    request: &TerminalRunRequest,
    allowlist: &AllowlistConfig,
    config: &McpTerminalConfig,
) -> Result<TerminalRunResponse, TerminalError> {
    execute(request, allowlist, config).await
}

/// Runs a restricted command and atomically persists its evidence record.
pub async fn verify(
    request: &TerminalRunRequest,
    allowlist: &AllowlistConfig,
    config: &McpTerminalConfig,
    data_dir: &Path,
) -> Result<TerminalRunResponse, TerminalError> {
    let mut response = execute(request, allowlist, config).await?;
    let id = Uuid::new_v4().to_string();
    let path = data_dir
        .join("evidence")
        .join("terminal")
        .join(format!("{id}.json"));
    response.evidence_id = Some(id);
    response.evidence_path = Some(path.to_string_lossy().into_owned());
    write_evidence(&path, &response).await?;
    Ok(response)
}

async fn execute(
    request: &TerminalRunRequest,
    allowlist: &AllowlistConfig,
    config: &McpTerminalConfig,
) -> Result<TerminalRunResponse, TerminalError> {
    validate_program(&request.program, &config.allowed_programs)?;
    let executable = resolve_executable(&request.program)?;
    if request.args.len() > config.max_args.max(1) {
        return Err(TerminalError::TooManyArgs);
    }
    if request.args.iter().any(|argument| argument.len() > 4096) {
        return Err(TerminalError::ArgumentTooLong);
    }
    if request
        .args
        .iter()
        .any(|argument| is_sensitive_env_key(argument))
    {
        return Err(TerminalError::SensitiveArgument);
    }
    let cwd = is_path_allowed(Path::new(&request.cwd), allowlist)
        .map_err(|error| TerminalError::Denied(error.to_string()))?;
    if !cwd.is_dir() {
        return Err(TerminalError::CwdNotDirectory);
    }
    let seconds = request
        .timeout_secs
        .unwrap_or(config.max_timeout_secs.min(300))
        .clamp(1, config.max_timeout_secs.max(1));
    let start = Instant::now();
    let mut command = Command::new(executable);
    command
        .args(&request.args)
        .current_dir(&cwd)
        .env_clear()
        .envs(filtered_environment())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let outcome = timeout(Duration::from_secs(seconds), command.output()).await;
    let duration_ms = start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    match outcome {
        Err(_) => Ok(TerminalRunResponse {
            evidence_id: None,
            evidence_path: None,
            label: request
                .label
                .as_deref()
                .map(|value| value.chars().take(160).collect()),
            program: request.program.clone(),
            args: request.args.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            exit_code: None,
            timed_out: true,
            duration_ms,
            stdout: String::new(),
            stderr: format!("command exceeded {seconds}s timeout"),
            truncated: false,
            completed_at: Utc::now().to_rfc3339(),
        }),
        Ok(Err(error)) => Err(TerminalError::Spawn(error.to_string())),
        Ok(Ok(output)) => {
            let max = config.max_output_bytes.max(1024);
            let (stdout, stdout_truncated) = truncate_lossy(&output.stdout, max / 2);
            let (stderr, stderr_truncated) = truncate_lossy(&output.stderr, max / 2);
            Ok(TerminalRunResponse {
                evidence_id: None,
                evidence_path: None,
                label: request
                    .label
                    .as_deref()
                    .map(|value| value.chars().take(160).collect()),
                program: request.program.clone(),
                args: request.args.clone(),
                cwd: cwd.to_string_lossy().into_owned(),
                exit_code: output.status.code(),
                timed_out: false,
                duration_ms,
                stdout,
                stderr,
                truncated: stdout_truncated || stderr_truncated,
                completed_at: Utc::now().to_rfc3339(),
            })
        }
    }
}

fn validate_program(program: &str, allowed: &[String]) -> Result<(), TerminalError> {
    if program.is_empty()
        || program.contains('/')
        || program.contains('\\')
        || program.contains(':')
        || program.chars().any(char::is_whitespace)
    {
        return Err(TerminalError::ProgramMustBeBasename);
    }
    let lower = program.to_ascii_lowercase();
    if lower.ends_with(".cmd") || lower.ends_with(".bat") || lower.ends_with(".ps1") {
        return Err(TerminalError::ShellProgramForbidden);
    }
    if [
        "cmd",
        "powershell",
        "pwsh",
        "sh",
        "bash",
        "zsh",
        "fish",
        "wsl",
    ]
    .contains(&lower.as_str())
    {
        return Err(TerminalError::ShellProgramForbidden);
    }
    if !allowed
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(program))
    {
        return Err(TerminalError::ProgramNotAllowed(program.to_string()));
    }
    Ok(())
}

#[cfg(windows)]
fn resolve_executable(program: &str) -> Result<PathBuf, TerminalError> {
    let path =
        std::env::var_os("PATH").ok_or_else(|| TerminalError::ProgramNotFound(program.into()))?;
    let names = if program.to_ascii_lowercase().ends_with(".exe")
        || program.to_ascii_lowercase().ends_with(".com")
    {
        vec![program.to_string()]
    } else {
        vec![format!("{program}.exe"), format!("{program}.com")]
    };
    for directory in std::env::split_paths(&path) {
        for name in &names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(TerminalError::ProgramNotFound(program.into()))
}

#[cfg(not(windows))]
fn resolve_executable(program: &str) -> Result<PathBuf, TerminalError> {
    Ok(PathBuf::from(program))
}

fn filtered_environment() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| !is_sensitive_env_key(key))
        .collect()
}

fn is_sensitive_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "APIKEY",
        "AUTH",
        "COOKIE",
        "CREDENTIAL",
        "PRIVATE_KEY",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

fn truncate_lossy(bytes: &[u8], max: usize) -> (String, bool) {
    if bytes.len() <= max {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    let mut output = String::from_utf8_lossy(&bytes[..max]).into_owned();
    output.push_str("\n[output truncated]");
    (output, true)
}

async fn write_evidence(path: &Path, response: &TerminalRunResponse) -> Result<(), TerminalError> {
    let parent = path.parent().ok_or(TerminalError::EvidencePath)?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| TerminalError::Evidence(error.to_string()))?;
    let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4().simple()));
    tokio::fs::write(
        &temporary,
        serde_json::to_vec_pretty(response)
            .map_err(|error| TerminalError::Evidence(error.to_string()))?,
    )
    .await
    .map_err(|error| TerminalError::Evidence(error.to_string()))?;
    tokio::fs::rename(temporary, path)
        .await
        .map_err(|error| TerminalError::Evidence(error.to_string()))
}

/// Terminal broker failure.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    /// cwd failed the allowlist.
    #[error("working directory denied: {0}")]
    Denied(String),
    /// cwd is not a directory.
    #[error("working directory is not a directory")]
    CwdNotDirectory,
    /// Program contained a path or shell expression.
    #[error("program must be a configured executable basename")]
    ProgramMustBeBasename,
    /// Batch and PowerShell scripts would reintroduce shell parsing.
    #[error("batch, cmd, and PowerShell script programs are forbidden")]
    ShellProgramForbidden,
    /// Program is absent from the configured allowlist.
    #[error("program is not allowed: {0}")]
    ProgramNotAllowed(String),
    /// Configured native executable could not be resolved.
    #[error("native executable was not found on PATH: {0}")]
    ProgramNotFound(String),
    /// Argument vector exceeded the configured count.
    #[error("argument count exceeds the configured maximum")]
    TooManyArgs,
    /// One argument was pathologically large.
    #[error("one argument exceeds 4096 bytes")]
    ArgumentTooLong,
    /// Argument looked like a credential-bearing flag.
    #[error("credential-like arguments are forbidden; configure credentials outside tool input")]
    SensitiveArgument,
    /// Process could not start.
    #[error("failed to start process: {0}")]
    Spawn(String),
    /// Evidence path had no parent.
    #[error("invalid evidence path")]
    EvidencePath,
    /// Evidence persistence failed.
    #[error("failed to persist verifier evidence: {0}")]
    Evidence(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn config() -> McpTerminalConfig {
        McpTerminalConfig {
            allowed_programs: vec!["git".into()],
            max_timeout_secs: 10,
            max_output_bytes: 4096,
            max_args: 8,
        }
    }

    #[tokio::test]
    async fn runs_allowlisted_program_and_persists_verifier_evidence() {
        let root = tempdir().unwrap();
        let data = tempdir().unwrap();
        let allowlist = AllowlistConfig::from_roots([root.path()]);
        let request = TerminalRunRequest {
            cwd: root.path().to_string_lossy().into_owned(),
            program: "git".into(),
            args: vec!["--version".into()],
            timeout_secs: Some(5),
            label: Some("git availability".into()),
        };
        let response = verify(&request, &allowlist, &config(), data.path())
            .await
            .unwrap();
        assert_eq!(response.exit_code, Some(0));
        assert!(response.stdout.contains("git version"));
        assert!(Path::new(response.evidence_path.as_deref().unwrap()).exists());
    }

    #[test]
    fn rejects_paths_shells_and_sensitive_environment_names() {
        let cfg = config();
        assert!(validate_program("C:/git.exe", &cfg.allowed_programs).is_err());
        assert!(validate_program("pwsh -c", &cfg.allowed_programs).is_err());
        assert!(validate_program("build.cmd", &["build.cmd".into()]).is_err());
        assert!(validate_program("bash", &["bash".into()]).is_err());
        assert!(validate_program("python", &cfg.allowed_programs).is_err());
        assert!(is_sensitive_env_key("GITHUB_TOKEN"));
        assert!(is_sensitive_env_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!is_sensitive_env_key("PATH"));
    }
}
