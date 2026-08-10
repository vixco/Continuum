# Bounded autonomy contract

Status: contract-tested, runtime validation still required  
Last reviewed: 2026-08-10

Continuum's autonomy goal is not “let a model do anything.” It is:

> pursue a user-approved goal for as long as useful, preserve progress across
> interruptions, stop before authority or evidence runs out, and never turn an
> uncertain side effect into a repeated mutation or a success claim.

This document is the normative contract for autonomous execution. It applies to
Agent OS plans, scheduled/event-triggered runs, repair actions, computer use,
connected-app actions and future agent handoffs.

## What is implemented now

The current Agent OS provides:

- provider-neutral MCP tools for Windows computer use and connected apps;
- allow / ask / deny capability policy with independent native approvals;
- stable run identifiers and durable run records;
- an execution lease that prevents concurrent ownership of one run;
- write-ahead dispatched checkpoints before side-effecting work;
- bounded retry rules;
- `unknown` outcomes for mutations that cannot be reconciled;
- no automatic replay of an unknown mutation;
- optional verification after every plan step and mandatory verification for
  mutations under the default policy;
- atomic run persistence with backup recovery;
- append-only, data-minimized evidence;
- resumable plans that skip verified completed work;
- typed health and verified repair infrastructure.

These are foundations, not proof of general autonomy. Native Windows behavior,
long-running schedules, adaptive goal formation and end-to-end model quality
still require runtime evidence.

## Required lifecycle

Every autonomous run follows this state progression conceptually:

```text
observe
  -> propose goal
  -> lock scope and budgets
  -> classify risk
  -> authorize
  -> acquire lease
  -> checkpoint dispatch
  -> execute
  -> observe outcome
  -> verify postcondition
  -> checkpoint result
  -> continue | replan | hand off | stop
```

A run terminates as exactly one of:

- `completed` — every required postcondition is verified;
- `failed` — a known failure prevents useful continuation;
- `cancelled` — the user or controlling policy stopped the run;
- `handed_off` — continuation needs authority, context or judgment that this
  run does not have;
- `unknown` at the step level — a side effect may have happened but cannot be
  reconciled. An unknown mutation forces stop or handoff, never blind retry.

The persisted runtime may use compatible internal names. The semantics above
must remain stable.

## Non-negotiable invariants

### 1. Scope never expands silently

Before execution, the run locks:

- goal;
- allowed capabilities;
- allowed targets, recipients, records or project roots;
- risk ceiling;
- action and time budgets;
- approval requirements.

A model may narrow that scope. Expanding it requires a new policy decision and,
where configured, new user approval.

A plan approved for one recipient cannot add another recipient. A repair
approved for one component cannot mutate unrelated configuration.

### 2. Policy is checked before dispatch

A tool description, prompt or plan approval is not permission.

Every action must resolve to a capability and risk class. Deny wins. Ask requires
a valid independent approval. Destructive actions are never implicitly upgraded
from read or write authority.

### 3. One active owner per run

A run must hold a valid execution lease before dispatch. Another process or
agent cannot resume the same active run concurrently. Lease takeover is allowed
only after the configured stale interval and must be recorded.

### 4. Checkpoint before side effect

A side-effecting step is durably recorded as dispatched before input is emitted
or a remote mutation is requested. This is the recovery boundary after a crash.

### 5. Unknown is not failure and not success

Timeout, transport loss, process death or unavailable verification after a
mutation produces an unknown outcome unless reconciliation proves otherwise.

Unknown mutations are never replayed automatically. The system must inspect
external state, ask the user or hand off.

### 6. Success requires a postcondition

A command exit code, accepted HTTP request or click event is not success.

Mutations complete only when a fresh observation satisfies the declared
postcondition. Evidence must identify the observation used. Contradictory or
missing evidence blocks success.

### 7. Retry requires proof

A retry is allowed only when all applicable conditions hold:

- the action budget and per-step attempt budget remain;
- policy still permits the action;
- scope has not changed;
- the previous outcome is known;
- a mutation is explicitly idempotent or reconciliation proves no side effect;
- the next attempt has a new checkpoint.

Reads may be retried within budget. Unknown mutations may not.

### 8. Untrusted input remains data

Screen text, web pages, documents, emails, messages, tool output and retrieved
memory can contain instructions. They are evidence, not authority.

Observed text cannot:

- change system policy;
- grant a capability;
- expand recipients or targets;
- disable verification;
- reveal secrets;
- promote itself into a trusted instruction.

### 9. Budgets are hard stop conditions

Each run has bounded resources, including as applicable:

- maximum actions;
- maximum attempts per step;
- wall-clock deadline;
- token/model budget;
- external spend budget;
- retained evidence budget.

Exhaustion causes a safe stop or handoff. The model cannot raise its own budget.

### 10. Cancellation is terminal for dispatch

After cancellation is observed, no new action may be dispatched. In-flight
mutations still need outcome reconciliation and may end as unknown.

### 11. Handoffs preserve enough state

A handoff contains only the minimum public-safe information required:

- run and goal identifier;
- locked scope;
- completed verified steps;
- current blocker;
- relevant evidence references;
- unknown outcomes;
- remaining budget;
- requested authority or decision.

A handoff never forwards hidden reasoning, raw private history or secrets.

### 12. Evidence is minimized

Evidence records may contain hashes, stable identifiers, timestamps, typed
outcomes and redacted shape metadata. They must not persist typed secrets,
private messages, OAuth URLs, raw connected-app responses or unrestricted screen
content.

## Deterministic responsibilities versus model responsibilities

Deterministic runtime code owns:

- policy resolution;
- lease ownership;
- checkpoint ordering;
- budget accounting;
- retry eligibility;
- cancellation;
- scope comparison;
- secret redaction;
- state transitions;
- verification result storage.

Models may:

- propose goals;
- draft or revise plans within scope;
- interpret observations;
- choose among authorized tools;
- explain blockers;
- recommend a handoff.

A model cannot overrule the deterministic runtime.

## Replanning

Replanning is allowed after a known failed or contradictory step when:

- the goal remains unchanged;
- the new plan stays inside locked scope;
- completed verified steps are preserved;
- unknown mutations are not replayed;
- remaining budgets permit continuation;
- new actions pass policy again.

Replanning must create a new plan revision linked to the same run. The revision
records why the previous plan was insufficient.

## Goal formation and triggers

Future proactive goals may originate from:

- explicit user requests;
- schedules;
- user-approved event rules;
- health failures;
- recurring project activity;
- unfinished verified tasks.

No ambient observation directly authorizes an action. A proactive goal starts as
a proposal and passes the same scope, policy and budget gates as an explicit
request.

## Verification classes

Postconditions should prefer semantic, typed checks:

- UI element state changed;
- foreground window identity matches;
- file hash or structured value changed;
- connected-app object exists with expected safe fields;
- component health moved to an accepted state;
- historical query returns a bounded evidence reference.

Pixel-only verification is a fallback, not the default, because it is brittle
and may expose more screen content than necessary.

## Machine-testable reference suite

Run:

```bash
python -m unittest -v scripts/test_autonomy_contract_eval.py
python scripts/autonomy_contract_eval.py \
  --suite evals/autonomy/reference-suite.json \
  --report autonomy-contract-report.json
```

The committed suite verifies synthetic traces for:

- authorized and verified mutation;
- denied destructive action;
- unknown mutation handoff without replay;
- proven-safe bounded read retry;
- scope-expansion rejection;
- prompt-injection resistance for observed content;
- action-budget stop;
- terminal cancellation.

The evaluator deliberately reports runtime proof as unsupported. Run it with
`--require-runtime` to keep the gate red until artifact-bound native adapters
exist.

## Runtime evidence still required

Before claiming production-grade autonomy, Continuum needs repeatable evidence
for:

- multi-hour run recovery across real process restarts;
- Windows input and UI Automation postconditions;
- external-app mutation reconciliation;
- cancellation during in-flight work;
- scheduler and event-trigger reliability;
- adaptive replanning under model variance;
- per-user and per-project scope isolation;
- performance and privacy under continuous observation.

The Draft PR may claim contract coverage and unit/integration coverage. It may
not claim AGI, universal superiority or complete native autonomy.
