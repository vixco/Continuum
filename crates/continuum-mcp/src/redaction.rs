//! # Safety-redaction harness (context engine spec §9, Task C6)
//!
//! Pushes a synthetic secret corpus through every stage of the context
//! engine and reports what survived — while the false-positive corpus
//! (git commit ids and friends) must survive.
//!
//! ## Why the harness lives in `continuum-mcp`
//!
//! Spec §9 requires it to cover "every MCP tool response". The handlers
//! live in this crate, and `continuum-core` cannot depend on
//! `continuum-mcp` without a dependency cycle — so this harness sits here
//! and imports the fixture, the replay and the metric helpers from
//! `continuum_core::bench`. Its three sibling harnesses live in
//! `continuum-core`.
//!
//! ## Why the *measurement* lives in the library rather than in the binary
//!
//! `src/bin/continuum-redaction-bench.rs` is a thin CLI wrapper: argument
//! handling, a table, an exit code. Everything that decides pass or fail
//! is here, where it runs under `cargo test -p continuum-mcp` — a unit
//! test inside a `src/bin` target only builds under `cargo test --bins`,
//! which cannot link this crate's dependency graph in rlib form on the
//! Windows toolchain the project targets.
//!
//! ## The stages
//!
//! 1. **capture → privacy** — `PrivacyFilter::scrub_text` and `scrub_path`
//!    on titles, captions, transcripts and paths, plus idempotence.
//! 2. **raw log** — a frame whose fields carried secrets is scrubbed at
//!    collector emit, persisted, and read back.
//! 3. **events** — the same content through the real events writer into
//!    `context_events`, read back from the table.
//! 4. **wake package render** — the spec §4.9 packager under a cloud-bound
//!    budget, including the `local_only` gate.
//! 5. **every MCP tool response** — all ten `context_*` tools plus the
//!    three §5.1-retrofitted `system_*` tools, answering from published
//!    files **deliberately poisoned with raw secrets**. This is the
//!    adversarial half: it assumes nothing upstream scrubbed and proves
//!    the egress gate does.
//!
//! ## Commit ids must survive
//!
//! Spec §4.1's scope rule: the entropy scrubbers apply to *free text*;
//! *structured fields from trusted collectors* (git commit id, branch,
//! frame ids, dedupe keys) are exempt **by construction** — meaning
//! `scrub_text` is never invoked on them. So the harness asserts a full
//! 40-character OID survives end to end through the structured path
//! (`context_git`'s `commit_id`, an event's `raw_reference`) and that a
//! short OID in prose survives the scrubbers.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use chrono::Utc;

use crate::tools::context as ctxtool;
use crate::tools::system as systool;
use continuum_core::bench::replay::wait_for_writer;
use continuum_core::config::{
    ContextPackageConfig, ContextToolsConfig, GitContextConfig, ObservationToggles,
};
use continuum_core::context::package::{
    ContextPackage, CurrentMoment, EventLine, MemoryLine, PackageBudget, SessionSection,
};
use continuum_core::context::project::{ProjectEntry, ProjectStatus};
use continuum_core::context::session_state::{SessionState, StampedText};
use continuum_core::memory::events::{
    spawn_event_writer, ContextEvent, EventSensitivity, EventSource, EventType,
};
use continuum_core::memory::raw_log::RawLog;
use continuum_core::runtime_publish::RuntimeSnapshot;
use continuum_core::senses::live_context::{
    write_snapshot, LiveContextHub, LiveWorldState, MonitorWorldState, PrivacyDisposition,
    ProjectWorldState, WindowWorldState,
};
use continuum_core::senses::privacy::PrivacyFilter;
use continuum_core::senses::types::{
    AudioObservation, ContextObservation, PerceptionFrame, ScreenObservation,
};

/// The home directory the harness pretends to run under, so path
/// scrubbing is deterministic on any machine.
pub const HOME_DIR: &str = "C:\\Users\\benchuser";

/// The username that goes with [`HOME_DIR`].
pub const USERNAME: &str = "benchuser";

/// A full git OID — a **structured** field, exempt from the scrubbers by
/// construction (spec §4.1). Must survive end to end.
pub const COMMIT_OID: &str = "9f1c2ba7de4055c8e1a37b2d6f480c9a5e73bd12";

/// The secret corpus. Every one of these must be gone from every stage.
///
/// Entries 8-12 are the fixwave-2 (M5) shapes: base64url payloads and the
/// vendor prefixes whose bodies are base64url. Before that fix the generic
/// base64 rule used the standard alphabet only, so these broke at their
/// first `-`/`_` and each fragment fell under the 32-char floor.
pub const SECRETS: [(&str, &str); 13] = [
    ("openai api key", "sk-proj-Ab12Cd34Ef56Gh78Ij90Kl12Mn34"),
    ("github token", "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8"),
    ("aws access key id", "AKIAIOSFODNN7EXAMPLE"),
    (
        "bearer token",
        "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9QWxhZGRpbg",
    ),
    ("credit card (Luhn-valid)", "4539 1488 0343 6467"),
    ("iban (mod-97 valid)", "GB82 WEST 1234 5698 7654 32"),
    (
        "sha256 digest in prose",
        "9b74c9897bac770ffc029102a200c5de1f1b2b2b2c3d4e5f60718293a4b5c6d7",
    ),
    ("session uuid", "3f2504e0-4f89-11d3-9a0c-0305e82c3301"),
    (
        "slack bot token",
        concat!("xoxb-", "2493847592-2493847598-AbCdEfGhIjKlMnOpQrStUvWx"),
    ),
    ("gitlab pat", concat!("glpat-", "AbCdEf01234567890xyz")),
    (
        "google oauth access token",
        "ya29.a0ARrdaM-9kQwErTyUiOpAsDfGhJkLzXcVbNm1234567890qwerty",
    ),
    (
        "jwt (base64url segments)",
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dQw4w9WgXcQ1234567890",
    ),
    (
        "base64url payload",
        "AbCdEf012345-9ZyXwVuTsRqPoNmLkJiHgFeDcBa0123456",
    ),
];

/// Free-text strings that look secret-ish but are not, and must survive
/// the scrubbers (spec §9's false-positive corpus).
pub const SURVIVORS: [(&str, &str); 6] = [
    ("short git OID", "9f1c2ba"),
    ("library name", "sk-learn-pipeline"),
    ("branch name", "feature/chart-legend"),
    ("version", "v1.2.3"),
    // Fixwave 2 (M5): the base64 alphabet now includes `-` and `_`, so
    // these two long hyphen/underscore runs are the ones most at risk of
    // becoming false positives.
    (
        "long kebab branch name",
        "feat/context-engine-privacy-fixwave-two",
    ),
    (
        "long snake_case module path",
        "src/very_long_module_name_with_underscores/mod.rs",
    ),
];

/// One thing that leaked.
#[derive(Debug)]
pub struct Leak {
    /// Which stage leaked.
    pub stage: &'static str,
    /// Which rendered surface leaked.
    pub surface: String,
    /// Which corpus entry leaked.
    pub secret: &'static str,
}

/// Collects leaks and survivals across the stages.
#[derive(Debug, Default)]
pub struct Report {
    /// Everything that survived a scrubber it should not have.
    pub leaks: Vec<Leak>,
    /// False-positive-corpus values that were redacted.
    pub missing_survivors: Vec<(&'static str, String)>,
    /// Every surface that was scanned.
    pub surfaces: BTreeSet<String>,
}

impl Report {
    /// Scans one rendered surface for every secret in the corpus.
    pub fn scan(&mut self, stage: &'static str, surface: &str, haystack: &str) {
        self.surfaces.insert(format!("{stage} / {surface}"));
        for (label, secret) in SECRETS {
            if contains_secret(haystack, secret) {
                self.leaks.push(Leak {
                    stage,
                    surface: surface.to_string(),
                    secret: label,
                });
            }
        }
    }

    /// Asserts a false-positive-corpus value is still present.
    pub fn expect_survivor(
        &mut self,
        label: &'static str,
        surface: &str,
        haystack: &str,
        needle: &str,
    ) {
        if !haystack.contains(needle) {
            self.missing_survivors
                .push((label, format!("{surface}: {needle}")));
        }
    }
}

/// Whether a scrubbed haystack still contains a secret.
///
/// Compared with whitespace removed as well as verbatim: a card number or
/// IBAN can be reformatted on the way through (`4539 1488` → `4539-1488`),
/// and a "redaction" that only broke up the spacing would still be a leak.
pub fn contains_secret(haystack: &str, secret: &str) -> bool {
    if haystack.contains(secret) {
        return true;
    }
    let squeeze = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect()
    };
    let needle = squeeze(secret);
    needle.len() >= 8 && squeeze(haystack).contains(&needle)
}

/// Builds a sentence carrying one secret, in the shape a collector would
/// actually see it.
pub fn poisoned(secret: &str) -> String {
    format!("export TOKEN={secret} # pasted into the terminal")
}

// ---------------------------------------------------------------------------
// Stage 1: capture → privacy
// ---------------------------------------------------------------------------

pub fn stage_privacy(filter: &PrivacyFilter, report: &mut Report) {
    for (_, secret) in SECRETS {
        let scrubbed = filter.scrub_text(&poisoned(secret));
        report.scan("privacy", "scrub_text", &scrubbed);
        // Idempotence (spec §4.1): scrubbing twice is scrubbing once.
        assert_eq!(
            filter.scrub_text(&scrubbed),
            scrubbed,
            "scrub_text must be idempotent"
        );
    }

    let path = format!("{HOME_DIR}\\code\\continuum\\src\\main.rs");
    let scrubbed = filter.scrub_path(&path);
    if scrubbed.contains(USERNAME) {
        report.leaks.push(Leak {
            stage: "privacy",
            surface: "scrub_path".into(),
            secret: "username",
        });
    }
    report.surfaces.insert("privacy / scrub_path".into());

    // The false-positive corpus survives free-text scrubbing.
    for (label, survivor) in SURVIVORS {
        let text = format!("checked out {survivor} for the review");
        report.expect_survivor(label, "scrub_text", &filter.scrub_text(&text), survivor);
    }
}

// ---------------------------------------------------------------------------
// Stage 2 + 3: raw log and events
// ---------------------------------------------------------------------------

pub async fn stage_raw_log_and_events(
    dir: &Path,
    filter: &PrivacyFilter,
    report: &mut Report,
) -> Result<()> {
    let db_path = dir.join("raw_log.sqlite");
    let raw_log = RawLog::open(&db_path.to_string_lossy()).await?;

    // A frame the collectors emitted through the §4.1 choke point: every
    // free-text field was scrubbed, the path was path-scrubbed.
    let now = Utc::now();
    for (index, (_, secret)) in SECRETS.iter().enumerate() {
        let frame = PerceptionFrame {
            id: uuid::Uuid::new_v4(),
            ts: now,
            screen: ScreenObservation {
                description: filter.scrub_text(&poisoned(secret)),
                world_compact: Some(filter.scrub_text(&format!("[window:foreground] {secret}"))),
                foreground_app: "WindowsTerminal.exe".into(),
                has_error_visible: false,
                confidence: 0.8,
                screenshot_path: Some(
                    filter.scrub_path(&format!("{HOME_DIR}\\shots\\{index}.jpg")),
                ),
                ts: now,
            },
            audio: Some(AudioObservation {
                transcript: filter.scrub_text(&format!("my key is {secret}")),
                language: "en".into(),
                duration_ms: 900,
                confidence: 0.9,
                ts: now,
            }),
            context: ContextObservation {
                foreground_window_title: filter.scrub_text(&poisoned(secret)),
                foreground_process_name: "WindowsTerminal.exe".into(),
                exe_path: Some(filter.scrub_path(&format!("{HOME_DIR}\\bin\\wt.exe"))),
                ts: now,
                ..Default::default()
            },
            salience_hint: 0.5,
        };
        raw_log.write_frame(&frame).await?;
    }
    let frames = raw_log
        .query_frames(
            now - chrono::Duration::minutes(5),
            now + chrono::Duration::minutes(5),
        )
        .await?;
    report.scan(
        "raw log",
        "perception_frames",
        &serde_json::to_string(&frames)?,
    );

    // Events through the real writer, including one row that carries a
    // RAW secret — the upstream leak the MCP egress gate must catch in
    // stage 5 — and one `local_only` row for the `omitted_private` path.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let events_cfg = continuum_core::config::EventsConfig::default();
    let (sender, writer) = spawn_event_writer(raw_log.clone(), &events_cfg, None, shutdown_rx);
    let mut sent = 0u64;
    // Each event gets its own `application`, because the spec §4.6 dedupe
    // key for classified events is
    // `hash(source, event_type, project, application)` — one shared
    // application would collapse the whole corpus into a single row and
    // the scan would prove nothing about the rows it never wrote.
    for (index, (_, secret)) in SECRETS.iter().enumerate() {
        sender.send(event(
            &format!("Terminal{index}.exe"),
            filter.scrub_text(&poisoned(secret)),
            EventSensitivity::CloudAllowed,
        ));
        sent += 1;
    }
    // Deliberate poison: a summary nobody scrubbed.
    sender.send(event(
        POISONED_APP,
        poisoned(SECRETS[0].1),
        EventSensitivity::CloudAllowed,
    ));
    sent += 1;
    // A local_only row: withheld at egress and counted.
    sender.send(event(
        "Private.exe",
        // Scrubbed like any other collector output — this row is here to
        // exercise the `omitted_private` counter, not to test the
        // scrubbers a second time.
        filter.scrub_text(&format!("private note about {}", SECRETS[1].1)),
        EventSensitivity::LocalOnly,
    ));
    sent += 1;
    // The structured commit id rides along as a raw_reference.
    sender.send(ContextEvent {
        ts: Utc::now(),
        source: EventSource::Git,
        application: "git".into(),
        window_title: String::new(),
        project_id: Some("continuum".into()),
        event_type: EventType::Commit,
        summary: "fix(core): stamp project ids on classified events".into(),
        importance: 0.2,
        confidence: 1.0,
        sensitivity: EventSensitivity::CloudAllowed,
        raw_reference: Some(COMMIT_OID.into()),
    });
    sent += 1;

    anyhow::ensure!(
        wait_for_writer(&writer, sent, std::time::Duration::from_secs(30)).await,
        "the events writer did not drain"
    );
    let _ = shutdown_tx.send(true);

    let rows = raw_log.list_context_events().await?;
    // `ContextEventRow` is a read-path struct with no `Serialize`; the
    // Debug rendering carries every field, which is what a scan needs.
    let encoded = format!("{rows:?}");
    // The scrubbed rows must be clean. The one deliberately-poisoned row
    // is expected to still hold its secret *in the table* — persistence is
    // not an egress point; stage 5 is where it must not come out.
    let clean: Vec<_> = rows
        .iter()
        .filter(|r| r.application != POISONED_APP)
        .collect();
    report.scan(
        "events",
        "context_events (collector-scrubbed rows)",
        &format!("{clean:?}"),
    );
    report.expect_survivor(
        "git commit id",
        "context_events.raw_reference",
        &encoded,
        COMMIT_OID,
    );

    raw_log.close().await;
    Ok(())
}

/// The application name carried by the one event whose summary is
/// deliberately left unscrubbed, so the scan can exclude it from the
/// "collector-scrubbed rows" surface while stage 5 proves the MCP egress
/// gate still redacts it.
pub const POISONED_APP: &str = "Poisoned.exe";

pub fn event(application: &str, summary: String, sensitivity: EventSensitivity) -> ContextEvent {
    ContextEvent {
        ts: Utc::now(),
        source: EventSource::Screen,
        application: application.to_string(),
        window_title: "cargo build".into(),
        project_id: None,
        event_type: EventType::Error,
        summary,
        importance: 0.7,
        confidence: 0.8,
        sensitivity,
        raw_reference: None,
    }
}

// ---------------------------------------------------------------------------
// Stage 4: wake package render
// ---------------------------------------------------------------------------

/// Marks the `local_only` package item the cloud render must drop.
pub const LOCAL_ONLY_MARKER: &str = "LOCALONLYMARKER";

pub fn stage_wake_package(filter: &PrivacyFilter, report: &mut Report) {
    let mut package = ContextPackage {
        current_moment: Some(CurrentMoment {
            window_title: Some(filter.scrub_text(&poisoned(SECRETS[0].1))),
            app: Some("WindowsTerminal.exe".into()),
            caption: filter.scrub_text(&poisoned(SECRETS[6].1)),
            world_compact: Some(
                filter.scrub_text(&format!("[window:foreground] {}", SECRETS[2].1)),
            ),
            audio: Some(filter.scrub_text(&format!("my key is {}", SECRETS[4].1))),
            local_only: false,
        }),
        session: Some(SessionSection {
            project: Some("continuum".into()),
            goal: Some(filter.scrub_text(&poisoned(SECRETS[3].1))),
            task: Some("fix the failing build".into()),
            confidence: 0.8,
            open_files: vec![filter.scrub_path(&format!("{HOME_DIR}\\code\\main.rs"))],
            local_only: false,
        }),
        ..ContextPackage::default()
    };
    package.just_before.push(EventLine {
        text: filter.scrub_text(&poisoned(SECRETS[5].1)),
        count: 1,
        age: "1m ago".into(),
        local_only: false,
    });
    // A local_only memory: the cloud gate must drop the whole item. Its
    // text is scrubbed like any other collector output — spec §4.1 lets a
    // *local* render see local_only content, so planting a raw secret here
    // would be testing the fixture, not the gate. The marker is what the
    // cloud render must not contain.
    package.memories.push(MemoryLine {
        text: format!(
            "{LOCAL_ONLY_MARKER} {}",
            filter.scrub_text(&poisoned(SECRETS[7].1))
        ),
        age: "2m ago".into(),
        local_only: true,
    });

    let rendered = package.render(&PackageBudget {
        cloud_bound: true,
        ..PackageBudget::default()
    });
    report.scan("wake package", "render (cloud-bound)", &rendered);
    assert!(
        !rendered.contains(LOCAL_ONLY_MARKER),
        "the cloud gate must drop local_only items entirely"
    );

    // A local render may legitimately see local_only text (spec §4.1:
    // local models are allowed) — it must still be free of *secrets*,
    // because those were scrubbed at collector emit.
    let local = package.render(&PackageBudget {
        cloud_bound: false,
        ..PackageBudget::default()
    });
    assert!(
        local.contains(LOCAL_ONLY_MARKER),
        "a local render is allowed to see local_only content (spec §4.1)"
    );
    report.scan("wake package", "render (local)", &local);
}

// ---------------------------------------------------------------------------
// Stage 5: every MCP tool response
// ---------------------------------------------------------------------------

pub async fn stage_mcp_tools(
    dir: &Path,
    filter: &PrivacyFilter,
    report: &mut Report,
) -> Result<u32> {
    // Poison the published files with RAW secrets: this stage assumes
    // nothing upstream scrubbed and proves the egress gate does.
    write_snapshot(&dir.join("live-context.json"), &poisoned_live_state())?;
    continuum_core::runtime_publish::write_snapshot(
        &dir.join("state.json"),
        &poisoned_runtime_snapshot(),
    )?;

    // The Projects table the `context_projects` / `context_git` tools read.
    let db_path = dir.join("raw_log.sqlite");
    let raw_log = RawLog::open(&db_path.to_string_lossy()).await?;
    raw_log
        .upsert_project(&ProjectEntry {
            id: "continuum".into(),
            name: "continuum".into(),
            root_paths: vec![format!("{HOME_DIR}\\code\\continuum")],
            repo: None,
            keywords: vec![],
            zone: None,
            status: ProjectStatus::Confirmed,
        })
        .await?;
    raw_log.close().await;

    let toggles = ObservationToggles::default();
    let git_context = GitContextConfig::default();
    let package_config = ContextPackageConfig::default();
    let tools = ContextToolsConfig::default();
    let gate = ctxtool::Gate {
        data_dir: dir,
        db_path: &db_path,
        filter,
        toggles: &toggles,
        git_context: &git_context,
        package_config: &package_config,
        enabled: tools.enabled,
    };

    let mut omitted_private = 0u32;
    let scan = |report: &mut Report, name: &'static str, value: serde_json::Value| {
        report.scan("mcp tool", name, &value.to_string());
    };

    scan(
        report,
        "context_session",
        serde_json::to_value(ctxtool::session(&gate))?,
    );
    scan(
        report,
        "context_window",
        serde_json::to_value(
            ctxtool::window(&gate, &ctxtool::ContextWindowRequest::default()).await,
        )?,
    );
    scan(
        report,
        "context_screen",
        serde_json::to_value(ctxtool::screen(&gate))?,
    );
    scan(
        report,
        "context_audio",
        serde_json::to_value(ctxtool::audio(&gate))?,
    );
    scan(
        report,
        "context_projects",
        serde_json::to_value(ctxtool::projects(&gate).await)?,
    );

    let timeline = ctxtool::timeline(&gate, &ctxtool::ContextTimelineRequest::default()).await;
    omitted_private += timeline.omitted_private;
    scan(report, "context_timeline", serde_json::to_value(&timeline)?);

    let search = ctxtool::search(
        &gate,
        &ctxtool::ContextSearchRequest {
            query: "export token".into(),
            limit: None,
        },
    )
    .await;
    omitted_private += search.omitted_private;
    scan(report, "context_search", serde_json::to_value(&search)?);

    let files = ctxtool::files(&gate, &ctxtool::ContextFilesRequest::default()).await;
    scan(report, "context_files", serde_json::to_value(&files)?);

    let git_live = ctxtool::git(&gate, &ctxtool::ContextGitRequest::default()).await;
    let git_json = serde_json::to_value(&git_live)?;
    report.expect_survivor(
        "git commit id",
        "context_git.commit_id",
        &git_json.to_string(),
        COMMIT_OID,
    );
    scan(report, "context_git (published)", git_json);
    scan(
        report,
        "context_git (named project)",
        serde_json::to_value(
            ctxtool::git(
                &gate,
                &ctxtool::ContextGitRequest {
                    project: Some("continuum".into()),
                },
            )
            .await,
        )?,
    );

    let package = ctxtool::package(
        &gate,
        &ctxtool::ContextPackageRequest::default(),
        ctxtool::PackageMemory::default(),
    )
    .await;
    if std::env::var("BENCH_DEBUG").is_ok() {
        println!(
            "--- context_package ---
{}
---",
            package.package
        );
    }
    scan(report, "context_package", serde_json::to_value(&package)?);

    // The three §5.1-retrofitted system tools.
    scan(
        report,
        "system_live_context",
        serde_json::to_value(systool::live_context(dir, filter).map_err(anyhow::Error::msg)?)?,
    );
    scan(
        report,
        "system_active_window",
        serde_json::to_value(systool::gate_active_window(
            filter,
            "WindowsTerminal.exe",
            &poisoned(SECRETS[0].1),
        ))?,
    );
    scan(
        report,
        "system_clipboard_get",
        serde_json::to_value(systool::gate_clipboard(
            filter,
            Some(poisoned(SECRETS[1].1)),
        ))?,
    );

    Ok(omitted_private)
}

/// A `live-context.json` with raw secrets in every free-text field and a
/// real username in every path.
pub fn poisoned_live_state() -> LiveWorldState {
    let now = Utc::now();
    let mut state = LiveContextHub::default().snapshot();
    state.generated_at = now;
    state.monitors = vec![MonitorWorldState {
        monitor_id: "display-1".into(),
        name: "display-1".into(),
        is_primary: true,
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        capture_event_sequence: 1,
        capture_sequence: 1,
        captured_at: now,
        change_score: 0.2,
        meaningful_change: true,
        description: poisoned(SECRETS[0].1),
        confidence: 0.8,
        vision_updated_at: Some(now),
        privacy: PrivacyDisposition::Visible,
        target_interval_ms: 200,
        capture_latency_ms: 5,
        dropped_before: 0,
    }];
    state.window = Some(WindowWorldState {
        process_name: "WindowsTerminal.exe".into(),
        title: poisoned(SECRETS[1].1),
        observed_at: now,
        in_call: false,
        privacy: PrivacyDisposition::Visible,
        pid: Some(4242),
        exe_path: Some(format!("{HOME_DIR}\\bin\\wt.exe")),
        monitor_id: Some("display-1".into()),
        active_since_secs: 12,
    });
    state.project = Some(ProjectWorldState {
        observed_at: now,
        terminal_active: true,
        terminal_process: Some("WindowsTerminal.exe".into()),
        project_root: Some(format!("{HOME_DIR}\\code\\continuum")),
        project_name: Some("continuum".into()),
        git_head: Some("main".into()),
        branch: Some("main".into()),
        dirty: 3,
        staged: 1,
        untracked: 2,
        ahead: 0,
        behind: 0,
        conflicts: 0,
        last_commit_id: Some(COMMIT_OID.into()),
        last_commit_subject: Some(poisoned(SECRETS[2].1)),
    });
    state.session_state = Some(poisoned_session_state());
    state
}

/// A published `RuntimeSnapshot` whose session state is full of secrets.
pub fn poisoned_runtime_snapshot() -> RuntimeSnapshot {
    RuntimeSnapshot {
        last_update: Utc::now().to_rfc3339(),
        session_state: Some(poisoned_session_state()),
        partial_transcript: Some(poisoned(SECRETS[3].1)),
        ..RuntimeSnapshot::default()
    }
}

pub fn poisoned_session_state() -> SessionState {
    let now = Utc::now();
    SessionState {
        active_project: Some("continuum".into()),
        current_goal: Some(poisoned(SECRETS[4].1)),
        current_task: Some(poisoned(SECRETS[5].1)),
        active_app: Some("WindowsTerminal.exe".into()),
        window_title: Some(poisoned(SECRETS[6].1)),
        open_files: vec![format!("{HOME_DIR}\\code\\continuum\\src\\main.rs")],
        last_error: Some(StampedText::new(poisoned(SECRETS[7].1), now)),
        last_success: None,
        last_user_command: Some(StampedText::new(poisoned(SECRETS[0].1), now)),
        confidence: 0.8,
        ..SessionState::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use continuum_core::bench::fixture;
    use continuum_core::bench::replay::{replay, Inferencer, ReplayOptions};
    use continuum_core::bench::BenchDir;
    use continuum_core::config::{ContextConfig, PrivacyConfig};

    /// The bench's fast path: every stage runs and nothing leaks.
    #[tokio::test]
    async fn no_secret_survives_any_stage() {
        let filter =
            PrivacyFilter::from_config(&ContextConfig::default(), &PrivacyConfig::default())
                .with_environment(Some(HOME_DIR.to_string()), Some(USERNAME.to_string()));
        let mut report = Report::default();
        let started = std::time::Instant::now();

        stage_privacy(&filter, &mut report);
        let dir = BenchDir::new("redaction-test").unwrap();
        stage_raw_log_and_events(dir.path(), &filter, &mut report)
            .await
            .unwrap();
        stage_wake_package(&filter, &mut report);
        let omitted = stage_mcp_tools(dir.path(), &filter, &mut report)
            .await
            .unwrap();

        assert!(
            started.elapsed().as_secs() < 60,
            "the redaction bench must stay CI-feasible"
        );
        assert!(
            report.leaks.is_empty(),
            "secrets survived: {:?}",
            report.leaks
        );
        assert!(
            report.missing_survivors.is_empty(),
            "false positives redacted: {:?}",
            report.missing_survivors
        );
        assert!(
            omitted >= 1,
            "the local_only row must be counted, not dropped"
        );
        assert!(
            report.surfaces.len() >= 15,
            "every stage contributes surfaces, got {}",
            report.surfaces.len()
        );
    }

    #[test]
    fn contains_secret_sees_through_reformatting() {
        assert!(contains_secret(
            "card 4539 1488 0343 6467",
            "4539 1488 0343 6467"
        ));
        assert!(contains_secret(
            "card 4539-1488-0343-6467",
            "4539 1488 0343 6467"
        ));
        assert!(!contains_secret("card [REDACTED]", "4539 1488 0343 6467"));
    }

    /// The replay's own events also have to survive an MCP egress scan —
    /// the fixture is the realistic corpus, the poisoned files are the
    /// adversarial one.
    #[tokio::test]
    async fn the_fixture_replays_into_clean_tool_responses() {
        let dir = BenchDir::new("redaction-fixture").unwrap();
        let db_path = dir.path().join("raw_log.sqlite");
        let raw_log = RawLog::open(&db_path.to_string_lossy()).await.unwrap();
        let options = ReplayOptions {
            inference: Inferencer::Off,
            ..ReplayOptions::default()
        };
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (sender, writer) =
            spawn_event_writer(raw_log.clone(), &options.config.events, None, shutdown_rx);
        let lines = fixture::load_fixture(&fixture::fixture_path()).unwrap();
        let labels = fixture::load_labels(&fixture::labels_path()).unwrap();
        let result = replay(&lines, &labels, &options, &sender).await.unwrap();
        assert!(
            wait_for_writer(
                &writer,
                result.emitted.len() as u64,
                std::time::Duration::from_secs(30)
            )
            .await
        );
        let _ = shutdown_tx.send(true);
        raw_log.close().await;

        let filter =
            PrivacyFilter::from_config(&ContextConfig::default(), &PrivacyConfig::default())
                .with_environment(Some(HOME_DIR.to_string()), Some(USERNAME.to_string()));
        let toggles = ObservationToggles::default();
        let git_context = GitContextConfig::default();
        let package_config = ContextPackageConfig::default();
        let gate = ctxtool::Gate {
            data_dir: dir.path(),
            db_path: &db_path,
            filter: &filter,
            toggles: &toggles,
            git_context: &git_context,
            package_config: &package_config,
            enabled: true,
        };
        let timeline = ctxtool::timeline(&gate, &ctxtool::ContextTimelineRequest::default()).await;
        assert!(timeline.available);
        assert!(!timeline.events.is_empty(), "the fixture produced rows");
        // The private-browsing frames are local_only and must be withheld.
        assert!(
            timeline.omitted_private >= 1,
            "the fixture's local_only rows must be counted"
        );
        let encoded = serde_json::to_string(&timeline).unwrap();
        assert!(
            !encoded.contains("private browsing"),
            "a local_only summary must never reach a tool response"
        );
    }
}
