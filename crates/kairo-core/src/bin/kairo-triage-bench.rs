//! # kairo-triage-bench
//!
//! Benchmarks the triage layer against a set of hand-labeled perception frames.
//!
//! Loads frames from `benchmarks/triage-frames.jsonl`, runs each through
//! `TriageLayer::evaluate`, and reports accuracy, latency statistics, and
//! per-frame results.
//!
//! # Usage
//!
//! ```bash
//! cargo run --bin kairo-triage-bench
//! ```
//!
//! Requires the triage model to be downloaded first:
//! ```bash
//! powershell scripts/download-models.ps1
//! ```

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

use kairo_core::config::kairo_dev_dir;
use kairo_core::senses::types::PerceptionFrame;
use kairo_core::triage::llm::{TriageConfig, TriageLayer};

/// A single benchmark entry: expected decision + perception frame.
#[derive(Debug, Deserialize)]
struct BenchEntry {
    expected: String,
    frame: PerceptionFrame,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,kairo_core::triage=info")),
        )
        .with_target(false)
        .compact()
        .init();

    println!("Kairo Triage Benchmark");
    println!("======================\n");

    // Locate model.
    let model_path = kairo_dev_dir()
        .join("models")
        .join("triage")
        .join("qwen3-4b-q4_k_m.gguf");

    if !model_path.exists() {
        eprintln!(
            "ERROR: Triage model not found at {}\n\
             Run: powershell scripts/download-models.ps1",
            model_path.display()
        );
        std::process::exit(1);
    }

    // Load benchmark frames.
    let bench_path = PathBuf::from("benchmarks/triage-frames.jsonl");
    if !bench_path.exists() {
        eprintln!("ERROR: Benchmark frames not found at {}", bench_path.display());
        std::process::exit(1);
    }

    let bench_data =
        std::fs::read_to_string(&bench_path).context("Failed to read benchmark file")?;

    let entries: Vec<BenchEntry> = bench_data
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("Failed to parse benchmark line {}", i + 1))
        })
        .collect::<Result<Vec<_>>>()?;

    println!("Loaded {} benchmark frames", entries.len());
    println!("Model: {}\n", model_path.display());

    // Debug: dump generated grammar
    let grammar = kairo_core::triage::prompts::build_triage_grammar();
    println!("--- Generated GBNF grammar ({} bytes) ---", grammar.len());
    println!("{}", &grammar[..grammar.len().min(2000)]);
    println!("--- End grammar ---\n");

    // Initialize triage layer.
    // Use most CPU threads for prompt processing speed.
    let n_threads = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
        .max(4)
        .min(14); // Leave 2 cores for the OS

    println!("Threads: {}", n_threads);

    let config = TriageConfig {
        model_path: model_path.to_string_lossy().into_owned(),
        context_size: 2048, // Triage prompts are short, save memory
        n_threads,
        gpu_layers: 0,
        max_tokens: 512, // Must accommodate Qwen 3 thinking tokens + JSON output
        temperature: 0.0,
        latency_warn_ms: 2000,
    };

    println!("Loading model...");
    let triage = TriageLayer::new(config)?;

    println!("Warming up...");
    triage.warmup().await?;
    println!("Ready.\n");

    // Run benchmark.
    let mut correct = 0u32;
    let mut total = 0u32;
    let mut latencies: Vec<f64> = Vec::new();

    println!(
        "{:<4} {:<18} {:<18} {:<8} {:<10}",
        "#", "Expected", "Got", "Match", "Latency"
    );
    println!("{}", "-".repeat(62));

    for (i, entry) in entries.iter().enumerate() {
        let start = Instant::now();
        let decision = triage.evaluate(&entry.frame, "").await;
        let elapsed = start.elapsed();
        let latency_ms = elapsed.as_secs_f64() * 1000.0;
        latencies.push(latency_ms);

        let got = decision.variant_name();
        let matched = got == entry.expected;
        if matched {
            correct += 1;
        }
        total += 1;

        let match_str = if matched { "OK" } else { "MISS" };
        println!(
            "{:<4} {:<18} {:<18} {:<8} {:.0}ms",
            i + 1,
            entry.expected,
            got,
            match_str,
            latency_ms
        );
    }

    // Statistics.
    println!("\n{}", "=".repeat(62));

    let accuracy = if total > 0 {
        correct as f64 / total as f64 * 100.0
    } else {
        0.0
    };

    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&latencies, 50);
    let p95 = percentile(&latencies, 95);
    let p99 = percentile(&latencies, 99);

    println!("\nResults:");
    println!("  Accuracy:  {correct}/{total} ({accuracy:.1}%)");
    println!("  Latency p50: {p50:.0}ms");
    println!("  Latency p95: {p95:.0}ms");
    println!("  Latency p99: {p99:.0}ms");

    // Pass/fail.
    let pass = accuracy >= 80.0 && p95 < 1500.0;
    println!();
    if pass {
        println!("RESULT: PASS");
    } else {
        println!("RESULT: FAIL");
        if accuracy < 80.0 {
            println!("  - Accuracy {accuracy:.1}% < 80% threshold");
        }
        if p95 >= 1500.0 {
            println!("  - P95 latency {p95:.0}ms >= 1500ms threshold");
        }
    }

    std::process::exit(if pass { 0 } else { 1 });
}

/// Compute the Nth percentile from a sorted slice.
fn percentile(sorted: &[f64], pct: u32) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((pct as f64 / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}
