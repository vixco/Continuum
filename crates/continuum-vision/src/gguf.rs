//! llama.cpp multimodal backend for the higher-quality SmolVLM2 model.

use std::num::NonZeroU32;
use std::path::Path;
use std::pin::pin;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use image::DynamicImage;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::mtmd::{
    mtmd_default_marker, MtmdBitmap, MtmdContext, MtmdContextParams, MtmdInputText,
};
use llama_cpp_2::sampling::LlamaSampler;
use tracing::{debug, info, instrument, warn};

use crate::{VisionModel, VisionOutput};

const MODEL_FILE: &str = "model-q4_k_m.gguf";
const MMPROJ_FILE: &str = "mmproj-f16.gguf";
const MODEL_NAME: &str = "smolvlm2-2.2b-q4-llama.cpp";
const DEFAULT_CONTEXT_SIZE: u32 = 4096;
const DEFAULT_BATCH_SIZE: u32 = 512;
const DEFAULT_THREADS: i32 = 8;

struct GgufInner {
    // Drop the context and projector before the model they reference.
    context: Mutex<LlamaContext<'static>>,
    mtmd: MtmdContext,
    model: LlamaModel,
    _backend: LlamaBackend,
    chat_template: LlamaChatTemplate,
    prompt: String,
    max_tokens: u32,
}

// llama.cpp serializes all mutable inference through `context`; model weights
// and the backend are immutable after construction.
unsafe impl Send for GgufInner {}
unsafe impl Sync for GgufInner {}

/// SmolVLM2 GGUF vision runtime backed by llama.cpp's multimodal API.
#[derive(Clone)]
pub struct GgufVisionModel {
    inner: Arc<GgufInner>,
}

impl GgufVisionModel {
    /// Load `model-q4_k_m.gguf` and `mmproj-f16.gguf` from `model_dir`.
    ///
    /// When `gpu` is true, all language-model layers and the multimodal
    /// projector are requested on the accelerator. A CUDA or Vulkan-enabled
    /// Continuum build is required for actual offload.
    #[instrument(skip_all, fields(layer = "senses", component = "vision", model_dir = %model_dir.as_ref().display(), gpu))]
    pub async fn new(
        model_dir: impl AsRef<Path>,
        gpu: bool,
        prompt: impl Into<String>,
        max_tokens: u32,
    ) -> Result<Self> {
        let model_dir = model_dir.as_ref().to_path_buf();
        let prompt = prompt.into();
        tokio::task::spawn_blocking(move || Self::load_sync(&model_dir, gpu, prompt, max_tokens))
            .await
            .context("GGUF vision model loader task failed")?
    }

    fn load_sync(model_dir: &Path, gpu: bool, prompt: String, max_tokens: u32) -> Result<Self> {
        if max_tokens == 0 {
            bail!("vision max_tokens must be greater than zero");
        }

        let model_path = model_dir.join(MODEL_FILE);
        let mmproj_path = model_dir.join(MMPROJ_FILE);
        for path in [&model_path, &mmproj_path] {
            if !path.is_file() {
                bail!("required GGUF vision file not found: {}", path.display());
            }
        }

        let gpu_requested = gpu;
        let gpu = gpu_requested && cfg!(any(feature = "cuda", feature = "vulkan"));
        if gpu_requested && !gpu {
            warn!(
                layer = "senses",
                component = "vision",
                "vision GPU requested but this build has no CUDA/Vulkan backend; using CPU"
            );
        }
        info!(
            layer = "senses",
            component = "vision",
            model = MODEL_NAME,
            gpu,
            "loading GGUF vision model"
        );
        let backend = LlamaBackend::init().context("failed to initialize llama.cpp")?;
        let gpu_layers = if gpu { 999 } else { 0 };
        let model_params = pin!(LlamaModelParams::default().with_n_gpu_layers(gpu_layers));
        let model = LlamaModel::load_from_file(&backend, &model_path, &model_params)
            .with_context(|| format!("failed to load {}", model_path.display()))?;
        let chat_template = model
            .chat_template(None)
            .context("SmolVLM2 GGUF has no usable chat template")?;

        let mtmd_params = MtmdContextParams {
            use_gpu: gpu,
            print_timings: false,
            n_threads: DEFAULT_THREADS,
            ..Default::default()
        };
        let mmproj_text = mmproj_path
            .to_str()
            .context("multimodal projector path is not valid UTF-8")?;
        let mtmd = MtmdContext::init_from_file(mmproj_text, &model, &mtmd_params)
            .with_context(|| format!("failed to load {}", mmproj_path.display()))?;
        if !mtmd.support_vision() {
            bail!("multimodal projector does not report vision support");
        }

        let context_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(DEFAULT_CONTEXT_SIZE))
            .with_n_batch(DEFAULT_BATCH_SIZE)
            .with_n_threads(DEFAULT_THREADS)
            .with_n_threads_batch(DEFAULT_THREADS);
        let context = model
            .new_context(&backend, context_params)
            .context("failed to create SmolVLM2 inference context")?;
        // SAFETY: `context` and `model` are owned by the same `GgufInner`.
        // Field order ensures the context is dropped before the model.
        let context =
            unsafe { std::mem::transmute::<LlamaContext<'_>, LlamaContext<'static>>(context) };

        Ok(Self {
            inner: Arc::new(GgufInner {
                context: Mutex::new(context),
                mtmd,
                model,
                _backend: backend,
                chat_template,
                prompt,
                max_tokens,
            }),
        })
    }
}

fn clean_caption(raw: &str) -> String {
    raw.split("<|im_end|>")
        .next()
        .unwrap_or(raw)
        .split("<|endoftext|>")
        .next()
        .unwrap_or(raw)
        .trim()
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn description_indicates_error(description: &str) -> bool {
    let lower = description.to_lowercase();
    [
        "error dialog",
        "build failed",
        "compilation failed",
        "stack trace",
        "unhandled exception",
        "file not found",
        "could not start",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

fn describe_sync(inner: &GgufInner, image: &DynamicImage) -> Result<VisionOutput> {
    let rgb = image.to_rgb8();
    let bitmap = MtmdBitmap::from_image_data(rgb.width(), rgb.height(), rgb.as_raw())
        .context("failed to create llama.cpp image bitmap")?;
    let content = format!("{}\n{}", mtmd_default_marker(), inner.prompt);
    let message = LlamaChatMessage::new("user".into(), content)
        .context("failed to create SmolVLM2 chat message")?;
    let templated = inner
        .model
        .apply_chat_template(&inner.chat_template, &[message], true)
        .context("failed to apply SmolVLM2 chat template")?;
    let chunks = inner
        .mtmd
        .tokenize(
            MtmdInputText {
                text: templated,
                add_special: true,
                parse_special: true,
            },
            &[&bitmap],
        )
        .context("failed to tokenize multimodal prompt")?;

    let mut context = inner.context.lock().unwrap_or_else(|e| e.into_inner());
    context.clear_kv_cache();
    let mut position = chunks
        .eval_chunks(&inner.mtmd, &context, 0, 0, DEFAULT_BATCH_SIZE as i32, true)
        .context("failed to evaluate multimodal prompt")?;

    let mut sampler = LlamaSampler::greedy();
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut output = String::new();
    let mut batch = LlamaBatch::new(1, 1);

    for _ in 0..inner.max_tokens {
        let token = sampler.sample(&context, -1);
        sampler.accept(token);
        if inner.model.is_eog_token(token) {
            break;
        }
        let piece = inner
            .model
            .token_to_piece(token, &mut decoder, true, None)
            .context("failed to decode SmolVLM2 output token")?;
        output.push_str(&piece);

        batch.clear();
        batch.add(token, position, &[0], true)?;
        context
            .decode(&mut batch)
            .context("failed to decode SmolVLM2 generation token")?;
        position += 1;
    }

    let description = clean_caption(&output);
    if description.is_empty() {
        bail!("SmolVLM2 generated an empty screen description");
    }
    Ok(VisionOutput {
        has_error_visible: description_indicates_error(&description),
        description,
        // MTMD evaluates the prompt outside the Rust wrapper's logits
        // bookkeeping, so a calibrated probability is not exposed safely.
        // Zero explicitly means "unavailable" for this backend.
        confidence: 0.0,
    })
}

#[async_trait]
impl VisionModel for GgufVisionModel {
    async fn describe(&self, image: &DynamicImage) -> Result<VisionOutput> {
        let inner = self.inner.clone();
        let image = image.clone();
        tokio::task::spawn_blocking(move || describe_sync(&inner, &image))
            .await
            .context("GGUF vision inference task failed")?
    }

    fn model_name(&self) -> &str {
        MODEL_NAME
    }

    async fn warmup(&self) -> Result<()> {
        debug!(
            layer = "senses",
            component = "vision",
            model = MODEL_NAME,
            "warming up GGUF vision model"
        );
        let image = DynamicImage::new_rgb8(32, 32);
        self.describe(&image).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_cleanup_removes_model_markers_and_newlines() {
        assert_eq!(
            clean_caption("  A dashboard\n is visible. <|im_end|>ignored"),
            "A dashboard is visible."
        );
    }

    #[test]
    fn visible_error_detection_is_specific() {
        assert!(description_indicates_error(
            "The editor shows Build failed and file not found."
        ));
        assert!(!description_indicates_error(
            "The dashboard reports no failures detected."
        ));
    }
}
