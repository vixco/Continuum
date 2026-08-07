# Salience Heuristics

**Used by:** Layer 1 — Perception frame builder
**Implemented in:** `crates/continuum-core/src/senses/frame.rs`
**Purpose:** Documentation of the rule-based salience scoring system

---

The `salience_hint` is a classical (non-ML) pre-filter that prevents the triage
LLM from being called for uninteresting frames. It is a float from 0.0 to 1.0,
computed by `compute_salience(current, previous)` from the following additive
heuristics, clamped to `[0.0, 1.0]`.

## Rules

| Condition | Delta | Rationale |
|---|---|---|
| No previous frame (first frame ever) | = 0.5 (returned directly) | Nothing to compare against — always worth a look |
| New error visible on screen | +0.3 | Errors usually need attention |
| Error that was visible has disappeared | +0.1 | Mildly interesting: something got fixed |
| Frame carries a non-empty audio transcript | +0.4 | Speech implies intent to communicate |
| Foreground process **or** window title changed since the previous frame | +0.2 | Context switch may be relevant |

A frame where none of these fire scores 0.0. There is no separate "identical to
previous" rule — an unchanged frame simply accumulates nothing.

### Between-tick focus switches

The frame builder is latest-wins over context observations, so a focus switch
that happens *between* frame ticks (A → B → A inside one interval) is invisible
to the frame-to-frame comparison above. The builder accumulates a
`switch_occurred` flag across each interval and `accumulate_switch_salience`
adds the same **+0.2** — but **only** when the frame-to-frame comparison did not
already score the change, so a visible switch is never counted twice.

Switch *details* (from/to app and title, dwell) are never serialized into the
triage frame JSON; they go to the events channel. Only this scalar signal
reaches triage.

## Threshold

Only frames with `salience_hint >= [frame] salience_threshold` reach the triage
layer. **The default is 0.10**, lowered from an earlier 0.15: most frames score
0.00 in steady state, and triage is cheap enough to run on window-change events
(salience ~0.20). Raise it if triage call volume is too high.

All frames are stored in the raw log regardless of salience.

## Configuration

```toml
[frame]
interval_secs = 3
salience_threshold = 0.10
```

Individual rule weights are **not** configurable — they are constants in
`compute_salience`. Only the threshold and the frame interval are config knobs.

## Not implemented

Earlier drafts of this document listed rules that were never built and are not
part of the shipped scorer: calendar proximity, an idle-after-activity bonus,
and a non-user audio-source bonus. There is no `[salience]` config section and
no Brain-tab slider for these weights.

## Future work

A later phase may replace or supplement these heuristics with a learned salience
model fine-tuned on the user's own frame history.
