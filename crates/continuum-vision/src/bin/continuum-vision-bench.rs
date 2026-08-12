//! Deterministic semantic benchmark for Continuum's local vision sense.

use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use continuum_vision::gguf::GgufVisionModel;
use continuum_vision::onnx::OnnxVisionModel;
use continuum_vision::VisionModel;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct EvalCase {
    name: String,
    file: PathBuf,
    required_any: Vec<Vec<String>>,
    forbidden: Vec<String>,
}

#[derive(Debug)]
struct CaseScore {
    matched: usize,
    total: usize,
    forbidden_hits: Vec<String>,
}

fn score_description(description: &str, case: &EvalCase) -> CaseScore {
    let normalized = description.to_lowercase();
    let matched = case
        .required_any
        .iter()
        .filter(|alternatives| {
            alternatives
                .iter()
                .any(|term| normalized.contains(&term.to_lowercase()))
        })
        .count();
    let forbidden_hits = case
        .forbidden
        .iter()
        .filter(|term| normalized.contains(&term.to_lowercase()))
        .cloned()
        .collect();

    CaseScore {
        matched,
        total: case.required_any.len(),
        forbidden_hits,
    }
}

fn default_model_dir() -> Result<PathBuf> {
    let profile = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .context("USERPROFILE and HOME are both unavailable")?;
    Ok(PathBuf::from(profile).join(".continuum-dev/models/vision/smolvlm-500m"))
}

fn mean_duration(total: Duration, count: usize) -> Duration {
    if count == 0 {
        Duration::ZERO
    } else {
        total / u32::try_from(count).unwrap_or(u32::MAX)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture_dir = manifest_dir.join("tests/fixtures");
    let manifest_path = env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| fixture_dir.join("vision-eval.json"));
    let model_dir = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(default_model_dir)?;
    let gpu = env::var("CONTINUUM_VISION_BENCH_GPU")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE"))
        .unwrap_or(false);

    let manifest = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let cases: Vec<EvalCase> = serde_json::from_str(&manifest)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    if cases.is_empty() {
        bail!("vision evaluation manifest contains no cases");
    }

    println!("Loading vision model: {}", model_dir.display());
    let load_started = Instant::now();
    let model: Box<dyn VisionModel> = if model_dir.join("model-q4_k_m.gguf").is_file() {
        Box::new(
            GgufVisionModel::new(
                &model_dir,
                gpu,
                "Describe the visible computer screen accurately. Include the application or scene, the main action or status, important readable text, and any visible error. Use one concise factual sentence.",
                64,
            )
            .await?,
        )
    } else {
        Box::new(OnnxVisionModel::new(&model_dir, gpu).await?)
    };
    println!(
        "Loaded {} in {:.2?}",
        model.model_name(),
        load_started.elapsed()
    );

    let mut matched = 0usize;
    let mut concepts = 0usize;
    let mut forbidden_hits = 0usize;
    let mut total_latency = Duration::ZERO;

    for case in &cases {
        let image_path = fixture_dir.join(&case.file);
        let image = image::open(&image_path)
            .with_context(|| format!("failed to open {}", image_path.display()))?;
        let started = Instant::now();
        let output = model.describe(&image).await?;
        let latency = started.elapsed();
        let score = score_description(&output.description, case);

        matched += score.matched;
        concepts += score.total;
        forbidden_hits += score.forbidden_hits.len();
        total_latency += latency;

        println!("\n[{}]", case.name);
        println!("  caption: {}", output.description);
        println!(
            "  concepts: {}/{} | forbidden: {:?} | confidence: {:.3} | latency: {:.2?}",
            score.matched, score.total, score.forbidden_hits, output.confidence, latency
        );
    }

    let semantic_score = matched as f64 / concepts as f64 * 100.0;
    println!("\n=== Continuum vision benchmark ===");
    println!("Model: {}", model.model_name());
    println!("Semantic concepts: {matched}/{concepts} ({semantic_score:.1}%)");
    println!("Forbidden hallucinations: {forbidden_hits}");
    println!(
        "Mean inference latency: {:.2?}",
        mean_duration(total_latency, cases.len())
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoring_accepts_alternatives_case_insensitively() {
        let case = EvalCase {
            name: "test".into(),
            file: "test.png".into(),
            required_any: vec![vec!["failed".into(), "error".into()], vec!["IDE".into()]],
            forbidden: vec!["successful".into()],
        };

        let score = score_description("The IDE shows a build ERROR.", &case);
        assert_eq!(score.matched, 2);
        assert_eq!(score.total, 2);
        assert!(score.forbidden_hits.is_empty());
    }

    #[test]
    fn scoring_reports_forbidden_hallucinations() {
        let case = EvalCase {
            name: "test".into(),
            file: "test.png".into(),
            required_any: vec![vec!["dashboard".into()]],
            forbidden: vec!["crash".into()],
        };

        let score = score_description("The dashboard reports a crash.", &case);
        assert_eq!(score.matched, 1);
        assert_eq!(score.forbidden_hits, vec!["crash"]);
    }
}
