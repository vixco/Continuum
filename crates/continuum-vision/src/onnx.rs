//! ONNX Runtime-backed vision model with full autoregressive decoding.
//!
//! Provides [`OnnxVisionModel`], which loads SmolVLM via three ONNX
//! sessions (vision encoder, token embedder, text decoder) and a HuggingFace
//! tokenizer. It implements the [`VisionModel`] trait to produce one-sentence
//! screen descriptions.
//!
//! # Model directory layout
//!
//! The model directory (typically `~/.continuum-dev/models/vision/smolvlm-500m/`)
//! must contain:
//!
//! - `vision_encoder.onnx` — the image encoder (or `encoder.onnx`)
//! - `embed_tokens.onnx` — the token embedding layer
//! - `decoder.onnx` — the autoregressive text decoder with KV-cache
//! - `tokenizer.json` — HuggingFace tokenizer config
//!
//! # Inference pipeline
//!
//! 1. Preprocess image (resize 512×512, normalize, NCHW)
//! 2. Run vision encoder → image feature embeddings
//! 3. Tokenize text prompt, prepend image-token placeholders
//! 4. Run embed_tokens to get text embeddings
//! 5. Splice image features into the embedding at placeholder positions
//! 6. Run decoder in an autoregressive loop with KV-cache until EOS
//! 7. Decode generated tokens back to text
//!
//! Part of Layer 1 (Senses) in the Continuum cognitive architecture.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use async_trait::async_trait;
use image::{DynamicImage, GenericImageView};
use ndarray::{Array2, Array4, Array5, ArrayD, IxDyn};
use ort::execution_providers::{CPUExecutionProvider, CUDAExecutionProvider};
use ort::session::Session;
use ort::value::Tensor;
use serde::Deserialize;
use tracing::{debug, info, instrument, warn};

use crate::error::VisionError;
use crate::{VisionModel, VisionOutput};

// ---------------------------------------------------------------------------
// SmolVLM model constants and metadata
// ---------------------------------------------------------------------------

// Kept only for the legacy preprocessing regression comparison below.
#[cfg(test)]
const IMAGE_SIZE: u32 = 512;
#[cfg(test)]
const CHANNEL_MEANS: [f32; 3] = [0.5, 0.5, 0.5];
#[cfg(test)]
const CHANNEL_STDS: [f32; 3] = [0.5, 0.5, 0.5];
const DEFAULT_PROMPT: &str = "Describe only what is visibly happening in the central scene in one concise factual sentence. Prioritize the main subject and action, and distinguish pointing from holding. Then name the visible application and specific page or file when clearly legible. Include only clearly readable text and never guess.";

/// Runtime-tunable behavior for the local SmolVLM screen describer.
#[derive(Debug, Clone)]
pub struct VisionOptions {
    /// User prompt inserted into the model's official chat template.
    pub prompt: String,
    /// Maximum number of generated text tokens.
    pub max_new_tokens: usize,
    /// Longest edge used before 512px tiling. `None` uses the model config.
    pub processor_max_edge: Option<u32>,
    /// Whether to create local-detail tiles plus a global overview image.
    pub image_splitting: bool,
}

impl Default for VisionOptions {
    fn default() -> Self {
        Self {
            prompt: DEFAULT_PROMPT.to_string(),
            max_new_tokens: 64,
            // 1536 preserves enough UI detail for application/site identity
            // while remaining bounded below the model's official 2048px max.
            processor_max_edge: Some(1536),
            image_splitting: true,
        }
    }
}

#[derive(Debug, Deserialize)]
struct EdgeSize {
    longest_edge: u32,
}

#[derive(Debug, Deserialize)]
struct PreprocessorFile {
    do_image_splitting: bool,
    do_normalize: bool,
    do_rescale: bool,
    image_mean: [f32; 3],
    image_std: [f32; 3],
    max_image_size: EdgeSize,
    resample: u32,
    size: EdgeSize,
}

#[derive(Debug, Deserialize)]
struct ProcessorFile {
    image_seq_len: usize,
}

#[derive(Debug, Deserialize)]
struct GenerationFile {
    eos_token_id: i64,
    pad_token_id: i64,
}

#[derive(Debug, Deserialize)]
struct ModelFile {
    image_token_id: i64,
    text_config: TextModelFile,
}

#[derive(Debug, Deserialize)]
struct TextModelFile {
    num_hidden_layers: usize,
    num_key_value_heads: usize,
    head_dim: usize,
}

#[derive(Debug, Deserialize)]
struct ChatTemplateFile {
    chat_template: String,
}

#[derive(Debug)]
struct ModelMetadata {
    image_size: u32,
    processor_max_edge: u32,
    image_seq_len: usize,
    means: [f32; 3],
    stds: [f32; 3],
    image_token_id: i64,
    eos_token_id: i64,
    pad_token_id: i64,
    hidden_layers: usize,
    kv_heads: usize,
    head_dim: usize,
}

#[derive(Debug)]
struct PreparedImage {
    pixel_values: Array5<f32>,
    pixel_mask: Array4<bool>,
    rows: usize,
    cols: usize,
}

#[derive(Debug)]
struct InferenceText {
    description: String,
    confidence: f32,
}

// ---------------------------------------------------------------------------
// Internal session bundle
// ---------------------------------------------------------------------------

/// Holds the three ONNX sessions so they can be locked together for inference.
struct ModelSessions {
    encoder: Session,
    embed_tokens: Session,
    decoder: Session,
}

// Sessions contain raw pointers but are safe to send across threads.
// SAFETY: ort::Session is internally thread-safe for sequential use.
unsafe impl Send for ModelSessions {}

impl std::fmt::Debug for ModelSessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelSessions").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// OnnxVisionModel
// ---------------------------------------------------------------------------

/// Build an ONNX session, optionally with a CUDA execution provider and a CPU
/// fallback. When `gpu` is true but the CUDA provider can't be initialised
/// (e.g. no CUDA-capable `onnxruntime.dll` at `ORT_DYLIB_PATH`), we warn and
/// fall back to CPU so vision still loads rather than failing entirely.
/// Best-effort: the resolved resource plan only asks for GPU when hardware
/// detection found CUDA, but the onnxruntime build may still lack the CUDA EP.
fn build_session(path: &Path, gpu: bool) -> anyhow::Result<Session> {
    let builder = Session::builder().context("session builder")?;
    if gpu {
        match builder.with_execution_providers([
            CUDAExecutionProvider::default().build(),
            CPUExecutionProvider::default().build(),
        ]) {
            Ok(b) => Ok(b.commit_from_file(path)?),
            Err(e) => {
                warn!(
                    layer = "vision",
                    component = "onnx",
                    error = %e,
                    "CUDA execution provider unavailable; using CPU"
                );
                Ok(Session::builder()
                    .context("cpu session builder")?
                    .commit_from_file(path)?)
            }
        }
    } else {
        Ok(builder.commit_from_file(path)?)
    }
}

fn initialize_onnx_runtime() -> anyhow::Result<Option<PathBuf>> {
    let configured = std::env::var_os("ORT_DYLIB_PATH").map(PathBuf::from);

    #[cfg(target_os = "windows")]
    let candidates = {
        let mut paths = Vec::new();
        if let Some(path) = configured {
            paths.push(path);
        }
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            paths.push(
                PathBuf::from(local_app_data)
                    .join("Continuum")
                    .join("onnxruntime")
                    .join("onnxruntime.dll"),
            );
        }
        paths.push(PathBuf::from(r"C:\onnxruntime\onnxruntime.dll"));
        paths
    };

    #[cfg(not(target_os = "windows"))]
    let candidates = configured.into_iter().collect::<Vec<_>>();

    for candidate in candidates {
        let path = if candidate.is_dir() {
            #[cfg(target_os = "windows")]
            let filename = "onnxruntime.dll";
            #[cfg(target_os = "linux")]
            let filename = "libonnxruntime.so";
            #[cfg(target_os = "macos")]
            let filename = "libonnxruntime.dylib";
            candidate.join(filename)
        } else {
            candidate
        };
        if !path.is_file() {
            continue;
        }

        ort::init_from(&path)
            .with_context(|| format!("loading ONNX Runtime from {}", path.display()))?
            .commit();
        return Ok(Some(path));
    }

    #[cfg(target_os = "windows")]
    anyhow::bail!(
        "compatible ONNX Runtime not found; set ORT_DYLIB_PATH or install it at C:\\onnxruntime\\onnxruntime.dll"
    );

    #[cfg(not(target_os = "windows"))]
    Ok(None)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading model metadata {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing model metadata {}", path.display()))
}

fn validate_model_contract(
    preprocessor: &PreprocessorFile,
    processor: &ProcessorFile,
    chat_template: &ChatTemplateFile,
    options: &VisionOptions,
) -> anyhow::Result<()> {
    if !preprocessor.do_rescale || !preprocessor.do_normalize {
        anyhow::bail!("SmolVLM metadata disables required rescale/normalize processing");
    }
    if preprocessor.resample != 1 {
        anyhow::bail!(
            "unsupported SmolVLM resample mode {}; expected Lanczos (1)",
            preprocessor.resample
        );
    }
    if preprocessor.max_image_size.longest_edge == 0 || processor.image_seq_len == 0 {
        anyhow::bail!("SmolVLM processor metadata contains zero-sized image settings");
    }
    if !chat_template.chat_template.contains("<|im_start|>")
        || !chat_template.chat_template.contains("<image>")
        || !chat_template.chat_template.contains("<end_of_utterance>")
        || !chat_template.chat_template.contains("Assistant:")
    {
        anyhow::bail!("unsupported SmolVLM chat template contract");
    }
    if options.prompt.trim().is_empty() {
        anyhow::bail!("vision prompt must not be empty");
    }
    if !(1..=256).contains(&options.max_new_tokens) {
        anyhow::bail!("vision max_new_tokens must be between 1 and 256");
    }
    let processor_edge = options
        .processor_max_edge
        .unwrap_or(preprocessor.size.longest_edge);
    let tile_edge = preprocessor.max_image_size.longest_edge;
    if processor_edge < tile_edge
        || processor_edge > preprocessor.size.longest_edge
        || !processor_edge.is_multiple_of(tile_edge)
    {
        anyhow::bail!(
            "vision processor_max_edge must be a multiple of {tile_edge} between {tile_edge} and {}",
            preprocessor.size.longest_edge
        );
    }
    if options.image_splitting && !preprocessor.do_image_splitting {
        anyhow::bail!("vision image splitting requested but disabled by model metadata");
    }
    Ok(())
}

fn build_image_prompt(rows: usize, cols: usize, image_seq_len: usize) -> String {
    let image_tokens = "<image>".repeat(image_seq_len);
    if rows == 0 || cols == 0 {
        return format!(
            "<fake_token_around_image><global-img>{image_tokens}<fake_token_around_image>"
        );
    }

    let mut prompt = String::new();
    for row in 1..=rows {
        for col in 1..=cols {
            prompt.push_str("<fake_token_around_image>");
            prompt.push_str(&format!("<row_{row}_col_{col}>"));
            prompt.push_str(&image_tokens);
        }
        prompt.push('\n');
    }
    prompt.push('\n');
    prompt.push_str("<fake_token_around_image><global-img>");
    prompt.push_str(&image_tokens);
    prompt.push_str("<fake_token_around_image>");
    prompt
}

fn detects_visible_error(description: &str) -> bool {
    let normalized = description.to_lowercase();
    if ["no error", "without errors", "error-free"]
        .iter()
        .any(|phrase| normalized.contains(phrase))
    {
        return false;
    }

    [
        "error dialog",
        "error message",
        "error screen",
        "fatal error",
        "unhandled exception",
        "stack trace",
        "traceback",
        "has crashed",
        "not responding",
        "shows an error",
        "displaying an error",
        "an error is displayed",
    ]
    .iter()
    .any(|phrase| normalized.contains(phrase))
}

/// ONNX Runtime-backed vision model with full autoregressive text generation.
///
/// Loads SmolVLM from three ONNX files plus its tokenizer and processor metadata. The
/// [`describe`](VisionModel::describe) method runs the complete
/// encode → embed → decode pipeline to produce natural-language descriptions
/// of screenshot images.
///
/// # Thread safety
///
/// All three sessions are behind a single `Mutex`. Inference calls are
/// serialized but safe to call from multiple async tasks via `Arc`.
#[derive(Debug)]
pub struct OnnxVisionModel {
    sessions: Arc<Mutex<ModelSessions>>,
    tokenizer: Arc<tokenizers::Tokenizer>,
    metadata: Arc<ModelMetadata>,
    options: VisionOptions,
    model_name: String,
    #[allow(dead_code)]
    model_dir: PathBuf,
}

impl OnnxVisionModel {
    /// Load the full SmolVLM model from the given directory.
    ///
    /// Expects `vision_encoder.onnx` (or `encoder.onnx`), `embed_tokens.onnx`,
    /// `decoder.onnx`, and `tokenizer.json` in the directory.
    #[instrument(skip_all, fields(layer = "senses", component = "vision", model_dir = %model_dir.as_ref().display()))]
    pub async fn new(model_dir: impl AsRef<Path>, gpu: bool) -> anyhow::Result<Self> {
        Self::new_with_options(model_dir, gpu, VisionOptions::default()).await
    }

    /// Load SmolVLM with explicit, configurable processor and generation options.
    #[instrument(skip_all, fields(layer = "senses", component = "vision", model_dir = %model_dir.as_ref().display()))]
    pub async fn new_with_options(
        model_dir: impl AsRef<Path>,
        gpu: bool,
        options: VisionOptions,
    ) -> anyhow::Result<Self> {
        let model_dir = model_dir.as_ref().to_path_buf();

        if !model_dir.is_dir() {
            return Err(VisionError::ModelDirectoryNotFound {
                path: model_dir.display().to_string(),
            }
            .into());
        }

        let runtime_path = initialize_onnx_runtime()?;
        if let Some(path) = &runtime_path {
            info!(
                layer = "senses",
                component = "vision",
                runtime_path = %path.display(),
                "selected explicit ONNX Runtime"
            );
        }

        // Resolve file paths (support both naming conventions).
        let encoder_path = if model_dir.join("vision_encoder.onnx").exists() {
            model_dir.join("vision_encoder.onnx")
        } else {
            model_dir.join("encoder.onnx")
        };
        let embed_path = model_dir.join("embed_tokens.onnx");
        let decoder_path = model_dir.join("decoder.onnx");
        let tokenizer_path = model_dir.join("tokenizer.json");
        let preprocessor_path = model_dir.join("preprocessor_config.json");
        let processor_path = model_dir.join("processor_config.json");
        let generation_path = model_dir.join("generation_config.json");
        let chat_template_path = model_dir.join("chat_template.json");
        let config_path = model_dir.join("config.json");

        for (name, path) in [
            ("vision encoder", &encoder_path),
            ("embed_tokens", &embed_path),
            ("decoder", &decoder_path),
            ("tokenizer", &tokenizer_path),
            ("preprocessor config", &preprocessor_path),
            ("processor config", &processor_path),
            ("generation config", &generation_path),
            ("chat template", &chat_template_path),
            ("model config", &config_path),
        ] {
            if !path.exists() {
                return Err(VisionError::ModelFileNotFound {
                    path: format!("{} ({})", path.display(), name),
                }
                .into());
            }
        }

        info!(
            layer = "senses",
            component = "vision",
            gpu_enabled = gpu,
            "loading SmolVLM ONNX sessions and tokenizer"
        );

        // Load tokenizer (fast, no need for blocking thread).
        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            VisionError::ModelLoadError {
                path: tokenizer_path.display().to_string(),
                reason: format!("{e}"),
            }
        })?;

        let preprocessor: PreprocessorFile = read_json(&preprocessor_path)?;
        let processor: ProcessorFile = read_json(&processor_path)?;
        let generation: GenerationFile = read_json(&generation_path)?;
        let chat_template: ChatTemplateFile = read_json(&chat_template_path)?;
        let model_config: ModelFile = read_json(&config_path)?;
        validate_model_contract(&preprocessor, &processor, &chat_template, &options)?;
        let tokenizer_image_token_id = tokenizer
            .token_to_id("<image>")
            .context("tokenizer does not define the required <image> token")?
            as i64;
        if tokenizer_image_token_id != model_config.image_token_id {
            anyhow::bail!(
                "model/tokenizer image token mismatch: {} != {}",
                model_config.image_token_id,
                tokenizer_image_token_id
            );
        }
        let metadata = ModelMetadata {
            image_size: preprocessor.max_image_size.longest_edge,
            processor_max_edge: preprocessor.size.longest_edge,
            image_seq_len: processor.image_seq_len,
            means: preprocessor.image_mean,
            stds: preprocessor.image_std,
            image_token_id: model_config.image_token_id,
            eos_token_id: generation.eos_token_id,
            pad_token_id: generation.pad_token_id,
            hidden_layers: model_config.text_config.num_hidden_layers,
            kv_heads: model_config.text_config.num_key_value_heads,
            head_dim: model_config.text_config.head_dim,
        };

        // Load ONNX sessions on a blocking thread.
        let ep = encoder_path.clone();
        let emp = embed_path.clone();
        let dp = decoder_path.clone();
        let sessions = tokio::task::spawn_blocking(move || -> anyhow::Result<ModelSessions> {
            let encoder =
                build_session(&ep, gpu).with_context(|| format!("loading {}", ep.display()))?;
            let embed_tokens =
                build_session(&emp, gpu).with_context(|| format!("loading {}", emp.display()))?;
            let decoder =
                build_session(&dp, gpu).with_context(|| format!("loading {}", dp.display()))?;

            Ok(ModelSessions {
                encoder,
                embed_tokens,
                decoder,
            })
        })
        .await
        .context("model loading task panicked")?
        .map_err(|e| VisionError::ModelLoadError {
            path: model_dir.display().to_string(),
            reason: format!("{e:#}"),
        })?;

        info!(
            layer = "senses",
            component = "vision",
            "all SmolVLM sessions loaded"
        );

        Ok(Self {
            sessions: Arc::new(Mutex::new(sessions)),
            tokenizer: Arc::new(tokenizer),
            metadata: Arc::new(metadata),
            options,
            model_name: format!(
                "{}-onnx",
                model_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("smolvlm")
            ),
            model_dir,
        })
    }

    /// Preprocess an image: resize (aspect-preserving) → pad → normalize.
    ///
    /// Returns `(pixel_values, pixel_attention_mask)`:
    /// - pixel_values: `[1, 1, 3, IMAGE_SIZE, IMAGE_SIZE]`
    /// - pixel_attention_mask: `[1, 1, IMAGE_SIZE, IMAGE_SIZE]` (bool, true where real pixels)
    #[cfg(test)]
    fn preprocess(image: &DynamicImage) -> (ndarray::Array5<f32>, Array4<bool>) {
        let sz = IMAGE_SIZE as usize;

        // Resize preserving aspect ratio so longest edge = IMAGE_SIZE.
        let resized = image.resize(
            IMAGE_SIZE,
            IMAGE_SIZE,
            image::imageops::FilterType::Triangle, // Bilinear
        );
        let rgb = resized.to_rgb8();
        let rh = rgb.height() as usize;
        let rw = rgb.width() as usize;

        // Create tensor with zero-padding.
        // Normalization: (pixel/255 - 0.5) / 0.5 → zeros become -1.0
        let mut tensor = ndarray::Array5::<f32>::from_elem((1, 1, 3, sz, sz), -1.0);
        let mut mask = Array4::<bool>::from_elem((1, 1, sz, sz), false);

        for y in 0..rh {
            for x in 0..rw {
                let pixel = rgb.get_pixel(x as u32, y as u32);
                for c in 0..3 {
                    let v = pixel[c] as f32 / 255.0;
                    tensor[[0, 0, c, y, x]] = (v - CHANNEL_MEANS[c]) / CHANNEL_STDS[c];
                }
                mask[[0, 0, y, x]] = true;
            }
        }
        (tensor, mask)
    }

    /// Run the full encode → embed → decode pipeline on a blocking thread.
    ///
    /// This is the core inference function. It is called inside
    /// `tokio::task::spawn_blocking` from [`describe`](VisionModel::describe).
    /// Reproduce the official Idefics3 image processor for one screenshot.
    fn preprocess_official(
        image: &DynamicImage,
        metadata: &ModelMetadata,
        options: &VisionOptions,
    ) -> PreparedImage {
        let tile_edge = metadata.image_size;
        let processor_edge = options
            .processor_max_edge
            .unwrap_or(metadata.processor_max_edge);
        let (source_width, source_height) = image.dimensions();
        let aspect_ratio = source_width as f64 / source_height.max(1) as f64;
        let (resized_width, resized_height) = if source_width >= source_height {
            let width = processor_edge;
            let mut height = (width as f64 / aspect_ratio) as u32;
            if !height.is_multiple_of(2) {
                height += 1;
            }
            (width, height.max(1))
        } else {
            let height = processor_edge;
            let mut width = (height as f64 * aspect_ratio) as u32;
            if !width.is_multiple_of(2) {
                width += 1;
            }
            (width.max(1), height)
        };
        let resized = image.resize_exact(
            resized_width,
            resized_height,
            image::imageops::FilterType::Lanczos3,
        );

        let (frames, rows, cols) = if options.image_splitting {
            let grid_width = resized_width.div_ceil(tile_edge) * tile_edge;
            let grid_height = resized_height.div_ceil(tile_edge) * tile_edge;
            let tiled = resized.resize_exact(
                grid_width,
                grid_height,
                image::imageops::FilterType::Lanczos3,
            );
            let rows = (grid_height / tile_edge) as usize;
            let cols = (grid_width / tile_edge) as usize;
            let mut frames = Vec::with_capacity(rows * cols + 1);
            for row in 0..rows {
                for col in 0..cols {
                    frames.push(tiled.crop_imm(
                        col as u32 * tile_edge,
                        row as u32 * tile_edge,
                        tile_edge,
                        tile_edge,
                    ));
                }
            }
            frames.push(tiled.resize_exact(
                tile_edge,
                tile_edge,
                image::imageops::FilterType::Lanczos3,
            ));
            (frames, rows, cols)
        } else {
            (
                vec![resized.resize_exact(
                    tile_edge,
                    tile_edge,
                    image::imageops::FilterType::Lanczos3,
                )],
                0,
                0,
            )
        };

        let sz = tile_edge as usize;
        let mut tensor = Array5::<f32>::zeros((1, frames.len(), 3, sz, sz));
        let mask = Array4::<bool>::from_elem((1, frames.len(), sz, sz), true);
        for (frame_index, frame) in frames.iter().enumerate() {
            let rgb = frame.to_rgb8();
            for y in 0..sz {
                for x in 0..sz {
                    let pixel = rgb.get_pixel(x as u32, y as u32);
                    for channel in 0..3 {
                        let value = pixel[channel] as f32 / 255.0;
                        tensor[[0, frame_index, channel, y, x]] =
                            (value - metadata.means[channel]) / metadata.stds[channel];
                    }
                }
            }
        }

        PreparedImage {
            pixel_values: tensor,
            pixel_mask: mask,
            rows,
            cols,
        }
    }

    fn run_inference(
        sessions: &mut ModelSessions,
        tokenizer: &tokenizers::Tokenizer,
        prepared: PreparedImage,
        metadata: &ModelMetadata,
        options: &VisionOptions,
    ) -> anyhow::Result<InferenceText> {
        // Helper: extract owned f32 ndarray from session output at given index.
        fn extract(
            outputs: &ort::session::SessionOutputs<'_>,
            idx: usize,
            name: &str,
        ) -> anyhow::Result<ArrayD<f32>> {
            let (shape, data) = outputs[idx]
                .try_extract_tensor::<f32>()
                .with_context(|| format!("extract '{name}'[{idx}]"))?;
            let dims: Vec<usize> = (0..shape.len()).map(|i| shape[i] as usize).collect();
            ArrayD::from_shape_vec(IxDyn(&dims), data.to_vec())
                .with_context(|| format!("reshape '{name}' {dims:?}"))
        }

        // ---- Step 1: Vision encoder ----
        let pv_tensor = Tensor::from_array(prepared.pixel_values).context("pixel_values tensor")?;
        let pm_tensor =
            Tensor::from_array(prepared.pixel_mask).context("pixel_attention_mask tensor")?;

        let encoder_out = sessions
            .encoder
            .run(ort::inputs![pv_tensor, pm_tensor])
            .context("vision encoder")?;

        let image_features = extract(&encoder_out, 0, "image_features")?;
        drop(encoder_out); // release borrow on encoder session

        let hidden_size = *image_features
            .shape()
            .last()
            .context("vision encoder returned a scalar")?;
        let feature_values = image_features
            .as_slice()
            .context("vision encoder output is not contiguous")?;
        let num_image_tokens = feature_values.len() / hidden_size;

        debug!(
            layer = "senses",
            component = "vision",
            num_image_tokens,
            hidden_size,
            "encoder produced image features"
        );

        // ---- Step 2: Build prompt token IDs from the model contract. ----
        // <fake_token_around_image> <image>×N <fake_token_around_image>
        // Use the exact official Idefics3 chat-template expansion.
        let image_prompt = build_image_prompt(prepared.rows, prepared.cols, metadata.image_seq_len);
        let prompt = format!(
            "<|im_start|>User:{image_prompt}{}<end_of_utterance>\nAssistant:",
            options.prompt.trim()
        );
        let encoding = tokenizer
            .encode(prompt, false)
            .map_err(|e| anyhow::anyhow!("tokenizer encode: {e}"))?;
        let input_ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let total_len = input_ids.len();

        debug!(
            layer = "senses",
            component = "vision",
            total_len,
            image_frames = prepared
                .rows
                .saturating_mul(prepared.cols)
                .saturating_add(1),
            "built input token sequence"
        );

        let placeholder_count = input_ids
            .iter()
            .filter(|&&token| token == metadata.image_token_id)
            .count();
        if placeholder_count != num_image_tokens {
            anyhow::bail!(
                "image prompt/encoder mismatch: {placeholder_count} placeholders for {num_image_tokens} features"
            );
        }

        // ---- Step 3: Embed tokens ----
        let ids_array =
            Array2::from_shape_vec((1, total_len), input_ids.clone()).context("ids array")?;
        let ids_tensor = Tensor::from_array(ids_array).context("ids tensor")?;

        let embed_out = sessions
            .embed_tokens
            .run(ort::inputs![ids_tensor])
            .context("embed_tokens")?;

        let mut inputs_embeds = extract(&embed_out, 0, "inputs_embeds")?;
        drop(embed_out); // release borrow so embed_tokens can be used again later

        // ---- Step 4: Replace image-token positions with vision features ----
        // Replace model-defined image placeholders with encoder features.
        let mut feat_idx = 0;
        for pos in 0..total_len {
            if input_ids[pos] == metadata.image_token_id && feat_idx < num_image_tokens {
                for j in 0..hidden_size {
                    inputs_embeds[[0, pos, j]] = feature_values[feat_idx * hidden_size + j];
                }
                feat_idx += 1;
            }
        }

        // ---- Step 5: Autoregressive decoder loop ----
        let mut attn_vec: Vec<i64> = vec![1i64; total_len];
        let mut pos_vec: Vec<i64> = (0..total_len as i64).collect();

        let mut kv_cache: Vec<ArrayD<f32>> = (0..(metadata.hidden_layers * 2))
            .map(|_| ArrayD::zeros(IxDyn(&[1, metadata.kv_heads, 0, metadata.head_dim])))
            .collect();

        let mut generated: Vec<i64> = Vec::new();
        let mut cur_embeds = inputs_embeds;

        let mut confidence_sum = 0.0f32;
        for step in 0..options.max_new_tokens {
            let seq_len = cur_embeds.shape()[1];

            let embeds_t = Tensor::from_array(cur_embeds.clone()).context("embeds")?;
            let attn_a = Array2::from_shape_vec((1, attn_vec.len()), attn_vec.clone())
                .context("attn array")?;
            let attn_t = Tensor::from_array(attn_a).context("attn")?;
            let pos_a =
                Array2::from_shape_vec((1, seq_len), pos_vec[pos_vec.len() - seq_len..].to_vec())
                    .context("pos array")?;
            let pos_t = Tensor::from_array(pos_a).context("pos")?;

            let mut dec_inputs = ort::inputs![
                "inputs_embeds" => embeds_t,
                "attention_mask" => attn_t,
                "position_ids" => pos_t,
            ];

            for layer in 0..metadata.hidden_layers {
                dec_inputs.push((
                    format!("past_key_values.{layer}.key").into(),
                    Tensor::from_array(kv_cache[layer * 2].clone())
                        .context("kv key")?
                        .into(),
                ));
                dec_inputs.push((
                    format!("past_key_values.{layer}.value").into(),
                    Tensor::from_array(kv_cache[layer * 2 + 1].clone())
                        .context("kv val")?
                        .into(),
                ));
            }

            let dec_out = sessions.decoder.run(dec_inputs).context("decoder")?;

            // Logits: [1, seq_len, vocab_size]
            let logits = extract(&dec_out, 0, "logits")?;
            let vocab = logits.shape()[2];
            let last = logits.shape()[1] - 1;

            // (d) Softmax → probabilities.
            // The official generation config specifies `do_sample=false`.
            // Greedy argmax keeps identical screenshots deterministic.
            let (best_index, best_logit) = (0..vocab)
                .map(|index| (index, logits[[0, last, index]]))
                .max_by(|(_, left), (_, right)| left.total_cmp(right))
                .context("decoder returned an empty vocabulary")?;
            let denominator: f32 = (0..vocab)
                .map(|index| (logits[[0, last, index]] - best_logit).exp())
                .sum();
            confidence_sum += 1.0 / denominator.max(1.0);
            let best_tok = best_index as i64;

            if best_tok == metadata.eos_token_id || best_tok == metadata.pad_token_id {
                debug!(layer = "senses", component = "vision", step, "EOS");
                break;
            }
            generated.push(best_tok);

            // Repetition safety net: if last 10 tokens contain a repeated 3-gram, stop.
            if generated.len() >= 6 {
                let start = generated.len().saturating_sub(10);
                let tail = &generated[start..];
                let mut seen = std::collections::HashSet::new();
                let mut repeated = false;
                for w in tail.windows(3) {
                    if !seen.insert((w[0], w[1], w[2])) {
                        repeated = true;
                        break;
                    }
                }
                if repeated {
                    debug!(
                        layer = "senses",
                        component = "vision",
                        step,
                        "Repetition safety net triggered"
                    );
                    break;
                }
            }

            // Update KV-cache from decoder outputs [1..61].
            let n_out = dec_out.len();
            for (i, kv) in kv_cache.iter_mut().enumerate() {
                let oi = i + 1;
                if oi < n_out {
                    *kv = extract(&dec_out, oi, "kv")?;
                }
            }
            drop(dec_out);

            // Embed next token.
            let nxt = Array2::from_shape_vec((1, 1), vec![best_tok]).context("nxt")?;
            let nxt_t = Tensor::from_array(nxt).context("nxt tensor")?;
            let nxt_out = sessions
                .embed_tokens
                .run(ort::inputs![nxt_t])
                .context("embed next")?;
            cur_embeds = extract(&nxt_out, 0, "nxt_embed")?;
            drop(nxt_out);

            attn_vec.push(1);
            let next_pos = *pos_vec.last().unwrap_or(&0) + 1;
            pos_vec.push(next_pos);
        }

        // ---- Step 6: Decode tokens to text ----
        let token_ids_u32: Vec<u32> = generated.iter().map(|&t| t as u32).collect();
        let description = tokenizer
            .decode(&token_ids_u32, true)
            .map_err(|e| anyhow::anyhow!("tokenizer decode failed: {e}"))?;

        let description = description.trim().to_string();

        debug!(
            layer = "senses",
            component = "vision",
            tokens = generated.len(),
            description = %description,
            "generated description"
        );

        let confidence = if generated.is_empty() {
            0.0
        } else {
            (confidence_sum / generated.len() as f32).clamp(0.0, 1.0)
        };
        Ok(InferenceText {
            description,
            confidence,
        })
    }
}

#[async_trait]
impl VisionModel for OnnxVisionModel {
    /// Describe the contents of a screenshot image.
    ///
    /// Runs the full SmolVLM pipeline: vision encoder → token embedding →
    /// autoregressive text decoder. Returns a natural-language description
    /// of what the user is looking at.
    #[instrument(skip_all, fields(layer = "senses", component = "vision"))]
    async fn describe(&self, image: &DynamicImage) -> anyhow::Result<VisionOutput> {
        let image_clone = image.clone();
        let sessions = Arc::clone(&self.sessions);
        let tokenizer = Arc::clone(&self.tokenizer);
        let metadata = Arc::clone(&self.metadata);
        let options = self.options.clone();

        let inference = tokio::task::spawn_blocking(move || {
            let prepared = Self::preprocess_official(&image_clone, &metadata, &options);

            let mut guard = sessions
                .lock()
                .map_err(|e| anyhow::anyhow!("session mutex poisoned: {e}"))?;

            Self::run_inference(&mut guard, &tokenizer, prepared, &metadata, &options)
        })
        .await
        .context("vision inference task panicked")??;

        Ok(VisionOutput {
            has_error_visible: detects_visible_error(&inference.description),
            description: inference.description,
            confidence: inference.confidence,
        })
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    /// Warm up all three ONNX sessions.
    #[instrument(skip_all, fields(layer = "senses", component = "vision"))]
    async fn warmup(&self) -> anyhow::Result<()> {
        info!(
            layer = "senses",
            component = "vision",
            "warming up SmolVLM model"
        );

        let dummy = DynamicImage::new_rgb8(self.metadata.image_size, self.metadata.image_size);
        let _ = self.describe(&dummy).await?;

        info!(
            layer = "senses",
            component = "vision",
            "SmolVLM warmup complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_metadata() -> ModelMetadata {
        ModelMetadata {
            image_size: 512,
            processor_max_edge: 2048,
            image_seq_len: 64,
            means: [0.5; 3],
            stds: [0.5; 3],
            image_token_id: 49190,
            eos_token_id: 49279,
            pad_token_id: 2,
            hidden_layers: 30,
            kv_heads: 3,
            head_dim: 64,
        }
    }

    #[tokio::test]
    async fn test_new_with_nonexistent_directory_returns_error() {
        let result = OnnxVisionModel::new("/nonexistent/path/to/model", false).await;
        assert!(result.is_err());
        let err_str = format!("{:#}", result.unwrap_err());
        assert!(
            err_str.contains("not found") || err_str.contains("Not found"),
            "expected 'not found' in error, got: {err_str}"
        );
    }

    #[tokio::test]
    async fn test_new_with_empty_directory_returns_model_file_error() {
        let tmp = std::env::temp_dir().join("continuum-vision-test-empty");
        let _ = std::fs::create_dir_all(&tmp);
        let result = OnnxVisionModel::new(&tmp, false).await;
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_preprocess_produces_correct_shape() {
        let img = DynamicImage::new_rgb8(800, 600);
        let (tensor, mask) = OnnxVisionModel::preprocess(&img);
        assert_eq!(
            tensor.shape(),
            &[1, 1, 3, IMAGE_SIZE as usize, IMAGE_SIZE as usize]
        );
        assert_eq!(
            mask.shape(),
            &[1, 1, IMAGE_SIZE as usize, IMAGE_SIZE as usize]
        );
    }

    #[test]
    fn test_preprocess_normalizes_white() {
        let img = DynamicImage::from(image::RgbImage::from_fn(100, 100, |_, _| {
            image::Rgb([255u8, 255, 255])
        }));
        let (tensor, _) = OnnxVisionModel::preprocess(&img);
        let r = tensor[[0, 0, 0, 0, 0]];
        // (1.0 - 0.5) / 0.5 = 1.0
        assert!((r - 1.0).abs() < 0.01, "expected ~1.0, got {r}");
    }

    #[test]
    fn official_processor_splits_wide_desktop_into_tiles_and_global_view() {
        let img = DynamicImage::new_rgb8(1280, 720);
        let prepared =
            OnnxVisionModel::preprocess_official(&img, &test_metadata(), &VisionOptions::default());
        assert_eq!((prepared.rows, prepared.cols), (2, 3));
        assert_eq!(prepared.pixel_values.shape(), &[1, 7, 3, 512, 512]);
        assert_eq!(prepared.pixel_mask.shape(), &[1, 7, 512, 512]);
    }

    #[test]
    fn official_prompt_has_one_placeholder_block_per_encoder_frame() {
        let prompt = build_image_prompt(2, 3, 64);
        assert_eq!(prompt.matches("<image>").count(), 7 * 64);
        assert!(prompt.contains("<row_1_col_1>"));
        assert!(prompt.contains("<row_2_col_3>"));
        assert!(prompt.contains("<global-img>"));
    }

    #[test]
    fn visible_error_detection_avoids_bare_keyword_false_positives() {
        assert!(detects_visible_error(
            "A fatal error dialog is displayed in the editor."
        ));
        assert!(detects_visible_error("The application is not responding."));
        assert!(!detects_visible_error(
            "A documentation page explains error handling."
        ));
        assert!(!detects_visible_error(
            "The test suite completed without errors."
        ));
    }

    /// Integration test: load real model and describe a screenshot.
    ///
    /// This test requires the SmolVLM model files to be downloaded.
    /// Run `scripts/download-models.ps1` first.
    /// Skipped if models are not present.
    #[tokio::test]
    async fn test_describe_real_screenshot() {
        let model_dir = std::env::var_os("CONTINUUM_VISION_TEST_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .expect("home directory")
                    .join(".continuum-dev/models/vision/smolvlm-500m")
            });

        if !model_dir.join("decoder.onnx").exists() {
            eprintln!("Skipping integration test: model files not downloaded");
            return;
        }

        let model = OnnxVisionModel::new(&model_dir, false)
            .await
            .expect("should load model");

        // Use the test fixture screenshot.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/youtube-browser-screenshot.jpg");

        if !fixture.exists() {
            eprintln!("Skipping: test fixture not found at {}", fixture.display());
            return;
        }

        let img = image::open(&fixture).expect("should open test image");
        let output = model.describe(&img).await.expect("describe should succeed");

        eprintln!(
            "Description: {} (confidence {:.3})",
            output.description, output.confidence
        );
        assert!(
            !output.description.is_empty(),
            "description should not be empty"
        );
        assert!(
            !output.description.contains("placeholder"),
            "should not contain placeholder text"
        );
        let normalized = output.description.to_lowercase();
        assert!(
            normalized.contains("point") && normalized.contains("elf"),
            "caption should recognize the visible pointing action and Elf image: {}",
            output.description
        );
        assert!(
            !normalized.contains("holding"),
            "caption should not claim the man is holding the wall image: {}",
            output.description
        );
        assert!(
            !["wordpress", "online test preparation", "steam application"]
                .iter()
                .any(|term| normalized.contains(term)),
            "caption repeated a known hallucination: {}",
            output.description
        );
        assert!((0.0..=1.0).contains(&output.confidence));
    }
}
