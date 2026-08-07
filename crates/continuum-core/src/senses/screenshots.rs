//! # Screenshot retention policy (Layer 1 artefacts)
//!
//! Screenshots are the only sensory artefact that lives outside SQLite:
//! `senses::vision` writes JPEGs under
//! `<screenshots_dir>/<monitor_id>/<YYYY-MM-DD>/<HH-MM-SS-mmm>.jpg` and
//! stores only the *path* on the `perception_frames` row. Two independent
//! things therefore have to delete them (context engine spec §4.11):
//!
//! 1. **Row-driven deletes** — `RawLog::rotate` removes the file named by
//!    every frame row it deletes. Exact, but only covers files a row
//!    actually references.
//! 2. **The mtime backstop sweep** — this module. It walks
//!    `screenshots_dir` and deletes any image older than
//!    `[storage] screenshot_max_age_hours` regardless of what the database
//!    knows. That is what reclaims files leaked by a crash between "image
//!    saved" and "row written", by a wiped database, or by an older build
//!    that stored paths differently.
//!
//! Both are gated by `[storage] delete_screenshots_with_rotation`; with it
//! off Continuum never deletes an image on its own.
//!
//! This module is deliberately free of database and feature-gated
//! dependencies (pure `std::fs` + a caller-supplied clock), so the policy
//! is unit-testable under `--no-default-features`.

use std::path::Path;
use std::time::{Duration, SystemTime};

use tracing::{debug, warn};

use crate::config::StorageConfig;

/// How deep the sweep descends below `screenshots_dir`. The writer layout
/// is `<monitor_id>/<date>/<file>.jpg` (2 directory levels); 4 gives room
/// for a layout change without ever turning into an unbounded walk of a
/// misconfigured directory.
const MAX_SWEEP_DEPTH: usize = 4;

/// File extensions the sweep will delete. Anything else in the tree
/// (README, a user's own notes, a stray `.zip`) is left alone — this
/// routine deletes Continuum's own captures, not "old files".
const SWEEPABLE_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// The screenshot half of `[storage]`, extracted so callers can pass the
/// policy around without dragging the whole config (and so tests can build
/// one in a line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenshotPolicy {
    /// `[storage] delete_screenshots_with_rotation` — the master switch
    /// for *both* row-driven deletes and the age sweep.
    pub delete_with_rotation: bool,
    /// `[storage] screenshot_max_age_hours` — `0` disables the age sweep.
    pub max_age_hours: u64,
}

impl ScreenshotPolicy {
    /// The policy that keeps every file: what a caller passes when the
    /// user has opted out of automatic screenshot deletion.
    pub const KEEP_EVERYTHING: Self = Self {
        delete_with_rotation: false,
        max_age_hours: 0,
    };

    /// The policy an **explicit user deletion** runs under (fixwave 2, I2).
    ///
    /// `delete_screenshots_with_rotation` is a *rotation* knob: it decides
    /// whether Continuum tidies up screenshots on its own schedule. It must
    /// never govern a deletion the user asked for by name — "Forget this
    /// frame" with the knob off used to drop the row and orphan the JPEG on
    /// disk, unreachable by any future targeted delete.
    pub const ALWAYS_DELETE: Self = Self {
        delete_with_rotation: true,
        max_age_hours: 0,
    };

    /// This policy, forced to delete referenced files — for the explicit
    /// deletion paths (`forget`, `delete_range`). See [`Self::ALWAYS_DELETE`].
    pub fn for_explicit_delete(&self) -> Self {
        Self {
            delete_with_rotation: true,
            max_age_hours: self.max_age_hours,
        }
    }

    /// True when row-driven screenshot deletes are allowed.
    pub fn deletes_referenced_files(&self) -> bool {
        self.delete_with_rotation
    }

    /// The mtime cutoff: files last modified before this are sweepable.
    /// `None` when the sweep is disabled (switch off, or age 0).
    pub fn cutoff(&self, now: SystemTime) -> Option<SystemTime> {
        if !self.delete_with_rotation || self.max_age_hours == 0 {
            return None;
        }
        now.checked_sub(Duration::from_secs(self.max_age_hours.saturating_mul(3600)))
    }
}

impl Default for ScreenshotPolicy {
    fn default() -> Self {
        Self {
            delete_with_rotation: true,
            max_age_hours: 720,
        }
    }
}

impl From<&StorageConfig> for ScreenshotPolicy {
    fn from(cfg: &StorageConfig) -> Self {
        Self {
            delete_with_rotation: cfg.delete_screenshots_with_rotation,
            max_age_hours: cfg.screenshot_max_age_hours,
        }
    }
}

/// What one sweep pass did. Reported by the rotation task and used by
/// tests; never an error type — a sweep that cannot read a directory logs
/// and moves on rather than failing rotation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepStats {
    /// Image files inspected.
    pub scanned: usize,
    /// Image files deleted because they were older than the cutoff.
    pub deleted: usize,
    /// Deletions that failed (permission, file in use, already gone).
    pub failed: usize,
    /// Now-empty directories removed after their images went away.
    pub dirs_removed: usize,
}

impl SweepStats {
    fn merge(&mut self, other: SweepStats) {
        self.scanned += other.scanned;
        self.deleted += other.deleted;
        self.failed += other.failed;
        self.dirs_removed += other.dirs_removed;
    }
}

/// Deletes screenshot files under `dir` whose mtime is older than the
/// policy cutoff, then removes the directories that became empty.
///
/// `now` is injected so retention is testable without sleeping. A missing
/// directory, an unreadable directory, or a file whose metadata cannot be
/// read is skipped — this is a best-effort backstop that must never abort
/// the rotation pass that calls it.
///
/// Returns zeroed stats when the policy disables the sweep.
pub fn sweep_screenshots_dir(dir: &Path, policy: &ScreenshotPolicy, now: SystemTime) -> SweepStats {
    let Some(cutoff) = policy.cutoff(now) else {
        debug!(
            layer = "senses",
            component = "screenshots",
            delete_with_rotation = policy.delete_with_rotation,
            max_age_hours = policy.max_age_hours,
            "screenshot sweep disabled by policy"
        );
        return SweepStats::default();
    };
    if !dir.is_dir() {
        return SweepStats::default();
    }
    // Note the root itself is never removed even when it empties: the
    // vision writer expects `screenshots_dir` to exist.
    let stats = sweep_dir(dir, cutoff, MAX_SWEEP_DEPTH);
    if stats.deleted > 0 || stats.failed > 0 {
        tracing::info!(
            layer = "senses",
            component = "screenshots",
            dir = %dir.display(),
            scanned = stats.scanned,
            deleted = stats.deleted,
            failed = stats.failed,
            dirs_removed = stats.dirs_removed,
            max_age_hours = policy.max_age_hours,
            "Screenshot mtime backstop sweep complete"
        );
    }
    stats
}

/// Recursive worker: deletes aged images in `dir`, recurses into
/// subdirectories, and removes each subdirectory that ends up empty.
fn sweep_dir(dir: &Path, cutoff: SystemTime, depth: usize) -> SweepStats {
    let mut stats = SweepStats::default();
    if depth == 0 {
        return stats;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            warn!(
                layer = "senses",
                component = "screenshots",
                dir = %dir.display(),
                error = %error,
                "Could not read screenshot directory during sweep"
            );
            return stats;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            stats.merge(sweep_dir(&path, cutoff, depth - 1));
            if directory_is_empty(&path) && std::fs::remove_dir(&path).is_ok() {
                stats.dirs_removed += 1;
            }
            continue;
        }
        if !is_sweepable_file(&path) {
            continue;
        }
        stats.scanned += 1;
        let old_enough = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .map(|modified| modified < cutoff)
            .unwrap_or(false);
        if !old_enough {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => stats.deleted += 1,
            Err(error) => {
                stats.failed += 1;
                warn!(
                    layer = "senses",
                    component = "screenshots",
                    path = %path.display(),
                    error = %error,
                    "Failed to delete aged screenshot during sweep"
                );
            }
        }
    }

    stats
}

fn directory_is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

fn is_sweepable_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            SWEEPABLE_EXTENSIONS
                .iter()
                .any(|allowed| ext.eq_ignore_ascii_case(allowed))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn write_image(path: &Path, age: Duration) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"jpeg-bytes").unwrap();
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    fn hours(n: u64) -> Duration {
        Duration::from_secs(n * 3600)
    }

    fn tree() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    #[test]
    fn sweep_deletes_only_files_older_than_the_cutoff() {
        let (_guard, root) = tree();
        let old = root.join("monitor-0/2026-01-01/10-00-00-000.jpg");
        let fresh = root.join("monitor-0/2026-08-05/10-00-00-000.jpg");
        write_image(&old, hours(800));
        write_image(&fresh, hours(2));

        let policy = ScreenshotPolicy {
            delete_with_rotation: true,
            max_age_hours: 720,
        };
        let stats = sweep_screenshots_dir(&root, &policy, SystemTime::now());

        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.deleted, 1);
        assert_eq!(stats.failed, 0);
        assert!(!old.exists(), "aged screenshot should be swept");
        assert!(fresh.exists(), "fresh screenshot must survive");
    }

    #[test]
    fn sweep_ignores_referenced_or_unreferenced_alike_it_is_mtime_only() {
        // The sweep is a *backstop*: it never consults the database, so a
        // file that a live row still references is deleted once it is old
        // enough. This test pins that documented behaviour (and is why
        // screenshot_max_age_hours must be >= retention_days * 24).
        let (_guard, root) = tree();
        let referenced = root.join("monitor-0/old/a.jpg");
        let orphan = root.join("monitor-0/old/b.jpg");
        write_image(&referenced, hours(1000));
        write_image(&orphan, hours(1000));

        let stats = sweep_screenshots_dir(&root, &ScreenshotPolicy::default(), SystemTime::now());

        assert_eq!(stats.deleted, 2);
        assert!(!referenced.exists());
        assert!(!orphan.exists());
    }

    #[test]
    fn sweep_is_a_no_op_when_deletion_is_disabled() {
        let (_guard, root) = tree();
        let old = root.join("monitor-0/old/a.jpg");
        write_image(&old, hours(5000));

        let policy = ScreenshotPolicy {
            delete_with_rotation: false,
            max_age_hours: 720,
        };
        let stats = sweep_screenshots_dir(&root, &policy, SystemTime::now());

        assert_eq!(stats, SweepStats::default());
        assert!(old.exists(), "policy off must keep every file");
    }

    #[test]
    fn zero_max_age_disables_the_sweep_but_not_row_driven_deletes() {
        let policy = ScreenshotPolicy {
            delete_with_rotation: true,
            max_age_hours: 0,
        };
        assert!(policy.cutoff(SystemTime::now()).is_none());
        assert!(policy.deletes_referenced_files());

        let (_guard, root) = tree();
        let old = root.join("m/old/a.jpg");
        write_image(&old, hours(9000));
        assert_eq!(
            sweep_screenshots_dir(&root, &policy, SystemTime::now()),
            SweepStats::default()
        );
        assert!(old.exists());
    }

    #[test]
    fn sweep_leaves_non_image_files_and_the_root_alone() {
        let (_guard, root) = tree();
        let note = root.join("monitor-0/old/notes.txt");
        let image = root.join("monitor-0/old/a.jpg");
        write_image(&note, hours(9000));
        write_image(&image, hours(9000));

        let stats = sweep_screenshots_dir(&root, &ScreenshotPolicy::default(), SystemTime::now());

        assert_eq!(stats.scanned, 1, "only image files are scanned");
        assert_eq!(stats.deleted, 1);
        assert!(note.exists(), "non-image files are never swept");
        assert!(root.is_dir(), "the screenshots root must survive");
    }

    #[test]
    fn sweep_removes_directories_that_it_emptied() {
        let (_guard, root) = tree();
        let image = root.join("monitor-0/2026-01-01/a.jpg");
        write_image(&image, hours(9000));

        let stats = sweep_screenshots_dir(&root, &ScreenshotPolicy::default(), SystemTime::now());

        assert_eq!(stats.deleted, 1);
        assert_eq!(stats.dirs_removed, 2, "date dir and monitor dir both empty");
        assert!(!root.join("monitor-0").exists());
        assert!(root.is_dir());
    }

    #[test]
    fn sweep_of_a_missing_directory_is_harmless() {
        let (_guard, root) = tree();
        let missing = root.join("does-not-exist");
        assert_eq!(
            sweep_screenshots_dir(&missing, &ScreenshotPolicy::default(), SystemTime::now()),
            SweepStats::default()
        );
    }

    #[test]
    fn policy_reads_from_storage_config() {
        let mut cfg = StorageConfig::default();
        assert_eq!(
            ScreenshotPolicy::from(&cfg),
            ScreenshotPolicy {
                delete_with_rotation: true,
                max_age_hours: 720,
            }
        );
        cfg.delete_screenshots_with_rotation = false;
        cfg.screenshot_max_age_hours = 24;
        assert_eq!(
            ScreenshotPolicy::from(&cfg),
            ScreenshotPolicy {
                delete_with_rotation: false,
                max_age_hours: 24,
            }
        );
    }
}
