//! # continuum-context-bench
//!
//! The **context-recall** harness (context engine spec §9): replays the
//! committed twenty-minute fixture through the real pipeline under a fake
//! clock and asserts that, at ten labeled checkpoints, the engine knew what
//! was going on.
//!
//! Spec thresholds (overridable on the command line for experiments, never
//! for a passing run):
//!
//! | field | recall |
//! |---|---|
//! | `project` | ≥ 0.90 |
//! | `goal`, `task` | ≥ 0.60 |
//! | `blocker`, `last_action` | ≥ 0.80 |
//!
//! Only checkpoints that *assert* a field count toward its denominator —
//! a `null` label means the narrative claims nothing there.
//!
//! # Usage
//!
//! ```bash
//! cargo run --bin continuum-context-bench                # mock mode (default, no GPU)
//! cargo run --bin continuum-context-bench -- --live      # with the real triage model
//! cargo run --bin continuum-context-bench -- --write-fixture
//! ```
//!
//! Mock mode is the CI-feasible gate: deterministic, offline, a second per
//! run. It proves resolution, hysteresis, session mechanics and the
//! privacy paths. Live mode is what measures the *model*; see
//! [`continuum_core::bench::replay`] for the distinction.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use continuum_core::bench::fixture;
use continuum_core::bench::metrics::{self, Bound, Scorecard};
use continuum_core::bench::replay::{
    replay, CheckpointObservation, Classifier, Inferencer, ReplayOptions,
};
use continuum_core::bench::score::{score_checkpoints, ACTION_RECALL, GOAL_RECALL, PROJECT_RECALL};
use continuum_core::memory::events::EventSender;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .compact()
        .init();

    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    if flag("--write-fixture") {
        let dir = value("--out")
            .map(PathBuf::from)
            .unwrap_or_else(fixture::fixture_dir);
        let (jsonl, labels) = fixture::write_fixture(&dir)?;
        println!("Wrote {}", jsonl.display());
        println!("Wrote {}", labels.display());
        return Ok(());
    }

    println!("Continuum Context-Recall Benchmark");
    println!("==================================\n");

    let fixture_path = value("--fixture")
        .map(PathBuf::from)
        .unwrap_or_else(fixture::fixture_path);
    let labels_path = value("--labels")
        .map(PathBuf::from)
        .unwrap_or_else(fixture::labels_path);
    let lines = fixture::load_fixture(&fixture_path)?;
    let labels = fixture::load_labels(&labels_path)?;

    let live = flag("--live");
    let mut options = ReplayOptions::default();
    if live {
        let triage = Arc::new(load_triage_model(&args)?);
        options.classifier = Classifier::Live(triage.clone());
        options.inference = Inferencer::Live(triage);
    }

    let span_min = lines.last().map(|l| l.t_ms).unwrap_or(0) as f64 / 60_000.0;
    println!(
        "Fixture: {} ({} lines, {:.1} min, {} checkpoints)",
        fixture_path.display(),
        lines.len(),
        span_min,
        labels.len()
    );
    println!(
        "Mode:    {}\n",
        if live {
            "LIVE (real triage model classifies and infers)"
        } else {
            "MOCK (deterministic stand-in; gates the pipeline, not the model)"
        }
    );

    let started = std::time::Instant::now();
    let result = replay(&lines, &labels, &options, &EventSender::log_only()).await?;
    let elapsed = started.elapsed();

    println!(
        "Replayed {} frames and {} events in {:.2}s — {} project switches, \
         {} inference attempts ({} applied), {} idle frames\n",
        result.frames,
        result.emitted.len(),
        elapsed.as_secs_f64(),
        result.switches.len(),
        result.inference_attempts,
        result.inferences,
        result.idle_frames
    );

    print_checkpoints(&result.checkpoints);

    let scores = score_checkpoints(&result.checkpoints);

    let project_floor = threshold(&args, "--project-recall", PROJECT_RECALL);
    let goal_floor = threshold(&args, "--goal-recall", GOAL_RECALL);
    let action_floor = threshold(&args, "--action-recall", ACTION_RECALL);

    let mut card = Scorecard::new();
    card.report(
        "checkpoints",
        format!("{} labeled", result.checkpoints.len()),
    );
    card.report(
        "labeled fields",
        scores
            .fields()
            .iter()
            .map(|(name, recall)| format!("{name} {}", recall.total))
            .collect::<Vec<_>>()
            .join(", "),
    );
    if live {
        let mut latencies = result.classify_ms.clone();
        latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        card.report(
            "classification latency",
            format!(
                "p50 {:.0}ms / p95 {:.0}ms",
                metrics::percentile(&latencies, 50),
                metrics::percentile(&latencies, 95)
            ),
        );
    }
    card.assert_metric(
        "project recall",
        scores.project.rate() as f64,
        Bound::AtLeast,
        project_floor,
        "",
    );
    card.assert_metric(
        "goal recall",
        scores.goal.rate() as f64,
        Bound::AtLeast,
        goal_floor,
        "",
    );
    card.assert_metric(
        "task recall",
        scores.task.rate() as f64,
        Bound::AtLeast,
        goal_floor,
        "",
    );
    card.assert_metric(
        "blocker recall",
        scores.blocker.rate() as f64,
        Bound::AtLeast,
        action_floor,
        "",
    );
    card.assert_metric(
        "last-action recall",
        scores.last_action.rate() as f64,
        Bound::AtLeast,
        action_floor,
        "",
    );
    // Spec §4.11: not a recall number, but the fixture's idle gap exists
    // precisely so this is measured on every run.
    card.assert_metric(
        "inferences during idle",
        result.idle_inferences as f64,
        Bound::Exactly,
        0.0,
        "",
    );

    std::process::exit(card.print());
}

fn print_checkpoints(checkpoints: &[CheckpointObservation]) {
    for observation in checkpoints {
        println!(
            "t={:>7.1}s  project={}",
            observation.t_ms as f64 / 1000.0,
            verdict_line(
                observation.expected.project.as_deref(),
                observation.observed.project.as_deref(),
                true
            )
        );
        for (label, expected, actual) in [
            (
                "goal",
                observation.expected.goal.as_deref(),
                observation.observed.goal.as_deref(),
            ),
            (
                "task",
                observation.expected.task.as_deref(),
                observation.observed.task.as_deref(),
            ),
            (
                "blocker",
                observation.expected.blocker.as_deref(),
                observation.observed.blocker.as_deref(),
            ),
            (
                "last_action",
                observation.expected.last_action.as_deref(),
                observation.observed.last_action.as_deref(),
            ),
        ] {
            if expected.is_none() {
                continue;
            }
            println!(
                "            {label:<12}{}",
                verdict_line(expected, actual, false)
            );
        }
    }
}

fn verdict_line(expected: Option<&str>, actual: Option<&str>, exact: bool) -> String {
    let Some(expected) = expected else {
        return format!("(unlabeled, got {})", show(actual));
    };
    let hit = if exact {
        metrics::exact_matches(expected, actual)
    } else {
        metrics::field_matches(expected, actual)
    };
    let score = actual
        .map(|actual| metrics::overlap(expected, actual))
        .unwrap_or(0.0);
    format!(
        "{} want \"{}\" got {}{}",
        if hit { "OK  " } else { "MISS" },
        truncate(expected, 60),
        show(actual),
        if exact {
            String::new()
        } else {
            format!(" (overlap {score:.2})")
        }
    )
}

fn show(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("\"{}\"", truncate(value, 72)),
        None => "(none)".to_string(),
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let cut: String = value.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn threshold(args: &[String], name: &str, default: f64) -> f64 {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(default)
}

/// Loads the real triage model with the same adaptive resource plan the
/// runtime resolves — the pattern `continuum-triage-bench` established.
fn load_triage_model(args: &[String]) -> Result<continuum_core::triage::llm::TriageLayer> {
    use continuum_core::config::{continuum_dev_dir, load_config};
    use continuum_core::triage::llm::{TriageConfig, TriageLayer};

    let dev_dir = continuum_dev_dir();
    let mut kcfg = load_config(&dev_dir.join("config.toml")).unwrap_or_default();
    let specs = continuum_core::hardware::probe_hardware();
    let plan = continuum_core::hardware::resolve_resource_policy(&specs, &kcfg.resources);
    kcfg.triage.gpu_layers = plan.triage_gpu_layers;
    let n_threads = args
        .iter()
        .position(|a| a == "--threads")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(plan.triage_threads);

    let model_path = kcfg.triage.resolve_model_path(&dev_dir);
    anyhow::ensure!(
        model_path.exists(),
        "Triage model not found at {} — run `powershell scripts/download-models.ps1` \
         or drop --live to use the deterministic mock",
        model_path.display()
    );
    println!("Loading triage model from {}…", model_path.display());
    TriageLayer::new(TriageConfig {
        model_path: model_path.to_string_lossy().into_owned(),
        context_size: kcfg.triage.context_size,
        n_threads,
        gpu_layers: kcfg.triage.gpu_layers,
        max_tokens: kcfg.triage.max_tokens,
        temperature: kcfg.triage.temperature,
        latency_warn_ms: kcfg.triage.latency_warn_ms,
    })
}
