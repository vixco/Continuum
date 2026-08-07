//! # continuum-memory-precision-bench
//!
//! The **memory-precision** harness (context engine spec §9): replays the
//! committed fixture into a throwaway raw log + vault, runs the *real*
//! distillation and curation paths over what came out, and scores what
//! ended up in memory.
//!
//! What it asserts:
//!
//! - **Duplicate rate ≤ 10 %.** Over every artifact that reached memory
//!   (distilled episodic memories + curated vault notes), how many are a
//!   near-copy of an earlier one — counted *within* each population, since
//!   a memory and the note it produced describe the same moment on
//!   purpose. This is what the whole compression ladder is for: thirty
//!   build failures must become one memory, not thirty.
//! - **Precision ≥ 70 % vs the labels.** A memory scores as correct when
//!   its **project attribution matches the labeled project at its
//!   timestamp** and its summary is **grounded** in the labeled narrative
//!   (it shares a significant token with the label corpus). Memories with
//!   no project attribution are excluded from the denominator, not counted
//!   as wrong: the distiller's frame-fallback path produces unattributed
//!   memories *by design* (Task B6 — the resolver's current project says
//!   nothing about a frame from twenty minutes ago).
//!
//! And reports, without a threshold (spec §9: "later-used reporting via
//! `last_used`"): the share of curated notes that a wake-time retrieval
//! actually surfaced.
//!
//! # Duplicate measure: text similarity, not embeddings
//!
//! The spec's preferred comparator is embedding similarity over the
//! episodic store. That store is LanceDB plus a fastembed model that is
//! **downloaded on first use** — a bench that reaches for the network is
//! not CI-feasible, not offline-safe, and not deterministic, and Continuum
//! does not phone home. So this harness runs the distiller's row selection
//! and mapping (`query_undistilled_events` + `event_to_memory_event`, the
//! exact functions `distill_once` calls) and compares the resulting
//! summaries with [`continuum_core::bench::metrics::text_similarity`], the
//! documented fallback. The one step it skips is the vector write itself.
//!
//! The measurement lives in
//! [`continuum_core::bench::score::run_memory_precision`], which is where
//! its unit gate runs too; this binary is the CLI around it.
//!
//! # Usage
//!
//! ```bash
//! cargo run --bin continuum-memory-precision-bench
//! cargo run --bin continuum-memory-precision-bench -- --precision 0.8 --duplicates 0.05
//! ```

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use continuum_core::bench::fixture;
use continuum_core::bench::metrics::{Bound, Scorecard};
use continuum_core::bench::score::{self, DUPLICATE_CEILING, PRECISION_FLOOR};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .compact()
        .init();

    println!("Continuum Memory-Precision Benchmark");
    println!("====================================\n");

    let args: Vec<String> = std::env::args().collect();
    let number = |name: &str, default: f64| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(default)
    };
    let precision_floor = number("--precision", PRECISION_FLOOR);
    let duplicate_ceiling = number("--duplicates", DUPLICATE_CEILING);

    let lines = fixture::load_fixture(&fixture::fixture_path())?;
    let labels = fixture::load_labels(&fixture::labels_path())?;
    let scores = score::run_memory_precision(&lines, &labels).await?;

    println!("Distilled memories ({}):", scores.memories.len());
    for memory in &scores.memories {
        println!(
            "  [{}] {:<12} {}",
            memory.ts.format("%H:%M:%S"),
            memory.project.as_deref().unwrap_or("(none)"),
            truncate(&memory.summary, 92)
        );
    }
    println!("\nCurated vault notes ({}):", scores.notes.len());
    for note in &scores.notes {
        println!("  {:<10} {}", note.kind, truncate(&note.title, 92));
    }

    let mut card = Scorecard::new();
    card.report(
        "duplicate comparator",
        "normalized-text similarity (offline fallback)",
    );
    card.report(
        "memories distilled",
        format!(
            "{} from {} event rows (min_event_importance {}) + the raw-frame \
             fallback (min_salience {})",
            scores.memories.len(),
            scores.event_rows,
            scores.min_event_importance,
            scores.min_salience
        ),
    );
    card.report(
        "candidates curated",
        format!(
            "{} proposed → {} written ({} gated as repeats of an open events row, \
             {} suppressed by the vault's title check)",
            scores.candidates_proposed + scores.candidates_suppressed,
            scores.notes.len(),
            scores.candidates_suppressed,
            scores.candidates_proposed - scores.notes.len()
        ),
    );
    card.report(
        "precision denominator",
        format!(
            "{} attributed memories ({} unattributed, excluded by design)",
            scores.scored, scores.unattributed
        ),
    );
    card.report(
        "wake retrieval hits",
        format!(
            "{} vault note(s) returned by retrieve_vault_context for the final frame",
            scores.retrieval_hits
        ),
    );
    card.report(
        "later-used rate",
        format!(
            "{:.1}% ({} of {} confirmed notes surfaced by a wake-time retrieval)",
            scores.later_used_rate() * 100.0,
            scores.later_used,
            scores.notes.len()
        ),
    );

    card.assert_metric(
        "duplicate rate",
        scores.duplicate_rate(),
        Bound::AtMost,
        duplicate_ceiling,
        "",
    );
    card.assert_metric(
        "precision vs labels",
        scores.precision(),
        Bound::AtLeast,
        precision_floor,
        "",
    );

    std::process::exit(card.print());
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let cut: String = value.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}
