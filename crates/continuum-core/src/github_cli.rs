//! # Official GitHub CLI credential and API bridge
//!
//! Continuum never reads GitHub tokens. It delegates OAuth and API requests to
//! the official `gh` CLI, removes token environment overrides, and accepts a
//! connection only when `gh auth status` reports OS-keyring storage.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;

/// GitHub connection state safe to expose to the desktop and agents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubAuthStatus {
    /// Whether a native `gh` executable was found.
    pub installed: bool,
    /// Whether the active github.com account is valid and keyring-backed.
    pub connected: bool,
    /// Active account login.
    pub login: Option<String>,
    /// GitHub CLI token source; never the token itself.
    pub token_source: Option<String>,
    /// OAuth scopes reported by GitHub CLI.
    pub scopes: Vec<String>,
    /// True only for the OS keyring source.
    pub secure_storage: bool,
    /// Human-readable status without credentials.
    pub detail: String,
}

/// Returns the active secure github.com CLI connection.
pub async fn status(timeout_secs: u64) -> GitHubAuthStatus {
    let executable = match resolve_gh() {
        Ok(path) => path,
        Err(error) => {
            return GitHubAuthStatus {
                detail: error.to_string(),
                ..GitHubAuthStatus::default()
            }
        }
    };
    let output = match run_gh(
        &executable,
        &[
            "auth",
            "status",
            "--active",
            "--hostname",
            "github.com",
            "--json",
            "hosts",
        ],
        timeout_secs,
        None,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            return GitHubAuthStatus {
                installed: true,
                detail: error.to_string(),
                ..GitHubAuthStatus::default()
            }
        }
    };
    parse_status(&output).unwrap_or_else(|error| GitHubAuthStatus {
        installed: true,
        detail: format!("Could not parse GitHub CLI status: {error}"),
        ..GitHubAuthStatus::default()
    })
}

/// Starts the official GitHub CLI browser/device flow and requires keyring storage.
pub async fn connect(timeout_secs: u64) -> Result<GitHubAuthStatus> {
    let executable = resolve_gh()?;
    run_gh(
        &executable,
        &[
            "auth",
            "login",
            "--web",
            "--clipboard",
            "--hostname",
            "github.com",
            "--git-protocol",
            "https",
            "--scopes",
            "repo,read:org",
        ],
        timeout_secs,
        None,
    )
    .await?;
    let result = status(30).await;
    if !result.connected || !result.secure_storage {
        if let Some(login) = result.login.as_deref() {
            let _ = run_gh(
                &executable,
                &[
                    "auth",
                    "logout",
                    "--hostname",
                    "github.com",
                    "--user",
                    login,
                ],
                30,
                None,
            )
            .await;
        }
        return Err(anyhow!(
            "GitHub login did not finish with OS-keyring storage: {}",
            result.detail
        ));
    }
    Ok(result)
}

/// Removes the local GitHub CLI authentication for an exact account.
pub async fn disconnect(login: &str, timeout_secs: u64) -> Result<GitHubAuthStatus> {
    validate_login(login)?;
    let executable = resolve_gh()?;
    run_gh(
        &executable,
        &[
            "auth",
            "logout",
            "--hostname",
            "github.com",
            "--user",
            login,
        ],
        timeout_secs,
        None,
    )
    .await?;
    Ok(status(30).await)
}

/// Executes an authenticated GET request through `gh api`.
pub async fn api_get(
    endpoint: &str,
    fields: &[(&str, String)],
    timeout_secs: u64,
    cwd: Option<&Path>,
) -> Result<Vec<u8>> {
    if !endpoint.starts_with('/') || endpoint.contains("..") || endpoint.contains(['\r', '\n']) {
        return Err(anyhow!("invalid GitHub API endpoint"));
    }
    let auth = status(timeout_secs.min(30)).await;
    if !auth.connected || !auth.secure_storage {
        return Err(anyhow!("GitHub is not securely connected: {}", auth.detail));
    }
    let executable = resolve_gh()?;
    let mut owned = vec![
        "api".to_string(),
        "--method".to_string(),
        "GET".to_string(),
        endpoint.to_string(),
    ];
    for (name, value) in fields {
        if name.is_empty()
            || !name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(anyhow!("invalid GitHub query field"));
        }
        owned.push("--raw-field".to_string());
        owned.push(format!("{name}={value}"));
    }
    let borrowed: Vec<&str> = owned.iter().map(String::as_str).collect();
    run_gh_bytes(&executable, &borrowed, timeout_secs, cwd).await
}

fn parse_status(raw: &[u8]) -> Result<GitHubAuthStatus> {
    let value: serde_json::Value = serde_json::from_slice(raw)?;
    let account = value
        .get("hosts")
        .and_then(|hosts| hosts.get("github.com"))
        .and_then(serde_json::Value::as_array)
        .and_then(|accounts| accounts.iter().find(|account| account["active"] == true));
    let Some(account) = account else {
        return Ok(GitHubAuthStatus {
            installed: true,
            detail: "No active github.com account".to_string(),
            ..GitHubAuthStatus::default()
        });
    };
    let login = account["login"].as_str().map(str::to_string);
    let token_source = account["tokenSource"].as_str().map(str::to_string);
    let secure_storage = token_source.as_deref() == Some("keyring");
    let valid = account["state"].as_str() == Some("success");
    let scopes = account["scopes"]
        .as_str()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_string)
        .collect();
    Ok(GitHubAuthStatus {
        installed: true,
        connected: valid && secure_storage,
        login: login.clone(),
        token_source: token_source.clone(),
        scopes,
        secure_storage,
        detail: if valid && secure_storage {
            format!(
                "Connected as {} through the OS keyring",
                login.unwrap_or_default()
            )
        } else if valid {
            format!(
                "GitHub CLI auth uses unsupported storage {:?}; reconnect with OS-keyring support",
                token_source
            )
        } else {
            "GitHub CLI reports an invalid or expired login".to_string()
        },
    })
}

async fn run_gh(
    executable: &Path,
    args: &[&str],
    timeout_secs: u64,
    cwd: Option<&Path>,
) -> Result<Vec<u8>> {
    run_gh_bytes(executable, args, timeout_secs, cwd).await
}

async fn run_gh_bytes(
    executable: &Path,
    args: &[&str],
    timeout_secs: u64,
    cwd: Option<&Path>,
) -> Result<Vec<u8>> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .env_remove("GITHUB_ENTERPRISE_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(path) = cwd {
        command.current_dir(path);
    }
    #[cfg(windows)]
    {
        command.creation_flags(0x0800_0000);
    }
    let output = timeout(
        Duration::from_secs(timeout_secs.clamp(1, 900)),
        command.output(),
    )
    .await
    .context("GitHub CLI timed out")?
    .context("failed to launch GitHub CLI")?;
    if !output.status.success() {
        let detail: String = String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(500)
            .collect();
        return Err(anyhow!("GitHub CLI failed: {}", detail.trim()));
    }
    Ok(output.stdout)
}

fn validate_login(login: &str) -> Result<()> {
    if login.is_empty()
        || login.len() > 39
        || login.starts_with('-')
        || login.ends_with('-')
        || !login
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(anyhow!("invalid GitHub login"));
    }
    Ok(())
}

fn resolve_gh() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let path = std::env::var_os("PATH").context("PATH is unavailable")?;
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("gh.exe");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        Err(anyhow!("GitHub CLI (gh.exe) is not installed"))
    }
    #[cfg(not(windows))]
    {
        Ok(PathBuf::from("gh"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_keyring_backed_active_accounts_as_connected() {
        let raw = br#"{"hosts":{"github.com":[{"state":"success","active":true,"host":"github.com","login":"octocat","tokenSource":"keyring","scopes":"read:org, repo","gitProtocol":"https"}]}}"#;
        let status = parse_status(raw).unwrap();
        assert!(status.connected);
        assert!(status.secure_storage);
        assert_eq!(status.login.as_deref(), Some("octocat"));
        assert_eq!(status.scopes, vec!["read:org", "repo"]);
    }

    #[test]
    fn rejects_plaintext_storage_and_unsafe_logins() {
        let raw = br#"{"hosts":{"github.com":[{"state":"success","active":true,"login":"octocat","tokenSource":"file","scopes":"repo"}]}}"#;
        assert!(!parse_status(raw).unwrap().connected);
        assert!(validate_login("octocat").is_ok());
        assert!(validate_login("--hostname=evil").is_err());
    }
}
