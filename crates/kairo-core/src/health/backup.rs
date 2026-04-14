//! # Backup rotation
//!
//! Every night at 04:00 local time we zip the user's config, permissions,
//! automations, and semantic facts into `~/.kairo-backups/<date>/kairo-<date>.zip`.
//! Raw log + large models are intentionally excluded — they're recoverable
//! from ongoing perception, and a backup bundle should stay small.
//!
//! We keep the last 7 backups and delete older ones. The repair agent has
//! read access and can call `repair_rollback_config`.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveTime, TimeZone, Utc};
use tokio::sync::watch;
use zip::write::SimpleFileOptions;

/// Paths included in each backup, relative to the Kairo dev dir.
const INCLUDED_FILES: &[&str] = &[
    "config.toml",
    "automations.json",
    "permissions.toml",
    "semantic.sqlite",
    "orchestrator-system.md",
];

/// Default number of backups retained.
pub const DEFAULT_RETENTION: u32 = 7;

/// Default backup hour (local time, 24h).
pub const DEFAULT_BACKUP_HOUR: u32 = 4;

/// Summary of the most recent backup.
#[derive(Debug, Clone)]
pub struct BackupResult {
    pub path: PathBuf,
    pub date: DateTime<Utc>,
    pub bytes: u64,
    pub included: Vec<String>,
}

/// Produce a single backup now.
pub fn run_backup(dev_dir: &Path, backups_dir: &Path) -> Result<BackupResult> {
    std::fs::create_dir_all(backups_dir)
        .with_context(|| format!("create {}", backups_dir.display()))?;

    let now_utc = Utc::now();
    let date_str = now_utc.format("%Y-%m-%d").to_string();
    let target_dir = backups_dir.join(&date_str);
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("create {}", target_dir.display()))?;
    let zip_path = target_dir.join(format!("kairo-{date_str}.zip"));

    let file = File::create(&zip_path)
        .with_context(|| format!("create {}", zip_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut included = Vec::new();
    for rel in INCLUDED_FILES {
        let src = dev_dir.join(rel);
        if !src.exists() {
            continue;
        }
        zip.start_file(*rel, options)
            .with_context(|| format!("start file {rel}"))?;
        let mut input =
            File::open(&src).with_context(|| format!("open {}", src.display()))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf).ok();
        zip.write_all(&buf)
            .with_context(|| format!("write {rel}"))?;
        included.push(rel.to_string());
    }
    zip.finish().context("finish zip")?;

    let bytes = std::fs::metadata(&zip_path)?.len();
    Ok(BackupResult {
        path: zip_path,
        date: now_utc,
        bytes,
        included,
    })
}

/// Delete backups older than the most-recent `retention` entries.
pub fn prune_backups(backups_dir: &Path, retention: u32) -> Result<u32> {
    if !backups_dir.exists() {
        return Ok(0);
    }
    let mut entries: Vec<(String, PathBuf)> = std::fs::read_dir(backups_dir)?
        .filter_map(|r| r.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();

    // Dates are ISO `YYYY-MM-DD`, so lexicographic sort is chronological.
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let excess = entries.len().saturating_sub(retention as usize);
    let mut removed = 0u32;
    for (_, path) in entries.iter().take(excess) {
        if std::fs::remove_dir_all(path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Count retained backups (for the Health tab indicator).
pub fn count_backups(backups_dir: &Path) -> u32 {
    std::fs::read_dir(backups_dir)
        .map(|rd| {
            rd.filter_map(|r| r.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .count() as u32
        })
        .unwrap_or(0)
}

/// Most recent backup timestamp (derived from directory name).
pub fn latest_backup_ts(backups_dir: &Path) -> Option<DateTime<Utc>> {
    let mut names: Vec<String> = std::fs::read_dir(backups_dir)
        .ok()?
        .filter_map(|r| r.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let last = names.last()?;
    chrono::NaiveDate::parse_from_str(last, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(4, 0, 0))
        .map(|dt| dt.and_utc())
}

/// Spawn the nightly backup task. Fires once when the local clock hits
/// the configured hour.
pub fn spawn_nightly(
    dev_dir: PathBuf,
    backups_dir: PathBuf,
    hour: u32,
    retention: u32,
    mut shutdown: watch::Receiver<bool>,
    on_complete: impl Fn(BackupResult) + Send + 'static,
) {
    tokio::spawn(async move {
        loop {
            let delay = seconds_until_next_local(hour);
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(delay)) => {}
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    } else {
                        continue;
                    }
                }
            }
            match run_backup(&dev_dir, &backups_dir) {
                Ok(result) => {
                    if let Err(e) = prune_backups(&backups_dir, retention) {
                        tracing::warn!(
                            layer = "health",
                            component = "backup",
                            error = %e,
                            "Backup prune failed"
                        );
                    }
                    tracing::info!(
                        layer = "health",
                        component = "backup",
                        bytes = result.bytes,
                        "Nightly backup written"
                    );
                    on_complete(result);
                }
                Err(e) => {
                    tracing::error!(
                        layer = "health",
                        component = "backup",
                        error = %e,
                        "Nightly backup failed"
                    );
                }
            }
        }
    });
}

fn seconds_until_next_local(hour: u32) -> u64 {
    let now = Local::now();
    let today_target = now
        .date_naive()
        .and_time(NaiveTime::from_hms_opt(hour, 0, 0).unwrap_or_default());
    let today_target = Local
        .from_local_datetime(&today_target)
        .single()
        .unwrap_or(now);
    let target = if today_target <= now {
        today_target + ChronoDuration::days(1)
    } else {
        today_target
    };
    (target - now).num_seconds().max(60) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use tempfile::TempDir;

    fn write(path: &Path, contents: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn run_backup_includes_existing_files_only() {
        let tmp = TempDir::new().unwrap();
        let dev = tmp.path().join("dev");
        let backups = tmp.path().join("backups");
        write(&dev.join("config.toml"), "[screen]\ninterval_secs=3\n");
        write(&dev.join("automations.json"), "[]");

        let result = run_backup(&dev, &backups).unwrap();
        assert!(result.path.exists());
        assert!(result.included.contains(&"config.toml".to_string()));
        assert!(result.included.contains(&"automations.json".to_string()));
        assert!(!result.included.iter().any(|f| f == "permissions.toml"));
    }

    #[test]
    fn prune_keeps_most_recent_n() {
        let tmp = TempDir::new().unwrap();
        let backups = tmp.path().join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        for date in ["2026-04-01", "2026-04-02", "2026-04-03", "2026-04-04"] {
            std::fs::create_dir_all(backups.join(date)).unwrap();
        }
        let removed = prune_backups(&backups, 2).unwrap();
        assert_eq!(removed, 2);
        assert!(!backups.join("2026-04-01").exists());
        assert!(!backups.join("2026-04-02").exists());
        assert!(backups.join("2026-04-03").exists());
        assert!(backups.join("2026-04-04").exists());
    }

    #[test]
    fn prune_noop_when_under_retention() {
        let tmp = TempDir::new().unwrap();
        let backups = tmp.path().join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        std::fs::create_dir_all(backups.join("2026-04-01")).unwrap();
        let removed = prune_backups(&backups, 7).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn count_and_latest_match_filesystem() {
        let tmp = TempDir::new().unwrap();
        let backups = tmp.path().join("backups");
        std::fs::create_dir_all(backups.join("2026-04-01")).unwrap();
        std::fs::create_dir_all(backups.join("2026-04-05")).unwrap();
        assert_eq!(count_backups(&backups), 2);
        let latest = latest_backup_ts(&backups).unwrap();
        assert_eq!(latest.year(), 2026);
        assert_eq!(latest.month(), 4);
        assert_eq!(latest.day(), 5);
    }

    #[test]
    fn seconds_until_next_local_is_positive() {
        let secs = seconds_until_next_local(4);
        assert!(secs >= 60);
        assert!(secs <= 24 * 60 * 60);
    }
}
