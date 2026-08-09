//! # Filesystem allowlist
//!
//! Single gatekeeper for every filesystem-touching MCP tool. Before reading a
//! file or listing a directory we call [`is_path_allowed`], which enforces
//! three independent checks in order:
//!
//! 1. **Root check** — the canonicalized target path must be inside one of the
//!    declared allowed roots (`~/.continuum-dev/`, `project.*.dir` facts, or
//!    `[mcp.fs].extra_paths` from config).
//! 2. **Deny-dir check** — if any canonicalized path component matches a name
//!    in [`DENY_DIRS`], reject. Applies regardless of root — even if the user
//!    allowlists their whole home, `.ssh` inside it stays blocked.
//! 3. **Deny-pattern check** — if the filename matches any glob in
//!    [`DENY_PATTERNS`], reject. Catches private keys, `.env` files, etc.
//!
//! The deny list is hardcoded and cannot be disabled or overridden from config.

use std::path::{Component, Path, PathBuf};

/// Directory names (case-insensitive, any depth) that are never readable.
/// Path component match, not substring — `.ssh` denies but `myssh` does not.
pub const DENY_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".docker",
    ".gradle",      // contains caches + init scripts with tokens
    "User Data",    // Chromium profile dir (cookies, passwords)
    "Profiles",     // Firefox profile dir
    "Crashpad",     // often alongside browser profiles
    "keychain",     // common macOS-style folder name
    "secrets",      // generic
    "private",      // generic
    "node_modules", // size, not security (per project spec)
    "target",       // Rust build artifacts — huge + not user content
    "AppData",      // Windows user-data container (covers Roaming + Local)
];

/// Glob patterns (case-insensitive, filename-only) that are never readable.
pub const DENY_PATTERNS: &[&str] = &[
    "*.pem",
    "*.key",
    "*.pfx",
    "*.p12",
    "*.ppk",
    "*.pkcs12",
    "*.crt", // certs often ship bundled with private keys in real repos
    "*.cer",
    "*.der",
    "*.jks", // Java keystores
    "*.asc", // often PGP private keys
    "id_rsa",
    "id_rsa.*",
    "id_ed25519",
    "id_ed25519.*",
    "id_ecdsa",
    "id_ecdsa.*",
    "id_dsa",
    "id_dsa.*",
    ".env",
    ".env.*",
    ".envrc",
    "*.sqlite-journal",
    "*.kdbx", // KeePass
    "*.1password",
];

/// Runtime-built allowlist: the union of all roots the orchestrator is allowed
/// to read under, combined with the hardcoded deny filters.
#[derive(Debug, Clone, Default)]
pub struct AllowlistConfig {
    /// Absolute, canonicalized roots. Paths outside all of these are rejected
    /// even before the deny list runs.
    pub allowed_roots: Vec<PathBuf>,
}

impl AllowlistConfig {
    /// Builds an allowlist from a set of roots, canonicalizing and deduplicating.
    /// Roots that cannot be canonicalized (e.g. don't exist) are dropped with
    /// a warn log — opting in to a nonexistent path is a configuration error,
    /// not a security one.
    pub fn from_roots<I, P>(roots: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut out = Vec::new();
        for r in roots {
            let r = r.as_ref();
            match canonicalize(r) {
                Ok(c) => {
                    if !out.contains(&c) {
                        out.push(c);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        layer = "mcp",
                        component = "allowlist",
                        path = %r.display(),
                        error = %e,
                        "Allowlist root not canonicalizable — skipped"
                    );
                }
            }
        }
        Self { allowed_roots: out }
    }
}

/// Why a path was rejected. Exposed to clients so the orchestrator can see a
/// precise reason (which may help it rephrase the request).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// The target path cannot be canonicalized (does not exist, permission
    /// denied, or contains invalid UTF-8).
    InvalidPath(String),
    /// The target is not under any allowed root.
    OutsideAllowedRoots,
    /// A path component is on the hardcoded deny list.
    DeniedDirectory(String),
    /// The filename matches a deny pattern (private key, `.env`, etc.).
    DeniedPattern(String),
}

impl std::fmt::Display for DenyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DenyReason::InvalidPath(s) => write!(f, "invalid path: {s}"),
            DenyReason::OutsideAllowedRoots => {
                write!(f, "path is outside the allowed filesystem roots")
            }
            DenyReason::DeniedDirectory(d) => {
                write!(f, "path contains denied directory component: {d}")
            }
            DenyReason::DeniedPattern(p) => {
                write!(f, "filename matches denied pattern: {p}")
            }
        }
    }
}

/// The single gatekeeper. Returns the canonicalized path on success.
///
/// A leading `~` is expanded to the current user's home directory before
/// canonicalization. This is intentionally done *inside* the gatekeeper so
/// every read/write tool gets identical behavior and the expanded target still
/// has to pass the normal allowed-root + hard-deny checks.
///
/// The deny-dir check runs on components **below** the matched allowed root —
/// if the user opts in to a root, everything in its ancestry is implicitly
/// approved (the user's home might legitimately contain a path component like
/// `AppData` on Windows, and the intent of allowlisting is to trust the root).
pub fn is_path_allowed(path: &Path, cfg: &AllowlistConfig) -> Result<PathBuf, DenyReason> {
    let expanded = expand_home(path);
    let canonical = canonicalize(&expanded).map_err(|e| DenyReason::InvalidPath(e.to_string()))?;

    // 1. Root check — find the first root this path is inside.
    let matched_root = cfg
        .allowed_roots
        .iter()
        .find(|root| canonical.starts_with(root))
        .ok_or(DenyReason::OutsideAllowedRoots)?;

    // 2. Deny-dir check — scan components below the matched root only.
    let suffix = canonical
        .strip_prefix(matched_root)
        .unwrap_or(canonical.as_path());
    for comp in suffix.components() {
        if let Component::Normal(name) = comp {
            let name_str = name.to_string_lossy();
            let lower = name_str.to_lowercase();
            for denied in DENY_DIRS {
                if lower == denied.to_lowercase() {
                    return Err(DenyReason::DeniedDirectory(name_str.into_owned()));
                }
            }
        }
    }

    // 3. Deny-pattern check — filename only (not full path).
    if let Some(file_name) = canonical.file_name().map(|n| n.to_string_lossy()) {
        for pattern in DENY_PATTERNS {
            if matches_glob_ci(&file_name, pattern) {
                return Err(DenyReason::DeniedPattern((*pattern).to_string()));
            }
        }
    }

    Ok(canonical)
}

/// Checks only the hard deny rules for a repository-relative path.
///
/// This is used for deleted Git paths that cannot be canonicalized. Absolute
/// paths and parent traversals are rejected as invalid.
pub fn is_relative_path_denied(path: &Path) -> Result<(), DenyReason> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(DenyReason::InvalidPath(
            "expected a repository-relative path".to_string(),
        ));
    }
    for component in path.components() {
        if let Component::Normal(name) = component {
            let value = name.to_string_lossy();
            if let Some(denied) = DENY_DIRS
                .iter()
                .find(|denied| value.eq_ignore_ascii_case(denied))
            {
                return Err(DenyReason::DeniedDirectory((*denied).to_string()));
            }
        }
    }
    if let Some(name) = path.file_name().map(|value| value.to_string_lossy()) {
        for pattern in DENY_PATTERNS {
            if matches_glob_ci(&name, pattern) {
                return Err(DenyReason::DeniedPattern((*pattern).to_string()));
            }
        }
    }
    Ok(())
}

/// Resolves a not-yet-existing direct child through its canonical parent.
///
/// Creation and move destinations use this instead of canonicalizing the
/// target itself. The parent must already exist and be allowlisted, and the
/// new filename still passes the hard deny rules.
pub fn resolve_new_path_allowed(path: &Path, cfg: &AllowlistConfig) -> Result<PathBuf, DenyReason> {
    let expanded = expand_home(path);
    if expanded.exists() {
        return Err(DenyReason::InvalidPath(
            "destination already exists".to_string(),
        ));
    }
    let parent = expanded.parent().ok_or_else(|| {
        DenyReason::InvalidPath("new path must have an existing parent".to_string())
    })?;
    let parent = is_path_allowed(parent, cfg)?;
    let name = expanded
        .file_name()
        .ok_or_else(|| DenyReason::InvalidPath("new path must have a filename".to_string()))?;
    is_relative_path_denied(Path::new(name))?;
    Ok(parent.join(name))
}

/// Expand only a leading standalone `~` path component. `~other` and tildes in
/// later components are treated literally. Falling back to the original path
/// keeps error reporting honest when the process has no discoverable home dir.
fn expand_home(path: &Path) -> PathBuf {
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return path.to_path_buf();
    };
    if first != "~" {
        return path.to_path_buf();
    }

    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    let Some(mut expanded) = home else {
        return path.to_path_buf();
    };
    for component in components {
        expanded.push(component.as_os_str());
    }
    expanded
}

/// Canonicalize a path and strip the Windows `\\?\` verbatim prefix if present
/// (which `std::fs::canonicalize` always emits on Windows).
fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let c = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        let s = c.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(stripped));
        }
    }
    Ok(c)
}

/// Case-insensitive glob match using the `glob` crate's pattern syntax.
/// Intentionally narrow — only supports `*`, `?`, and `[abc]` patterns.
fn matches_glob_ci(name: &str, pattern: &str) -> bool {
    let pattern_lower = pattern.to_lowercase();
    let name_lower = name.to_lowercase();
    match glob::Pattern::new(&pattern_lower) {
        Ok(p) => p.matches(&name_lower),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn allows_file_inside_root() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("ok.txt");
        std::fs::write(&file, "hi").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        assert!(is_path_allowed(&file, &cfg).is_ok());
    }

    #[test]
    fn rejects_file_outside_roots() {
        let dir = tempdir().unwrap();
        let other = tempdir().unwrap();
        let file = other.path().join("sneaky.txt");
        std::fs::write(&file, "hi").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        assert_eq!(
            is_path_allowed(&file, &cfg).unwrap_err(),
            DenyReason::OutsideAllowedRoots
        );
    }

    #[test]
    fn rejects_env_file_even_in_allowed_root() {
        let dir = tempdir().unwrap();
        let env = dir.path().join(".env");
        std::fs::write(&env, "SECRET=hi").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        assert!(matches!(
            is_path_allowed(&env, &cfg).unwrap_err(),
            DenyReason::DeniedPattern(_)
        ));
    }

    #[test]
    fn rejects_pem_file() {
        let dir = tempdir().unwrap();
        let key = dir.path().join("cert.pem");
        std::fs::write(&key, "KEY").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        assert!(matches!(
            is_path_allowed(&key, &cfg).unwrap_err(),
            DenyReason::DeniedPattern(_)
        ));
    }

    #[test]
    fn lexical_check_rejects_deleted_secret_paths_and_traversal() {
        assert!(is_relative_path_denied(Path::new("config/.env")).is_err());
        assert!(is_relative_path_denied(Path::new("../outside.txt")).is_err());
        assert!(is_relative_path_denied(Path::new("src/main.rs")).is_ok());
    }

    #[test]
    fn new_paths_require_an_allowed_parent_and_safe_name() {
        let dir = tempdir().unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        let path = resolve_new_path_allowed(&dir.path().join("new.txt"), &cfg).unwrap();
        assert!(path.ends_with("new.txt"));
        assert!(resolve_new_path_allowed(&dir.path().join(".env"), &cfg).is_err());
    }

    #[test]
    fn rejects_file_in_ssh_subdir() {
        let dir = tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let file = ssh.join("config");
        std::fs::write(&file, "Host *").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        assert!(matches!(
            is_path_allowed(&file, &cfg).unwrap_err(),
            DenyReason::DeniedDirectory(_)
        ));
    }

    #[test]
    fn rejects_file_in_node_modules_subdir() {
        let dir = tempdir().unwrap();
        let nm = dir.path().join("node_modules").join("pkg");
        std::fs::create_dir_all(&nm).unwrap();
        let file = nm.join("index.js");
        std::fs::write(&file, "x").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        assert!(matches!(
            is_path_allowed(&file, &cfg).unwrap_err(),
            DenyReason::DeniedDirectory(_)
        ));
    }

    #[test]
    fn rejects_nonexistent_path() {
        let dir = tempdir().unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        let res = is_path_allowed(&dir.path().join("nope.txt"), &cfg);
        assert!(matches!(res, Err(DenyReason::InvalidPath(_))));
    }

    #[test]
    fn rejects_id_rsa_file() {
        let dir = tempdir().unwrap();
        let key = dir.path().join("id_rsa");
        std::fs::write(&key, "KEY").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        assert!(matches!(
            is_path_allowed(&key, &cfg).unwrap_err(),
            DenyReason::DeniedPattern(_)
        ));
    }

    #[test]
    fn rejects_parent_escape_via_symlink() {
        // Symlinks get resolved by canonicalize, so a symlink pointing outside
        // the allowed root should be caught by OutsideAllowedRoots.
        #[cfg(unix)]
        {
            let dir = tempdir().unwrap();
            let outside = tempdir().unwrap();
            let target = outside.path().join("secret.txt");
            std::fs::write(&target, "secret").unwrap();
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let cfg = AllowlistConfig::from_roots([dir.path()]);
            assert_eq!(
                is_path_allowed(&link, &cfg).unwrap_err(),
                DenyReason::OutsideAllowedRoots
            );
        }
    }

    #[test]
    fn deny_dir_check_is_case_insensitive() {
        let dir = tempdir().unwrap();
        // Windows filesystems are case-insensitive, but path components on Unix
        // preserve case. The deny check lower-cases both sides so this works
        // either way.
        let ssh = dir.path().join(".SSH");
        std::fs::create_dir(&ssh).unwrap();
        let file = ssh.join("config");
        std::fs::write(&file, "x").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        let r = is_path_allowed(&file, &cfg);
        assert!(matches!(r.unwrap_err(), DenyReason::DeniedDirectory(_)));
    }

    #[test]
    fn deny_dir_does_not_match_substring() {
        // "myssh" should not trip the ".ssh" deny rule.
        let dir = tempdir().unwrap();
        let subdir = dir.path().join("myssh");
        std::fs::create_dir(&subdir).unwrap();
        let file = subdir.join("notes.txt");
        std::fs::write(&file, "x").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        assert!(is_path_allowed(&file, &cfg).is_ok());
    }

    #[test]
    fn expands_leading_tilde_to_home() {
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
        if let Some(home) = home {
            let expanded = expand_home(Path::new("~/.continuum-dev/config.toml"));
            assert!(expanded.starts_with(PathBuf::from(home)));
            assert!(expanded.ends_with(Path::new(".continuum-dev/config.toml")));
        }
    }

    #[test]
    fn does_not_expand_nonleading_tilde() {
        assert_eq!(expand_home(Path::new("project/~/file")), PathBuf::from("project/~/file"));
        assert_eq!(expand_home(Path::new("~other/file")), PathBuf::from("~other/file"));
    }
}
