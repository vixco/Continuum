//! # Episodic memory
//!
//! LanceDB-backed vector store for distilled memories. Every 10–15 minutes,
//! a background task reads the raw log and uses the triage LLM to summarize
//! the window into 1–5 episodic memory entries, which are embedded via
//! fastembed and stored with vector indexing.
//!
//! Retrieved via semantic similarity when the orchestrator wakes up.
//! No automatic deletion — users manage retention via the dashboard.
