//! # Surgical `config.toml` edits (spec §4.13)
//!
//! Two Context-page actions must survive a restart by changing the user's
//! `config.toml`: confirming/adding a project appends a
//! `[[projects.known]]` entry (spec §4.3), and flipping an honest toggle
//! writes `[privacy.toggles].<name>` (spec §4.1).
//!
//! Both go through this module rather than through
//! `runtime::persist_config`, which re-serialises the *whole*
//! [`crate::config::ContinuumConfig`] struct: that would rewrite every knob
//! the user had left at its default as an explicit value, and would silently
//! drop any key this build does not know about. Here the file is parsed as a
//! plain [`toml::Value`] document, exactly one key is touched, and the result
//! is written atomically (`.tmp` + rename).
//!
//! ## Failure policy
//!
//! The **table is authoritative**, config is only the boot seed (spec §4.3).
//! So when the file cannot be parsed — a user's hand-edit mid-save, a
//! half-written file — the config write is *skipped with a warning* and the
//! caller's database mutation stands. A confirmed project that fails to reach
//! `config.toml` is still confirmed; it is simply re-seeded from the Projects
//! table at next boot, which is where reconciliation reads it from anyway.
//!
//! **Comments are not preserved.** The `toml` crate's value model has no
//! room for them, so a rewrite drops them. That is why only these two
//! narrowly-scoped writers exist and why nothing else in the runtime edits
//! the file behind the user's back.
//!
//! # Layer
//!
//! Cross-layer control plane. Pure `std::fs` + `toml` — ungated.

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::ProjectConfigEntry;
use crate::context::intents::ToggleName;
use crate::runtime_control::RuntimeServiceName;

/// Reads `path` into a TOML document, or an empty document when the file
/// does not exist yet (first run seeds defaults on demand).
fn read_document(path: &Path) -> Result<toml::Table> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse::<toml::Table>()
            .with_context(|| format!("Failed to parse {} as TOML", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => {
            Err(anyhow::Error::new(e)).with_context(|| format!("Failed to read {}", path.display()))
        }
    }
}

/// Writes a TOML document atomically.
fn write_document(path: &Path, doc: &toml::Table) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(doc).context("Failed to serialize config document")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("Failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to finalize {}", path.display()))?;
    Ok(())
}

/// Returns `doc["<key>"]` as a mutable table, creating it when absent and
/// replacing it when the existing value is not a table (a corrupt scalar
/// where a section belongs must not block a privacy toggle from
/// persisting).
fn table_mut<'a>(doc: &'a mut toml::Table, key: &str) -> &'a mut toml::Table {
    let entry = doc
        .entry(key.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !entry.is_table() {
        *entry = toml::Value::Table(toml::Table::new());
    }
    entry.as_table_mut().expect("just ensured it is a table")
}

/// Appends (or replaces by id) a `[[projects.known]]` entry.
///
/// Entries live under `projects.known` rather than a bare `[[projects]]`
/// array because TOML cannot host both the `[projects]` knob table and a
/// `[[projects]]` array under one key (Task A4's note).
///
/// Idempotent: an entry whose `id` already exists is replaced in place, so
/// confirming the same project twice never grows the file.
pub fn upsert_known_project(path: &Path, entry: &ProjectConfigEntry) -> Result<()> {
    let mut doc = read_document(path)?;
    let projects = table_mut(&mut doc, "projects");
    let known = projects
        .entry("known".to_string())
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    if !known.is_array() {
        *known = toml::Value::Array(Vec::new());
    }
    let array = known.as_array_mut().expect("just ensured it is an array");

    let value = toml::Value::try_from(entry).context("Failed to encode project entry as TOML")?;
    let existing = array.iter().position(|v| {
        v.get("id")
            .and_then(|id| id.as_str())
            .map(|id| id == entry.id)
            .unwrap_or(false)
    });
    match existing {
        Some(index) => array[index] = value,
        None => array.push(value),
    }
    write_document(path, &doc)
}

/// Writes `[privacy.toggles].<name> = <value>`.
pub fn set_toggle(path: &Path, name: ToggleName, value: bool) -> Result<()> {
    let mut doc = read_document(path)?;
    let privacy = table_mut(&mut doc, "privacy");
    let toggles = table_mut(privacy, "toggles");
    toggles.insert(name.as_str().to_string(), toml::Value::Boolean(value));
    write_document(path, &doc)
}

/// Writes the persisted config key for one optional runtime service.
///
/// Privacy toggles and service enablement are deliberately separate: turning a
/// service on never overrides `pause_all` or a source privacy toggle.
pub fn set_runtime_service(path: &Path, service: RuntimeServiceName, enabled: bool) -> Result<()> {
    let mut doc = read_document(path)?;
    let (section, key) = match service {
        RuntimeServiceName::FileActivity => ("file_watcher", "enabled"),
        RuntimeServiceName::BackgroundActivity => ("process_watcher", "enabled"),
        RuntimeServiceName::TriageEvaluation => ("triage", "evaluation_enabled"),
    };
    table_mut(&mut doc, section).insert(key.to_string(), toml::Value::Boolean(enabled));
    write_document(path, &doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config;
    use tempfile::TempDir;

    fn entry(id: &str) -> ProjectConfigEntry {
        ProjectConfigEntry {
            id: id.to_string(),
            name: format!("Project {id}"),
            root_paths: vec!["D:/work/x".to_string()],
            repo: None,
            keywords: vec!["x".to_string()],
            zone: None,
        }
    }

    #[test]
    fn appends_a_project_to_an_absent_file_and_load_config_reads_it_back() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        upsert_known_project(&path, &entry("simcharts")).unwrap();

        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.projects.known.len(), 1);
        assert_eq!(cfg.projects.known[0].id, "simcharts");
        assert_eq!(cfg.projects.known[0].root_paths, vec!["D:/work/x"]);
    }

    #[test]
    fn appending_a_second_project_keeps_the_first() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        upsert_known_project(&path, &entry("one")).unwrap();
        upsert_known_project(&path, &entry("two")).unwrap();

        let cfg = load_config(&path).unwrap();
        let ids: Vec<&str> = cfg.projects.known.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["one", "two"]);
    }

    #[test]
    fn upserting_the_same_id_replaces_rather_than_duplicates() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        upsert_known_project(&path, &entry("one")).unwrap();
        let mut renamed = entry("one");
        renamed.name = "Renamed".into();
        upsert_known_project(&path, &renamed).unwrap();

        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.projects.known.len(), 1);
        assert_eq!(cfg.projects.known[0].name, "Renamed");
    }

    #[test]
    fn unrelated_keys_survive_a_project_append() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[triage]\nthreshold = 0.42\n\n[some_future_section]\nunknown_key = true\n",
        )
        .unwrap();
        upsert_known_project(&path, &entry("one")).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: toml::Table = raw.parse().unwrap();
        assert!(doc.contains_key("some_future_section"));
        assert_eq!(
            doc["triage"]["threshold"].as_float().unwrap(),
            0.42,
            "existing knobs must survive"
        );
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.projects.known.len(), 1);
    }

    #[test]
    fn set_toggle_writes_the_privacy_section_and_load_config_honours_it() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        set_toggle(&path, ToggleName::Mic, false).unwrap();
        set_toggle(&path, ToggleName::PauseAll, true).unwrap();

        let cfg = load_config(&path).unwrap();
        assert!(!cfg.privacy.toggles.mic);
        assert!(cfg.privacy.toggles.pause_all);
        // Untouched toggles keep their defaults.
        assert!(cfg.privacy.toggles.screen);
    }

    #[test]
    fn set_toggle_preserves_other_privacy_keys() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[privacy]\nscrub_emails = true\n\n[privacy.toggles]\ngit = false\n",
        )
        .unwrap();
        set_toggle(&path, ToggleName::Mic, false).unwrap();

        let cfg = load_config(&path).unwrap();
        assert!(cfg.privacy.scrub_emails);
        assert!(!cfg.privacy.toggles.git, "pre-existing toggle survives");
        assert!(!cfg.privacy.toggles.mic);
    }

    #[test]
    fn a_scalar_where_a_section_belongs_is_replaced_not_fatal() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "privacy = 3\n").unwrap();
        set_toggle(&path, ToggleName::Files, false).unwrap();
        let cfg = load_config(&path).unwrap();
        assert!(!cfg.privacy.toggles.files);
    }

    #[test]
    fn unparseable_config_is_an_error_the_caller_can_skip_on() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "this is [not toml").unwrap();
        assert!(set_toggle(&path, ToggleName::Mic, false).is_err());
        assert!(upsert_known_project(&path, &entry("one")).is_err());
        // The bad file is left exactly as it was — no half-write.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "this is [not toml");
    }

    #[test]
    fn set_runtime_service_persists_each_service_without_touching_privacy() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        set_toggle(&path, ToggleName::Files, false).unwrap();
        set_runtime_service(&path, RuntimeServiceName::FileActivity, true).unwrap();
        set_runtime_service(&path, RuntimeServiceName::BackgroundActivity, true).unwrap();
        set_runtime_service(&path, RuntimeServiceName::TriageEvaluation, false).unwrap();

        let cfg = load_config(&path).unwrap();
        assert!(cfg.file_watcher.enabled);
        assert!(cfg.process_watcher.enabled);
        assert!(!cfg.triage.evaluation_enabled);
        assert!(
            !cfg.privacy.toggles.files,
            "service control must not weaken privacy"
        );
    }

    #[test]
    fn writes_are_atomic_leaving_no_tmp_behind() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        set_toggle(&path, ToggleName::Screen, false).unwrap();
        assert!(!path.with_extension("toml.tmp").exists());
    }
}
