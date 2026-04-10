//! # kairo-vision
//!
//! Local vision model runtime for Kairo's senses layer (Layer 1).
//!
//! This crate provides the [`VisionModel`] trait for abstracting over different
//! vision backends, and an [`OnnxVisionModel`](onnx::OnnxVisionModel) implementation
//! that uses SmolVLM-256M via ONNX Runtime to produce one-sentence screen
//! descriptions for the perception frame builder.
//!
//! # Architecture
//!
//! In the Kairo four-layer cognitive architecture, vision sits in Layer 1
//! (Senses). The pipeline is:
//!
//! 1. A screenshot is captured (by the MCP `perception_screenshot` tool)
//! 2. The image is passed to [`VisionModel::describe()`]
//! 3. The resulting [`VisionOutput`] is included in the perception frame
//! 4. The frame flows up to Layer 2 (Triage) for decision-making
//!
//! # Model files
//!
//! Model files live in `~/.kairo/models/vision/` and are **never** checked
//! into git. Use `scripts/download-models.ps1` to fetch them.
//!
//! Default model: SmolVLM-256M (0.25B parameters, ~200 MB RAM, ~500ms per
//! image on CPU). FP16 quantization by default, configurable via the
//! dashboard.
//!
//! # Example
//!
//! ```rust,no_run
//! use kairo_vision::VisionModel;
//! use kairo_vision::onnx::OnnxVisionModel;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let model = OnnxVisionModel::new("~/.kairo/models/vision/smolvlm-256m").await?;
//! model.warmup().await?;
//!
//! let img = image::open("screenshot.png")?;
//! let output = model.describe(&img).await?;
//! println!("Screen shows: {}", output.description);
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod onnx;

use anyhow::Result;
use async_trait::async_trait;
use image::DynamicImage;

/// Output produced by a vision model when describing a screenshot.
///
/// Contains the description text, whether an error dialog or stack trace
/// was detected, and the model's confidence in its output.
#[derive(Debug, Clone)]
pub struct VisionOutput {
    /// One-sentence description of the screen contents.
    pub description: String,
    /// Whether the model detected an error dialog, stack trace, or similar.
    pub has_error_visible: bool,
    /// Model's confidence in the description (0.0 to 1.0).
    pub confidence: f32,
}

/// Trait for local vision models that describe screenshots.
///
/// Implementations wrap a specific model runtime (ONNX, llama.cpp, etc.)
/// and produce a [`VisionOutput`] from a [`DynamicImage`].
///
/// The trait is object-safe so that `kairo-core` can hold a
/// `Arc<dyn VisionModel>` without knowing the concrete model type.
///
/// # Implementors
///
/// - [`onnx::OnnxVisionModel`] — ONNX Runtime backend (default)
///
/// Additional backends (e.g., llama.cpp multimodal, direct CUDA) can be
/// added by implementing this trait.
#[async_trait]
pub trait VisionModel: Send + Sync {
    /// Describe the contents of a screenshot image.
    ///
    /// Returns a [`VisionOutput`] with a one-sentence description,
    /// error detection flag, and confidence score. Implementations
    /// should handle errors internally and return low-confidence
    /// empty descriptions rather than panicking.
    ///
    /// # Errors
    ///
    /// Returns an error if image preprocessing or model inference fails.
    async fn describe(&self, image: &DynamicImage) -> Result<VisionOutput>;

    /// Return the name of the model for logging and display.
    fn model_name(&self) -> &str;

    /// Run one dummy inference to warm up the model.
    ///
    /// Call this at startup to force lazy initialization (graph optimization,
    /// memory allocation, GPU context creation) so that the first real frame
    /// does not incur startup latency.
    ///
    /// # Errors
    ///
    /// Returns an error if the warmup inference fails, which typically
    /// indicates a fundamental problem with the model or runtime.
    async fn warmup(&self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that `VisionModel` can be used as a trait object.
    ///
    /// This is a compile-time check that the trait is object-safe and that
    /// `Box<dyn VisionModel>` works as expected in the orchestration layer.
    #[tokio::test]
    async fn test_vision_model_is_object_safe() {
        struct MockVisionModel;

        #[async_trait]
        impl VisionModel for MockVisionModel {
            async fn describe(&self, _image: &DynamicImage) -> Result<VisionOutput> {
                Ok(VisionOutput {
                    description: "mock description".to_string(),
                    has_error_visible: false,
                    confidence: 0.9,
                })
            }
            fn model_name(&self) -> &str {
                "mock-vision"
            }
            async fn warmup(&self) -> Result<()> {
                Ok(())
            }
        }

        let model: Box<dyn VisionModel> = Box::new(MockVisionModel);
        let img = DynamicImage::new_rgb8(10, 10);

        let output = model.describe(&img).await.expect("mock should not fail");
        assert_eq!(output.description, "mock description");
        assert!(!output.has_error_visible);
        assert!((output.confidence - 0.9).abs() < f32::EPSILON);
        assert_eq!(model.model_name(), "mock-vision");

        model.warmup().await.expect("mock warmup should not fail");
    }

    /// Verify that `Arc<dyn VisionModel>` works for shared ownership across tasks.
    #[tokio::test]
    async fn test_vision_model_works_with_arc() {
        struct MockVisionModel;

        #[async_trait]
        impl VisionModel for MockVisionModel {
            async fn describe(&self, _image: &DynamicImage) -> Result<VisionOutput> {
                Ok(VisionOutput {
                    description: "arc mock".to_string(),
                    has_error_visible: false,
                    confidence: 0.8,
                })
            }
            fn model_name(&self) -> &str {
                "mock-arc"
            }
            async fn warmup(&self) -> Result<()> {
                Ok(())
            }
        }

        let model: std::sync::Arc<dyn VisionModel> = std::sync::Arc::new(MockVisionModel);
        let model_clone = model.clone();

        let handle = tokio::spawn(async move {
            let img = DynamicImage::new_rgb8(10, 10);
            model_clone.describe(&img).await
        });

        let result = handle.await.expect("task should not panic");
        let output = result.expect("describe should succeed");
        assert_eq!(output.description, "arc mock");
    }

    #[test]
    fn test_vision_output_clone() {
        let output = VisionOutput {
            description: "test".to_string(),
            has_error_visible: true,
            confidence: 0.5,
        };
        let cloned = output.clone();
        assert_eq!(cloned.description, "test");
        assert!(cloned.has_error_visible);
        assert!((cloned.confidence - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_vision_output_debug() {
        let output = VisionOutput {
            description: "debug test".to_string(),
            has_error_visible: false,
            confidence: 0.75,
        };
        let debug_str = format!("{output:?}");
        assert!(debug_str.contains("debug test"));
        assert!(debug_str.contains("0.75"));
    }
}
