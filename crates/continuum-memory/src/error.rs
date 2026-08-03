//! Error type for the memory vault (library errors use thiserror per house rules).

/// Errors produced by the memory vault.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// Filesystem error, annotated with the path involved.
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// A markdown file's frontmatter could not be parsed.
    #[error("frontmatter parse error: {0}")]
    Parse(String),
    /// SQLite/index error.
    #[error("index database error: {0}")]
    Db(#[from] sqlx::Error),
    /// Note id or slug not found.
    #[error("note not found: {0}")]
    NotFound(String),
    /// Caller-supplied input was invalid (bad type, empty title, traversal…).
    #[error("invalid input: {0}")]
    Invalid(String),
    /// File-watcher error.
    #[error("watcher error: {0}")]
    Watch(String),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, MemoryError>;

impl MemoryError {
    /// Short, user-presentable message (dashboard surfaces this string).
    pub fn user_message(&self) -> String {
        match self {
            Self::Io { path, .. } => format!("Could not access {path}"),
            Self::Parse(m) => format!("This note's header is not valid YAML: {m}"),
            Self::Db(_) => {
                "The memory index hit a database error; it will rebuild on restart.".into()
            }
            Self::NotFound(id) => format!("Memory {id} no longer exists"),
            Self::Invalid(m) => m.clone(),
            Self::Watch(_) => "The vault file-watcher failed; live updates are paused.".into(),
        }
    }

    /// Helper to build an Io error from a path + io::Error.
    pub fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}
