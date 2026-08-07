//! # The committed evaluation fixture (context engine spec §9, Task C6)
//!
//! A **synthetic**, hand-authored twenty minutes of context-engine input,
//! plus the label sidecar the recall and precision benches score against.
//! Both files live in `crates/continuum-core/benches/data/`:
//!
//! - `context-20min.jsonl` — the [`crate::bench::record`] JSONL the replay
//!   harness reads (post-privacy frames + collector events, relative `t_ms`).
//! - `context-20min.labels.json` — ten checkpoints of ground truth:
//!   `[{ t_ms, expected: { project, goal, task, blocker, last_action } }]`.
//!   A `null` field means "the narrative asserts nothing here" and is not
//!   scored — recall is measured only over the checkpoints that label a
//!   field.
//!
//! **No real recording is ever committed** (see [`crate::bench::record`]).
//! Everything here is invented: the projects, the paths, the error text,
//! the commit subject. The narrative is scripted so each bench has a
//! target it can actually fail on.
//!
//! ## The narrative (t in seconds)
//!
//! | t | what happens | which bench cares |
//! |---|---|---|
//! | 0–230 | editing `continuum` in VS Code; file + focus events | project recall, memory precision |
//! | 240–530 | **the build-failure loop**: 30 frames with an error visible, each with a *different* summary | dedupe (spec §4.6: screen/audio summaries are not keyed) |
//! | 540–590 | the build goes green; a git commit lands | recall (`last_action`), memory precision |
//! | 600–770 | **idle gap** (`idle_seconds` ≥ 300) | §4.11: no inference while idle |
//! | 780–1010 | **project switch** to `simcharts`; branch switch, file events, then a second, *distinct* error loop | project recall, dedupe distinct-key loss |
//! | 1020–1050 | a **voice command** | recall (`last_action`) |
//! | 1060–1090 | a `never_observe` sentinel frame and a `local_only` private-browsing frame | redaction, zone propagation |
//! | 1100–1190 | back to `continuum`; tests pass | recall (project switch back) |
//!
//! ## Reproducibility
//!
//! [`synthetic_narrative`] and [`synthetic_labels`] are the source of
//! truth; the committed files are their serialization. `cargo run --bin
//! continuum-context-bench -- --write-fixture` regenerates them, and
//! [`tests::committed_fixture_matches_the_generator`] fails if the two
//! ever drift. Reviewers should read the generator; the JSONL is committed
//! so the benches are reproducible without running it.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::bench::record::{self, RecordLine};
use crate::config::{ProjectConfigEntry, ProjectsConfig};
use crate::memory::events::{
    ContextEvent, EventSensitivity, EventSource, EventType, COLLECTOR_EVENT_IMPORTANCE,
};
use crate::senses::types::{
    AudioObservation, ContextObservation, PerceptionFrame, ScreenObservation,
};

/// File name of the committed fixture.
pub const FIXTURE_FILE: &str = "context-20min.jsonl";

/// File name of the committed label sidecar.
pub const LABELS_FILE: &str = "context-20min.labels.json";

/// The wall-clock instant `t_ms = 0` maps to when a bench replays the
/// fixture. Fixed, so every run of every bench sees identical timestamps.
pub const FIXTURE_BASE: &str = "2026-08-05T09:00:00Z";

/// Total scripted span in milliseconds (twenty minutes).
pub const FIXTURE_SPAN_MS: i64 = 20 * 60 * 1_000;

/// One labeled checkpoint of ground truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Offset into the fixture, matching the `t_ms` axis of the JSONL.
    pub t_ms: i64,
    /// What the narrative asserts was true at that moment.
    pub expected: Expected,
}

/// Ground truth for one checkpoint. Every field is optional: `None` means
/// "the narrative asserts nothing", and unasserted fields are excluded
/// from the recall denominator rather than counted as misses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Expected {
    /// Resolved project id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The larger goal a human would name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// The concrete task in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// What is in the way, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
    /// The most recent thing that happened / was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action: Option<String>,
}

/// The directory holding the committed fixture.
///
/// Resolved from `CARGO_MANIFEST_DIR` of **this crate**, so a bench binary
/// in any crate and a unit test run from any working directory all find
/// the same files. Falls back to the path relative to the workspace root
/// when the compiled-in directory no longer exists (an installed binary).
pub fn fixture_dir() -> PathBuf {
    let compiled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/data");
    if compiled.is_dir() {
        return compiled;
    }
    PathBuf::from("crates/continuum-core/benches/data")
}

/// Path of the committed fixture JSONL.
pub fn fixture_path() -> PathBuf {
    fixture_dir().join(FIXTURE_FILE)
}

/// Path of the committed label sidecar.
pub fn labels_path() -> PathBuf {
    fixture_dir().join(LABELS_FILE)
}

/// The instant `t_ms = 0` maps to.
pub fn fixture_base() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 5, 9, 0, 0)
        .single()
        .expect("FIXTURE_BASE is a valid instant")
}

/// Reads a fixture JSONL from disk.
pub fn load_fixture(path: &Path) -> anyhow::Result<Vec<RecordLine>> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
    record::parse_jsonl(&text).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
}

/// Reads a label sidecar from disk.
pub fn load_labels(path: &Path) -> anyhow::Result<Vec<Checkpoint>> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
}

/// Writes the generated fixture + labels into `dir`, creating it if
/// needed. Used by `continuum-context-bench --write-fixture`.
pub fn write_fixture(dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let jsonl = record::to_jsonl(&FIXTURE_HEADER, &synthetic_narrative())?;
    let fixture = dir.join(FIXTURE_FILE);
    std::fs::write(&fixture, jsonl)?;

    let labels = dir.join(LABELS_FILE);
    let mut encoded = serde_json::to_string_pretty(&synthetic_labels())?;
    encoded.push('\n');
    std::fs::write(&labels, encoded)?;
    Ok((fixture, labels))
}

/// Provenance header written into the fixture JSONL.
const FIXTURE_HEADER: [&str; 4] = [
    "Continuum context-engine evaluation fixture (spec §9, Task C6).",
    "SYNTHETIC — hand-authored, contains no real observation. Generated by",
    "continuum_core::bench::fixture::synthetic_narrative (regenerate with",
    "`cargo run --bin continuum-context-bench -- --write-fixture`).",
];

/// The projects the fixture's window titles resolve to. Benches build
/// their [`crate::context::project::ProjectResolver`] from this so the
/// resolution tiers under test are the real ones (spec §4.3 tier 2:
/// an editor title segment equal to the project name).
pub fn fixture_projects() -> ProjectsConfig {
    ProjectsConfig {
        // Off: the fixture's titles carry no absolute paths, and a bench
        // must never propose a project from whatever happens to exist on
        // the machine running it.
        auto_discover: false,
        switch_min_secs: 20,
        discover_min_secs: 30,
        known: vec![
            ProjectConfigEntry {
                id: "continuum".to_string(),
                name: "continuum".to_string(),
                root_paths: vec!["D:\\Continuum\\Continuum-main".to_string()],
                repo: None,
                keywords: vec!["continuum".to_string()],
                zone: None,
            },
            ProjectConfigEntry {
                id: "simcharts".to_string(),
                name: "simcharts".to_string(),
                root_paths: vec!["D:\\Projects\\SimCharts".to_string()],
                repo: None,
                keywords: vec!["simcharts".to_string()],
                zone: None,
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Narrative construction
// ---------------------------------------------------------------------------

/// Seconds between scripted frames (the fixture's frame cadence).
const FRAME_INTERVAL_SECS: i64 = 10;

/// Builder state: collects lines and hands out deterministic frame ids.
struct Script {
    lines: Vec<RecordLine>,
    next_id: u128,
}

impl Script {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            next_id: 1,
        }
    }

    fn frame_id(&mut self) -> Uuid {
        let id = Uuid::from_u128(self.next_id);
        self.next_id += 1;
        id
    }

    /// A scripted frame. `ts` inside the frame is the rebased instant so
    /// the file is self-consistent; the replay recomputes it from `t_ms`
    /// anyway.
    #[allow(clippy::too_many_arguments)]
    fn frame(
        &mut self,
        t_secs: i64,
        process: &str,
        title: &str,
        description: &str,
        has_error: bool,
        idle_seconds: u64,
        salience: f32,
        transcript: Option<&str>,
    ) {
        let ts = fixture_base() + Duration::seconds(t_secs);
        let id = self.frame_id();
        let frame = PerceptionFrame {
            id,
            ts,
            screen: ScreenObservation {
                description: description.to_string(),
                world_compact: None,
                foreground_app: process.to_string(),
                has_error_visible: has_error,
                confidence: if description.is_empty() { 0.0 } else { 0.8 },
                screenshot_path: None,
                ts,
            },
            audio: transcript.map(|text| AudioObservation {
                transcript: text.to_string(),
                language: "nl".to_string(),
                duration_ms: 1_800,
                confidence: 0.9,
                ts,
            }),
            context: ContextObservation {
                foreground_window_title: title.to_string(),
                foreground_process_name: process.to_string(),
                idle_seconds,
                in_call: false,
                pid: None,
                exe_path: None,
                active_since_secs: 0,
                monitor_id: Some("display-1".to_string()),
                privacy: None,
                ts,
            },
            salience_hint: salience,
        };
        self.lines.push(RecordLine::frame(t_secs * 1_000, frame));
    }

    /// A scripted collector event (the kind a watcher pushes into the
    /// events channel — already scrubbed, already project-stamped where
    /// the producer knows its project).
    #[allow(clippy::too_many_arguments)]
    fn event(
        &mut self,
        t_secs: i64,
        source: EventSource,
        event_type: EventType,
        application: &str,
        window_title: &str,
        project: Option<&str>,
        summary: &str,
        raw_reference: Option<&str>,
    ) {
        let event = ContextEvent {
            ts: fixture_base() + Duration::seconds(t_secs),
            source,
            application: application.to_string(),
            window_title: window_title.to_string(),
            project_id: project.map(str::to_string),
            event_type,
            summary: summary.to_string(),
            importance: COLLECTOR_EVENT_IMPORTANCE,
            confidence: 1.0,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference: raw_reference.map(str::to_string),
        };
        self.lines.push(RecordLine::event(t_secs * 1_000, event));
    }

    fn finish(mut self) -> Vec<RecordLine> {
        // A recording is chronological; the generator interleaves phases,
        // so sort by t_ms and keep frames before events at the same
        // instant (the frame loop produces the frame, the collectors
        // react to it). `sort_by_key` is stable, so the within-instant
        // order is the insertion order of the two passes below.
        self.lines
            .sort_by_key(|line| (line.t_ms, line.kind() == "event"));
        self.lines
    }
}

/// VS Code window title for a file in a project (the spec §4.3 tier-2
/// editor pattern: `file — folder — app`).
fn code_title(file: &str, project: &str) -> String {
    format!("{file} — {project} — Visual Studio Code")
}

/// The scripted twenty minutes. See the module docs for the narrative.
pub fn synthetic_narrative() -> Vec<RecordLine> {
    let mut s = Script::new();

    // --- Phase 1: editing continuum (0–230) ---------------------------
    let editing_title = code_title("events.rs", "continuum");
    for t in (0..=230).step_by(FRAME_INTERVAL_SECS as usize) {
        s.frame(
            t as i64,
            "Code.exe",
            &editing_title,
            "VS Code with the events writer open; no errors visible",
            false,
            (t as u64) % 40,
            0.10,
            None,
        );
    }
    s.event(
        30,
        EventSource::File,
        EventType::FileModified,
        "",
        "",
        Some("continuum"),
        "crates/continuum-core/src/memory/events.rs",
        None,
    );
    s.event(
        90,
        EventSource::File,
        EventType::FileModified,
        "",
        "",
        Some("continuum"),
        "crates/continuum-core/src/memory/events.rs",
        None,
    );
    s.event(
        150,
        EventSource::File,
        EventType::FileModified,
        "",
        "",
        Some("continuum"),
        "crates/continuum-core/src/memory/raw_log.rs",
        None,
    );
    s.event(
        230,
        EventSource::Window,
        EventType::FocusSwitch,
        "Code.exe",
        &editing_title,
        Some("continuum"),
        "Code.exe → WindowsTerminal.exe after 230s",
        None,
    );

    // --- Phase 2: the build-failure loop (240–530) --------------------
    //
    // Thirty frames, each with a *different* error line. Spec §4.6 keys
    // classified screen events on (source, type, project, application)
    // and deliberately NOT on the summary, so all thirty must collapse
    // into one row — that is what the dedupe bench measures.
    let build_title = "continuum — cargo build — Windows Terminal";
    //
    // The six variants all report the *same* compile failure with
    // different detail, which is what a real loop looks like: the summary
    // is unstable, the situation is not. That is exactly why §4.6 keys on
    // the situation.
    let build_errors = [
        "cargo build failed: mismatched types at crates/continuum-core/src/memory/events.rs:214",
        "cargo build failed: mismatched types, expected EventType found EventSource (events.rs:214)",
        "cargo build failed: mismatched types at crates/continuum-core/src/memory/events.rs:311",
        "cargo build failed: 2 errors emitted, mismatched types in continuum-core",
        "cargo build failed: mismatched types, expected ContextEvent found &ContextEvent",
        "cargo build failed: mismatched types in the dedupe key builder",
    ];
    for (index, t) in (240..=530)
        .step_by(FRAME_INTERVAL_SECS as usize)
        .enumerate()
    {
        s.frame(
            t as i64,
            "WindowsTerminal.exe",
            build_title,
            build_errors[index % build_errors.len()],
            true,
            0,
            0.60,
            None,
        );
    }
    s.event(
        300,
        EventSource::Git,
        EventType::DirtyChange,
        "git",
        "",
        Some("continuum"),
        "working tree dirty: 3 modified",
        None,
    );

    // --- Phase 3: green build + commit (540–590) ----------------------
    for t in (540..=590).step_by(FRAME_INTERVAL_SECS as usize) {
        s.frame(
            t as i64,
            "WindowsTerminal.exe",
            build_title,
            "cargo build finished: 0 errors, 41 warnings",
            false,
            0,
            0.50,
            None,
        );
    }
    s.event(
        560,
        EventSource::Git,
        EventType::Commit,
        "git",
        "",
        Some("continuum"),
        "fix(core): stamp project ids on classified events",
        // A full 40-char git OID. Structured field, exempt from the
        // secret scrubbers by construction (spec §4.1) — the redaction
        // bench asserts it survives end to end.
        Some("9f1c2ba7de4055c8e1a37b2d6f480c9a5e73bd12"),
    );

    // --- Phase 4: idle gap (600–770) ----------------------------------
    //
    // idle_seconds stays above `[performance] idle_pause_after_secs`
    // (300) for the whole gap, so spec §4.11 forbids session inference
    // here and the replay asserts none ran.
    for (index, t) in (600..=770)
        .step_by(FRAME_INTERVAL_SECS as usize)
        .enumerate()
    {
        s.frame(
            t as i64,
            "Code.exe",
            &editing_title,
            "",
            false,
            310 + (index as u64) * FRAME_INTERVAL_SECS as u64,
            0.0,
            None,
        );
    }
    s.event(
        600,
        EventSource::System,
        EventType::IdleStart,
        "",
        "",
        None,
        "user idle",
        None,
    );
    s.event(
        780,
        EventSource::System,
        EventType::IdleEnd,
        "",
        "",
        None,
        "user returned",
        None,
    );

    // --- Phase 5: project switch to simcharts (780–1010) --------------
    let charts_title = code_title("Dashboard.tsx", "simcharts");
    for t in (780..=930).step_by(FRAME_INTERVAL_SECS as usize) {
        s.frame(
            t as i64,
            "Code.exe",
            &charts_title,
            "VS Code editing the SimCharts dashboard component",
            false,
            (t as u64) % 30,
            0.20,
            None,
        );
    }
    s.event(
        820,
        EventSource::Git,
        EventType::BranchSwitch,
        "git",
        "",
        Some("simcharts"),
        "branch main → feature/chart-legend",
        None,
    );
    s.event(
        850,
        EventSource::File,
        EventType::FileModified,
        "",
        "",
        Some("simcharts"),
        "src/components/Dashboard.tsx",
        None,
    );
    s.event(
        900,
        EventSource::File,
        EventType::FileModified,
        "",
        "",
        Some("simcharts"),
        "src/components/Legend.tsx",
        None,
    );

    // A second, DISTINCT error loop: same event type, different project
    // and application, so it must occupy its own dedupe key (the "no
    // distinct-event loss" half of the dedupe bench).
    let ts_errors = [
        "TypeScript error: ChartProps has no property series (src/components/Chart.tsx:81)",
        "TypeScript error: ChartProps is missing the legend property (src/components/Legend.tsx:42)",
        "TypeScript error: ChartProps series expects SeriesId, found string",
    ];
    for (index, t) in (940..=1010)
        .step_by(FRAME_INTERVAL_SECS as usize)
        .enumerate()
    {
        s.frame(
            t as i64,
            "Code.exe",
            &charts_title,
            ts_errors[index % ts_errors.len()],
            true,
            0,
            0.60,
            None,
        );
    }

    // --- Phase 6: voice command (1020–1050) ---------------------------
    s.frame(
        1020,
        "Code.exe",
        &charts_title,
        "VS Code editing the SimCharts dashboard component",
        false,
        0,
        0.40,
        Some("ga door met de dashboard tests"),
    );
    s.event(
        1020,
        EventSource::Voice,
        EventType::VoiceCommand,
        "",
        "",
        Some("simcharts"),
        "ga door met de dashboard tests",
        None,
    );
    for t in (1030..=1050).step_by(FRAME_INTERVAL_SECS as usize) {
        s.frame(
            t as i64,
            "Code.exe",
            &charts_title,
            "VS Code editing the SimCharts dashboard component",
            false,
            0,
            0.20,
            None,
        );
    }

    // --- Phase 7: privacy edges (1060–1090) ---------------------------
    //
    // Two frames of the §4.1 `never_observe` sentinel (no title, no
    // caption, `[excluded]` process) and two frames of a `local_only`
    // private-browsing window. Between them they prove that an excluded
    // window produces no event at all and a local_only one produces an
    // event tagged `local_only`.
    for t in (1060..=1070).step_by(FRAME_INTERVAL_SECS as usize) {
        s.frame(
            t as i64,
            crate::senses::privacy::EXCLUDED_PROCESS,
            crate::senses::privacy::EXCLUDED_TITLE,
            "",
            false,
            0,
            0.20,
            None,
        );
    }
    for t in (1080..=1090).step_by(FRAME_INTERVAL_SECS as usize) {
        s.frame(
            t as i64,
            "msedge.exe",
            "InPrivate — Search — Microsoft Edge",
            "A private browsing window is open",
            false,
            0,
            0.20,
            None,
        );
    }

    // --- Phase 8: back to continuum, tests green (1100–1190) ----------
    for t in (1100..=1140).step_by(FRAME_INTERVAL_SECS as usize) {
        s.frame(
            t as i64,
            "Code.exe",
            &editing_title,
            "VS Code with the events writer open; no errors visible",
            false,
            0,
            0.20,
            None,
        );
    }
    for t in (1150..=1190).step_by(FRAME_INTERVAL_SECS as usize) {
        s.frame(
            t as i64,
            "WindowsTerminal.exe",
            build_title,
            "cargo test finished: 214 passed, 0 failed",
            false,
            0,
            0.50,
            None,
        );
    }
    s.event(
        1160,
        EventSource::File,
        EventType::FileModified,
        "",
        "",
        Some("continuum"),
        "crates/continuum-core/src/memory/events.rs",
        None,
    );

    s.finish()
}

/// The ten labeled checkpoints. See the module docs for the `null`
/// (unasserted, unscored) convention.
///
/// The texts are **paraphrases** of what the narrative asserts, not copies
/// of any summary the pipeline produces: the recall matcher
/// ([`crate::bench::metrics::field_matches`]) is a lenient token-stem
/// overlap precisely so a label can read like something a human would say.
/// The one deliberate exception is the voice utterance — a user's own
/// words *are* the ground truth, so `last_action` quotes them.
///
/// Two conventions keep the labels honest rather than fitted:
///
/// - A field is labeled only where the narrative genuinely asserts it. The
///   engine holds a stale `last_error` after the build goes green, for
///   instance, so `blocker` is labeled during and just after the failure —
///   where the state is right — and left `null` once the narrative has
///   moved on. Labeling it there would be scoring a behaviour the spec does
///   not define.
/// - Checkpoints sit *between* scripted lines (odd offsets), so "what was
///   believed at t" is never ambiguous about whether the line at exactly
///   `t` had been applied.
pub fn synthetic_labels() -> Vec<Checkpoint> {
    vec![
        // Settled into continuum; only file events so far, nothing inferred.
        Checkpoint {
            t_ms: 185_000,
            expected: Expected {
                project: Some("continuum".to_string()),
                last_action: Some("modified raw_log.rs in the continuum core crate".to_string()),
                ..Expected::default()
            },
        },
        // Mid build-failure loop.
        Checkpoint {
            t_ms: 425_000,
            expected: Expected {
                project: Some("continuum".to_string()),
                goal: Some(
                    "get continuum building again after the mismatched types failure".to_string(),
                ),
                task: Some("fix the mismatched types in the cargo build".to_string()),
                blocker: Some("cargo build failing with mismatched types".to_string()),
                last_action: Some("cargo build failed again with mismatched types".to_string()),
            },
        },
        // Build just went green.
        Checkpoint {
            t_ms: 585_000,
            expected: Expected {
                project: Some("continuum".to_string()),
                blocker: Some("cargo build failing with mismatched types".to_string()),
                last_action: Some("cargo build finished with 0 errors".to_string()),
                ..Expected::default()
            },
        },
        // Deep in the idle gap: state must hold, nothing may be inferred.
        Checkpoint {
            t_ms: 705_000,
            expected: Expected {
                project: Some("continuum".to_string()),
                last_action: Some("cargo build finished with 0 errors".to_string()),
                ..Expected::default()
            },
        },
        // Settled into simcharts after the switch (hysteresis is 20 s).
        Checkpoint {
            t_ms: 905_000,
            expected: Expected {
                project: Some("simcharts".to_string()),
                last_action: Some("cargo build finished with 0 errors".to_string()),
                ..Expected::default()
            },
        },
        // Mid TypeScript error loop in simcharts.
        Checkpoint {
            t_ms: 1_005_000,
            expected: Expected {
                project: Some("simcharts".to_string()),
                blocker: Some("a TypeScript error on ChartProps".to_string()),
                ..Expected::default()
            },
        },
        // Just after the voice command.
        Checkpoint {
            t_ms: 1_045_000,
            expected: Expected {
                project: Some("simcharts".to_string()),
                goal: Some("fix the ChartProps typing errors in simcharts".to_string()),
                task: Some("fix the ChartProps TypeScript error in the dashboard".to_string()),
                blocker: Some("a TypeScript error on ChartProps".to_string()),
                last_action: Some("ga door met de dashboard tests".to_string()),
            },
        },
        // The private-browsing window must not move the project.
        Checkpoint {
            t_ms: 1_095_000,
            expected: Expected {
                project: Some("simcharts".to_string()),
                blocker: Some("a TypeScript error on ChartProps".to_string()),
                last_action: Some("ga door met de dashboard tests".to_string()),
                ..Expected::default()
            },
        },
        // Back on continuum after the second switch.
        Checkpoint {
            t_ms: 1_145_000,
            expected: Expected {
                project: Some("continuum".to_string()),
                last_action: Some("ga door met de dashboard tests".to_string()),
                ..Expected::default()
            },
        },
        // Tests green at the end of the recording.
        Checkpoint {
            t_ms: 1_195_000,
            expected: Expected {
                project: Some("continuum".to_string()),
                last_action: Some("cargo test finished with 214 passed".to_string()),
                ..Expected::default()
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_fixture_matches_the_generator() {
        let committed = load_fixture(&fixture_path()).expect("committed fixture parses");
        let generated = synthetic_narrative();
        assert_eq!(
            committed.len(),
            generated.len(),
            "committed fixture is out of date — regenerate with \
             `cargo run --bin continuum-context-bench -- --write-fixture`"
        );
        for (index, (a, b)) in committed.iter().zip(generated.iter()).enumerate() {
            assert_eq!(a.t_ms, b.t_ms, "line {index}");
            assert_eq!(a.kind(), b.kind(), "line {index}");
            assert_eq!(
                serde_json::to_value(a).unwrap(),
                serde_json::to_value(b).unwrap(),
                "line {index} differs — regenerate the fixture"
            );
        }
    }

    #[test]
    fn committed_labels_match_the_generator() {
        let committed = load_labels(&labels_path()).expect("committed labels parse");
        assert_eq!(committed, synthetic_labels());
    }

    #[test]
    fn fixture_is_chronological_and_twenty_minutes_long() {
        let lines = synthetic_narrative();
        assert!(lines.len() > 100, "a reviewable but real narrative");
        let mut previous = i64::MIN;
        for line in &lines {
            assert!(line.t_ms >= previous, "t_ms must not go backwards");
            assert!(line.t_ms >= 0 && line.t_ms <= FIXTURE_SPAN_MS);
            previous = line.t_ms;
        }
        let last = lines.last().unwrap().t_ms;
        assert!(
            last >= FIXTURE_SPAN_MS - 20_000,
            "the narrative should run out the full twenty minutes, ended at {last}"
        );
    }

    #[test]
    fn narrative_contains_every_scripted_beat() {
        let lines = synthetic_narrative();
        let frames: Vec<_> = lines.iter().filter_map(|l| l.as_frame()).collect();
        let events: Vec<_> = lines.iter().filter_map(|l| l.as_event()).collect();

        // Two projects, reachable through the resolver's tier-2 editor rule.
        assert!(frames
            .iter()
            .any(|f| f.context.foreground_window_title.contains("continuum")));
        assert!(frames
            .iter()
            .any(|f| f.context.foreground_window_title.contains("simcharts")));

        // A build-failure loop with varying summaries (the dedupe target).
        let loop_frames: Vec<_> = frames
            .iter()
            .filter(|f| {
                f.screen.has_error_visible
                    && f.context.foreground_process_name == "WindowsTerminal.exe"
            })
            .collect();
        assert!(
            loop_frames.len() >= 20,
            "{} error frames",
            loop_frames.len()
        );
        let distinct: std::collections::HashSet<&str> = loop_frames
            .iter()
            .map(|f| f.screen.description.as_str())
            .collect();
        assert!(
            distinct.len() > 1,
            "the loop must vary its summaries or it proves nothing about §4.6"
        );

        // A success, an idle gap, a voice command, the privacy edges.
        assert!(frames
            .iter()
            .any(|f| f.screen.description.contains("0 errors")));
        assert!(frames.iter().any(|f| f.context.idle_seconds >= 300));
        assert!(frames.iter().any(|f| f.audio.is_some()));
        assert!(
            frames
                .iter()
                .any(|f| f.context.foreground_process_name
                    == crate::senses::privacy::EXCLUDED_PROCESS)
        );
        assert!(frames
            .iter()
            .any(|f| f.context.foreground_window_title.contains("InPrivate")));

        // Collector events across the sources the spec names.
        for source in [
            EventSource::File,
            EventSource::Git,
            EventSource::Window,
            EventSource::System,
            EventSource::Voice,
        ] {
            assert!(
                events.iter().any(|e| e.source == source),
                "no {source:?} event in the fixture"
            );
        }
        // A full git OID rides along as a structured raw_reference.
        assert!(events
            .iter()
            .any(|e| e.raw_reference.as_deref().is_some_and(|r| r.len() == 40)));
    }

    #[test]
    fn every_fixture_event_satisfies_the_closed_registry() {
        for event in synthetic_narrative().iter().filter_map(|l| l.as_event()) {
            assert!(
                event.event_type.valid_for(event.source),
                "{:?}/{:?} is not in the §4.6 registry",
                event.source,
                event.event_type
            );
        }
    }

    #[test]
    fn labels_are_inside_the_recording_and_name_known_projects() {
        let labels = synthetic_labels();
        assert!(labels.len() >= 10, "spec §9 asks for ~10 checkpoints");
        let known: Vec<String> = fixture_projects()
            .known
            .iter()
            .map(|p| p.id.clone())
            .collect();
        let mut previous = i64::MIN;
        for checkpoint in &labels {
            assert!(checkpoint.t_ms > previous, "checkpoints must be ordered");
            assert!(checkpoint.t_ms <= FIXTURE_SPAN_MS);
            previous = checkpoint.t_ms;
            if let Some(project) = &checkpoint.expected.project {
                assert!(known.contains(project), "unknown project {project}");
            }
        }
    }

    #[test]
    fn labels_round_trip_through_the_sidecar_shape() {
        let labels = synthetic_labels();
        let encoded = serde_json::to_string(&labels).unwrap();
        let back: Vec<Checkpoint> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(labels, back);
        // Unasserted fields are omitted, not serialized as null noise.
        assert!(!encoded.contains("\"goal\":null"));
        // And a sidecar that *does* spell out nulls still loads.
        let with_nulls = r#"[{"t_ms":1,"expected":{"project":null,"goal":null,"task":null,"blocker":null,"last_action":null}}]"#;
        let parsed: Vec<Checkpoint> = serde_json::from_str(with_nulls).unwrap();
        assert_eq!(parsed[0].expected, Expected::default());
    }
}
