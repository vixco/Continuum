//! # Skill loader
//!
//! Scans a directory for `*/SKILL.md` files, parses each one, and caches the
//! result. Supports hot-reloading: when [`SkillLoader::reload_if_changed`] is
//! called (periodically by the runtime), any file whose mtime has advanced
//! is re-parsed and every deleted/added file is reflected.
//!
//! The loader is deliberately read-only — file writes (create, edit, delete)
//! live in [`super::installer`] so the two concerns don't tangle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::RwLock;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use super::frontmatter::parse_skill_file;
use super::types::Skill;

/// Shared, cloneable handle to the skill cache.
#[derive(Clone, Default)]
pub struct SkillLoader {
    inner: Arc<RwLock<LoaderInner>>,
    root: Arc<PathBuf>,
}

/// Returned from [`SkillLoader::spawn_watcher`] so the caller can stop the
/// watcher cleanly on shutdown.
pub struct SkillWatchHandle {
    pub handle: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct LoaderInner {
    /// Canonical name → loaded skill.
    by_name: HashMap<String, Skill>,
    /// Skills that failed to parse, keyed by their path (for dashboard display).
    errors: HashMap<PathBuf, String>,
    /// Names disabled via config — preserved across reloads.
    disabled: Vec<String>,
}

impl SkillLoader {
    /// Build a loader pointed at `root`. The directory does not need to exist
    /// yet — if it's missing, every operation is a no-op.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(LoaderInner::default())),
            root: Arc::new(root.into()),
        }
    }

    /// Replace the disabled-names list (from config). Does not rescan.
    pub fn set_disabled(&self, names: Vec<String>) {
        let mut inner = self.inner.write();
        inner.disabled = names;
        let disabled = inner.disabled.clone();
        for skill in inner.by_name.values_mut() {
            skill.enabled = !disabled.iter().any(|d| d == &skill.frontmatter.name);
        }
    }

    /// Scan the root directory and replace the cache. Errors for individual
    /// skills are stored so the dashboard can show "this skill failed to
    /// parse" without aborting the whole reload.
    pub fn reload(&self) -> Result<()> {
        let root = self.root.as_path();
        if !root.exists() {
            debug!(
                layer = "skills",
                component = "loader",
                root = %root.display(),
                "Skill root missing; empty skill set"
            );
            let mut inner = self.inner.write();
            inner.by_name.clear();
            inner.errors.clear();
            return Ok(());
        }

        let mut by_name: HashMap<String, Skill> = HashMap::new();
        let mut errors: HashMap<PathBuf, String> = HashMap::new();

        let entries = std::fs::read_dir(root)
            .with_context(|| format!("Failed to read skills root {}", root.display()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }
            match parse_skill_file(&skill_md) {
                Ok(skill) => {
                    let name = skill.frontmatter.name.clone();
                    by_name.insert(name, skill);
                }
                Err(e) => {
                    warn!(
                        layer = "skills",
                        component = "loader",
                        path = %skill_md.display(),
                        error = %e,
                        "Skipping invalid skill"
                    );
                    errors.insert(skill_md, e.to_string());
                }
            }
        }

        {
            let mut inner = self.inner.write();
            let disabled = inner.disabled.clone();
            for skill in by_name.values_mut() {
                skill.enabled = !disabled.iter().any(|d| d == &skill.frontmatter.name);
            }
            info!(
                layer = "skills",
                component = "loader",
                count = by_name.len(),
                errors = errors.len(),
                "Skills loaded"
            );
            inner.by_name = by_name;
            inner.errors = errors;
        }
        Ok(())
    }

    /// Check whether any SKILL.md mtime is newer than what's in the cache.
    /// Triggers a full reload if so. Returns `true` when a reload happened.
    pub fn reload_if_changed(&self) -> Result<bool> {
        let root = self.root.as_path();
        if !root.exists() {
            return Ok(false);
        }

        let snapshot: HashMap<PathBuf, Option<chrono::DateTime<chrono::Utc>>> = {
            let inner = self.inner.read();
            inner
                .by_name
                .values()
                .map(|s| (s.path.clone(), s.modified_at))
                .collect()
        };

        let mut changed = false;
        let mut seen: Vec<PathBuf> = Vec::new();

        for entry in std::fs::read_dir(root)?.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }
            seen.push(skill_md.clone());

            let disk_mod = std::fs::metadata(&skill_md)
                .and_then(|m| m.modified())
                .ok()
                .map(chrono::DateTime::<chrono::Utc>::from);
            match snapshot.get(&skill_md) {
                Some(Some(existing)) => {
                    if disk_mod.map(|d| d > *existing).unwrap_or(true) {
                        changed = true;
                        break;
                    }
                }
                _ => {
                    changed = true;
                    break;
                }
            }
        }

        if !changed {
            // Any cached skill whose file was deleted?
            for path in snapshot.keys() {
                if !seen.contains(path) {
                    changed = true;
                    break;
                }
            }
        }

        if changed {
            self.reload()?;
        }
        Ok(changed)
    }

    /// Returns every skill currently loaded (including disabled ones).
    pub fn list(&self) -> Vec<Skill> {
        let inner = self.inner.read();
        let mut out: Vec<Skill> = inner.by_name.values().cloned().collect();
        out.sort_by(|a, b| a.frontmatter.name.cmp(&b.frontmatter.name));
        out
    }

    /// Returns every skill that is currently active (loaded AND enabled).
    pub fn enabled(&self) -> Vec<Skill> {
        self.list().into_iter().filter(|s| s.enabled).collect()
    }

    /// Get one skill by name.
    pub fn get(&self, name: &str) -> Option<Skill> {
        self.inner.read().by_name.get(name).cloned()
    }

    /// Returns the parse errors surface for the dashboard.
    pub fn errors(&self) -> HashMap<PathBuf, String> {
        self.inner.read().errors.clone()
    }

    /// Directory the loader watches.
    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// Spawns a background poll loop that re-scans the directory every
    /// `interval` (ceiling 5 s) and stops when `shutdown` flips to true.
    pub fn spawn_watcher(
        &self,
        interval: Duration,
        mut shutdown: watch::Receiver<bool>,
    ) -> SkillWatchHandle {
        let loader = self.clone();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(500)));
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(e) = loader.reload_if_changed() {
                            warn!(
                                layer = "skills",
                                component = "loader",
                                error = %e,
                                "Skill hot reload failed"
                            );
                        }
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
        });
        SkillWatchHandle { handle }
    }

    /// Test/dashboard helper: reload from disk *and* return the list.
    pub fn reload_list(&self) -> Result<Vec<Skill>> {
        self.reload()?;
        Ok(self.list())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_skill(root: &Path, name: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        let content = format!(
            "---\nname: {name}\ndescription: test skill\ntriggers:\n  - {name}\n---\n{body}\n"
        );
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn loads_two_skills_from_directory() {
        let tmp = TempDir::new().unwrap();
        write_skill(tmp.path(), "alpha", "do alpha things");
        write_skill(tmp.path(), "beta", "do beta things");

        let loader = SkillLoader::new(tmp.path());
        loader.reload().unwrap();
        let list = loader.list();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|s| s.frontmatter.name == "alpha"));
        assert!(list.iter().any(|s| s.frontmatter.name == "beta"));
    }

    #[test]
    fn disabled_names_are_respected() {
        let tmp = TempDir::new().unwrap();
        write_skill(tmp.path(), "alpha", "a");
        write_skill(tmp.path(), "beta", "b");

        let loader = SkillLoader::new(tmp.path());
        loader.set_disabled(vec!["beta".into()]);
        loader.reload().unwrap();

        let enabled: Vec<_> = loader
            .enabled()
            .into_iter()
            .map(|s| s.frontmatter.name)
            .collect();
        assert_eq!(enabled, vec!["alpha"]);
    }

    #[test]
    fn reload_if_changed_detects_new_file() {
        let tmp = TempDir::new().unwrap();
        write_skill(tmp.path(), "alpha", "a");

        let loader = SkillLoader::new(tmp.path());
        loader.reload().unwrap();
        assert_eq!(loader.list().len(), 1);

        // Add a new skill and confirm reload picks it up.
        write_skill(tmp.path(), "beta", "b");
        let changed = loader.reload_if_changed().unwrap();
        assert!(changed);
        assert_eq!(loader.list().len(), 2);
    }

    #[test]
    fn errors_surface_for_invalid_skill() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("broken");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "this has no frontmatter").unwrap();

        let loader = SkillLoader::new(tmp.path());
        loader.reload().unwrap();
        let errs = loader.errors();
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn missing_root_yields_empty_set() {
        let loader = SkillLoader::new("/definitely/does/not/exist/kairo-skills-xyz");
        loader.reload().unwrap();
        assert!(loader.list().is_empty());
    }
}
