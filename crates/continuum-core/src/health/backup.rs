//! # Backup rotation
//!
//! Every night at 04:00 local time we zip the user's config, permissions,
//! automations, and semantic facts into versioned archives under
//! `~/.continuum-backups/<date>/`.
//! Raw log + large models are intentionally excluded — they're recoverable
//! from ongoing perception, and a backup bundle should stay small.
//!
//! Retention defaults to the last 7 archives and is configurable. The safe
//! Health repair session cannot roll back configuration; the separate guarded
//! rollback path validates the archive before use.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use zip::write::SimpleFileOptions;

/// Paths included in each backup, relative to the Continuum dev dir.
const INCLUDED_FILES: &[&str] = &[
    "config.toml",
    "automations.json",
    "permissions.toml",
    "semantic.sqlite",
    "orchestrator-system.md",
];
/// Configuration is text and should remain small; cap it to avoid malicious
/// or accidental archive expansion during rollback.
pub const MAX_CONFIG_BYTES: u64 = 10 * 1024 * 1024;

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

/// Integrity metadata embedded in every backup archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupManifest {
    schema_version: u32,
    created_at: DateTime<Utc>,
    files: Vec<BackupManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackupManifestFile {
    path: String,
    bytes: u64,
}

struct TempBackupGuard {
    path: PathBuf,
    published: bool,
}

impl Drop for TempBackupGuard {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.path);
        }
    }
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
    let version = now_utc.format("%Y%m%dT%H%M%S%6fZ");
    let nonce = uuid::Uuid::new_v4();
    let zip_path = target_dir.join(format!("continuum-{version}-{nonce}.zip"));
    let temp_path = target_dir.join(format!(".continuum-{version}-{nonce}.tmp"));
    let mut temp_guard = TempBackupGuard {
        path: temp_path.clone(),
        published: false,
    };

    let file =
        File::create(&temp_path).with_context(|| format!("create {}", temp_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut included = Vec::new();
    let mut manifest_files = Vec::new();
    for rel in INCLUDED_FILES {
        let src = dev_dir.join(rel);
        if !src.exists() {
            continue;
        }
        zip.start_file(*rel, options)
            .with_context(|| format!("start file {rel}"))?;
        let buf = if *rel == "config.toml" {
            read_consistent_config(&src)?
        } else {
            let mut input = File::open(&src).with_context(|| format!("open {}", src.display()))?;
            let mut buf = Vec::new();
            input
                .read_to_end(&mut buf)
                .with_context(|| format!("read {}", src.display()))?;
            buf
        };
        if *rel == "config.toml" && buf.len() as u64 > MAX_CONFIG_BYTES {
            anyhow::bail!("config.toml exceeds the 10 MiB backup safety limit");
        }
        zip.write_all(&buf)
            .with_context(|| format!("write {rel}"))?;
        included.push(rel.to_string());
        manifest_files.push(BackupManifestFile {
            path: rel.to_string(),
            bytes: buf.len() as u64,
        });
    }
    let manifest = BackupManifest {
        schema_version: 1,
        created_at: now_utc,
        files: manifest_files,
    };
    zip.start_file("manifest.json", options)
        .context("start backup manifest")?;
    zip.write_all(
        serde_json::to_string_pretty(&manifest)
            .context("serialize backup manifest")?
            .as_bytes(),
    )
    .context("write backup manifest")?;
    let finished = zip.finish().context("finish zip")?;
    finished.sync_all().context("sync backup archive")?;

    verify_backup(&temp_path).context("verify newly-written backup")?;
    std::fs::rename(&temp_path, &zip_path).with_context(|| {
        format!(
            "publish verified backup {} -> {}",
            temp_path.display(),
            zip_path.display()
        )
    })?;
    temp_guard.published = true;

    let bytes = std::fs::metadata(&zip_path)?.len();
    Ok(BackupResult {
        path: zip_path,
        date: now_utc,
        bytes,
        included,
    })
}

fn read_consistent_config(path: &Path) -> Result<Vec<u8>> {
    for _ in 0..3 {
        let before = std::fs::metadata(path)
            .with_context(|| format!("inspect {} before backup", path.display()))?;
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let confirmation =
            std::fs::read(path).with_context(|| format!("re-read {}", path.display()))?;
        let after = std::fs::metadata(path)
            .with_context(|| format!("inspect {} after backup", path.display()))?;
        if before.len() == after.len()
            && before.modified().ok() == after.modified().ok()
            && bytes.len() as u64 == after.len()
            && bytes == confirmation
        {
            return Ok(bytes);
        }
        std::thread::yield_now();
    }
    anyhow::bail!(
        "{} changed repeatedly while backing up; mutation blocked",
        path.display()
    )
}

/// Re-open an archive and validate its manifest, entries and ZIP CRCs.
pub fn verify_backup(path: &Path) -> Result<()> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("read backup zip")?;
    let manifest: BackupManifest = {
        let mut entry = archive
            .by_name("manifest.json")
            .context("manifest.json missing from backup")?;
        if entry.size() > 1024 * 1024 {
            anyhow::bail!("backup manifest exceeds 1 MiB");
        }
        let mut text = String::new();
        entry
            .read_to_string(&mut text)
            .context("read backup manifest")?;
        serde_json::from_str(&text).context("parse backup manifest")?
    };
    if manifest.schema_version != 1 {
        anyhow::bail!(
            "unsupported backup manifest version {}",
            manifest.schema_version
        );
    }
    let mut seen = HashSet::new();
    for expected in manifest.files {
        if !INCLUDED_FILES.contains(&expected.path.as_str()) {
            anyhow::bail!("unexpected backup entry in manifest: {}", expected.path);
        }
        if !seen.insert(expected.path.clone()) {
            anyhow::bail!("duplicate backup entry in manifest: {}", expected.path);
        }
        if expected.path == "config.toml" && expected.bytes > MAX_CONFIG_BYTES {
            anyhow::bail!("config.toml exceeds the 10 MiB backup safety limit");
        }
        let mut entry = archive
            .by_name(&expected.path)
            .with_context(|| format!("{} missing from backup", expected.path))?;
        if entry.size() != expected.bytes {
            anyhow::bail!(
                "backup entry {} declared length mismatch: expected {}, got {}",
                expected.path,
                expected.bytes,
                entry.size()
            );
        }
        let bytes = std::io::copy(&mut entry, &mut std::io::sink())
            .with_context(|| format!("read {} from backup", expected.path))?;
        if bytes != expected.bytes {
            anyhow::bail!(
                "backup entry {} length mismatch: expected {}, got {}",
                expected.path,
                expected.bytes,
                bytes
            );
        }
    }
    if archive.len() != seen.len() + 1 {
        anyhow::bail!("backup contains entries not declared in its manifest");
    }
    Ok(())
}

/// Delete backups older than the most-recent `retention` entries.
pub fn prune_backups(backups_dir: &Path, retention: u32) -> Result<u32> {
    if !backups_dir.exists() {
        return Ok(0);
    }
    let mut entries = list_backup_files(backups_dir)?;
    entries.sort();
    let excess = entries.len().saturating_sub(retention as usize);
    let mut removed = 0u32;
    for path in entries.iter().take(excess) {
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("prune backup {}", path.display()));
            }
        }
        if let Some(parent) = path.parent() {
            let is_empty = std::fs::read_dir(parent)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false);
            if is_empty {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
    Ok(removed)
}

/// Count retained backups (for the Health tab indicator).
pub fn count_backups(backups_dir: &Path) -> u32 {
    list_backup_files(backups_dir)
        .map(|entries| entries.len() as u32)
        .unwrap_or(0)
}

/// Read backup timestamp and count without converting filesystem errors into
/// a misleading zero-value UI status.
pub fn backup_status(backups_dir: &Path) -> Result<(Option<DateTime<Utc>>, u32)> {
    let mut files = list_backup_files(backups_dir)?;
    files.sort();
    let latest = match files.last() {
        Some(path) => Some(DateTime::<Utc>::from(
            std::fs::metadata(path)
                .with_context(|| format!("inspect {}", path.display()))?
                .modified()
                .with_context(|| format!("read modified time for {}", path.display()))?,
        )),
        None => None,
    };
    Ok((latest, files.len() as u32))
}

/// Most recent backup timestamp (derived from directory name).
pub fn latest_backup_ts(backups_dir: &Path) -> Option<DateTime<Utc>> {
    let mut files = list_backup_files(backups_dir).ok()?;
    files.sort();
    let modified = std::fs::metadata(files.last()?).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified))
}

/// Find the newest verified backup inside a strict `YYYY-MM-DD` directory.
pub fn latest_backup_for_date(backups_dir: &Path, date: &str) -> Result<PathBuf> {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .with_context(|| format!("invalid backup date {date:?}; expected YYYY-MM-DD"))?;
    let date_dir = backups_dir.join(date);
    let mut files = Vec::new();
    for entry in
        std::fs::read_dir(&date_dir).with_context(|| format!("read {}", date_dir.display()))?
    {
        let entry = entry.with_context(|| format!("read entry in {}", date_dir.display()))?;
        let path = entry.path();
        if entry
            .file_type()
            .with_context(|| format!("inspect {}", path.display()))?
            .is_file()
            && path.extension().and_then(|ext| ext.to_str()) == Some("zip")
        {
            files.push(path);
        }
    }
    files.sort();
    let path = files
        .pop()
        .with_context(|| format!("no backup archive found for {date}"))?;
    verify_backup(&path)?;
    Ok(path)
}

fn list_backup_files(backups_dir: &Path) -> Result<Vec<PathBuf>> {
    if !backups_dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for date_entry in std::fs::read_dir(backups_dir)? {
        let date_entry = date_entry?;
        if !date_entry.file_type()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(date_entry.path())? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("zip")
            {
                files.push(path);
            }
        }
    }
    Ok(files)
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
        verify_backup(&result.path).unwrap();
    }

    #[test]
    fn repeated_backups_are_versioned_and_counted() {
        let tmp = TempDir::new().unwrap();
        let dev = tmp.path().join("dev");
        let backups = tmp.path().join("backups");
        write(&dev.join("config.toml"), "[screen]\ninterval_secs=3\n");
        let first = run_backup(&dev, &backups).unwrap();
        let second = run_backup(&dev, &backups).unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(count_backups(&backups), 2);
    }

    #[test]
    fn verification_rejects_corrupted_archive() {
        let tmp = TempDir::new().unwrap();
        let dev = tmp.path().join("dev");
        let backups = tmp.path().join("backups");
        write(&dev.join("config.toml"), "[screen]\ninterval_secs=3\n");
        let backup = run_backup(&dev, &backups).unwrap();
        std::fs::write(&backup.path, b"not a zip").unwrap();
        assert!(verify_backup(&backup.path).is_err());
    }

    #[test]
    fn verification_rejects_undeclared_archive_entries() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("tampered.zip");
        let file = File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let manifest = BackupManifest {
            schema_version: 1,
            created_at: Utc::now(),
            files: Vec::new(),
        };
        zip.start_file("manifest.json", options).unwrap();
        zip.write_all(serde_json::to_string(&manifest).unwrap().as_bytes())
            .unwrap();
        zip.start_file("unexpected.txt", options).unwrap();
        zip.write_all(b"not allowlisted").unwrap();
        zip.finish().unwrap();
        assert!(verify_backup(&path).is_err());
    }

    #[test]
    fn prune_keeps_most_recent_n() {
        let tmp = TempDir::new().unwrap();
        let backups = tmp.path().join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        for date in ["2026-04-01", "2026-04-02", "2026-04-03", "2026-04-04"] {
            let dir = backups.join(date);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("continuum-{date}.zip")), "fixture").unwrap();
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
        std::fs::write(backups.join("2026-04-01/continuum.zip"), "fixture").unwrap();
        let removed = prune_backups(&backups, 7).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn count_and_latest_match_filesystem() {
        let tmp = TempDir::new().unwrap();
        let backups = tmp.path().join("backups");
        write(&dev_for_count(&backups, "2026-04-01"), "fixture");
        write(&dev_for_count(&backups, "2026-04-05"), "fixture");
        assert_eq!(count_backups(&backups), 2);
        let latest = latest_backup_ts(&backups).unwrap();
        assert!(latest.year() >= 2026);
        let (strict_latest, strict_count) = backup_status(&backups).unwrap();
        assert_eq!(strict_count, 2);
        assert!(strict_latest.is_some());
    }

    #[test]
    fn seconds_until_next_local_is_positive() {
        let secs = seconds_until_next_local(4);
        assert!(secs >= 60);
        assert!(secs <= 24 * 60 * 60);
    }

    fn dev_for_count(backups: &Path, date: &str) -> PathBuf {
        backups.join(date).join(format!("continuum-{date}.zip"))
    }
}
