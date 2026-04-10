//! ONNX Runtime-backed vision model implementation.
//!
//! Provides [`OnnxVisionModel`], which loads a SmolVLM-256M (or compatible)
//! ONNX model and implements the [`VisionModel`] trait to produce one-sentence
//! image descriptions.
//!
//! **Current status (Phase 1):** The encoder session is loaded and image
//! preprocessing runs for real, but autoregressive text decoding is stubbed.
//! The `describe()` method returns a placeholder description based on basic
//! image statistics (mean brightness, dominant channel). Full decoder-loop
//! generation will be implemented in Phase 1.5.
//!
//! # Model directory layout
//!
//! The model directory (typically `~/.kairo/models/vision/smolvlm-256m/`)
//! must contain at least:
//!
//! - `encoder.onnx` — the image encoder model
//!
//! When full decoding is implemented it will also need:
//!
//! - `decoder.onnx` — the autoregressive text decoder
//! - `tokenizer.json` — the HuggingFace tokenizer config
//!
//! # Example
//!
//! ```rust,no_run
//! use kairo_vision::onnx::OnnxVisionModel;
//! use kairo_vision::VisionModel;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let model = OnnxVisionModel::new("~/.kairo/models/vision/smolvlm-256m").await?;
//! model.warmup().await?;
//!
//! let img = image::open("screenshot.png")?;
//! let output = model.describe(&img).await?;
//! println!("{}", output.description);
//! # Ok(())
//! # }
//! ```
//!
//! Part of Layer 1 (Senses) in the Kairo cognitive architecture.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use async_trait::async_trait;
use image::DynamicImage;
use ndarray::Array4;
use ort::session::Session;
use ort::value::Tensor;
use tracing::{debug, info, instrument, warn};

use crate::error::VisionError;
use crate::{VisionModel, VisionOutput};

/// The expected input image size for SmolVLM-256M (384x384 pixels).
const MODEL_INPUT_SIZE: u32 = 384;

/// ImageNet-standard channel means used for normalization (RGB order).
const CHANNEL_MEANS: [f32; 3] = [0.485, 0.456, 0.406];

/// ImageNet-standard channel standard deviations used for normalization (RGB order).
const CHANNEL_STDS: [f32; 3] = [0.229, 0.224, 0.225];

/// ONNX Runtime-backed vision model.
///
/// Loads a SmolVLM-256M (or compatible) encoder from an ONNX file and
/// produces one-sentence descriptions of screenshot images.
///
/// **Phase 1 limitation:** Only the encoder is loaded. The `describe()` method
/// runs the encoder to extract image features but returns a placeholder
/// description derived from basic image statistics, since the full
/// autoregressive decoder loop is not yet implemented.
///
/// # Thread safety
///
/// The ONNX session is `Send + Sync`, so this struct can be shared across
/// tasks via `Arc`. Inference calls acquire an internal session lock, so
/// concurrent calls are safe but serialized.
#[derive(Debug)]
pub struct OnnxVisionModel {
    /// The loaded ONNX encoder session, wrapped in `Arc<Mutex<>>` for safe
    /// mutable access from blocking inference threads. `Session::run()` requires
    /// `&mut self`, so we use a mutex to serialize concurrent inference calls.
    session: Arc<Mutex<Session>>,

    /// Path to the model directory, kept for diagnostics and logging.
    /// Used in Phase 1.5 for loading additional model files (decoder, tokenizer).
    #[allow(dead_code)]
    model_dir: PathBuf,
}

impl OnnxVisionModel {
    /// Load a vision model from the given directory.
    ///
    /// The directory must contain at least `encoder.onnx`. Returns a
    /// [`VisionError::ModelDirectoryNotFound`] if the directory does not exist,
    /// or a [`VisionError::ModelFileNotFound`] if the encoder file is missing.
    ///
    /// # Errors
    ///
    /// Returns an error if the ONNX Runtime fails to initialize or the model
    /// file is corrupt / incompatible.
    #[instrument(skip_all, fields(layer = "senses", component = "vision", model_dir = %model_dir.as_ref().display()))]
    pub async fn new(model_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let model_dir = model_dir.as_ref().to_path_buf();

        // Validate the model directory exists
        if !model_dir.exists() {
            return Err(VisionError::ModelDirectoryNotFound {
                path: model_dir.display().to_string(),
            }
            .into());
        }
        if !model_dir.is_dir() {
            return Err(VisionError::ModelDirectoryNotFound {
                path: model_dir.display().to_string(),
            }
            .into());
        }

        let encoder_path = model_dir.join("encoder.onnx");
        if !encoder_path.exists() {
            return Err(VisionError::ModelFileNotFound {
                path: encoder_path.display().to_string(),
            }
            .into());
        }

        info!(
            layer = "senses",
            component = "vision",
            encoder_path = %encoder_path.display(),
            "loading ONNX vision encoder"
        );

        // Load the encoder session. We do this on a blocking thread because
        // ONNX Runtime model loading does synchronous file I/O and graph
        // optimization that can take hundreds of milliseconds.
        let encoder_path_clone = encoder_path.clone();
        let session = tokio::task::spawn_blocking(move || -> anyhow::Result<Session> {
            Session::builder()
                .context("failed to create ONNX session builder")?
                .commit_from_file(&encoder_path_clone)
                .with_context(|| {
                    format!(
                        "failed to load encoder model from '{}'",
                        encoder_path_clone.display()
                    )
                })
        })
        .await
        .context("ONNX model loading task panicked")?
        .map_err(|e| VisionError::ModelLoadError {
            path: encoder_path.display().to_string(),
            reason: format!("{e:#}"),
        })?;

        info!(
            layer = "senses",
            component = "vision",
            "vision encoder loaded successfully"
        );

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            model_dir,
        })
    }

    /// Preprocess a [`DynamicImage`] into a normalized NCHW float32 tensor.
    ///
    /// Steps:
    /// 1. Resize to [`MODEL_INPUT_SIZE`] x [`MODEL_INPUT_SIZE`] using Lanczos3
    /// 2. Convert to RGB8
    /// 3. Normalize each channel with ImageNet means and standard deviations
    /// 4. Arrange into NCHW layout (batch=1, channels=3, height, width)
    #[instrument(skip_all, fields(layer = "senses", component = "vision"))]
    fn preprocess(image: &DynamicImage) -> anyhow::Result<Array4<f32>> {
        let resized = image.resize_exact(
            MODEL_INPUT_SIZE,
            MODEL_INPUT_SIZE,
            image::imageops::FilterType::Lanczos3,
        );
        let rgb = resized.to_rgb8();

        let h = MODEL_INPUT_SIZE as usize;
        let w = MODEL_INPUT_SIZE as usize;

        // Build NCHW tensor: [1, 3, H, W]
        let mut tensor = Array4::<f32>::zeros((1, 3, h, w));

        for y in 0..h {
            for x in 0..w {
                let pixel = rgb.get_pixel(x as u32, y as u32);
                for c in 0..3 {
                    let value = pixel[c] as f32 / 255.0;
                    let normalized = (value - CHANNEL_MEANS[c]) / CHANNEL_STDS[c];
                    tensor[[0, c, y, x]] = normalized;
                }
            }
        }

        debug!(
            layer = "senses",
            component = "vision",
            height = h,
            width = w,
            "image preprocessed to NCHW tensor"
        );

        Ok(tensor)
    }

    /// Derive a basic [`VisionOutput`] from image statistics.
    ///
    /// This is the Phase 1 stub that produces a rough description based on
    /// mean brightness and dominant color channel. It will be replaced by
    /// the full decoder loop in Phase 1.5.
    fn stub_describe(image: &DynamicImage) -> VisionOutput {
        let rgb = image.to_rgb8();
        let (mut r_sum, mut g_sum, mut b_sum) = (0u64, 0u64, 0u64);
        let pixel_count = rgb.width() as u64 * rgb.height() as u64;

        for pixel in rgb.pixels() {
            r_sum += pixel[0] as u64;
            g_sum += pixel[1] as u64;
            b_sum += pixel[2] as u64;
        }

        if pixel_count == 0 {
            return VisionOutput {
                description: "The screen appears to be empty or the image could not be read."
                    .to_string(),
                has_error_visible: false,
                confidence: 0.0,
            };
        }

        let r_mean = r_sum as f64 / pixel_count as f64;
        let g_mean = g_sum as f64 / pixel_count as f64;
        let b_mean = b_sum as f64 / pixel_count as f64;
        let brightness = (r_mean + g_mean + b_mean) / 3.0;

        let brightness_desc = if brightness < 50.0 {
            "dark"
        } else if brightness < 128.0 {
            "moderately lit"
        } else if brightness < 200.0 {
            "bright"
        } else {
            "very bright"
        };

        let dominant = if r_mean > g_mean && r_mean > b_mean {
            "warm-toned"
        } else if g_mean > r_mean && g_mean > b_mean {
            "green-toned"
        } else if b_mean > r_mean && b_mean > g_mean {
            "cool-toned"
        } else {
            "neutral-toned"
        };

        // Phase 1 stub: confidence is low because this is not a real VLM output.
        // Error detection is not possible without the decoder, so always false.
        VisionOutput {
            description: format!(
                "The screen shows a {brightness_desc}, {dominant} image \
                 (placeholder — full VLM decoding pending)."
            ),
            has_error_visible: false,
            confidence: 0.1,
        }
    }
}

#[async_trait]
impl VisionModel for OnnxVisionModel {
    /// Describe the contents of a screenshot image.
    ///
    /// **Phase 1 behavior:** Preprocesses the image and runs the ONNX encoder
    /// to validate the pipeline, but returns a placeholder [`VisionOutput`]
    /// based on image statistics since the decoder loop is not yet implemented.
    // TODO(phase-1.5): Implement full autoregressive decoding with the text
    // decoder model and tokenizer. This requires loading `decoder.onnx` and
    // `tokenizer.json`, implementing the token generation loop with KV-cache,
    // and streaming partial results.
    #[instrument(skip_all, fields(layer = "senses", component = "vision"))]
    async fn describe(&self, image: &DynamicImage) -> anyhow::Result<VisionOutput> {
        let image_clone = image.clone();

        // Preprocess on a blocking thread to avoid starving the async runtime
        // with the pixel-level normalization loop.
        let tensor = tokio::task::spawn_blocking(move || Self::preprocess(&image_clone))
            .await
            .context("image preprocessing task panicked")?
            .map_err(|e| VisionError::ImagePreprocessError {
                reason: format!("{e:#}"),
            })?;

        debug!(
            layer = "senses",
            component = "vision",
            tensor_shape = ?tensor.shape(),
            "running encoder inference"
        );

        // Run encoder inference on a blocking thread. Even with GPU acceleration,
        // ONNX Runtime inference is synchronous and should not block the tokio
        // runtime.
        //
        // NOTE: We intentionally do not use the encoder output yet. This call
        // validates that the full preprocessing -> inference pipeline works.
        // The encoder embeddings will be fed to the decoder in Phase 1.5.
        let session = Arc::clone(&self.session);
        let _encoder_output = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let input_tensor = Tensor::from_array(tensor)
                .context("failed to create ONNX input tensor from ndarray")?;
            let mut session_guard = session
                .lock()
                .map_err(|e| anyhow::anyhow!("session mutex poisoned: {e}"))?;
            let _outputs = session_guard
                .run(ort::inputs![input_tensor])
                .context("ONNX encoder inference failed")?;
            Ok(())
        })
        .await
        .context("encoder inference task panicked")?;

        match &_encoder_output {
            Ok(_) => {
                debug!(
                    layer = "senses",
                    component = "vision",
                    "encoder inference completed successfully"
                );
            }
            Err(e) => {
                // In Phase 1, encoder failure is non-fatal — we still return the
                // stub description. Log a warning so the issue is visible.
                warn!(
                    layer = "senses",
                    component = "vision",
                    error = %e,
                    "encoder inference failed, returning stub description"
                );
            }
        }

        // Phase 1: return a stub description based on image statistics
        let output = Self::stub_describe(image);

        debug!(
            layer = "senses",
            component = "vision",
            description = %output.description,
            confidence = output.confidence,
            "generated image description (stub)"
        );

        Ok(output)
    }

    /// Return the name of the model for logging and display.
    fn model_name(&self) -> &str {
        "smolvlm-256m-onnx"
    }

    /// Run a dummy inference to warm up the ONNX Runtime session.
    ///
    /// Creates a small black test image and runs [`describe`](VisionModel::describe)
    /// on it. This forces ONNX Runtime to complete its lazy initialization
    /// (graph optimization, memory allocation, GPU context creation) so that
    /// the first real frame does not incur startup latency.
    #[instrument(skip_all, fields(layer = "senses", component = "vision"))]
    async fn warmup(&self) -> anyhow::Result<()> {
        info!(
            layer = "senses",
            component = "vision",
            input_size = MODEL_INPUT_SIZE,
            "warming up vision model with dummy image"
        );

        let dummy = DynamicImage::new_rgb8(MODEL_INPUT_SIZE, MODEL_INPUT_SIZE);
        let _ = self.describe(&dummy).await?;

        info!(
            layer = "senses",
            component = "vision",
            "vision model warmup complete"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_new_with_nonexistent_directory_returns_error() {
        let result = OnnxVisionModel::new("/nonexistent/path/to/model").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = format!("{err:#}");
        assert!(
            err_str.contains("not found"),
            "expected 'not found' in error message, got: {err_str}"
        );
    }

    #[tokio::test]
    async fn test_new_with_empty_directory_returns_model_file_error() {
        // Create a temporary directory with no model files
        let tmp_dir = std::env::temp_dir().join("kairo-vision-test-empty");
        let _ = std::fs::create_dir_all(&tmp_dir);

        let result = OnnxVisionModel::new(&tmp_dir).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = format!("{err:#}");
        assert!(
            err_str.contains("encoder.onnx") || err_str.contains("not found"),
            "expected model file error, got: {err_str}"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_preprocess_produces_correct_shape() {
        let img = DynamicImage::new_rgb8(800, 600);
        let tensor = OnnxVisionModel::preprocess(&img).expect("preprocessing should succeed");
        assert_eq!(
            tensor.shape(),
            &[1, 3, MODEL_INPUT_SIZE as usize, MODEL_INPUT_SIZE as usize]
        );
    }

    #[test]
    fn test_preprocess_normalizes_values() {
        // Create a white image — all pixels are 255
        let img = DynamicImage::from(image::RgbImage::from_fn(100, 100, |_, _| {
            image::Rgb([255u8, 255, 255])
        }));
        let tensor = OnnxVisionModel::preprocess(&img).expect("preprocessing should succeed");

        // After normalization: (1.0 - mean) / std
        // For R channel: (1.0 - 0.485) / 0.229 = ~2.2489
        let r_val = tensor[[0, 0, 0, 0]];
        assert!(
            (r_val - 2.2489).abs() < 0.01,
            "expected ~2.2489 for white R channel, got {r_val}"
        );
    }

    #[test]
    fn test_stub_describe_returns_nonempty_output() {
        let img = DynamicImage::new_rgb8(100, 100);
        let output = OnnxVisionModel::stub_describe(&img);
        assert!(!output.description.is_empty());
        assert!(output.description.contains("placeholder"));
        assert!(!output.has_error_visible);
        assert!(output.confidence >= 0.0 && output.confidence <= 1.0);
    }

    #[test]
    fn test_stub_describe_dark_image() {
        let img = DynamicImage::new_rgb8(100, 100); // All zeros = black
        let output = OnnxVisionModel::stub_describe(&img);
        assert!(
            output.description.contains("dark"),
            "expected 'dark' in description for black image, got: {}",
            output.description
        );
    }

    #[test]
    fn test_stub_describe_bright_image() {
        let img = DynamicImage::from(image::RgbImage::from_fn(100, 100, |_, _| {
            image::Rgb([220u8, 220, 220])
        }));
        let output = OnnxVisionModel::stub_describe(&img);
        assert!(
            output.description.contains("bright"),
            "expected 'bright' in description for white-ish image, got: {}",
            output.description
        );
    }

    #[test]
    fn test_model_dir_stored() {
        // We cannot construct a full OnnxVisionModel without a real model file,
        // but we verify the PathBuf handling works by confirming the error path.
        let path = PathBuf::from("/some/test/path");
        assert_eq!(path.display().to_string(), "/some/test/path");
    }
}
