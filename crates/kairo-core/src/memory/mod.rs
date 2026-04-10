//! # Memory system
//!
//! Kairo uses three stores for three kinds of memory, mirroring the cognitive
//! science distinction between raw experience, episodic memory, and semantic
//! knowledge:
//!
//! - [`raw_log`] — SQLite store of every perception frame, verbatim
//! - [`episodic`] — LanceDB vector store of distilled memories
//! - [`semantic`] — SQLite store of stable facts and relationships
//!
//! The memory retrieval flow runs before every orchestrator wake-up:
//! 1. Vector search episodic memory using the current frame as query
//! 2. Re-rank results via the triage LLM for relevance
//! 3. Inject top results into the orchestrator's context

pub mod episodic;
pub mod raw_log;
pub mod semantic;
