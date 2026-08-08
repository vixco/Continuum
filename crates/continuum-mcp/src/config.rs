//! # continuum-mcp configuration
//!
//! Loads the `[mcp]` section of `~/.continuum-dev/config.toml`. Parsing is
//! best-effort — bad or missing config must never block MCP server startup,
//! since the orchestrator spawns us on every wake and a failed start breaks
//! the whole cognitive loop.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// MCP-specific configuration (subset of the full Continuum config).
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct McpServerConfig {
    pub fs: McpFsConfig,
    pub terminal: McpTerminalConfig,
    pub ide: McpIdeConfig,
}

/// Filesystem-allowlist expansion settings.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct McpFsConfig {
    /// Extra absolute paths the user opts in to beyond the hardcoded defaults.
    /// `~` is expanded at load time. Denied dirs/patterns still apply even
    /// inside these roots.
    pub extra_paths: Vec<PathBuf>,
    /// Maximum UTF-8 bytes accepted by create/patch actions.
    pub max_write_bytes: usize,
    /// Maximum substitutions in one replace-all patch.
    pub max_patch_replacements: usize,
}

impl Default for McpFsConfig {
    fn default() -> Self {
        Self {
            extra_paths: Vec::new(),
            max_write_bytes: 1024 * 1024,
            max_patch_replacements: 100,
        }
    }
}

/// Terminal broker limits and executable allowlist.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct McpTerminalConfig {
    /// Executable basenames the broker may launch. Paths and shells are rejected.
    pub allowed_programs: Vec<String>,
    /// Maximum command runtime.
    pub max_timeout_secs: u64,
    /// Combined stdout/stderr byte cap returned and persisted.
    pub max_output_bytes: usize,
    /// Maximum number of arguments in one invocation.
    pub max_args: usize,
}

impl Default for McpTerminalConfig {
    fn default() -> Self {
        Self {
            allowed_programs: vec![
                "cargo".into(),
                "git".into(),
                "dotnet".into(),
                "rustc".into(),
                "rustfmt".into(),
            ],
            max_timeout_secs: 900,
            max_output_bytes: 512 * 1024,
            max_args: 128,
        }
    }
}

/// Native editor bridge limits and executable allowlist.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct McpIdeConfig {
    /// Supported editor command aliases. Paths and arbitrary programs are rejected.
    pub allowed_editors: Vec<String>,
    /// Maximum time to wait for the editor CLI handoff.
    pub launch_timeout_secs: u64,
}

impl Default for McpIdeConfig {
    fn default() -> Self {
        Self {
            allowed_editors: vec!["code".into(), "code-insiders".into(), "codium".into()],
            launch_timeout_secs: 15,
        }
    }
}

/// Loads the `[mcp]` section from `<data_dir>/config.toml`. Returns defaults
/// on any failure and logs a warning; never errors upward.
pub fn load(data_dir: &Path) -> McpServerConfig {
    let path = data_dir.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return McpServerConfig::default(),
    };

    #[derive(Deserialize)]
    struct Wrapped {
        #[serde(default)]
        mcp: McpServerConfig,
    }

    match toml::from_str::<Wrapped>(&text) {
        Ok(w) => expand_home(w.mcp, data_dir),
        Err(e) => {
            tracing::warn!(
                layer = "mcp",
                component = "config",
                path = %path.display(),
                error = %e,
                "Failed to parse [mcp] section — using defaults"
            );
            McpServerConfig::default()
        }
    }
}

fn expand_home(mut cfg: McpServerConfig, data_dir: &Path) -> McpServerConfig {
    // Expand a leading `~` in extra_paths. Use the parent of data_dir as the
    // home directory (since data_dir is usually `~/.continuum-dev/`). Fall back to
    // dirs::home_dir() if that heuristic fails.
    let home = dirs::home_dir()
        .or_else(|| data_dir.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    for p in &mut cfg.fs.extra_paths {
        if let Ok(rest) = p.strip_prefix("~") {
            *p = home.join(rest);
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_config_returns_defaults() {
        let dir = tempdir().unwrap();
        let cfg = load(dir.path());
        assert!(cfg.fs.extra_paths.is_empty());
    }

    #[test]
    fn valid_config_is_parsed() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            r#"
            [mcp.fs]
            extra_paths = ["C:/code/simcharts", "D:/notes"]
            "#,
        )
        .unwrap();
        let cfg = load(dir.path());
        assert_eq!(cfg.fs.extra_paths.len(), 2);
        assert_eq!(cfg.fs.max_write_bytes, 1024 * 1024);
        assert!(cfg
            .ide
            .allowed_editors
            .iter()
            .any(|editor| editor == "code"));
    }

    #[test]
    fn malformed_config_returns_defaults_not_panic() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "this is not valid toml [[[").unwrap();
        let cfg = load(dir.path());
        assert!(cfg.fs.extra_paths.is_empty());
    }

    #[test]
    fn tilde_expansion_in_extra_paths() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            r#"
            [mcp.fs]
            extra_paths = ["~/notes"]
            "#,
        )
        .unwrap();
        let cfg = load(dir.path());
        assert_eq!(cfg.fs.extra_paths.len(), 1);
        let p = cfg.fs.extra_paths[0].to_string_lossy().to_string();
        assert!(!p.starts_with('~'), "tilde should be expanded, got {p}");
    }
}
