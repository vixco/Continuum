//! # Semantic memory
//!
//! SQLite-backed store for stable facts about the user, their projects,
//! relationships, and preferences. Uses a key-value table with confidence
//! scores and a graph structure for relationships between facts.
//!
//! Examples: `user.name`, `project.simcharts.stack`, `routine.morning_start_time`.
