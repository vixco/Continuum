# Salience Heuristics

**Used by:** Layer 1 — Perception frame builder
**Purpose:** Documentation of the rule-based salience scoring system

---

The `salience_hint` is a classical (non-ML) pre-filter that prevents the triage
LLM from being called for uninteresting frames. It is a float from 0.0 to 1.0,
computed from the following additive heuristics:

## Rules

| Condition | Delta | Rationale |
|---|---|---|
| Frame identical to previous | = 0.0 (skip) | Nothing changed, no point triaging |
| New error visible on screen | +0.3 | Errors usually need attention |
| User spoke within last 5 seconds | +0.4 | Speech implies intent to communicate |
| New window focused | +0.2 | Context switch may be relevant |
| Calendar event within 15 minutes | +0.3 | Upcoming commitments need reminders |
| Idle time > 5 minutes after activity | +0.1 | User may have stepped away |
| New audio from non-user source | +0.1 | Notification or call sound |

## Threshold

Only frames with `salience_hint >= 0.15` reach the triage layer. All frames are
stored in the raw log regardless of salience.

## Configuration

The threshold and individual rule weights are configurable via
`~/.kairo/config.toml` under the `[salience]` section. The dashboard exposes
these as sliders in the Brain tab.

## Future work

Phase 2+ may replace or supplement these heuristics with a learned salience
model fine-tuned on the user's own frame history.
