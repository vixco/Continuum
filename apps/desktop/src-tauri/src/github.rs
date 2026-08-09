//! GitHub settings commands backed exclusively by the official `gh` CLI.

use std::sync::Arc;

use continuum_core::audit::{Actor, AuditLog};
use continuum_core::github_cli::GitHubAuthStatus;
use tauri::State;

use crate::AppState;

/// Return secure GitHub CLI connection state without exposing a token.
#[tauri::command]
pub async fn github_status(app: State<'_, Arc<AppState>>) -> Result<GitHubAuthStatus, String> {
    let config = app.runtime.config_snapshot().github;
    if !config.enabled {
        return Ok(GitHubAuthStatus {
            detail: "GitHub integration is disabled in config".to_string(),
            ..GitHubAuthStatus::default()
        });
    }
    Ok(continuum_core::github_cli::status(config.api_timeout_secs).await)
}

/// Start the official GitHub CLI web/device login after an explicit UI click.
#[tauri::command]
pub async fn github_connect(app: State<'_, Arc<AppState>>) -> Result<GitHubAuthStatus, String> {
    let config = app.runtime.config_snapshot().github;
    if !config.enabled {
        return Err("GitHub integration is disabled in config".to_string());
    }
    let status = continuum_core::github_cli::connect(config.connect_timeout_secs)
        .await
        .map_err(|error| error.to_string())?;
    AuditLog::new(&app.runtime.dev_dir()).record(
        "github_connected",
        Actor::User,
        format!(
            "GitHub connected as {}",
            status.login.as_deref().unwrap_or("unknown")
        ),
        Some(serde_json::json!({
            "login": status.login,
            "token_source": status.token_source,
            "scopes": status.scopes,
        })),
    );
    Ok(status)
}

/// Remove local GitHub CLI auth for the active account.
#[tauri::command]
pub async fn github_disconnect(app: State<'_, Arc<AppState>>) -> Result<GitHubAuthStatus, String> {
    let config = app.runtime.config_snapshot().github;
    let current = continuum_core::github_cli::status(config.api_timeout_secs).await;
    let login = current
        .login
        .ok_or_else(|| "No active GitHub account to disconnect".to_string())?;
    let status = continuum_core::github_cli::disconnect(&login, config.api_timeout_secs)
        .await
        .map_err(|error| error.to_string())?;
    AuditLog::new(&app.runtime.dev_dir()).record(
        "github_disconnected",
        Actor::User,
        format!("Removed local GitHub CLI auth for {login}"),
        Some(serde_json::json!({ "login": login, "remote_token_revoked": false })),
    );
    Ok(status)
}
