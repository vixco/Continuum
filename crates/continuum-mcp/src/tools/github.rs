//! # Read-only GitHub tools through the official `gh` CLI

use base64::Engine;
use continuum_core::config::GitHubConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Repository identifier shared by read tools.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GitHubRepoRequest {
    /// Repository owner or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
}

/// Input for repository listing.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GitHubListReposRequest {
    /// Maximum repositories, clamped to 100.
    pub limit: Option<u8>,
    /// `all`, `public`, or `private`.
    pub visibility: Option<String>,
}

/// Input for issue/PR listing.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GitHubListIssuesRequest {
    /// Repository owner or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// `open`, `closed`, or `all`.
    pub state: Option<String>,
    /// Maximum rows, clamped to 100.
    pub limit: Option<u8>,
}

/// Input for reading one repository file or directory listing.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GitHubGetFileRequest {
    /// Repository owner or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Repository-relative path without traversal.
    pub path: String,
    /// Optional branch, tag, or commit SHA.
    pub git_ref: Option<String>,
}

/// Decoded repository file response.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitHubFileResponse {
    /// GitHub API object type (`file` or `dir`).
    pub kind: String,
    /// Repository-relative path.
    pub path: String,
    /// Blob SHA when this is a file.
    pub sha: Option<String>,
    /// Decoded UTF-8 content for a file.
    pub content: Option<String>,
    /// Directory entries when this is a directory.
    pub entries: Option<Value>,
}

/// Returns the authenticated GitHub user profile.
pub async fn me(config: &GitHubConfig) -> Result<Value, GitHubToolError> {
    get_json("/user", &[], config).await
}

/// Lists repositories visible to the connected account.
pub async fn list_repos(
    request: &GitHubListReposRequest,
    config: &GitHubConfig,
) -> Result<Value, GitHubToolError> {
    let visibility = request.visibility.as_deref().unwrap_or("all");
    if !["all", "public", "private"].contains(&visibility) {
        return Err(GitHubToolError::InvalidInput(
            "visibility must be all, public, or private".into(),
        ));
    }
    let fields = [
        (
            "per_page",
            request.limit.unwrap_or(30).clamp(1, 100).to_string(),
        ),
        ("visibility", visibility.to_string()),
        ("sort", "updated".to_string()),
        (
            "affiliation",
            "owner,collaborator,organization_member".to_string(),
        ),
    ];
    get_json("/user/repos", &fields, config).await
}

/// Returns repository metadata.
pub async fn get_repo(
    request: &GitHubRepoRequest,
    config: &GitHubConfig,
) -> Result<Value, GitHubToolError> {
    validate_repo(&request.owner, &request.repo)?;
    get_json(
        &format!("/repos/{}/{}", request.owner, request.repo),
        &[],
        config,
    )
    .await
}

/// Lists issues and pull requests from GitHub's issues endpoint.
pub async fn list_issues(
    request: &GitHubListIssuesRequest,
    config: &GitHubConfig,
) -> Result<Value, GitHubToolError> {
    validate_repo(&request.owner, &request.repo)?;
    let state = request.state.as_deref().unwrap_or("open");
    if !["open", "closed", "all"].contains(&state) {
        return Err(GitHubToolError::InvalidInput(
            "state must be open, closed, or all".into(),
        ));
    }
    let fields = [
        (
            "per_page",
            request.limit.unwrap_or(30).clamp(1, 100).to_string(),
        ),
        ("state", state.to_string()),
    ];
    get_json(
        &format!("/repos/{}/{}/issues", request.owner, request.repo),
        &fields,
        config,
    )
    .await
}

/// Reads one UTF-8 file, or returns a directory listing.
pub async fn get_file(
    request: &GitHubGetFileRequest,
    config: &GitHubConfig,
) -> Result<GitHubFileResponse, GitHubToolError> {
    validate_repo(&request.owner, &request.repo)?;
    let path = validate_path(&request.path)?;
    let endpoint = format!("/repos/{}/{}/contents/{path}", request.owner, request.repo);
    let fields: Vec<(&str, String)> = request
        .git_ref
        .as_ref()
        .map(|git_ref| vec![("ref", git_ref.clone())])
        .unwrap_or_default();
    let value = get_json(&endpoint, &fields, config).await?;
    if let Some(entries) = value.as_array() {
        return Ok(GitHubFileResponse {
            kind: "dir".into(),
            path: request.path.clone(),
            sha: None,
            content: None,
            entries: Some(Value::Array(entries.to_vec())),
        });
    }
    let kind = value["type"].as_str().unwrap_or("unknown").to_string();
    if kind != "file" {
        return Err(GitHubToolError::InvalidResponse(
            "GitHub content is neither a file nor directory".into(),
        ));
    }
    let encoded = value["content"]
        .as_str()
        .ok_or_else(|| GitHubToolError::InvalidResponse("file content is absent".into()))?
        .replace(['\r', '\n'], "");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| GitHubToolError::InvalidResponse(error.to_string()))?;
    if bytes.len() > config.max_response_bytes {
        return Err(GitHubToolError::ResponseTooLarge(bytes.len()));
    }
    let content = String::from_utf8(bytes).map_err(|_| GitHubToolError::NonUtf8)?;
    Ok(GitHubFileResponse {
        kind,
        path: value["path"].as_str().unwrap_or(&request.path).to_string(),
        sha: value["sha"].as_str().map(str::to_string),
        content: Some(content),
        entries: None,
    })
}

async fn get_json(
    endpoint: &str,
    fields: &[(&str, String)],
    config: &GitHubConfig,
) -> Result<Value, GitHubToolError> {
    if !config.enabled {
        return Err(GitHubToolError::Disabled);
    }
    let bytes =
        continuum_core::github_cli::api_get(endpoint, fields, config.api_timeout_secs, None)
            .await
            .map_err(|error| GitHubToolError::Cli(error.to_string()))?;
    if bytes.len() > config.max_response_bytes {
        return Err(GitHubToolError::ResponseTooLarge(bytes.len()));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| GitHubToolError::InvalidResponse(error.to_string()))
}

fn validate_repo(owner: &str, repo: &str) -> Result<(), GitHubToolError> {
    for (label, value) in [("owner", owner), ("repo", repo)] {
        if value.is_empty()
            || value.len() > 100
            || !value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
        {
            return Err(GitHubToolError::InvalidInput(format!("invalid {label}")));
        }
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<String, GitHubToolError> {
    if path.is_empty() || path.len() > 1024 || path.contains(['\r', '\n', '\\']) {
        return Err(GitHubToolError::InvalidInput(
            "invalid repository path".into(),
        ));
    }
    let mut url = url::Url::parse("https://api.github.com/")
        .map_err(|error| GitHubToolError::InvalidInput(error.to_string()))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| GitHubToolError::InvalidInput("invalid repository path".into()))?;
        segments.clear();
        for segment in path.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(GitHubToolError::InvalidInput(
                    "path traversal is forbidden".into(),
                ));
            }
            segments.push(segment);
        }
    }
    Ok(url.path().trim_start_matches('/').to_string())
}

/// Read-only GitHub tool error.
#[derive(Debug, thiserror::Error)]
pub enum GitHubToolError {
    /// Integration is disabled in config.
    #[error("GitHub integration is disabled")]
    Disabled,
    /// Input validation failed.
    #[error("invalid GitHub input: {0}")]
    InvalidInput(String),
    /// Official CLI/auth/API failure.
    #[error("GitHub CLI error: {0}")]
    Cli(String),
    /// Response exceeded the configured cap.
    #[error("GitHub response is {0} bytes, above the configured maximum")]
    ResponseTooLarge(usize),
    /// Response JSON/content was malformed.
    #[error("invalid GitHub response: {0}")]
    InvalidResponse(String),
    /// Binary repository files are not returned as text.
    #[error("GitHub file is not UTF-8 text")]
    NonUtf8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_and_path_validation_blocks_injection() {
        assert!(validate_repo("openai", "codex").is_ok());
        assert!(validate_repo("../evil", "repo").is_err());
        assert_eq!(validate_path("src/main.rs").unwrap(), "src/main.rs");
        assert!(validate_path("../secret").is_err());
        assert!(validate_path("a//b").is_err());
    }
}
