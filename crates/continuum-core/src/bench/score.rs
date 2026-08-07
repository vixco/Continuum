//! # Bench scoring (context engine spec §9, Task C6)
//!
//! The measurement half of three of the four harnesses, living in the
//! library rather than in the bench binaries.
//!
//! **Why here and not in the `src/bin/*` files:** the binaries are thin
//! CLI wrappers — argument parsing, a table, an exit code — and everything
//! that decides pass or fail sits in this module, where it runs under the
//! project's standard gate (`cargo test -p continuum-core --lib`). Unit
//! tests inside a `src/bin` target only build under `cargo test --bins`,
//! which cannot link this crate's dependency graph in rlib form on the
//! Windows toolchain the project targets — so a bench whose gate lived in
//! its binary would silently never run.
//!
//! The fourth harness (`continuum-redaction-bench`) scores in
//! `continuum-mcp`, because it must call the real MCP tool handlers.

use std::collections::{BTreeMap, HashSet};

use chrono::Duration;
use continuum_memory::Vault;

use crate::bench::fixture::{self, Checkpoint};
use crate::bench::metrics::{self, Recall, DUPLICATE_SIMILARITY};
use crate::bench::record::RecordLine;
use crate::bench::replay::{
    replay, wait_for_writer, CheckpointObservation, Inferencer, ReplayOptions,
};
use crate::bench::BenchDir;
use crate::curator::extract::is_duplicate;
use crate::memory::distill::{event_to_memory_event, frame_to_memory_event};
use crate::memory::episodic::EpisodicEvent;
use crate::memory::events::{dedupe_key, spawn_event_writer, EventType};
use crate::memory::raw_log::RawLog;

/// How long a bench waits for the events writer to drain before failing.
pub const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Spec §9 recall floors.
pub const PROJECT_RECALL: f64 = 0.90;
/// Spec §9 goal/task recall floor.
pub const GOAL_RECALL: f64 = 0.60;
/// Spec §9 blocker/last-action recall floor.
pub const ACTION_RECALL: f64 = 0.80;
/// Spec §9 collapse floor for the repeated-failure loop.
pub const COLLAPSE_FLOOR: f64 = 0.90;
/// Spec §9 memory-precision floor.
pub const PRECISION_FLOOR: f64 = 0.70;
/// Spec §9 duplicate ceiling.
pub const DUPLICATE_CEILING: f64 = 0.10;

// ---------------------------------------------------------------------------
// Context recall
// ---------------------------------------------------------------------------

/// Per-field recall over the labeled checkpoints.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecallScores {
    /// Resolved project (compared exactly — a slug is an identifier).
    pub project: Recall,
    /// Inferred goal.
    pub goal: Recall,
    /// Inferred task.
    pub task: Recall,
    /// Latest observed error.
    pub blocker: Recall,
    /// Latest user command / success.
    pub last_action: Recall,
}

impl RecallScores {
    /// Every field with its label, for table rendering.
    pub fn fields(&self) -> [(&'static str, Recall); 5] {
        [
            ("project", self.project),
            ("goal", self.goal),
            ("task", self.task),
            ("blocker", self.blocker),
            ("last_action", self.last_action),
        ]
    }
}

/// Scores one field of one checkpoint. An unasserted label is skipped
/// entirely — it widens neither numerator nor denominator.
pub fn score_field(recall: &mut Recall, expected: Option<&str>, actual: Option<&str>, exact: bool) {
    let Some(expected) = expected else {
        return;
    };
    let hit = if exact {
        metrics::exact_matches(expected, actual)
    } else {
        metrics::field_matches(expected, actual)
    };
    recall.record(hit);
}

/// Scores every labeled checkpoint of a replay.
pub fn score_checkpoints(observations: &[CheckpointObservation]) -> RecallScores {
    let mut scores = RecallScores::default();
    for o in observations {
        score_field(
            &mut scores.project,
            o.expected.project.as_deref(),
            o.observed.project.as_deref(),
            true,
        );
        score_field(
            &mut scores.goal,
            o.expected.goal.as_deref(),
            o.observed.goal.as_deref(),
            false,
        );
        score_field(
            &mut scores.task,
            o.expected.task.as_deref(),
            o.observed.task.as_deref(),
            false,
        );
        score_field(
            &mut scores.blocker,
            o.expected.blocker.as_deref(),
            o.observed.blocker.as_deref(),
            false,
        );
        score_field(
            &mut scores.last_action,
            o.expected.last_action.as_deref(),
            o.observed.last_action.as_deref(),
            false,
        );
    }
    scores
}

// ---------------------------------------------------------------------------
// Dedupe precision
// ---------------------------------------------------------------------------

/// What the dedupe harness measured after replaying through the real
/// events writer into a throwaway database.
#[derive(Debug, Clone, Default)]
pub struct DedupeScores {
    /// Events handed to the sender.
    pub emitted: usize,
    /// Rows the writer wrote.
    pub rows: usize,
    /// `SUM(count)` over the rows: every occurrence is a row or a bump.
    pub total_counted: i64,
    /// Distinct dedupe keys the replay produced.
    pub expected_keys: usize,
    /// Distinct dedupe keys that reached the table.
    pub actual_keys: usize,
    /// Keys that were emitted but never persisted (must be empty).
    pub lost_keys: Vec<String>,
    /// Keys in the table that were never emitted (must be empty).
    pub invented_keys: Vec<String>,
    /// Occurrences of the build-failure loop's key.
    pub loop_occurrences: usize,
    /// Rows the build-failure loop occupies.
    pub loop_rows: usize,
    /// Occurrences per key, for the table.
    pub occurrences: BTreeMap<String, usize>,
    /// Rows per key, for the table.
    pub rows_per_key: BTreeMap<String, i64>,
    /// `SUM(count)` per key, for the table.
    pub counted_per_key: BTreeMap<String, i64>,
    /// First summary per key, for the table.
    pub summary_per_key: BTreeMap<String, String>,
}

impl DedupeScores {
    /// Collapse achieved on the repeated-failure loop.
    pub fn loop_collapse(&self) -> f64 {
        if self.loop_occurrences == 0 {
            return 0.0;
        }
        1.0 - (self.loop_rows as f64 / self.loop_occurrences as f64)
    }

    /// Collapse across the whole recording.
    pub fn overall_collapse(&self) -> f64 {
        if self.emitted == 0 {
            return 0.0;
        }
        1.0 - (self.rows as f64 / self.emitted as f64)
    }
}

/// Replays `lines` through the real writer and measures §4.6 collapse.
pub async fn run_dedupe(
    lines: &[RecordLine],
    labels: &[Checkpoint],
) -> anyhow::Result<DedupeScores> {
    let dir = BenchDir::new("dedupe")?;
    let raw_log = RawLog::open(&dir.path().join("dedupe.sqlite").to_string_lossy()).await?;
    let options = ReplayOptions {
        // Inference is irrelevant to dedupe and costs nothing to skip.
        inference: Inferencer::Off,
        ..ReplayOptions::default()
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (sender, writer) =
        spawn_event_writer(raw_log.clone(), &options.config.events, None, shutdown_rx);

    let result = replay(lines, labels, &options, &sender).await?;
    let drained = wait_for_writer(&writer, result.emitted.len() as u64, DRAIN_TIMEOUT).await;
    let _ = shutdown_tx.send(true);
    anyhow::ensure!(
        drained,
        "the events writer did not drain {} events in {DRAIN_TIMEOUT:?}",
        result.emitted.len()
    );

    let rows = raw_log.list_context_events().await?;
    raw_log.close().await;

    let expected: HashSet<String> = result.emitted.iter().map(dedupe_key).collect();
    let actual: HashSet<String> = rows.iter().map(|r| r.dedupe_key.clone()).collect();

    let mut scores = DedupeScores {
        emitted: result.emitted.len(),
        rows: rows.len(),
        total_counted: rows.iter().map(|r| r.count).sum(),
        expected_keys: expected.len(),
        actual_keys: actual.len(),
        lost_keys: expected.difference(&actual).cloned().collect(),
        invented_keys: actual.difference(&expected).cloned().collect(),
        ..DedupeScores::default()
    };
    scores.lost_keys.sort();
    scores.invented_keys.sort();

    for event in &result.emitted {
        *scores.occurrences.entry(dedupe_key(event)).or_default() += 1;
    }
    for row in &rows {
        *scores
            .rows_per_key
            .entry(row.dedupe_key.clone())
            .or_default() += 1;
        *scores
            .counted_per_key
            .entry(row.dedupe_key.clone())
            .or_default() += row.count;
        scores
            .summary_per_key
            .entry(row.dedupe_key.clone())
            .or_insert_with(|| row.summary.clone());
    }

    let loop_key = result
        .classified
        .iter()
        .find(|e| e.event_type == EventType::Error && e.application == "WindowsTerminal.exe")
        .map(dedupe_key)
        .ok_or_else(|| anyhow::anyhow!("the fixture no longer contains a build-failure loop"))?;
    scores.loop_occurrences = scores.occurrences.get(&loop_key).copied().unwrap_or(0);
    scores.loop_rows = scores
        .rows_per_key
        .get(&loop_key)
        .copied()
        .unwrap_or(0)
        .max(0) as usize;
    Ok(scores)
}

// ---------------------------------------------------------------------------
// Memory precision
// ---------------------------------------------------------------------------

/// One curated note, flattened for reporting.
#[derive(Debug, Clone)]
pub struct NoteView {
    /// Vault node type token.
    pub kind: String,
    /// Note title.
    pub title: String,
}

/// What the memory-precision harness measured.
#[derive(Debug, Clone, Default)]
pub struct MemoryScores {
    /// Distilled episodic memories.
    pub memories: Vec<EpisodicEvent>,
    /// Curated vault notes.
    pub notes: Vec<NoteView>,
    /// Rows in `context_events` after the replay.
    pub event_rows: usize,
    /// Candidates the consumption plan produced and the gate admitted.
    pub candidates_proposed: usize,
    /// Candidates the §4.6 candidate gate suppressed.
    pub candidates_suppressed: usize,
    /// `[memory] distillation_min_salience` in force — the **fallback**
    /// (raw-frame) rung's threshold.
    pub min_salience: f32,
    /// `[memory] distillation_min_event_importance` in force — the
    /// **primary** (deduped events) rung's threshold. Reported separately
    /// since fixwave 3a: one shared number made the primary rung
    /// unreachable, so a report that prints only one of them hides the
    /// thing that went wrong.
    pub min_event_importance: f32,
    /// Memories with a project attribution (the precision denominator).
    pub scored: usize,
    /// Memories that matched their label.
    pub correct: usize,
    /// Memories with no attribution (excluded by design, Task B6).
    pub unattributed: usize,
    /// Near-duplicates within each population.
    pub duplicates: usize,
    /// Total artifacts that reached memory.
    pub artifacts: usize,
    /// Confirmed notes a wake-time retrieval surfaced.
    pub later_used: usize,
    /// Notes the retrieval returned at all.
    pub retrieval_hits: usize,
}

impl MemoryScores {
    /// Correctly attributed and grounded memories over attributed ones.
    pub fn precision(&self) -> f64 {
        if self.scored == 0 {
            return 0.0;
        }
        self.correct as f64 / self.scored as f64
    }

    /// Near-duplicates over everything that reached memory.
    pub fn duplicate_rate(&self) -> f64 {
        if self.artifacts == 0 {
            return 0.0;
        }
        self.duplicates as f64 / self.artifacts as f64
    }

    /// Report-only (spec §9): share of confirmed notes that a wake-time
    /// retrieval actually surfaced.
    pub fn later_used_rate(&self) -> f64 {
        if self.notes.is_empty() {
            return 0.0;
        }
        self.later_used as f64 / self.notes.len() as f64
    }
}

/// Replay → events → distillation → curation → scoring (spec §9).
///
/// Runs the distiller's own row selection and mapping — **both rungs of
/// the §4.11 compression ladder**, in the same order and with the same
/// budget split `distill_once` uses:
///
/// 1. `query_undistilled_events` + [`event_to_memory_event`]
/// 2. `query_undistilled_unclassified_frames` + [`frame_to_memory_event`]
///
/// Covering rung 2 is not decoration (fixwave 3a, C3): the bench reported
/// a 0.000 duplicate rate while every collapsed frame was *also* being
/// recorded as a raw-frame memory, because it only ever exercised rung 1.
/// A regression in the collapsed-frame suppression now shows up here as
/// near-duplicate memories of the same moment.
///
/// The only step skipped is the LanceDB vector write, whose embedding
/// model is downloaded on first use and would make the bench neither
/// offline nor deterministic.
pub async fn run_memory_precision(
    lines: &[RecordLine],
    labels: &[Checkpoint],
) -> anyhow::Result<MemoryScores> {
    let dir = BenchDir::new("memory")?;
    let raw_log = RawLog::open(&dir.path().join("raw-log.sqlite").to_string_lossy()).await?;
    let vault = Vault::open(&dir.path().join("vault")).await?;

    let options = ReplayOptions {
        persist_frames: Some(raw_log.clone()),
        ..ReplayOptions::default()
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (sender, writer) =
        spawn_event_writer(raw_log.clone(), &options.config.events, None, shutdown_rx);

    let result = replay(lines, labels, &options, &sender).await?;
    let drained = wait_for_writer(&writer, result.emitted.len() as u64, DRAIN_TIMEOUT).await;
    let _ = shutdown_tx.send(true);
    anyhow::ensure!(drained, "the events writer did not drain in time");

    // --- Distillation ---------------------------------------------------
    let memory_cfg = &options.config.memory;
    let since = options.base - Duration::minutes(1);
    let until =
        options.base + Duration::milliseconds(fixture::FIXTURE_SPAN_MS) + Duration::minutes(1);
    let event_rows = raw_log.context_event_count().await? as usize;
    let budget = memory_cfg.distillation_batch_size.max(1_000);
    let rows = raw_log
        .query_undistilled_events(
            since,
            until,
            memory_cfg.distillation_min_event_importance,
            budget,
        )
        .await?;
    let mut memories: Vec<EpisodicEvent> = rows.iter().map(event_to_memory_event).collect();
    // Which memories came from a *deterministic* collector rather than
    // from model-authored text — see the precision loop below.
    let mut mechanical: Vec<bool> = rows
        .iter()
        .map(|row| {
            !matches!(
                row.source,
                crate::memory::events::EventSource::Screen
                    | crate::memory::events::EventSource::Audio
            )
        })
        .collect();
    raw_log
        .mark_events_distilled(&rows.iter().map(|r| r.id).collect::<Vec<_>>())
        .await?;

    // Rung 2: frames that produced no usable event. `distill_once` gives
    // this rung whatever budget the events left, so the bench does too.
    let frames = raw_log
        .query_undistilled_unclassified_frames(
            since,
            until,
            memory_cfg.distillation_min_salience,
            budget.saturating_sub(rows.len()),
        )
        .await?;
    memories.extend(frames.iter().map(frame_to_memory_event));
    // A raw-frame memory is built from the vision caption and the
    // transcript — model-authored text, so it IS scored for grounding.
    mechanical.extend(std::iter::repeat_n(false, frames.len()));
    raw_log
        .mark_frames_distilled(&frames.iter().map(|f| f.id).collect::<Vec<_>>())
        .await?;

    // --- Curation: the same duplicate gate ClassificationConsumer uses ---
    let candidates_proposed = result.candidates.len();
    for draft in result.candidates {
        if is_duplicate(&vault, &draft.title).await? {
            continue;
        }
        vault.create(draft).await?;
    }
    let written = vault
        .graph(&continuum_memory::GraphFilter::default())
        .await?;
    let notes: Vec<NoteView> = written
        .nodes
        .iter()
        .map(|node| NoteView {
            kind: format!("{:?}", node.node_type).to_lowercase(),
            title: node.title.clone(),
        })
        .collect();

    // --- Duplicate rate, counted WITHIN each population ------------------
    //
    // A distilled memory and the vault note it came from describe the same
    // moment on purpose — one memory recorded in two stores, not a
    // duplicate — so pooling the populations would manufacture duplicates
    // the design intends.
    let memory_summaries: Vec<String> = memories.iter().map(|m| m.summary.clone()).collect();
    let note_titles: Vec<String> = notes.iter().map(|n| n.title.clone()).collect();
    let duplicates = count_duplicates(&memory_summaries) + count_duplicates(&note_titles);
    let artifacts = memory_summaries.len() + note_titles.len();

    // --- Precision vs the labels -----------------------------------------
    let corpus = label_corpus(labels);
    let mut scored = 0usize;
    let mut correct = 0usize;
    let mut unattributed = 0usize;
    for (index, memory) in memories.iter().enumerate() {
        let Some(project) = memory.project.as_deref() else {
            unattributed += 1;
            continue;
        };
        scored += 1;
        let offset = memory
            .ts
            .signed_duration_since(options.base)
            .num_milliseconds();
        let attributed = project_at(labels, offset).is_some_and(|expected| expected == project);
        // Grounding is a *hallucination* check, and only model-authored
        // text can hallucinate. A deterministic collector event ("project
        // a → b", "src/main.rs", "3 commits on main") is a mechanical
        // transcription of something the machine observed directly — it is
        // grounded by construction, and its wording is deliberately absent
        // from the narrative corpus the labels describe. Since fixwave 3a
        // those events actually reach the distiller (I2), so scoring them
        // against the narrative corpus would have measured vocabulary
        // overlap, not truthfulness. Attribution is still scored for them.
        let truthful =
            mechanical.get(index).copied().unwrap_or(false) || grounded(&corpus, &memory.summary);
        if attributed && truthful {
            correct += 1;
        }
    }

    // --- Later-used: one simulated wake-time retrieval --------------------
    //
    // Two stand-ins, both stated rather than hidden:
    //
    // 1. The bench confirms the candidates, standing in for the curator /
    //    human review queue. `retrieve_vault_context` only returns
    //    *confirmed* notes, so without this the report would be a constant
    //    zero for any fixture shorter than the review cycle.
    // 2. One wake is simulated at the end of the recording, through the
    //    real `retrieve_vault_context` — the same call `do_wake` makes.
    for node in &written.nodes {
        vault
            .resolve_candidate(&node.id, continuum_memory::Resolution::Confirm)
            .await?;
    }
    let mut later_used = 0usize;
    let mut retrieval_hits = 0usize;
    if let Some(trigger) = lines.iter().filter_map(|l| l.as_frame()).next_back() {
        let (hits, pending) = crate::memory::retrieval::retrieve_vault_context(
            &vault,
            trigger,
            &options.config.memory.curator,
        )
        .await;
        let ids: Vec<String> = hits
            .iter()
            .chain(pending.iter())
            .map(|hit| hit.id.clone())
            .collect();
        retrieval_hits = ids.len();
        vault.touch_last_used(&ids).await?;
        for node in &written.nodes {
            if vault.get(&node.id).await?.frontmatter.last_used.is_some() {
                later_used += 1;
            }
        }
    }

    raw_log.close().await;

    Ok(MemoryScores {
        memories,
        notes,
        event_rows,
        candidates_proposed,
        candidates_suppressed: result.candidates_suppressed,
        min_salience: memory_cfg.distillation_min_salience,
        min_event_importance: memory_cfg.distillation_min_event_importance,
        scored,
        correct,
        unattributed,
        duplicates,
        artifacts,
        later_used,
        retrieval_hits,
    })
}

/// Near-duplicates within one population.
pub fn count_duplicates(entries: &[String]) -> usize {
    let mut duplicates = 0usize;
    for (index, entry) in entries.iter().enumerate() {
        if entries[..index]
            .iter()
            .any(|earlier| metrics::text_similarity(earlier, entry) >= DUPLICATE_SIMILARITY)
        {
            duplicates += 1;
        }
    }
    duplicates
}

/// Every free-text field the labels assert, joined — the narrative a
/// memory has to be about to count as grounded.
pub fn label_corpus(labels: &[Checkpoint]) -> String {
    let mut corpus = String::new();
    for checkpoint in labels {
        for text in [
            &checkpoint.expected.goal,
            &checkpoint.expected.task,
            &checkpoint.expected.blocker,
            &checkpoint.expected.last_action,
            &checkpoint.expected.project,
        ]
        .into_iter()
        .flatten()
        {
            corpus.push_str(text);
            corpus.push(' ');
        }
    }
    corpus
}

/// Whether a memory summary is about something the narrative labels.
///
/// One shared significant token is enough: the labels are ten sentences
/// covering twenty minutes, so demanding majority overlap would punish a
/// perfectly good memory for being specific. What this catches is the
/// failure that matters — a memory about something nobody was doing (a
/// private-browsing caption, another project's error, a leaked path).
pub fn grounded(corpus: &str, summary: &str) -> bool {
    let known: HashSet<String> = metrics::tokens(corpus).into_iter().collect();
    metrics::tokens(summary)
        .iter()
        .any(|token| known.iter().any(|k| metrics::token_matches(k, token)))
}

/// The labeled project at a fixture offset: the newest checkpoint at or
/// before it that names one.
pub fn project_at(labels: &[Checkpoint], offset_ms: i64) -> Option<&str> {
    labels
        .iter()
        .rfind(|c| c.t_ms <= offset_ms)
        .or_else(|| labels.first())
        .and_then(|c| c.expected.project.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::events::EventSender;

    fn corpus() -> (Vec<RecordLine>, Vec<Checkpoint>) {
        (
            fixture::load_fixture(&fixture::fixture_path()).expect("committed fixture parses"),
            fixture::load_labels(&fixture::labels_path()).expect("committed labels parse"),
        )
    }

    /// `continuum-context-bench`'s gate: the fixture replays and clears
    /// every spec §9 recall floor in mock mode, in seconds.
    #[tokio::test]
    async fn context_recall_meets_the_spec_thresholds() {
        let (lines, labels) = corpus();
        let started = std::time::Instant::now();
        let result = replay(
            &lines,
            &labels,
            &ReplayOptions::default(),
            &EventSender::log_only(),
        )
        .await
        .unwrap();
        assert!(
            started.elapsed().as_secs() < 20,
            "the mock replay must stay CI-feasible"
        );
        assert_eq!(result.checkpoints.len(), labels.len());

        let scores = score_checkpoints(&result.checkpoints);
        assert!(
            scores.project.rate() as f64 >= PROJECT_RECALL,
            "project recall {}/{}",
            scores.project.hits,
            scores.project.total
        );
        assert!(
            scores.goal.rate() as f64 >= GOAL_RECALL,
            "goal recall {}/{}",
            scores.goal.hits,
            scores.goal.total
        );
        assert!(
            scores.task.rate() as f64 >= GOAL_RECALL,
            "task recall {}/{}",
            scores.task.hits,
            scores.task.total
        );
        assert!(
            scores.blocker.rate() as f64 >= ACTION_RECALL,
            "blocker recall {}/{}",
            scores.blocker.hits,
            scores.blocker.total
        );
        assert!(
            scores.last_action.rate() as f64 >= ACTION_RECALL,
            "last-action recall {}/{}",
            scores.last_action.hits,
            scores.last_action.total
        );
        assert_eq!(result.idle_inferences, 0, "spec §4.11");
        for (name, recall) in scores.fields() {
            assert!(recall.total > 0, "no checkpoint labels {name}");
        }
    }

    /// `continuum-dedupe-bench`'s gate: the real writer collapses the
    /// failure loop past the spec floor and loses nothing.
    #[tokio::test]
    async fn dedupe_collapses_the_loop_without_losing_distinct_events() {
        let (lines, labels) = corpus();
        let started = std::time::Instant::now();
        let scores = run_dedupe(&lines, &labels).await.unwrap();
        assert!(
            started.elapsed().as_secs() < 30,
            "the dedupe bench must stay CI-feasible"
        );

        assert!(
            scores.loop_collapse() >= COLLAPSE_FLOOR,
            "collapse {:.3} from {} occurrences into {} rows",
            scores.loop_collapse(),
            scores.loop_occurrences,
            scores.loop_rows
        );
        assert!(scores.lost_keys.is_empty(), "lost {:?}", scores.lost_keys);
        assert!(
            scores.invented_keys.is_empty(),
            "invented {:?}",
            scores.invented_keys
        );
        assert_eq!(scores.expected_keys, scores.actual_keys);
        assert_eq!(
            scores.total_counted, scores.emitted as i64,
            "every occurrence is either a row or a count bump"
        );
        assert!(
            scores.rows < scores.emitted,
            "collapse actually happened: {} rows from {} events",
            scores.rows,
            scores.emitted
        );
    }

    /// The two error loops must not share a row: same event type,
    /// different project and application (spec §4.6 key material).
    #[tokio::test]
    async fn the_two_error_loops_keep_separate_rows() {
        let (lines, labels) = corpus();
        let options = ReplayOptions {
            inference: Inferencer::Off,
            ..ReplayOptions::default()
        };
        let result = replay(&lines, &labels, &options, &EventSender::log_only())
            .await
            .unwrap();
        let terminal = result
            .classified
            .iter()
            .find(|e| e.event_type == EventType::Error && e.application == "WindowsTerminal.exe")
            .map(dedupe_key)
            .expect("the continuum build loop");
        let editor = result
            .classified
            .iter()
            .find(|e| e.event_type == EventType::Error && e.application == "Code.exe")
            .map(dedupe_key)
            .expect("the simcharts TypeScript loop");
        assert_ne!(terminal, editor);
    }

    /// `continuum-memory-precision-bench`'s gate: the distil + curate
    /// pipeline clears both spec floors.
    #[tokio::test]
    async fn memory_is_precise_and_not_duplicated() {
        let (lines, labels) = corpus();
        let started = std::time::Instant::now();
        let scores = run_memory_precision(&lines, &labels).await.unwrap();
        assert!(
            started.elapsed().as_secs() < 60,
            "the memory bench must stay CI-feasible"
        );

        assert!(!scores.memories.is_empty(), "the fixture produces memories");
        assert!(!scores.notes.is_empty(), "the fixture produces vault notes");
        assert!(
            scores.duplicate_rate() <= DUPLICATE_CEILING,
            "duplicate rate {:.3} ({} of {})",
            scores.duplicate_rate(),
            scores.duplicates,
            scores.artifacts
        );
        assert!(
            scores.precision() >= PRECISION_FLOOR,
            "precision {:.3} ({} of {})",
            scores.precision(),
            scores.correct,
            scores.scored
        );

        // The compression ladder actually ran: thirty build failures are
        // one memory, not thirty.
        let build_memories = scores
            .memories
            .iter()
            .filter(|m| m.summary.contains("cargo build failed"))
            .count();
        assert_eq!(build_memories, 1, "the failure loop distils to one memory");
        assert!(
            scores.memories.iter().any(|m| m.summary.contains("(×")),
            "a collapsed memory carries its occurrence count (spec §4.11)"
        );
        assert!(
            scores.candidates_suppressed > 0,
            "the candidate gate suppressed the loop's repeats"
        );

        // Fixwave 3a (C1 + I2): a private-browsing caption MAY now be
        // distilled — it is a memory of the user's own day, and the local
        // store is exactly where it belongs — but it must carry its zone
        // so the cloud egress gate withholds it. The previous contract
        // ("never reaches memory") held only by accident: one shared 0.35
        // threshold excluded every low-importance event, private or not,
        // which is the same accident that made the whole primary rung of
        // the compression ladder unreachable.
        let mut private_memories = 0;
        for memory in &scores.memories {
            if memory.summary.contains("private browsing") {
                private_memories += 1;
                assert_eq!(
                    memory.sensitivity,
                    crate::memory::events::EventSensitivity::LocalOnly,
                    "a private-browsing memory reached episodic memory untagged: {}",
                    memory.summary
                );
            }
        }
        assert!(
            private_memories > 0,
            "the fixture's private-browsing window must exercise the zone path"
        );
    }

    #[test]
    fn project_at_walks_the_checkpoints() {
        let labels = fixture::synthetic_labels();
        assert_eq!(
            project_at(&labels, 0),
            Some("continuum"),
            "before the first"
        );
        assert_eq!(project_at(&labels, 1_000_000), Some("simcharts"));
        assert_eq!(project_at(&labels, i64::MAX), Some("continuum"));
    }

    #[test]
    fn grounded_rejects_a_memory_the_narrative_never_mentions() {
        let corpus = label_corpus(&fixture::synthetic_labels());
        assert!(grounded(
            &corpus,
            "cargo build failed: mismatched types (×30)"
        ));
        assert!(!grounded(&corpus, "Bekeken: pinguïnvoedertijden bij Artis"));
    }

    /// `continuum-triage-bench --prompt-fit-only`'s gate, widened to the
    /// context fixture (spec §4.7 budget + the §4.10 blob-leak scan). The
    /// fixture's frames carry real window titles, captions and
    /// transcripts, so a regression that only shows on longer frames
    /// cannot slip past the twenty hand-labeled benchmark ones. Needs no
    /// model, so it gates every change; the accuracy/latency re-baseline
    /// still needs the GPU.
    #[test]
    fn context_fixture_frames_fit_the_triage_prompt_budget() {
        let cfg = crate::config::TriageSection::default();
        let budget = cfg.context_size.saturating_sub(cfg.max_tokens);
        let lines = fixture::load_fixture(&fixture::fixture_path()).unwrap();
        let frames: Vec<_> = lines.iter().filter_map(|l| l.as_frame()).collect();
        assert!(!frames.is_empty());

        let mut worst = 0usize;
        for frame in frames {
            let prompt = crate::triage::prompts::build_triage_prompt(frame, "");
            assert!(
                !prompt.contains("world_compact") && !prompt.contains("live-context/v"),
                "the slim TriagePromptFrame projection leaked the §4.10 blob"
            );
            worst = worst.max(prompt.len());
        }
        let tokens = (worst as f64 / 3.5).ceil() as u32;
        assert!(
            tokens < budget,
            "worst-case fixture prompt ~{tokens} tokens >= budget {budget}"
        );
    }

    #[test]
    fn duplicate_counting_is_within_a_population() {
        // A near-copy (only the occurrence count differs) is a duplicate.
        let entries = vec![
            "cargo build failed: mismatched types (×30). Context: cargo build in WindowsTerminal.exe"
                .to_string(),
            "cargo build failed: mismatched types (×3). Context: cargo build in WindowsTerminal.exe"
                .to_string(),
            "ga door met de dashboard tests".to_string(),
        ];
        assert_eq!(count_duplicates(&entries), 1);
        assert_eq!(count_duplicates(&[]), 0);

        // Two failures at *different* sites are not duplicates: the
        // comparator must not merge distinct memories just because they
        // share a template.
        let distinct = vec![
            "cargo build failed: mismatched types at events.rs:214".to_string(),
            "cargo build failed: mismatched types at events.rs:311".to_string(),
        ];
        assert_eq!(count_duplicates(&distinct), 0);
    }
}
