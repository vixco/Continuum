//! Vision-specific error types for the `kairo-vision` crate.
//!
//! Uses `thiserror` for structured, library-grade errors that callers
//! can match on. These are distinct from the `anyhow` errors used at
//! application boundaries.
//!
//! Part of Layer 1 (Senses) in the Kairo cognitive architecture.

/// Errors that can occur during vision model operations.
#[derive(Debug, thiserror::Error)]
pub enum VisionError {
    /// Failed to load one or more ONNX model files from the model directory.
    #[error("failed to load vision model from '{path}': {reason}")]
    ModelLoadError {
        /// The path that was attempted.
        path: String,
        /// A human-readable explanation of what went wrong.
        reason: String,
    },

    /// Failed during ONNX inference execution.
    #[error("vision inference failed: {reason}")]
    InferenceError {
        /// A human-readable explanation of the inference failure.
        reason: String,
    },

    /// Failed to preprocess an image before feeding it to the model.
    #[error("image preprocessing failed: {reason}")]
    ImagePreprocessError {
        /// A human-readable explanation of the preprocessing failure.
        reason: String,
    },

    /// The model directory does not exist or is not a directory.
    #[error("model directory not found: '{path}'")]
    ModelDirectoryNotFound {
        /// The path that was checked.
        path: String,
    },

    /// A required model file is missing from the model directory.
    #[error("required model file not found: '{path}'")]
    ModelFileNotFound {
        /// The expected file path.
        path: String,
    },
}
