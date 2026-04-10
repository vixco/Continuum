//! # kairo-vision
//!
//! Local vision model runtime for Kairo's senses layer (Layer 1).
//!
//! Wraps Moondream 2 (or alternatives like Florence-2, MiniCPM-V) via
//! ONNX Runtime to provide single-sentence screen descriptions for the
//! perception frame builder.
//!
//! Default model: Moondream 2 (1.8B parameters, ~300 MB RAM, ~1s per image on CPU).
//! Model files are stored in `~/.kairo/models/vision/` and never checked into git.
