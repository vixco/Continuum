//! # Triage LLM interface
//!
//! Handles communication with the local LLM used by the triage layer.
//! Wraps the `kairo-llm` crate to provide a triage-specific API that
//! sends perception frames and receives structured decisions.
//!
//! Includes 3-retry fallback logic: grammar mode first, then prompt-only
//! retries with "JSON only, no prose" appended, defaulting to Ignore on
//! total failure.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, error, info, warn};

use kairo_llm::{GenerateOpts, LlmConfig, LocalLlm};

use crate::senses::types::PerceptionFrame;
use crate::triage::prompts::build_triage_prompt;
use crate::triage::TriageDecision;

/// Configuration for the triage layer.
#[derive(Debug, Clone)]
pub struct TriageConfig {
    /// Path to the GGUF model file.
    pub model_path: String,
    /// Context window size in tokens.
    pub context_size: u32,
    /// Number of CPU threads.
    pub n_threads: u32,
    /// Number of GPU layers to offload.
    pub gpu_layers: u32,
    /// Maximum tokens for triage generation.
    pub max_tokens: u32,
    /// Temperature for sampling. Low for classification tasks.
    pub temperature: f32,
    /// Latency warning threshold in milliseconds.
    pub latency_warn_ms: u64,
}

impl Default for TriageConfig {
    fn default() -> Self {
        Self {
            model_path: String::new(),
            context_size: 4096,
            n_threads: 4,
            gpu_layers: 999,
            max_tokens: 128,
            temperature: 0.1,
            latency_warn_ms: 2000,
        }
    }
}

/// The triage layer: evaluates perception frames using a local LLM.
///
/// Thread-safe and cloneable (wraps `LocalLlm` which uses `Arc` internally).
#[derive(Clone)]
pub struct TriageLayer {
    llm: LocalLlm,
    config: TriageConfig,
    /// Counter of consecutive total failures (3 retries all failed).
    consecutive_failures: std::sync::Arc<AtomicU32>,
}

impl TriageLayer {
    /// Create a new triage layer by loading the LLM model.
    ///
    /// This is a blocking operation (model load). Call during startup.
    pub fn new(config: TriageConfig) -> Result<Self> {
        info!(
            layer = "triage",
            component = "layer",
            model = %config.model_path,
            "Initializing triage layer"
        );

        let llm_config = LlmConfig {
            model_path: config.model_path.clone(),
            context_size: config.context_size,
            n_threads: config.n_threads,
            n_threads_batch: config.n_threads,
            gpu_layers: config.gpu_layers,
            max_tokens: config.max_tokens,
        };

        let llm = LocalLlm::new(llm_config)?;

        Ok(Self {
            llm,
            config,
            consecutive_failures: std::sync::Arc::new(AtomicU32::new(0)),
        })
    }

    /// Run a warmup generation to prime caches.
    pub async fn warmup(&self) -> Result<()> {
        self.llm.warmup().await
    }

    /// Evaluate a perception frame and return a triage decision.
    ///
    /// Uses GBNF grammar-constrained generation as primary method.
    /// Falls back to prompt-only generation with retries on grammar failure.
    /// Returns `TriageDecision::Ignore` as safe default on total failure.
    pub async fn evaluate(
        &self,
        frame: &PerceptionFrame,
        memory_summary: &str,
    ) -> TriageDecision {
        let start = Instant::now();
        let prompt = build_triage_prompt(frame, memory_summary);

        let opts = GenerateOpts {
            temperature: 0.0, // Greedy for classification — deterministic, no randomness
            top_k: 1,
            top_p: 1.0,
            max_tokens: Some(self.config.max_tokens),
            seed: 0,
        };

        // Prompt-only generation with brace-depth early stopping.
        // Grammar mode is disabled due to GGML_ASSERT crashes on Qwen 3.
        // The early-stop-on-} in kairo-llm cuts generation as soon as the
        // JSON object closes, keeping output to ~10-30 tokens.
        for attempt in 1..=3 {
            match self.llm.generate(&prompt, &opts).await {
                Ok(raw) => {
                    let trimmed = raw.trim();
                    warn!(
                        layer = "triage",
                        component = "llm",
                        attempt = attempt,
                        raw_output = %trimmed,
                        "Fallback attempt raw output"
                    );

                    if let Some(decision) = TriageDecision::from_json(trimmed) {
                        let elapsed = start.elapsed();
                        self.consecutive_failures.store(0, Ordering::Relaxed);
                        self.log_evaluation(frame, &decision, elapsed.as_millis() as u64);
                        return decision.truncated();
                    }
                }
                Err(e) => {
                    warn!(
                        layer = "triage",
                        component = "llm",
                        error = %e,
                        attempt = attempt,
                        "Fallback generation failed"
                    );
                }
            }
        }

        // Total failure: all 3 attempts failed.
        let prev = self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        let consecutive = prev + 1;

        warn!(
            layer = "triage",
            component = "llm",
            frame_id = %frame.id,
            consecutive_failures = consecutive,
            "All 3 triage attempts failed, defaulting to Ignore"
        );

        if consecutive >= 3 {
            // 3 consecutive total failures — raise a health alert.
            error!(
                layer = "triage",
                component = "health",
                consecutive_failures = consecutive,
                "HEALTH ALERT: Triage layer has failed {consecutive} consecutive evaluations. \
                 Model may be corrupt, misconfigured, or OOM. Check logs and model file."
            );
        }

        let elapsed = start.elapsed();
        debug!(
            layer = "triage",
            component = "llm",
            latency_ms = elapsed.as_millis() as u64,
            "Triage defaulted to Ignore after total failure"
        );

        TriageDecision::Ignore
    }

    /// Log the evaluation result with latency tracking.
    fn log_evaluation(&self, frame: &PerceptionFrame, decision: &TriageDecision, latency_ms: u64) {
        if latency_ms > self.config.latency_warn_ms {
            warn!(
                layer = "triage",
                component = "llm",
                latency_ms = latency_ms,
                threshold_ms = self.config.latency_warn_ms,
                frame_id = %frame.id,
                decision = decision.variant_name(),
                "Triage evaluation exceeded latency threshold"
            );
        } else {
            debug!(
                layer = "triage",
                component = "llm",
                latency_ms = latency_ms,
                frame_id = %frame.id,
                decision = decision.variant_name(),
                "Triage evaluation complete"
            );
        }
    }
}
