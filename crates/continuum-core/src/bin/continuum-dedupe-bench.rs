//! # continuum-dedupe-bench
//!
//! The **dedupe-precision** harness (context engine spec §9): replays the
//! committed fixture through the *real* events writer into a throwaway
//! SQLite database and measures collapse against the §4.6 algorithm.
//!
//! What it asserts:
//!
//! - **≥ 90 % collapse on the build-failure loop.** The fixture's thirty
//!   error frames each carry a *different* summary. Spec §4.6 keys
//!   classified screen/audio events on
//!   `hash(source, event_type, project_id, application)` and deliberately
//!   **not** on the summary, precisely so the flagship "build failed ×30"
//!   case collapses regardless of summary variance. If someone ever adds
//!   the summary back into the key, this number falls off a cliff.
//! - **No distinct-event loss.** The distinct dedupe keys in the database
//!   must be exactly the ones the replay emitted: collapsing is only
//!   correct if nothing *different* was collapsed away. The simcharts
//!   TypeScript loop exists for exactly this reason — same `error` type,
//!   different project and application, so it must keep its own row.
//! - **No occurrence loss.** `SUM(count)` over the rows must equal the
//!   number of events emitted: every occurrence is a row or a bump.
//!
//! The measurement lives in [`continuum_core::bench::score::run_dedupe`],
//! which is where its unit gate runs too; this binary is the CLI around it.
//!
//! # Usage
//!
//! ```bash
//! cargo run --bin continuum-dedupe-bench
//! cargo run --bin continuum-dedupe-bench -- --collapse 0.95
//! ```

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use continuum_core::bench::fixture;
use continuum_core::bench::metrics::{Bound, Scorecard};
use continuum_core::bench::score::{self, COLLAPSE_FLOOR};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .compact()
        .init();

    println!("Continuum Dedupe-Precision Benchmark");
    println!("====================================\n");

    let args: Vec<String> = std::env::args().collect();
    let collapse_floor = args
        .iter()
        .position(|a| a == "--collapse")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(COLLAPSE_FLOOR);

    let lines = fixture::load_fixture(&fixture::fixture_path())?;
    let labels = fixture::load_labels(&fixture::labels_path())?;
    println!("Fixture: {} lines\n", lines.len());

    let scores = score::run_dedupe(&lines, &labels).await?;

    println!(
        "{:<18} {:>7} {:>6} {:>7}  first summary",
        "dedupe key", "sent", "rows", "count"
    );
    println!("{}", "-".repeat(96));
    for (key, sent) in &scores.occurrences {
        println!(
            "{key:<18} {sent:>7} {:>6} {:>7}  {}",
            scores.rows_per_key.get(key).copied().unwrap_or(0),
            scores.counted_per_key.get(key).copied().unwrap_or(0),
            scores
                .summary_per_key
                .get(key)
                .map(|s| truncate(s, 46))
                .unwrap_or_else(|| "(row missing!)".to_string())
        );
    }

    let mut card = Scorecard::new();
    card.report("events emitted", scores.emitted.to_string());
    card.report("rows written", scores.rows.to_string());
    card.report(
        "overall collapse",
        format!("{:.1}%", scores.overall_collapse() * 100.0),
    );
    card.report(
        "build-failure loop",
        format!(
            "{} occurrences → {} row(s)",
            scores.loop_occurrences, scores.loop_rows
        ),
    );
    if !scores.invented_keys.is_empty() {
        card.report("keys not emitted", format!("{:?}", scores.invented_keys));
    }
    if !scores.lost_keys.is_empty() {
        card.report("keys lost", format!("{:?}", scores.lost_keys));
    }
    card.assert_metric(
        "build-loop collapse",
        scores.loop_collapse(),
        Bound::AtLeast,
        collapse_floor,
        "",
    );
    card.assert_metric(
        "distinct keys persisted",
        scores.actual_keys as f64,
        Bound::Exactly,
        scores.expected_keys as f64,
        "",
    );
    card.assert_metric(
        "distinct keys lost",
        scores.lost_keys.len() as f64,
        Bound::Exactly,
        0.0,
        "",
    );
    card.assert_metric(
        "occurrences accounted (SUM(count))",
        scores.total_counted as f64,
        Bound::Exactly,
        scores.emitted as f64,
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
