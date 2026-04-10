//! ONNX Runtime-backed vision model with full autoregressive decoding.
//!
//! Provides [`OnnxVisionModel`], which loads SmolVLM-256M via three ONNX
//! sessions (vision encoder, token embedder, text decoder) and a HuggingFace
//! tokenizer. It implements the [`VisionModel`] trait to produce one-sentence
//! screen descriptions.
//!
//! # Model directory layout
//!
//! The model directory (typically `~/.kairo-dev/models/vision/smolvlm-256m/`)
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
//! Part of Layer 1 (Senses) in the Kairo cognitive architecture.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use async_trait::async_trait;
use image::DynamicImage;
use ndarray::{Array2, Array4, ArrayD, IxDyn};
use ort::session::Session;
use ort::value::Tensor;
use tracing::{debug, info, instrument};

use crate::error::VisionError;
use crate::{VisionModel, VisionOutput};

// ---------------------------------------------------------------------------
// SmolVLM-256M model constants (from HuggingFace config.json)
// ---------------------------------------------------------------------------

/// Vision encoder input size (pixels).
const IMAGE_SIZE: u32 = 512;

/// SmolVLM normalization means (all channels identical).
const CHANNEL_MEANS: [f32; 3] = [0.5, 0.5, 0.5];

/// SmolVLM normalization stds (all channels identical).
const CHANNEL_STDS: [f32; 3] = [0.5, 0.5, 0.5];

/// Token ID that marks image-feature positions in the input sequence.
const IMAGE_TOKEN_ID: i64 = 49190;

/// Sentinel token that wraps image-token sequences.
const FAKE_TOKEN_AROUND_IMAGE: i64 = 49189;

/// `<|im_start|>` chat template token.
const IM_START: i64 = 1;

/// `<|im_end|>` / EOS token.
const IM_END: i64 = 2;

/// Number of transformer layers in the text decoder.
const NUM_HIDDEN_LAYERS: usize = 30;

/// Number of key-value attention heads per layer.
const NUM_KV_HEADS: usize = 3;

/// Dimension of each attention head.
const HEAD_DIM: usize = 64;

/// Maximum tokens to generate.
const MAX_NEW_TOKENS: usize = 64;

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

/// ONNX Runtime-backed vision model with full autoregressive text generation.
///
/// Loads SmolVLM-256M from three ONNX files plus a tokenizer. The
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
    #[allow(dead_code)]
    model_dir: PathBuf,
}

impl OnnxVisionModel {
    /// Load the full SmolVLM model from the given directory.
    ///
    /// Expects `vision_encoder.onnx` (or `encoder.onnx`), `embed_tokens.onnx`,
    /// `decoder.onnx`, and `tokenizer.json` in the directory.
    #[instrument(skip_all, fields(layer = "senses", component = "vision", model_dir = %model_dir.as_ref().display()))]
    pub async fn new(model_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let model_dir = model_dir.as_ref().to_path_buf();

        if !model_dir.is_dir() {
            return Err(VisionError::ModelDirectoryNotFound {
                path: model_dir.display().to_string(),
            }
            .into());
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

        for (name, path) in [
            ("vision encoder", &encoder_path),
            ("embed_tokens", &embed_path),
            ("decoder", &decoder_path),
            ("tokenizer", &tokenizer_path),
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
            "loading SmolVLM ONNX sessions and tokenizer"
        );

        // Load tokenizer (fast, no need for blocking thread).
        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| VisionError::ModelLoadError {
                path: tokenizer_path.display().to_string(),
                reason: format!("{e}"),
            })?;

        // Load ONNX sessions on a blocking thread.
        let ep = encoder_path.clone();
        let emp = embed_path.clone();
        let dp = decoder_path.clone();
        let sessions = tokio::task::spawn_blocking(move || -> anyhow::Result<ModelSessions> {
            let encoder = Session::builder()
                .context("encoder session builder")?
                .commit_from_file(&ep)
                .with_context(|| format!("loading {}", ep.display()))?;

            let embed_tokens = Session::builder()
                .context("embed_tokens session builder")?
                .commit_from_file(&emp)
                .with_context(|| format!("loading {}", emp.display()))?;

            let decoder = Session::builder()
                .context("decoder session builder")?
                .commit_from_file(&dp)
                .with_context(|| format!("loading {}", dp.display()))?;

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
            model_dir,
        })
    }

    /// Preprocess an image: resize (aspect-preserving) → pad → normalize.
    ///
    /// Returns `(pixel_values, pixel_attention_mask)`:
    /// - pixel_values: `[1, 1, 3, IMAGE_SIZE, IMAGE_SIZE]`
    /// - pixel_attention_mask: `[1, 1, IMAGE_SIZE, IMAGE_SIZE]` (bool, true where real pixels)
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
    fn run_inference(
        sessions: &mut ModelSessions,
        tokenizer: &tokenizers::Tokenizer,
        pixel_values: ndarray::Array5<f32>,
        pixel_mask: Array4<bool>,
    ) -> anyhow::Result<String> {
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
        let pv_tensor = Tensor::from_array(pixel_values).context("pixel_values tensor")?;
        let pm_tensor = Tensor::from_array(pixel_mask).context("pixel_attention_mask tensor")?;

        let encoder_out = sessions
            .encoder
            .run(ort::inputs![pv_tensor, pm_tensor])
            .context("vision encoder")?;

        let image_features = extract(&encoder_out, 0, "image_features")?;
        drop(encoder_out); // release borrow on encoder session

        let num_image_tokens = image_features.shape()[1];
        let hidden_size = image_features.shape()[2];

        debug!(
            layer = "senses",
            component = "vision",
            num_image_tokens,
            hidden_size,
            "encoder produced image features"
        );

        // ---- Step 2: Build prompt token IDs ----
        // SmolVLM chat format:
        //   <|im_start|>user\n
        //   <fake_token_around_image><image>...<image><fake_token_around_image>
        //   \nDescribe what you see on this screen in one sentence.<|im_end|>\n
        //   <|im_start|>assistant\n
        let user_text = "\nDescribe what you see on this screen in one sentence.";
        let user_enc = tokenizer
            .encode(user_text, false)
            .map_err(|e| anyhow::anyhow!("tokenizer encode: {e}"))?;
        let user_ids: Vec<i64> = user_enc.get_ids().iter().map(|&id| id as i64).collect();

        let assistant_text = "\nassistant\n";
        let asst_enc = tokenizer
            .encode(assistant_text, false)
            .map_err(|e| anyhow::anyhow!("tokenizer encode: {e}"))?;
        let asst_ids: Vec<i64> = asst_enc.get_ids().iter().map(|&id| id as i64).collect();

        let mut input_ids: Vec<i64> = Vec::new();
        // <|im_start|>user\n
        input_ids.push(IM_START);
        let user_hdr = tokenizer.encode("user\n", false)
            .map_err(|e| anyhow::anyhow!("tokenizer: {e}"))?;
        input_ids.extend(user_hdr.get_ids().iter().map(|&id| id as i64));
        // <fake_token_around_image> <image>×N <fake_token_around_image>
        input_ids.push(FAKE_TOKEN_AROUND_IMAGE);
        input_ids.extend(std::iter::repeat_n(IMAGE_TOKEN_ID, num_image_tokens));
        input_ids.push(FAKE_TOKEN_AROUND_IMAGE);
        // \nDescribe...<|im_end|>
        input_ids.extend_from_slice(&user_ids);
        input_ids.push(IM_END);
        // \n<|im_start|>assistant\n
        input_ids.push(IM_START);
        input_ids.extend_from_slice(&asst_ids);

        let total_len = input_ids.len();

        debug!(
            layer = "senses",
            component = "vision",
            total_len,
            prompt_tokens = user_ids.len(),
            "built input token sequence"
        );

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
        // Find positions where input_ids == IMAGE_TOKEN_ID and replace embeddings.
        let mut feat_idx = 0;
        for pos in 0..total_len {
            if input_ids[pos] == IMAGE_TOKEN_ID && feat_idx < num_image_tokens {
                for j in 0..hidden_size {
                    inputs_embeds[[0, pos, j]] = image_features[[0, feat_idx, j]];
                }
                feat_idx += 1;
            }
        }

        // ---- Step 5: Autoregressive decoder loop ----
        let mut attn_vec: Vec<i64> = vec![1i64; total_len];
        let mut pos_vec: Vec<i64> = (0..total_len as i64).collect();

        let mut kv_cache: Vec<ArrayD<f32>> = (0..(NUM_HIDDEN_LAYERS * 2))
            .map(|_| ArrayD::zeros(IxDyn(&[1, NUM_KV_HEADS, 0, HEAD_DIM])))
            .collect();

        let mut generated: Vec<i64> = Vec::new();
        let mut cur_embeds = inputs_embeds;

        for step in 0..MAX_NEW_TOKENS {
            let seq_len = cur_embeds.shape()[1];

            let embeds_t = Tensor::from_array(cur_embeds.clone()).context("embeds")?;
            let attn_a = Array2::from_shape_vec((1, attn_vec.len()), attn_vec.clone())
                .context("attn array")?;
            let attn_t = Tensor::from_array(attn_a).context("attn")?;
            let pos_a = Array2::from_shape_vec(
                (1, seq_len),
                pos_vec[pos_vec.len() - seq_len..].to_vec(),
            )
            .context("pos array")?;
            let pos_t = Tensor::from_array(pos_a).context("pos")?;

            let mut dec_inputs = ort::inputs![
                "inputs_embeds" => embeds_t,
                "attention_mask" => attn_t,
                "position_ids" => pos_t,
            ];

            for layer in 0..NUM_HIDDEN_LAYERS {
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

            let mut best_tok: i64 = 0;
            let mut best_val = f32::NEG_INFINITY;
            for v in 0..vocab {
                let s = logits[[0, last, v]];
                if s > best_val {
                    best_val = s;
                    best_tok = v as i64;
                }
            }

            if best_tok == IM_END || best_tok == 0 {
                debug!(layer = "senses", component = "vision", step, "EOS");
                break;
            }
            generated.push(best_tok);

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

        Ok(description)
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

        let description = tokio::task::spawn_blocking(move || {
            let (pixel_values, pixel_mask) = Self::preprocess(&image_clone);

            let mut guard = sessions
                .lock()
                .map_err(|e| anyhow::anyhow!("session mutex poisoned: {e}"))?;

            Self::run_inference(&mut guard, &tokenizer, pixel_values, pixel_mask)
        })
        .await
        .context("vision inference task panicked")??;

        // Simple keyword-based error detection.
        let lower = description.to_lowercase();
        let has_error = ["error", "exception", "crash", "fatal", "traceback", "not responding"]
            .iter()
            .any(|kw| lower.contains(kw));

        Ok(VisionOutput {
            description,
            has_error_visible: has_error,
            confidence: 0.8,
        })
    }

    fn model_name(&self) -> &str {
        "smolvlm-256m-onnx"
    }

    /// Warm up all three ONNX sessions.
    #[instrument(skip_all, fields(layer = "senses", component = "vision"))]
    async fn warmup(&self) -> anyhow::Result<()> {
        info!(
            layer = "senses",
            component = "vision",
            "warming up SmolVLM model"
        );

        let dummy = DynamicImage::new_rgb8(IMAGE_SIZE, IMAGE_SIZE);
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

    #[tokio::test]
    async fn test_new_with_nonexistent_directory_returns_error() {
        let result = OnnxVisionModel::new("/nonexistent/path/to/model").await;
        assert!(result.is_err());
        let err_str = format!("{:#}", result.unwrap_err());
        assert!(
            err_str.contains("not found") || err_str.contains("Not found"),
            "expected 'not found' in error, got: {err_str}"
        );
    }

    #[tokio::test]
    async fn test_new_with_empty_directory_returns_model_file_error() {
        let tmp = std::env::temp_dir().join("kairo-vision-test-empty");
        let _ = std::fs::create_dir_all(&tmp);
        let result = OnnxVisionModel::new(&tmp).await;
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
        assert!(
            (r - 1.0).abs() < 0.01,
            "expected ~1.0, got {r}"
        );
    }

    /// Integration test: load real model and describe a screenshot.
    ///
    /// This test requires the SmolVLM model files to be downloaded.
    /// Run `scripts/download-models.ps1` first.
    /// Skipped if models are not present.
    #[tokio::test]
    async fn test_describe_real_screenshot() {
        let model_dir = dirs::home_dir()
            .unwrap()
            .join(".kairo-dev/models/vision/smolvlm-256m");

        if !model_dir.join("decoder.onnx").exists() {
            eprintln!("Skipping integration test: model files not downloaded");
            return;
        }

        let model = OnnxVisionModel::new(&model_dir)
            .await
            .expect("should load model");

        // Use the test fixture screenshot.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vscode-screenshot.jpg");

        if !fixture.exists() {
            eprintln!("Skipping: test fixture not found at {}", fixture.display());
            return;
        }

        let img = image::open(&fixture).expect("should open test image");
        let output = model.describe(&img).await.expect("describe should succeed");

        eprintln!("Description: {}", output.description);
        assert!(
            !output.description.is_empty(),
            "description should not be empty"
        );
        assert!(
            !output.description.contains("placeholder"),
            "should not contain placeholder text"
        );
    }
}
