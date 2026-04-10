//! # Perception frame builder
//!
//! Combines observations from the three senses watchers (vision, audio, context)
//! into unified [`PerceptionFrame`] objects at a configurable interval (2–5 seconds).
//!
//! Also computes the `salience_hint` — a rule-based score (0.0 to 1.0) that
//! pre-filters frames before they reach the triage LLM. Heuristics include:
//! - Frame identical to previous? salience = 0.0
//! - New error visible on screen? salience += 0.3
//! - User spoke within last 5 seconds? salience += 0.4
//! - New window focused? salience += 0.2
//! - Calendar event within 15 minutes? salience += 0.3
//!
//! Only frames above threshold (default 0.15) reach the triage layer.
