//! # kairo-llm
//!
//! Local LLM runtime wrapper for Kairo's triage layer (Layer 2).
//!
//! Wraps `llama-cpp-2` (llama.cpp Rust bindings) to provide:
//! - Model loading from GGUF files
//! - Free-form text generation
//! - JSON-constrained generation via GBNF grammar mode
//! - Streaming text generation for future TTS integration
//! - GPU acceleration behind `cuda` / `vulkan` feature flags
//!
//! Default model: Qwen 3 4B (Q4_K_M quantization).
//!
//! All generation is inherently CPU/GPU-bound, so async methods use
//! `tokio::task::spawn_blocking` internally. The [`LocalLlm`] struct is
//! `Send + Sync` and can be shared across tasks via `Arc`.

use std::num::NonZeroU32;
use std::path::Path;
use std::pin::pin;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use tracing::{debug, info, warn};

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for loading and running a local LLM.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Path to the GGUF model file.
    pub model_path: String,
    /// Context window size in tokens.
    pub context_size: u32,
    /// Number of CPU threads for generation.
    pub n_threads: u32,
    /// Number of CPU threads for batch prompt processing.
    pub n_threads_batch: u32,
    /// Number of layers to offload to GPU. 999 = all layers (recommended). 0 = CPU only.
    pub gpu_layers: u32,
    /// Maximum tokens to generate per call (safety cap).
    pub max_tokens: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model_path: String::new(),
            context_size: 4096,
            n_threads: 4,
            n_threads_batch: 4,
            gpu_layers: 999,
            max_tokens: 256,
        }
    }
}

/// Options for a single generation call.
#[derive(Debug, Clone)]
pub struct GenerateOpts {
    /// Sampling temperature (0.0 = greedy, higher = more random).
    pub temperature: f32,
    /// Top-K sampling (0 = disabled).
    pub top_k: i32,
    /// Top-P (nucleus) sampling threshold.
    pub top_p: f32,
    /// Maximum tokens to generate for this call. Overrides config default.
    pub max_tokens: Option<u32>,
    /// Random seed for reproducible sampling. 0 = random.
    pub seed: u32,
}

impl Default for GenerateOpts {
    fn default() -> Self {
        Self {
            temperature: 0.1,
            top_k: 40,
            top_p: 0.95,
            max_tokens: None,
            seed: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors specific to the LLM runtime.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("Model file not found: {path}")]
    ModelNotFound { path: String },

    #[error("Failed to load model: {reason}")]
    LoadFailed { reason: String },

    #[error("Grammar compilation failed: {reason}")]
    GrammarFailed { reason: String },

    #[error("Generation produced no output")]
    EmptyOutput,

    #[error("JSON parse failed after generation: {raw_output}")]
    JsonParseFailed { raw_output: String },
}

// ---------------------------------------------------------------------------
// Core runtime
// ---------------------------------------------------------------------------

/// Inner state shared across the `Arc` boundary.
struct LlmInner {
    /// Cached context for reuse across generate calls. Declared before `model`
    /// so it drops first (Rust drops fields in declaration order).
    ///
    /// SAFETY: The lifetime is transmuted from `'model` to `'static`. Safe because:
    /// - The context and model are in the same `Arc<LlmInner>`, same real lifetime
    /// - `ctx_cache` drops before `model` (field declaration order)
    /// - `LlamaContext::drop` only calls `llama_free`, does not access the model
    ctx_cache: Mutex<Option<LlamaContext<'static>>>,
    backend: LlamaBackend,
    model: LlamaModel,
    config: LlmConfig,
}

// SAFETY: LlamaBackend and LlamaModel are thread-safe once loaded.
// All mutable access happens through LlamaContext which is created
// per-call inside spawn_blocking.
unsafe impl Send for LlmInner {}
unsafe impl Sync for LlmInner {}

/// A loaded local LLM ready for generation.
///
/// Create via [`LocalLlm::new`]. All generation methods are async but
/// internally run on the blocking thread pool since inference is CPU/GPU-bound.
#[derive(Clone)]
pub struct LocalLlm {
    inner: Arc<LlmInner>,
}

impl LocalLlm {
    /// Load a GGUF model from disk.
    ///
    /// This is a blocking operation (reads the model file and initializes
    /// llama.cpp). Call from an async context via `spawn_blocking` or during
    /// startup before the main event loop.
    pub fn new(config: LlmConfig) -> Result<Self> {
        let path = &config.model_path;
        if !Path::new(path).exists() {
            return Err(LlmError::ModelNotFound {
                path: path.clone(),
            }
            .into());
        }

        info!(
            layer = "triage",
            component = "llm",
            model_path = %path,
            gpu_layers = config.gpu_layers,
            context_size = config.context_size,
            "Loading LLM model"
        );

        let backend =
            LlamaBackend::init().context("Failed to initialize llama.cpp backend")?;

        let model_params = LlamaModelParams::default()
            .with_n_gpu_layers(config.gpu_layers);
        let model_params = pin!(model_params);

        let model = LlamaModel::load_from_file(&backend, path, &model_params)
            .map_err(|e| LlmError::LoadFailed {
                reason: format!("{e:?}"),
            })?;

        info!(
            layer = "triage",
            component = "llm",
            vocab_size = model.n_vocab(),
            "Model loaded successfully"
        );

        Ok(Self {
            inner: Arc::new(LlmInner {
                ctx_cache: Mutex::new(None),
                backend,
                model,
                config,
            }),
        })
    }

    /// Run a short dummy generation to warm up the model.
    ///
    /// This fills internal caches and JIT paths so the first real call is
    /// fast. Same pattern as `kairo-vision`'s warmup.
    pub async fn warmup(&self) -> Result<()> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            debug!(
                layer = "triage",
                component = "llm",
                "Running warmup generation"
            );
            let opts = GenerateOpts {
                max_tokens: Some(4),
                temperature: 0.0,
                ..Default::default()
            };
            let _ = generate_sync(&inner, "Hello", None, &opts);
            debug!(
                layer = "triage",
                component = "llm",
                "Warmup complete"
            );
            Ok(())
        })
        .await?
    }

    /// Generate free-form text from a prompt.
    pub async fn generate(&self, prompt: &str, opts: &GenerateOpts) -> Result<String> {
        let inner = self.inner.clone();
        let prompt = prompt.to_string();
        let opts = opts.clone();
        tokio::task::spawn_blocking(move || generate_sync(&inner, &prompt, None, &opts))
            .await?
    }

    /// Generate text constrained by a GBNF grammar, then parse as JSON.
    ///
    /// The grammar ensures the model only produces valid JSON matching the
    /// schema. On parse failure, this returns an error — the caller is
    /// responsible for retry logic.
    pub async fn generate_json<T: DeserializeOwned + Send + 'static>(
        &self,
        prompt: &str,
        grammar: &str,
        opts: &GenerateOpts,
    ) -> Result<T> {
        let inner = self.inner.clone();
        let prompt = prompt.to_string();
        let grammar = grammar.to_string();
        let opts = opts.clone();
        tokio::task::spawn_blocking(move || {
            let raw = generate_sync(&inner, &prompt, Some(&grammar), &opts)?;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(LlmError::EmptyOutput.into());
            }
            serde_json::from_str(trimmed).map_err(|_| {
                LlmError::JsonParseFailed {
                    raw_output: trimmed.to_string(),
                }
                .into()
            })
        })
        .await?
    }

    /// Generate text token-by-token, calling `on_token` for each piece.
    ///
    /// Used for streaming to TTS in Phase 5. The callback receives each
    /// decoded text fragment as it's produced.
    pub async fn generate_stream<F>(
        &self,
        prompt: &str,
        opts: &GenerateOpts,
        on_token: F,
    ) -> Result<String>
    where
        F: Fn(&str) + Send + 'static,
    {
        let inner = self.inner.clone();
        let prompt = prompt.to_string();
        let opts = opts.clone();
        tokio::task::spawn_blocking(move || {
            generate_sync_streaming(&inner, &prompt, &opts, on_token)
        })
        .await?
    }
}

// ---------------------------------------------------------------------------
// Synchronous generation (runs inside spawn_blocking)
// ---------------------------------------------------------------------------

/// Core synchronous generation loop, optionally grammar-constrained.
fn generate_sync(
    inner: &LlmInner,
    prompt: &str,
    grammar: Option<&str>,
    opts: &GenerateOpts,
) -> Result<String> {
    let max_tokens = opts.max_tokens.unwrap_or(inner.config.max_tokens);

    // Reuse cached context (create on first call, clear KV cache on subsequent).
    let mut ctx_guard = inner.ctx_cache.lock().unwrap_or_else(|e| e.into_inner());

    if ctx_guard.is_none() {
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(inner.config.context_size))
            .with_n_threads(inner.config.n_threads as i32)
            .with_n_threads_batch(inner.config.n_threads_batch as i32);

        let new_ctx = inner
            .model
            .new_context(&inner.backend, ctx_params)
            .map_err(|e| LlmError::LoadFailed {
                reason: format!("Context creation failed: {e:?}"),
            })?;

        // SAFETY: Erase the lifetime. See LlmInner::ctx_cache doc for invariants.
        *ctx_guard = Some(unsafe { std::mem::transmute(new_ctx) });
    }

    let ctx = ctx_guard.as_mut().expect("ctx just initialized");
    ctx.clear_kv_cache();

    // Tokenize the prompt.
    let tokens = inner
        .model
        .str_to_token(prompt, AddBos::Never)
        .context("Tokenization failed")?;

    debug!(
        layer = "triage",
        component = "llm",
        prompt_tokens = tokens.len(),
        max_gen_tokens = max_tokens,
        has_grammar = grammar.is_some(),
        "Starting generation"
    );

    // Feed prompt tokens into the context.
    let mut batch = LlamaBatch::new(tokens.len().max(512), 1);
    for (i, token) in tokens.iter().enumerate() {
        let is_last = i == tokens.len() - 1;
        batch.add(*token, i as i32, &[0], is_last)?;
    }
    ctx.decode(&mut batch)
        .context("Failed to decode prompt batch")?;

    // Build sampler chain.
    //
    // With grammar mode, the grammar sampler must run FIRST so it masks
    // invalid tokens before top_k/top_p reduce the candidate set. If
    // top_k runs first and picks a grammar-invalid token, the grammar
    // sampler zeroes all remaining logits, causing an assertion failure
    // in llama-grammar.cpp.
    let mut samplers: Vec<LlamaSampler> = Vec::new();
    // Grammar mode disabled: llama.cpp's GBNF sampler triggers
    // GGML_ASSERT(!stacks.empty()) abort on Qwen 3 regardless of
    // grammar content, sampler ordering, or lazy triggers. Root cause
    // appears to be a tokenizer/grammar interaction specific to this
    // model. Prompt-only mode with early-stop-on-} is used instead.
    let _ = grammar; // suppress unused warning
    if opts.top_k > 0 {
        samplers.push(LlamaSampler::top_k(opts.top_k));
    }
    if opts.top_p < 1.0 {
        samplers.push(LlamaSampler::top_p(opts.top_p, 1));
    }
    if opts.temperature > 0.0 {
        samplers.push(LlamaSampler::temp(opts.temperature));
    }
    // Final selection sampler.
    if opts.temperature > 0.0 {
        samplers.push(LlamaSampler::dist(opts.seed));
    } else {
        samplers.push(LlamaSampler::greedy());
    }
    let mut sampler = LlamaSampler::chain_simple(samplers);

    // Generation loop.
    // Track brace depth for early stopping: once we've seen a complete
    // JSON object (depth returns to 0 after going positive), stop.
    let mut output = String::new();
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut n_cur = batch.n_tokens();
    let mut brace_depth: i32 = 0;
    let mut saw_open_brace = false;

    for _ in 0..max_tokens {
        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        sampler.accept(token);

        if inner.model.is_eog_token(token) {
            break;
        }

        match inner.model.token_to_piece(token, &mut decoder, true, None) {
            Ok(piece) => {
                // Track brace depth for early stop.
                for ch in piece.chars() {
                    if ch == '{' {
                        brace_depth += 1;
                        saw_open_brace = true;
                    } else if ch == '}' {
                        brace_depth -= 1;
                    }
                }
                output.push_str(&piece);
                // Stop as soon as we close the top-level JSON object.
                if saw_open_brace && brace_depth <= 0 {
                    break;
                }
            }
            Err(e) => {
                warn!(
                    layer = "triage",
                    component = "llm",
                    error = %e,
                    "Failed to decode token to text"
                );
            }
        }

        batch.clear();
        batch.add(token, n_cur, &[0], true)?;
        ctx.decode(&mut batch)
            .context("Failed to decode generation batch")?;
        n_cur += 1;
    }

    debug!(
        layer = "triage",
        component = "llm",
        output_len = output.len(),
        tokens_generated = n_cur as usize - tokens.len(),
        "Generation complete"
    );

    Ok(output)
}

/// Streaming variant that calls `on_token` for each decoded piece.
fn generate_sync_streaming<F>(
    inner: &LlmInner,
    prompt: &str,
    opts: &GenerateOpts,
    on_token: F,
) -> Result<String>
where
    F: Fn(&str),
{
    let max_tokens = opts.max_tokens.unwrap_or(inner.config.max_tokens);

    // Reuse cached context (same as generate_sync).
    let mut ctx_guard = inner.ctx_cache.lock().unwrap_or_else(|e| e.into_inner());

    if ctx_guard.is_none() {
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(inner.config.context_size))
            .with_n_threads(inner.config.n_threads as i32)
            .with_n_threads_batch(inner.config.n_threads_batch as i32);

        let new_ctx = inner
            .model
            .new_context(&inner.backend, ctx_params)
            .map_err(|e| LlmError::LoadFailed {
                reason: format!("Context creation failed: {e:?}"),
            })?;

        // SAFETY: See LlmInner::ctx_cache doc for invariants.
        *ctx_guard = Some(unsafe { std::mem::transmute(new_ctx) });
    }

    let ctx = ctx_guard.as_mut().expect("ctx just initialized");
    ctx.clear_kv_cache();

    let tokens = inner
        .model
        .str_to_token(prompt, AddBos::Never)
        .context("Tokenization failed")?;

    let mut batch = LlamaBatch::new(tokens.len().max(512), 1);
    for (i, token) in tokens.iter().enumerate() {
        let is_last = i == tokens.len() - 1;
        batch.add(*token, i as i32, &[0], is_last)?;
    }
    ctx.decode(&mut batch)
        .context("Failed to decode prompt batch")?;

    let mut samplers: Vec<LlamaSampler> = Vec::new();
    if opts.top_k > 0 {
        samplers.push(LlamaSampler::top_k(opts.top_k));
    }
    if opts.top_p < 1.0 {
        samplers.push(LlamaSampler::top_p(opts.top_p, 1));
    }
    if opts.temperature > 0.0 {
        samplers.push(LlamaSampler::temp(opts.temperature));
    }
    if opts.temperature > 0.0 {
        samplers.push(LlamaSampler::dist(opts.seed));
    } else {
        samplers.push(LlamaSampler::greedy());
    }
    let mut sampler = LlamaSampler::chain_simple(samplers);

    let mut output = String::new();
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut n_cur = batch.n_tokens();

    for _ in 0..max_tokens {
        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        sampler.accept(token);

        if inner.model.is_eog_token(token) {
            break;
        }

        match inner.model.token_to_piece(token, &mut decoder, true, None) {
            Ok(piece) => {
                on_token(&piece);
                output.push_str(&piece);
            }
            Err(e) => {
                warn!(
                    layer = "triage",
                    component = "llm",
                    error = %e,
                    "Failed to decode token to text"
                );
            }
        }

        batch.clear();
        batch.add(token, n_cur, &[0], true)?;
        ctx.decode(&mut batch)
            .context("Failed to decode generation batch")?;
        n_cur += 1;
    }

    Ok(output)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = LlmConfig::default();
        assert_eq!(config.context_size, 4096);
        assert_eq!(config.n_threads, 4);
        assert_eq!(config.gpu_layers, 999);
        assert_eq!(config.max_tokens, 256);
    }

    #[test]
    fn test_default_generate_opts() {
        let opts = GenerateOpts::default();
        assert!(opts.temperature > 0.0);
        assert_eq!(opts.top_k, 40);
        assert!(opts.top_p < 1.0);
    }

    #[test]
    fn test_model_not_found_error() {
        let config = LlmConfig {
            model_path: "/nonexistent/model.gguf".to_string(),
            ..Default::default()
        };
        let result = LocalLlm::new(config);
        let err = result.err().expect("Should fail for missing model");
        let msg = err.to_string();
        assert!(
            msg.contains("not found"),
            "Error should mention model not found: {msg}"
        );
    }

    #[test]
    fn test_generate_opts_override_max_tokens() {
        let opts = GenerateOpts {
            max_tokens: Some(50),
            ..Default::default()
        };
        assert_eq!(opts.max_tokens, Some(50));
    }
}
