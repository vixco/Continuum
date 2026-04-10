//! # kairo-llm
//!
//! Local LLM runtime wrapper for Kairo's triage layer (Layer 2).
//!
//! Wraps llama.cpp via Rust bindings to provide:
//! - Model loading from GGUF files
//! - Streaming text generation
//! - JSON-constrained generation via grammar mode
//! - GPU acceleration when CUDA is available, CPU fallback otherwise
//!
//! Default model: Qwen 2.5 3B Instruct (Q4_K_M quantization).
//! The llama.cpp native dependency will be added in Phase 2.
