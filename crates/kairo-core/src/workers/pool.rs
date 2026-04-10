//! # Worker pool
//!
//! Manages concurrent Claude Code worker sessions. Enforces the
//! `max_concurrent_workers` setting (default 3, max 10) and queues
//! excess requests. Tracks worker status for the dashboard Home tab.
