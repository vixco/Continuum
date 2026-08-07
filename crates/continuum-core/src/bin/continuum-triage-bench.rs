//! # continuum-triage-bench
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
//! cargo run --bin continuum-triage-bench                     # accuracy + latency (needs the model)
//! cargo run --bin continuum-triage-bench -- --prompt-fit-only # gate only, no model, no GPU
//! ```
//!
//! The accuracy/latency run requires the triage model to be downloaded
//! first:
//! ```bash
//! powershell scripts/download-models.ps1
//! ```
//!
//! `--prompt-fit-only` runs just the §4.7 prompt-fit gate and the §4.10
//! blob-leak scan — over the hand-labeled benchmark frames **and** the
//! Task C6 context fixture (`crates/continuum-core/benches/data/`), whose
//! frames are longer and more varied than the benchmark's. That half needs
//! no model, so it can gate a change; the accuracy and latency
//! re-baseline still needs the GPU and stays a manual step.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

use continuum_core::config::{continuum_dev_dir, load_config};
use continuum_core::senses::types::PerceptionFrame;
use continuum_core::triage::llm::{TriageConfig, TriageLayer};

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
                .unwrap_or_else(|_| EnvFilter::new("warn,continuum_core::triage=info")),
        )
        .with_target(false)
        .compact()
        .init();

    println!("Continuum Triage Benchmark");
    println!("======================\n");

    // Load config so bench honours the same triage knobs as the runtime.
    let dev_dir = continuum_dev_dir();
    let cfg_path = dev_dir.join("config.toml");
    let mut kcfg = load_config(&cfg_path).unwrap_or_default();

    // Adaptive resource policy: resolve the same plan the runtime uses so the
    // bench reflects real-world thread / GPU settings. `--threads N` overrides
    // the resolved thread count for explicit benchmarking.
    let hardware_specs = continuum_core::hardware::probe_hardware();
    let resource_plan =
        continuum_core::hardware::resolve_resource_policy(&hardware_specs, &kcfg.resources);
    kcfg.triage.gpu_layers = resource_plan.triage_gpu_layers;
    let args: Vec<String> = std::env::args().collect();
    let n_threads: u32 = args
        .iter()
        .position(|a| a == "--threads")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(resource_plan.triage_threads);
    println!(
        "Hardware: {} cores, {} MB RAM, CUDA={}, VRAM={:?} → triage_threads={}, gpu_layers={}",
        hardware_specs.logical_cores,
        hardware_specs.total_ram_mb,
        hardware_specs.has_cuda,
        hardware_specs.vram_mb,
        n_threads,
        kcfg.triage.gpu_layers
    );

    // Locate model. `--prompt-fit-only` never touches it.
    let prompt_fit_only = args.iter().any(|a| a == "--prompt-fit-only");
    let model_path = kcfg.triage.resolve_model_path(&dev_dir);

    if !model_path.exists() && !prompt_fit_only {
        eprintln!(
            "ERROR: Triage model not found at {}\n\
             Run: powershell scripts/download-models.ps1\n\
             (or pass --prompt-fit-only to run just the §4.7 prompt-fit gate)",
            model_path.display()
        );
        std::process::exit(1);
    }

    // Load benchmark frames.
    let bench_path = PathBuf::from("benchmarks/triage-frames.jsonl");
    if !bench_path.exists() {
        eprintln!(
            "ERROR: Benchmark frames not found at {}",
            bench_path.display()
        );
        std::process::exit(1);
    }

    let bench_data =
        std::fs::read_to_string(&bench_path).context("Failed to read benchmark file")?;

    let entries: Vec<BenchEntry> = bench_data
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim().starts_with("//"))
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("Failed to parse benchmark line {}", i + 1))
        })
        .collect::<Result<Vec<_>>>()?;

    println!("Loaded {} benchmark frames", entries.len());
    println!("Model: {}\n", model_path.display());

    // Prompt-fit gate (spec §4.7): prompt_tokens < n_ctx − max_tokens must
    // hold for every frame or generation silently truncates. Bytes/3.5 is
    // the accepted heuristic for Qwen-family tokenizers (bytes proxy).
    //
    // Task B4 tightens this: the prompt now renders the slim
    // `TriagePromptFrame` projection, so the ~1.4 kB compact world-state
    // blob (`screen.world_compact`, spec §4.10) is out of the budget. The
    // gate below asserts BOTH that the budget holds and that the blob is
    // genuinely absent — a regression that re-serialized the whole frame
    // would still fit the budget on these small benchmark frames and
    // silently pass an arithmetic-only check.
    //
    // Task C6 widens the corpus: the same gate also runs over the context
    // fixture's frames, which carry real window titles, per-monitor
    // captions and transcripts and are the closest thing to a recording
    // that is committed. A regression that only shows up on longer frames
    // would slip past the twenty hand-labeled benchmark ones.
    let mut corpus: Vec<(String, PerceptionFrame)> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (format!("benchmark frame {}", i + 1), e.frame.clone()))
        .collect();
    match continuum_core::bench::fixture::load_fixture(
        &continuum_core::bench::fixture::fixture_path(),
    ) {
        Ok(lines) => {
            let before = corpus.len();
            for (index, frame) in lines.iter().filter_map(|l| l.as_frame()).enumerate() {
                corpus.push((format!("fixture frame {}", index + 1), frame.clone()));
            }
            println!(
                "Prompt-fit corpus: {} benchmark frames + {} context-fixture frames",
                before,
                corpus.len() - before
            );
        }
        Err(error) => println!("Prompt-fit corpus: benchmark frames only ({error})"),
    }

    let mut leaked_blob: Option<String> = None;
    let mut worst = (0usize, String::new());
    for (label, frame) in &corpus {
        let prompt = continuum_core::triage::prompts::build_triage_prompt(frame, "");
        if leaked_blob.is_none()
            && (prompt.contains("world_compact") || prompt.contains("live-context/v"))
        {
            leaked_blob = Some(label.clone());
        }
        if prompt.len() > worst.0 {
            worst = (prompt.len(), label.clone());
        }
    }
    anyhow::ensure!(
        leaked_blob.is_none(),
        "Triage prompt leaked the compact world-state blob (spec §4.10) on {} \
         — the slim TriagePromptFrame projection was bypassed",
        leaked_blob.unwrap_or_default()
    );
    let max_prompt_bytes = worst.0;
    let est_prompt_tokens = (max_prompt_bytes as f64 / 3.5).ceil() as u32;
    let token_budget = kcfg
        .triage
        .context_size
        .saturating_sub(kcfg.triage.max_tokens);
    println!(
        "Prompt fit: worst-case {} bytes ({}) ≈ {} tokens; budget {} (n_ctx {} − max_tokens {})\n",
        max_prompt_bytes,
        worst.1,
        est_prompt_tokens,
        token_budget,
        kcfg.triage.context_size,
        kcfg.triage.max_tokens
    );
    anyhow::ensure!(
        est_prompt_tokens < token_budget,
        "Triage prompt does not fit: ~{est_prompt_tokens} tokens >= budget {token_budget} \
         (context_size {} − max_tokens {})",
        kcfg.triage.context_size,
        kcfg.triage.max_tokens
    );

    if prompt_fit_only {
        println!("RESULT: PASS (prompt-fit gate only — accuracy and latency need the model)");
        return Ok(());
    }

    // Debug: dump grammar
    let grammar = continuum_core::triage::prompts::TRIAGE_GRAMMAR;
    println!("--- GBNF grammar ({} bytes) ---", grammar.len());
    println!("{grammar}");
    println!("--- End grammar ---\n");

    // Initialize triage layer.
    // Threads come from the resolved adaptive resource plan (override with
    // `--threads N`); gpu_layers from the plan.
    println!("Threads: {}", n_threads);

    let config = TriageConfig {
        model_path: model_path.to_string_lossy().into_owned(),
        context_size: kcfg.triage.context_size,
        n_threads,
        gpu_layers: kcfg.triage.gpu_layers,
        max_tokens: kcfg.triage.max_tokens,
        temperature: kcfg.triage.temperature,
        latency_warn_ms: kcfg.triage.latency_warn_ms,
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
    let mut results: Vec<(&str, String)> = Vec::new(); // (expected, got)

    println!(
        "{:<4} {:<18} {:<18} {:<8} {:<10}",
        "#", "Expected", "Got", "Match", "Latency"
    );
    println!("{}", "-".repeat(62));

    for (i, entry) in entries.iter().enumerate() {
        let start = Instant::now();
        let decision = triage.evaluate(&entry.frame, "").await.decision;
        let elapsed = start.elapsed();
        let latency_ms = elapsed.as_secs_f64() * 1000.0;
        latencies.push(latency_ms);

        let got = decision.variant_name().to_owned();
        let matched = got == entry.expected;
        if matched {
            correct += 1;
        }
        total += 1;
        results.push((entry.expected.as_str(), got.clone()));

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

    // Per-decision accuracy breakdown.
    println!("\n{}", "=".repeat(62));

    let mut per_decision: std::collections::BTreeMap<&str, (u32, u32)> =
        std::collections::BTreeMap::new();
    for (expected, got) in &results {
        let (c, t) = per_decision.entry(*expected).or_insert((0, 0));
        *t += 1;
        if got == expected {
            *c += 1;
        }
    }

    println!("\nPer-decision accuracy:");
    for (decision, (c, t)) in &per_decision {
        let pct = if *t > 0 {
            *c as f64 / *t as f64 * 100.0
        } else {
            0.0
        };
        println!("  {:<18} {}/{} ({:.0}%)", decision, c, t, pct);
    }

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
