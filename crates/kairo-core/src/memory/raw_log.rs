//! # Raw log storage
//!
//! SQLite-backed store for every perception frame produced by the senses layer.
//! One row per frame, including screenshot paths, transcripts, and context.
//!
//! Retention: default 30 days, configurable 1–365. Rotated nightly.
//! Screenshots are saved to `~/.kairo/screenshots/<date>/` as files,
//! with paths stored in the database (not blobs).
