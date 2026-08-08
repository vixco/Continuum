//! Durable, local-only leases for pausing every observation source.
//!
//! The desktop writes this small control-plane record before queueing the
//! `pause_all` context intent. The runtime reads it at boot and while running,
//! so a timed privacy pause survives restarts and expires without the desktop
//! needing to stay open.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Days, Local, LocalResult, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// Filename below the Continuum data directory.
pub const OBSERVATION_PAUSE_FILE: &str = "observation-pause.json";

/// User-facing pause choices accepted by the trusted desktop boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationPausePreset {
    /// Pause for fifteen minutes.
    FifteenMinutes,
    /// Pause for one hour.
    OneHour,
    /// Pause for four hours.
    FourHours,
    /// Pause until 08:00 on the next local calendar day.
    UntilTomorrow,
    /// Pause until the user explicitly resumes observation.
    Indefinite,
}

/// Safe status returned to the desktop. It contains no observed user data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationPauseStatus {
    /// Whether the lease is active at the time it was read.
    pub paused: bool,
    /// UTC expiry for a timed pause, or `None` for indefinite/not paused.
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ObservationPauseRecord {
    version: u8,
    started_at: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
}

impl ObservationPauseRecord {
    fn status_at(&self, now: DateTime<Utc>) -> ObservationPauseStatus {
        let paused = self.until.is_none_or(|until| until > now);
        ObservationPauseStatus {
            paused,
            until: paused.then_some(self.until).flatten(),
        }
    }
}

/// Returns the durable privacy-pause file path for a data directory.
pub fn pause_path(data_dir: &Path) -> PathBuf {
    data_dir.join(OBSERVATION_PAUSE_FILE)
}

/// Reads the current lease. Missing files mean observation is not paused.
pub fn read_status(data_dir: &Path, now: DateTime<Utc>) -> Result<ObservationPauseStatus> {
    let path = pause_path(data_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ObservationPauseStatus {
                paused: false,
                until: None,
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()));
        }
    };
    let record: ObservationPauseRecord = serde_json::from_str(&raw)
        .with_context(|| format!("Invalid privacy pause record at {}", path.display()))?;
    Ok(record.status_at(now))
}

/// Creates or replaces a pause lease atomically.
pub fn pause(
    data_dir: &Path,
    preset: ObservationPausePreset,
    now: DateTime<Utc>,
) -> Result<ObservationPauseStatus> {
    let until = match preset {
        ObservationPausePreset::FifteenMinutes => Some(now + chrono::Duration::minutes(15)),
        ObservationPausePreset::OneHour => Some(now + chrono::Duration::hours(1)),
        ObservationPausePreset::FourHours => Some(now + chrono::Duration::hours(4)),
        ObservationPausePreset::UntilTomorrow => Some(tomorrow_at_eight(now)?),
        ObservationPausePreset::Indefinite => None,
    };
    let record = ObservationPauseRecord {
        version: 1,
        started_at: now,
        until,
    };
    write_record(data_dir, &record)?;
    Ok(record.status_at(now))
}

/// Writes a durable resume tombstone for a running/offline runtime to consume.
pub fn resume(data_dir: &Path) -> Result<ObservationPauseStatus> {
    let now = Utc::now();
    write_record(
        data_dir,
        &ObservationPauseRecord {
            version: 1,
            started_at: now,
            until: Some(now),
        },
    )?;
    Ok(ObservationPauseStatus {
        paused: false,
        until: None,
    })
}

/// Removes a pause/resume record after the runtime has applied it.
pub fn clear(data_dir: &Path) -> Result<()> {
    let path = pause_path(data_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to remove {}", path.display()));
        }
    }
    Ok(())
}

/// A corrupt record cannot be trusted to resume observation automatically.
/// Callers should fail closed and report the error through component health.
pub fn should_restart(data_dir: &Path) -> bool {
    read_status(data_dir, Utc::now()).is_err()
}

fn tomorrow_at_eight(now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let local_now = now.with_timezone(&Local);
    let tomorrow = local_now
        .date_naive()
        .checked_add_days(Days::new(1))
        .context("Local calendar overflow while calculating tomorrow")?;
    let naive = tomorrow.and_time(NaiveTime::from_hms_opt(8, 0, 0).expect("08:00 is valid"));
    let local = match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(first, second) => first.min(second),
        LocalResult::None => Local
            .from_local_datetime(&(naive + chrono::Duration::hours(1)))
            .earliest()
            .context("Tomorrow's local pause deadline does not exist")?,
    };
    Ok(local.with_timezone(&Utc))
}

fn write_record(data_dir: &Path, record: &ObservationPauseRecord) -> Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("Failed to create {}", data_dir.display()))?;
    let path = pause_path(data_dir);
    let temporary = path.with_extension("json.tmp");
    let previous = path.with_extension("json.previous");
    let bytes = serde_json::to_vec_pretty(record).context("Failed to encode privacy pause")?;
    std::fs::write(&temporary, bytes)
        .with_context(|| format!("Failed to write {}", temporary.display()))?;
    if path.exists() {
        let _ = std::fs::remove_file(&previous);
        std::fs::rename(&path, &previous).with_context(|| {
            format!(
                "Failed to stage existing privacy pause at {}",
                path.display()
            )
        })?;
    }
    if let Err(error) = std::fs::rename(&temporary, &path) {
        if previous.exists() {
            let _ = std::fs::rename(&previous, &path);
        }
        return Err(error).with_context(|| {
            format!(
                "Failed to atomically replace privacy pause {} -> {}",
                temporary.display(),
                path.display()
            )
        });
    }
    let _ = std::fs::remove_file(previous);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-09T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn timed_pause_is_active_then_expires() {
        let dir = tempfile::tempdir().unwrap();
        let status = pause(dir.path(), ObservationPausePreset::OneHour, now()).unwrap();
        assert!(status.paused);
        assert_eq!(status.until, Some(now() + chrono::Duration::hours(1)));
        assert!(
            !read_status(dir.path(), now() + chrono::Duration::hours(2))
                .unwrap()
                .paused
        );
    }

    #[test]
    fn indefinite_pause_only_ends_when_resumed() {
        let dir = tempfile::tempdir().unwrap();
        pause(dir.path(), ObservationPausePreset::Indefinite, now()).unwrap();
        assert!(
            read_status(dir.path(), now() + chrono::Duration::days(365))
                .unwrap()
                .paused
        );
        assert!(!resume(dir.path()).unwrap().paused);
        assert!(
            !read_status(dir.path(), Utc::now() + chrono::Duration::seconds(1))
                .unwrap()
                .paused
        );
        clear(dir.path()).unwrap();
        assert!(!pause_path(dir.path()).exists());
    }

    #[test]
    fn corrupt_record_fails_closed_for_callers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(pause_path(dir.path()), b"not json").unwrap();
        assert!(read_status(dir.path(), now()).is_err());
        assert!(should_restart(dir.path()));
    }
}
