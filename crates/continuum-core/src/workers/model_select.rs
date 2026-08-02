//! # Model selection heuristic
//!
//! Chooses a concrete model id for a worker. Explicit orchestrator choices
//! always win; Auto mode falls back to a keyword-based guess.
//!
//! The heuristic is deliberately simple — no ML, no LLM call — so it stays
//! deterministic, cheap, and easy to debug from a log line. The orchestrator
//! always has the last word by passing `WorkerModelTier::Explicit` or
//! `Power`/`Budget`.

use crate::config::WorkersConfig;

use super::types::WorkerModelTier;

/// The model id we should hand to `claude --model`, plus a one-line reason
/// that goes into the worker's snapshot for operator transparency.
#[derive(Debug, Clone)]
pub struct ModelChoice {
    pub model: String,
    pub reason: String,
}

/// Keywords that strongly suggest a Sonnet-sized job (mechanical, small scope).
const BUDGET_KEYWORDS: &[&str] = &[
    "format",
    "rename",
    "move files",
    "move file",
    "summarize",
    "summarise",
    "draft",
    "boilerplate",
    "scaffold",
    "tidy",
    "lint",
    "list files",
    "generate docstring",
    "generate comments",
    "rewrite wording",
    "email",
    "todo",
];

/// Keywords that signal the task needs Opus: planning, reasoning, deep debug.
const POWER_KEYWORDS: &[&str] = &[
    "refactor",
    "architect",
    "architecture",
    "design",
    "debug complex",
    "root cause",
    "propose",
    "trade-off",
    "tradeoff",
    "investigate",
    "migration",
    "redesign",
    "audit",
    "security review",
    "algorithmic",
    "performance tuning",
];

/// Returns the model id Continuum should pass to claude CLI for this worker.
pub fn choose_model(cfg: &WorkersConfig, tier: &WorkerModelTier, task: &str) -> ModelChoice {
    // Global mode overrides. `"budget"` and `"power"` are deliberately
    // hard modes — user gave up the nuance in the dashboard.
    let mode = cfg.mode.to_ascii_lowercase();
    if mode == "budget" {
        return ModelChoice {
            model: cfg.budget_model.clone(),
            reason: "workers.mode=budget forces Sonnet for every worker".into(),
        };
    }
    if mode == "power" {
        return ModelChoice {
            model: cfg.power_model.clone(),
            reason: "workers.mode=power forces Opus for every worker".into(),
        };
    }

    // Per-spawn explicit overrides.
    match tier {
        WorkerModelTier::Budget => {
            return ModelChoice {
                model: cfg.budget_model.clone(),
                reason: "orchestrator requested tier=budget".into(),
            }
        }
        WorkerModelTier::Power => {
            return ModelChoice {
                model: cfg.power_model.clone(),
                reason: "orchestrator requested tier=power".into(),
            }
        }
        WorkerModelTier::Explicit(id) => {
            return ModelChoice {
                model: id.clone(),
                reason: format!("orchestrator requested explicit model {id}"),
            }
        }
        WorkerModelTier::Auto => {}
    }

    // Auto-mode heuristic: count keyword hits. Power wins on ties (complex
    // work is more expensive to mis-downgrade than to mis-upgrade).
    let lower = task.to_ascii_lowercase();
    let budget_hits: Vec<&&str> = BUDGET_KEYWORDS
        .iter()
        .filter(|k| lower.contains(*k))
        .collect();
    let power_hits: Vec<&&str> = POWER_KEYWORDS
        .iter()
        .filter(|k| lower.contains(*k))
        .collect();

    if power_hits.is_empty() && !budget_hits.is_empty() {
        return ModelChoice {
            model: cfg.budget_model.clone(),
            reason: format!("auto: budget — keywords {:?}", budget_hits),
        };
    }
    if !power_hits.is_empty() && budget_hits.is_empty() {
        return ModelChoice {
            model: cfg.power_model.clone(),
            reason: format!("auto: power — keywords {:?}", power_hits),
        };
    }
    if power_hits.is_empty() && budget_hits.is_empty() {
        // No signal either way — default to budget so the typical worker is
        // cheap. Opus-worthy tasks tend to contain at least one power keyword.
        return ModelChoice {
            model: cfg.budget_model.clone(),
            reason: "auto: budget — no strong keyword signal".into(),
        };
    }
    // Both kinds matched. Lean power because the task probably has scope.
    ModelChoice {
        model: cfg.power_model.clone(),
        reason: format!(
            "auto: power — mixed signal, power keywords {:?} took precedence over {:?}",
            power_hits, budget_hits
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> WorkersConfig {
        WorkersConfig::default()
    }

    #[test]
    fn mode_budget_forces_sonnet() {
        let mut c = cfg();
        c.mode = "budget".into();
        let choice = choose_model(&c, &WorkerModelTier::Power, "refactor everything");
        assert_eq!(choice.model, c.budget_model);
    }

    #[test]
    fn mode_power_forces_opus() {
        let mut c = cfg();
        c.mode = "power".into();
        let choice = choose_model(&c, &WorkerModelTier::Budget, "rename one file");
        assert_eq!(choice.model, c.power_model);
    }

    #[test]
    fn explicit_tier_wins_in_auto_mode() {
        let c = cfg();
        let choice = choose_model(&c, &WorkerModelTier::Power, "rename one file");
        assert_eq!(choice.model, c.power_model);
    }

    #[test]
    fn explicit_model_id_passes_through() {
        let c = cfg();
        let choice = choose_model(
            &c,
            &WorkerModelTier::Explicit("claude-opus-4-6".into()),
            "anything",
        );
        assert_eq!(choice.model, "claude-opus-4-6");
    }

    #[test]
    fn auto_picks_power_for_refactor() {
        let c = cfg();
        let choice = choose_model(
            &c,
            &WorkerModelTier::Auto,
            "Please refactor the auth middleware and investigate the root cause",
        );
        assert_eq!(choice.model, c.power_model);
    }

    #[test]
    fn auto_picks_budget_for_rename() {
        let c = cfg();
        let choice = choose_model(
            &c,
            &WorkerModelTier::Auto,
            "rename FooBar to BazQux and move files into src/util",
        );
        assert_eq!(choice.model, c.budget_model);
    }

    #[test]
    fn auto_defaults_to_budget_with_no_signal() {
        let c = cfg();
        let choice = choose_model(&c, &WorkerModelTier::Auto, "print hello world");
        assert_eq!(choice.model, c.budget_model);
        assert!(choice.reason.contains("no strong keyword"));
    }

    #[test]
    fn auto_breaks_tie_toward_power() {
        let c = cfg();
        let choice = choose_model(
            &c,
            &WorkerModelTier::Auto,
            "refactor the boilerplate email template",
        );
        assert_eq!(choice.model, c.power_model);
    }
}
