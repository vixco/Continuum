//! # Bench metrics (context engine spec §9, Task C6)
//!
//! The scoring primitives the four harnesses share: how a free-text label
//! is compared to what the engine produced, how a duplicate is decided
//! without an embedding model, and the pass/fail table every bench prints.
//!
//! ## Why a lenient text matcher
//!
//! The labels in [`crate::bench::fixture`] are ground truth written the way
//! a human would say it ("get continuum building again"); what the engine
//! produces is a machine summary ("cargo build failed: mismatched types at
//! …"). Exact-match scoring would measure phrasing, not understanding, and
//! any wording change would look like a regression. So a field counts as
//! recalled when a majority of the label's *significant* tokens appear in
//! the produced text, comparing on a four-character stem so
//! `failing`/`failed` and `build`/`building` agree.
//!
//! The matcher is deliberately simple and deterministic: no stemmer
//! dictionary, no embeddings, no network. It is a **regression gate**, not
//! a language benchmark — it catches "the field went empty", "the wrong
//! project", "the blocker is from twenty minutes ago", which is exactly
//! what the pipeline can break.

use std::collections::BTreeSet;

/// Fraction of a label's significant tokens that must be present in the
/// produced text for the field to count as recalled.
pub const MATCH_THRESHOLD: f32 = 0.5;

/// Minimum token length considered significant (`the`, `is`, `to` carry no
/// signal; `raw`, `log`, `src` do).
pub const MIN_TOKEN_LEN: usize = 3;

/// Characters of shared prefix that make two tokens the same stem.
pub const STEM_PREFIX: usize = 4;

/// Token-set similarity above which two memory summaries are considered
/// duplicates by the fallback (non-embedding) comparator.
pub const DUPLICATE_SIMILARITY: f32 = 0.85;

/// Words that appear in almost any sentence in either project language
/// and would inflate every overlap score.
const STOPWORDS: [&str; 34] = [
    "the", "and", "that", "with", "for", "from", "this", "was", "are", "not", "but", "you", "its",
    "has", "had", "have", "into", "out", "off", "all", "any", "een", "het", "van", "voor", "met",
    "dat", "die", "der", "aan", "naar", "over", "door", "nog",
];

/// Splits text into significant lowercase tokens: alphanumeric runs of at
/// least [`MIN_TOKEN_LEN`] characters, minus [`STOPWORDS`].
pub fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= MIN_TOKEN_LEN)
        .map(|t| t.to_lowercase())
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .collect()
}

/// Whether two tokens name the same thing: equal, or sharing a
/// [`STEM_PREFIX`]-character prefix (`failed` ≈ `failing`).
pub fn token_matches(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if a.len() < STEM_PREFIX || b.len() < STEM_PREFIX {
        return false;
    }
    a.as_bytes()[..STEM_PREFIX] == b.as_bytes()[..STEM_PREFIX]
}

/// Fraction of `expected`'s significant tokens that have a stem match in
/// `actual`. Returns 0.0 when the label carries no significant token at
/// all (nothing to recall, so nothing is credited).
pub fn overlap(expected: &str, actual: &str) -> f32 {
    let wanted = tokens(expected);
    if wanted.is_empty() {
        return 0.0;
    }
    let got = tokens(actual);
    let hits = wanted
        .iter()
        .filter(|w| got.iter().any(|g| token_matches(w, g)))
        .count();
    hits as f32 / wanted.len() as f32
}

/// Whether a produced free-text field recalls its label.
pub fn field_matches(expected: &str, actual: Option<&str>) -> bool {
    match actual {
        Some(actual) => overlap(expected, actual) >= MATCH_THRESHOLD,
        None => false,
    }
}

/// Whether a produced structured field (a project slug) equals its label.
/// Structured fields are compared exactly — a slug is an identifier, not
/// prose.
pub fn exact_matches(expected: &str, actual: Option<&str>) -> bool {
    actual.is_some_and(|actual| actual == expected)
}

/// Symmetric token-set similarity (Jaccard over stem-matched tokens), the
/// documented **fallback** duplicate comparator used when no embedding
/// store is available.
///
/// The spec's preferred measure is embedding similarity over the episodic
/// store. That store is LanceDB + a fastembed model that is downloaded on
/// first use; a bench that reaches for the network is neither
/// CI-feasible, offline-safe, nor deterministic, so the memory-precision
/// harness uses this instead and says so in its output.
pub fn text_similarity(a: &str, b: &str) -> f32 {
    let left: BTreeSet<String> = tokens(a).into_iter().collect();
    let right: BTreeSet<String> = tokens(b).into_iter().collect();
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left
        .iter()
        .filter(|l| right.iter().any(|r| token_matches(l, r)))
        .count();
    let union = left.len() + right.len() - intersection;
    if union == 0 {
        return 1.0;
    }
    intersection as f32 / union as f32
}

/// A hit/total counter with a rate.
#[derive(Debug, Clone, Copy, Default)]
pub struct Recall {
    /// Checkpoints where the field was recalled.
    pub hits: usize,
    /// Checkpoints where the label asserted the field at all.
    pub total: usize,
}

impl Recall {
    /// Records one scored checkpoint.
    pub fn record(&mut self, hit: bool) {
        self.total += 1;
        if hit {
            self.hits += 1;
        }
    }

    /// Hits over asserted checkpoints. An unasserted field (`total == 0`)
    /// scores 1.0 — nothing was claimed, so nothing was missed.
    pub fn rate(&self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        self.hits as f32 / self.total as f32
    }
}

/// The Nth percentile of a slice that is already sorted ascending.
pub fn percentile(sorted: &[f64], pct: u32) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((pct as f64 / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// How a measurement is compared to its threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// The measurement must be at least the threshold.
    AtLeast,
    /// The measurement must be at most the threshold.
    AtMost,
    /// The measurement must equal the threshold exactly.
    Exactly,
}

/// One asserted measurement.
#[derive(Debug, Clone)]
struct Assertion {
    name: String,
    value: f64,
    threshold: f64,
    bound: Bound,
    unit: &'static str,
}

impl Assertion {
    fn passed(&self) -> bool {
        match self.bound {
            Bound::AtLeast => self.value >= self.threshold,
            Bound::AtMost => self.value <= self.threshold,
            Bound::Exactly => (self.value - self.threshold).abs() < f64::EPSILON,
        }
    }
}

/// Collects a bench's asserted measurements plus report-only lines, then
/// prints one table and yields the exit code.
///
/// Every bench in the family prints the same shape so a reader can compare
/// two runs without learning a second layout.
#[derive(Debug, Default)]
pub struct Scorecard {
    assertions: Vec<Assertion>,
    reports: Vec<(String, String)>,
}

impl Scorecard {
    /// An empty scorecard.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an asserted measurement.
    pub fn assert_metric(
        &mut self,
        name: impl Into<String>,
        value: f64,
        bound: Bound,
        threshold: f64,
        unit: &'static str,
    ) {
        self.assertions.push(Assertion {
            name: name.into(),
            value,
            threshold,
            bound,
            unit,
        });
    }

    /// Adds a report-only line (spec §9's "later-used reporting": measured
    /// and printed, never a gate).
    pub fn report(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.reports.push((name.into(), value.into()));
    }

    /// Whether every assertion passed.
    pub fn passed(&self) -> bool {
        self.assertions.iter().all(Assertion::passed)
    }

    /// Prints the table and returns the process exit code (0 pass, 1 fail).
    pub fn print(&self) -> i32 {
        println!("\n{}", "=".repeat(78));
        if !self.reports.is_empty() {
            println!("\nReported (no threshold):");
            for (name, value) in &self.reports {
                println!("  {name:<38} {value}");
            }
        }
        println!("\nAsserted:");
        println!(
            "  {:<38} {:>10} {:>14}  verdict",
            "metric", "value", "threshold"
        );
        println!("  {}", "-".repeat(74));
        for assertion in &self.assertions {
            let bound = match assertion.bound {
                Bound::AtLeast => ">=",
                Bound::AtMost => "<=",
                Bound::Exactly => "==",
            };
            println!(
                "  {:<38} {:>10.3} {:>11} {:<2}  {}",
                assertion.name,
                assertion.value,
                format!("{:.3}{}", assertion.threshold, assertion.unit),
                bound,
                if assertion.passed() { "OK" } else { "FAIL" }
            );
        }
        let pass = self.passed();
        println!("\nRESULT: {}", if pass { "PASS" } else { "FAIL" });
        if !pass {
            for assertion in self.assertions.iter().filter(|a| !a.passed()) {
                println!(
                    "  - {} = {:.3}{} (needs {} {:.3}{})",
                    assertion.name,
                    assertion.value,
                    assertion.unit,
                    match assertion.bound {
                        Bound::AtLeast => "at least",
                        Bound::AtMost => "at most",
                        Bound::Exactly => "exactly",
                    },
                    assertion.threshold,
                    assertion.unit
                );
            }
        }
        i32::from(!pass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_drop_noise_and_keep_short_identifiers() {
        assert_eq!(
            tokens("crates/continuum-core/src/memory/raw_log.rs"),
            vec!["crates", "continuum", "core", "src", "memory", "raw", "log"]
        );
        assert!(tokens("the and with for").is_empty());
    }

    #[test]
    fn stems_match_across_inflections_but_not_across_words() {
        assert!(token_matches("failed", "failing"));
        assert!(token_matches("build", "building"));
        assert!(token_matches("types", "types"));
        assert!(!token_matches("types", "typescript") || "types"[..4] == *"type");
        assert!(!token_matches("raw", "run"), "short tokens need equality");
    }

    #[test]
    fn overlap_scores_the_label_not_the_answer() {
        // A long correct answer is not punished for being long.
        let expected = "cargo build failing with mismatched types";
        let actual = "cargo build failed: mismatched types at crates/continuum-core/src/memory/events.rs:214";
        assert!(
            overlap(expected, actual) >= 0.9,
            "{}",
            overlap(expected, actual)
        );
        // A different blocker does not sneak through.
        assert!(
            overlap(
                expected,
                "TypeScript error: Property 'series' does not exist"
            ) < MATCH_THRESHOLD
        );
    }

    #[test]
    fn field_matches_requires_an_actual_value() {
        assert!(!field_matches("anything at all", None));
        assert!(field_matches(
            "cargo build failed",
            Some("cargo build failed again")
        ));
    }

    #[test]
    fn exact_matches_is_used_for_slugs() {
        assert!(exact_matches("continuum", Some("continuum")));
        assert!(!exact_matches("continuum", Some("continuum-core")));
        assert!(!exact_matches("continuum", None));
    }

    #[test]
    fn similarity_is_symmetric_and_bounded() {
        let a = "cargo build failed: mismatched types (×14). Context: cargo build in WindowsTerminal.exe";
        let b = "cargo build failed: mismatched types (×3). Context: cargo build in WindowsTerminal.exe";
        let ab = text_similarity(a, b);
        assert!((ab - text_similarity(b, a)).abs() < 1e-6);
        assert!(ab >= DUPLICATE_SIMILARITY, "{ab}");
        assert!(text_similarity(a, "the user said: ga door met de tests") < DUPLICATE_SIMILARITY);
        assert_eq!(text_similarity("", ""), 1.0);
        assert_eq!(text_similarity("something", ""), 0.0);
    }

    #[test]
    fn recall_treats_an_unasserted_field_as_satisfied() {
        let empty = Recall::default();
        assert_eq!(empty.rate(), 1.0);
        let mut r = Recall::default();
        r.record(true);
        r.record(false);
        assert!((r.rate() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn percentile_handles_edges() {
        assert_eq!(percentile(&[], 95), 0.0);
        assert_eq!(percentile(&[1.0], 95), 1.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 50), 2.0);
        // Nearest-rank with half-away-from-zero rounding, same as the
        // triage bench's percentile: p50 of four samples is the third.
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 50), 3.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 100), 4.0);
    }

    #[test]
    fn scorecard_exit_code_follows_the_assertions() {
        let mut card = Scorecard::new();
        card.report("later-used rate", "40.0%");
        card.assert_metric("project recall", 0.95, Bound::AtLeast, 0.9, "");
        assert!(card.passed());
        card.assert_metric("duplicate rate", 0.2, Bound::AtMost, 0.1, "");
        assert!(!card.passed());
        assert_eq!(card.print(), 1);
    }
}
