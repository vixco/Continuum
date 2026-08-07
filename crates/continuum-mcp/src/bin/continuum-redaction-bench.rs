//! # continuum-redaction-bench
//!
//! The **safety-redaction** harness (context engine spec §9): pushes a
//! synthetic secret corpus through every stage of the context engine and
//! asserts that **zero secrets survive** — while the false-positive corpus
//! (git commit ids and friends) **does**.
//!
//! This binary is the CLI around `continuum_mcp::redaction`, which holds
//! the corpus, the stages, and the unit gate. See that module for what
//! each stage covers and why the harness lives in this crate.
//!
//! # Usage
//!
//! ```bash
//! cargo run --bin continuum-redaction-bench
//! ```

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use continuum_core::bench::metrics::{Bound, Scorecard};
use continuum_core::bench::BenchDir;
use continuum_core::config::{ContextConfig, PrivacyConfig};
use continuum_core::senses::privacy::PrivacyFilter;
use continuum_mcp::redaction::{self, Report, HOME_DIR, SECRETS, SURVIVORS, USERNAME};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .compact()
        .init();

    println!("Continuum Safety-Redaction Benchmark");
    println!("====================================\n");

    let filter = PrivacyFilter::from_config(&ContextConfig::default(), &PrivacyConfig::default())
        .with_environment(Some(HOME_DIR.to_string()), Some(USERNAME.to_string()));

    let mut report = Report::default();
    redaction::stage_privacy(&filter, &mut report);
    let dir = BenchDir::new("redaction")?;
    redaction::stage_raw_log_and_events(dir.path(), &filter, &mut report).await?;
    redaction::stage_wake_package(&filter, &mut report);
    let omitted_private = redaction::stage_mcp_tools(dir.path(), &filter, &mut report).await?;

    println!("Surfaces scanned ({}):", report.surfaces.len());
    for surface in &report.surfaces {
        println!("  {surface}");
    }
    if !report.leaks.is_empty() {
        println!("\nLEAKS:");
        for leak in &report.leaks {
            println!("  {} / {}: {}", leak.stage, leak.surface, leak.secret);
        }
    }
    if !report.missing_survivors.is_empty() {
        println!("\nFALSE POSITIVES (should have survived):");
        for (label, where_) in &report.missing_survivors {
            println!("  {label} — {where_}");
        }
    }

    let mut card = Scorecard::new();
    card.report("secret corpus", format!("{} secrets", SECRETS.len()));
    card.report(
        "false-positive corpus",
        format!("{} survivors + one 40-char commit id", SURVIVORS.len()),
    );
    card.report("surfaces scanned", report.surfaces.len().to_string());
    card.report(
        "private rows withheld",
        format!("{omitted_private} (counted, not silently dropped)"),
    );
    card.assert_metric(
        "secrets surviving",
        report.leaks.len() as f64,
        Bound::Exactly,
        0.0,
        "",
    );
    card.assert_metric(
        "false positives redacted",
        report.missing_survivors.len() as f64,
        Bound::Exactly,
        0.0,
        "",
    );
    // The `omitted_private` contract (Task C4): a withheld row is counted,
    // so a caller learns that something exists without learning what. A
    // zero here would mean the local_only path never ran.
    card.assert_metric(
        "private rows counted",
        f64::from(omitted_private),
        Bound::AtLeast,
        1.0,
        "",
    );

    std::process::exit(card.print());
}
