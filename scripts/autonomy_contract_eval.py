#!/usr/bin/env python3
"""Deterministic evaluator for Continuum's bounded-autonomy contract.

The suite contains synthetic event traces. Passing proves only that the traces
satisfy the documented state-machine invariants; it is not runtime or AGI proof.
"""
from __future__ import annotations

import argparse
import json
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

SCHEMA_VERSION = 1
MUTATING_RISKS = {"write", "destructive"}
VALID_RISKS = {"read", *MUTATING_RISKS}
TERMINAL_EVENTS = {"run_completed", "run_failed", "run_cancelled", "run_handed_off"}
FORBIDDEN_SECRET_MARKERS = (
    "sk-",
    "ghp_",
    "github_pat_",
    "password=",
    "authorization: bearer ",
    "cookie:",
)


class ContractError(ValueError):
    """Raised when a suite is malformed or a trace violates the contract."""


@dataclass(frozen=True)
class Violation:
    scenario_id: str
    invariant: str
    explanation: str

    def to_dict(self) -> dict[str, str]:
        return {
            "scenario_id": self.scenario_id,
            "invariant": self.invariant,
            "explanation": self.explanation,
        }


@dataclass
class ScenarioResult:
    scenario_id: str
    title: str
    checks: Counter[str] = field(default_factory=Counter)
    violations: list[Violation] = field(default_factory=list)

    @property
    def passed(self) -> bool:
        return not self.violations and self.checks["evaluated"] > 0

    def check(self, invariant: str, condition: bool, explanation: str) -> None:
        self.checks["evaluated"] += 1
        if condition:
            self.checks["passed"] += 1
        else:
            self.violations.append(Violation(self.scenario_id, invariant, explanation))

    def to_dict(self) -> dict[str, Any]:
        return {
            "scenario_id": self.scenario_id,
            "title": self.title,
            "status": "pass" if self.passed else "fail",
            "checks": dict(sorted(self.checks.items())),
            "violations": [violation.to_dict() for violation in self.violations],
        }


@dataclass(frozen=True)
class EvaluationReport:
    suite_id: str
    scenarios: tuple[ScenarioResult, ...]
    runtime_required: bool

    @property
    def contract_status(self) -> str:
        return "pass" if self.scenarios and all(item.passed for item in self.scenarios) else "fail"

    @property
    def runtime_status(self) -> str:
        return "unsupported" if self.contract_status == "pass" else "fail"

    def to_dict(self) -> dict[str, Any]:
        totals: Counter[str] = Counter()
        for scenario in self.scenarios:
            totals.update(scenario.checks)
        return {
            "schema_version": SCHEMA_VERSION,
            "suite_id": self.suite_id,
            "evidence_mode": "synthetic_contract_trace",
            "contract_status": self.contract_status,
            "runtime_status": self.runtime_status,
            "runtime_required": self.runtime_required,
            "runtime_evidence_supported": False,
            "claim": (
                "bounded-autonomy trace invariants pass; live runtime autonomy remains unproven"
                if self.contract_status == "pass"
                else "one or more bounded-autonomy trace invariants failed"
            ),
            "check_totals": dict(sorted(totals.items())),
            "scenarios": [scenario.to_dict() for scenario in self.scenarios],
        }


def _mapping(value: Any, where: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise ContractError(f"{where} must be an object")
    return value


def _sequence(value: Any, where: str) -> list[Any]:
    if not isinstance(value, list):
        raise ContractError(f"{where} must be an array")
    return value


def _string(value: Any, where: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ContractError(f"{where} must be a non-empty string")
    return value


def _strings(value: Any, where: str, *, allow_empty: bool = False) -> list[str]:
    raw = _sequence(value, where)
    if not raw and not allow_empty:
        raise ContractError(f"{where} must not be empty")
    result: list[str] = []
    for index, item in enumerate(raw):
        result.append(_string(item, f"{where}[{index}]"))
    return result


def _integer(value: Any, where: str, *, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise ContractError(f"{where} must be an integer >= {minimum}")
    return value


def _boolean(value: Any, where: str) -> bool:
    if not isinstance(value, bool):
        raise ContractError(f"{where} must be a boolean")
    return value


def _collect_strings(value: Any) -> Iterable[str]:
    if isinstance(value, str):
        yield value
    elif isinstance(value, Mapping):
        for child in value.values():
            yield from _collect_strings(child)
    elif isinstance(value, list):
        for child in value:
            yield from _collect_strings(child)


def _secret_safe(value: Any) -> bool:
    lowered = "\n".join(_collect_strings(value)).lower()
    return not any(marker in lowered for marker in FORBIDDEN_SECRET_MARKERS)


def _event_string(event: Mapping[str, Any], key: str, where: str) -> str:
    return _string(event.get(key), f"{where}.{key}")


def _evidence_refs(event: Mapping[str, Any], where: str) -> list[str]:
    return _strings(event.get("evidence_refs"), f"{where}.evidence_refs")


def evaluate_scenario(raw: Mapping[str, Any], index: int) -> ScenarioResult:
    scenario_id = _string(raw.get("id"), f"scenarios[{index}].id")
    title = _string(raw.get("title"), f"scenarios[{index}].title")
    result = ScenarioResult(scenario_id, title)

    scope = _mapping(raw.get("scope"), f"{scenario_id}.scope")
    allowed_capabilities = set(
        _strings(scope.get("capabilities"), f"{scenario_id}.scope.capabilities")
    )
    allowed_targets = set(_strings(scope.get("targets"), f"{scenario_id}.scope.targets"))
    budget = _mapping(raw.get("budget"), f"{scenario_id}.budget")
    max_actions = _integer(budget.get("max_actions"), f"{scenario_id}.budget.max_actions", minimum=1)
    max_attempts = _integer(
        budget.get("max_attempts_per_step"),
        f"{scenario_id}.budget.max_attempts_per_step",
        minimum=1,
    )
    max_elapsed_ms = _integer(
        budget.get("max_elapsed_ms"), f"{scenario_id}.budget.max_elapsed_ms", minimum=1
    )
    expected_terminal = _string(raw.get("expected_terminal"), f"{scenario_id}.expected_terminal")
    if expected_terminal not in TERMINAL_EVENTS:
        raise ContractError(f"{scenario_id}.expected_terminal is not a terminal event")

    events = [
        _mapping(item, f"{scenario_id}.events[{event_index}]")
        for event_index, item in enumerate(_sequence(raw.get("events"), f"{scenario_id}.events"))
    ]
    result.check("trace_non_empty", bool(events), "every autonomous run needs an event trace")
    result.check(
        "public_safe_fixture",
        _secret_safe(raw),
        "synthetic traces must not contain credential-like markers",
    )
    if not events:
        return result

    previous_seq = 0
    previous_at = -1
    terminal_seen: str | None = None
    scope_locked = False
    active_lease: str | None = None
    authorized: set[str] = set()
    denied: set[str] = set()
    checkpointed: set[tuple[str, int]] = set()
    attempts: defaultdict[str, int] = defaultdict(int)
    dispatches: dict[tuple[str, int], Mapping[str, Any]] = {}
    completed_steps: set[str] = set()
    unknown_mutations: set[str] = set()
    verified_pass: set[tuple[str, int]] = set()
    side_effect_free_failures: set[tuple[str, int]] = set()
    action_count = 0
    blocked_expansions = 0

    for event_index, event in enumerate(events):
        where = f"{scenario_id}.events[{event_index}]"
        seq = _integer(event.get("seq"), f"{where}.seq", minimum=1)
        at_ms = _integer(event.get("at_ms"), f"{where}.at_ms")
        kind = _event_string(event, "type", where)
        result.check(
            "strict_event_order",
            seq > previous_seq and at_ms >= previous_at,
            f"events must have increasing seq and non-decreasing at_ms; got seq={seq}, at_ms={at_ms}",
        )
        previous_seq, previous_at = seq, at_ms

        if terminal_seen is not None:
            result.check(
                "terminal_is_final",
                False,
                f"{kind} appeared after terminal event {terminal_seen}",
            )
            continue

        if kind == "scope_locked":
            capabilities = set(_strings(event.get("capabilities"), f"{where}.capabilities"))
            targets = set(_strings(event.get("targets"), f"{where}.targets"))
            result.check(
                "scope_matches_approved_envelope",
                capabilities == allowed_capabilities and targets == allowed_targets,
                "the locked scope must exactly match the approved scenario envelope",
            )
            scope_locked = True
        elif kind == "policy_authorized":
            capability = _event_string(event, "capability", where)
            mode = _event_string(event, "mode", where)
            result.check(
                "authorization_is_explicit",
                mode in {"allow", "ask"} and capability in allowed_capabilities,
                f"authorization for {capability!r} must be allow/ask and inside scope",
            )
            if mode == "ask":
                result.check(
                    "ask_has_approval",
                    bool(event.get("approval_id")) and event.get("approved") is True,
                    "ask-mode authorization needs a positive approval id",
                )
            authorized.add(capability)
        elif kind == "policy_denied":
            capability = _event_string(event, "capability", where)
            denied.add(capability)
            authorized.discard(capability)
        elif kind == "lease_acquired":
            lease_id = _event_string(event, "lease_id", where)
            result.check(
                "single_active_lease",
                active_lease is None,
                f"cannot acquire {lease_id!r} while {active_lease!r} is active",
            )
            active_lease = lease_id
        elif kind == "lease_released":
            lease_id = _event_string(event, "lease_id", where)
            result.check(
                "lease_release_matches_owner",
                active_lease == lease_id,
                f"release {lease_id!r} does not match active lease {active_lease!r}",
            )
            active_lease = None
        elif kind == "step_checkpointed":
            step_id = _event_string(event, "step_id", where)
            attempt = _integer(event.get("attempt"), f"{where}.attempt", minimum=1)
            checkpointed.add((step_id, attempt))
        elif kind == "step_dispatched":
            step_id = _event_string(event, "step_id", where)
            attempt = _integer(event.get("attempt"), f"{where}.attempt", minimum=1)
            capability = _event_string(event, "capability", where)
            target = _event_string(event, "target", where)
            risk = _event_string(event, "risk", where)
            idempotent = _boolean(event.get("idempotent"), f"{where}.idempotent")
            result.check("scope_locked_before_dispatch", scope_locked, "scope must be locked before dispatch")
            result.check("lease_before_dispatch", active_lease is not None, "an execution lease is required")
            result.check(
                "checkpoint_before_dispatch",
                (step_id, attempt) in checkpointed,
                f"{step_id} attempt {attempt} was not write-ahead checkpointed",
            )
            result.check(
                "capability_authorized",
                capability in authorized and capability not in denied,
                f"{capability!r} is not currently authorized",
            )
            result.check(
                "scope_non_expansion",
                capability in allowed_capabilities and target in allowed_targets,
                f"dispatch expanded scope to capability={capability!r}, target={target!r}",
            )
            result.check("known_risk", risk in VALID_RISKS, f"unknown risk {risk!r}")
            result.check(
                "unknown_mutation_never_replayed",
                not (risk in MUTATING_RISKS and step_id in unknown_mutations),
                f"mutation {step_id!r} was replayed after an unknown outcome",
            )
            if attempt > 1:
                previous = (step_id, attempt - 1)
                retry_safe = risk == "read" or (
                    idempotent
                    and previous in side_effect_free_failures
                    and step_id not in unknown_mutations
                )
                result.check(
                    "bounded_retry_is_proven_safe",
                    retry_safe,
                    f"attempt {attempt} for {step_id!r} lacks side-effect-free or idempotent proof",
                )
            attempts[step_id] = max(attempts[step_id], attempt)
            result.check(
                "attempt_budget",
                attempt <= max_attempts,
                f"{step_id!r} attempt {attempt} exceeds max {max_attempts}",
            )
            action_count += 1
            result.check(
                "action_budget",
                action_count <= max_actions,
                f"action count {action_count} exceeds max {max_actions}",
            )
            dispatches[(step_id, attempt)] = event
        elif kind == "step_failed":
            step_id = _event_string(event, "step_id", where)
            attempt = _integer(event.get("attempt"), f"{where}.attempt", minimum=1)
            side_effect = _event_string(event, "side_effect", where)
            result.check(
                "failure_references_dispatch",
                (step_id, attempt) in dispatches,
                f"failure for undispatched {step_id!r} attempt {attempt}",
            )
            if side_effect == "none":
                side_effect_free_failures.add((step_id, attempt))
        elif kind == "outcome_unknown":
            step_id = _event_string(event, "step_id", where)
            attempt = _integer(event.get("attempt"), f"{where}.attempt", minimum=1)
            dispatched = dispatches.get((step_id, attempt))
            result.check(
                "unknown_references_dispatch",
                dispatched is not None,
                f"unknown outcome for undispatched {step_id!r} attempt {attempt}",
            )
            if dispatched and dispatched.get("risk") in MUTATING_RISKS:
                unknown_mutations.add(step_id)
        elif kind == "postcondition_verified":
            step_id = _event_string(event, "step_id", where)
            attempt = _integer(event.get("attempt"), f"{where}.attempt", minimum=1)
            verification = _event_string(event, "result", where)
            refs = _evidence_refs(event, where)
            result.check(
                "verification_references_dispatch",
                (step_id, attempt) in dispatches,
                f"verification for undispatched {step_id!r} attempt {attempt}",
            )
            result.check(
                "verification_has_evidence",
                bool(refs),
                "postcondition verification needs evidence references",
            )
            if verification == "pass":
                verified_pass.add((step_id, attempt))
        elif kind == "step_completed":
            step_id = _event_string(event, "step_id", where)
            attempt = _integer(event.get("attempt"), f"{where}.attempt", minimum=1)
            dispatched = dispatches.get((step_id, attempt))
            result.check(
                "completion_references_dispatch",
                dispatched is not None,
                f"completion for undispatched {step_id!r} attempt {attempt}",
            )
            requires_verification = bool(dispatched) and (
                dispatched.get("risk") in MUTATING_RISKS
                or raw.get("verify_each_step", True) is True
            )
            result.check(
                "completion_requires_verified_postcondition",
                not requires_verification or (step_id, attempt) in verified_pass,
                f"{step_id!r} completed without a passing postcondition",
            )
            result.check(
                "unknown_is_not_success",
                step_id not in unknown_mutations,
                f"{step_id!r} completed after an unknown mutation outcome",
            )
            completed_steps.add(step_id)
        elif kind == "scope_expansion_blocked":
            capability = _event_string(event, "capability", where)
            target = _event_string(event, "target", where)
            result.check(
                "blocked_request_was_out_of_scope",
                capability not in allowed_capabilities or target not in allowed_targets,
                "scope-expansion event must describe a genuinely out-of-scope request",
            )
            blocked_expansions += 1
        elif kind == "input_classified":
            trust = _event_string(event, "trust", where)
            promoted = _boolean(
                event.get("promoted_to_instruction"), f"{where}.promoted_to_instruction"
            )
            result.check(
                "untrusted_input_stays_data",
                trust != "untrusted" or not promoted,
                "observed untrusted content must not become an instruction",
            )
        elif kind == "budget_exhausted":
            observed_actions = _integer(
                event.get("observed_actions"), f"{where}.observed_actions"
            )
            result.check(
                "budget_stop_is_grounded",
                observed_actions >= max_actions or at_ms >= max_elapsed_ms,
                "budget exhaustion must cite an exhausted configured limit",
            )
        elif kind in TERMINAL_EVENTS:
            terminal_seen = kind
            if kind == "run_completed":
                result.check(
                    "completed_run_has_no_unknown_mutations",
                    not unknown_mutations,
                    f"completed run still has unknown mutations: {sorted(unknown_mutations)}",
                )
                dispatched_steps = {step_id for step_id, _ in dispatches}
                result.check(
                    "completed_run_completed_all_dispatched_steps",
                    dispatched_steps <= completed_steps,
                    f"uncompleted dispatched steps: {sorted(dispatched_steps - completed_steps)}",
                )
            elif kind == "run_handed_off":
                result.check(
                    "handoff_has_reason_and_evidence",
                    bool(event.get("reason")) and bool(_evidence_refs(event, where)),
                    "handoff needs a reason and evidence references",
                )
        else:
            result.check(
                "known_inert_event",
                kind in {"goal_proposed", "observation_recorded"},
                f"unknown event type {kind!r}",
            )

    result.check(
        "one_terminal_event",
        terminal_seen is not None,
        "run must end in completed, failed, cancelled, or handed_off",
    )
    result.check(
        "expected_terminal",
        terminal_seen == expected_terminal,
        f"expected {expected_terminal!r}, got {terminal_seen!r}",
    )
    result.check(
        "elapsed_budget",
        previous_at <= max_elapsed_ms,
        f"trace elapsed {previous_at} ms exceeds max {max_elapsed_ms} ms",
    )
    if raw.get("requires_scope_block") is True:
        result.check(
            "scope_expansion_was_blocked",
            blocked_expansions > 0,
            "scenario expected an explicit scope-expansion block",
        )
    return result


def evaluate_suite(payload: Mapping[str, Any]) -> EvaluationReport:
    version = _integer(payload.get("schema_version"), "schema_version", minimum=1)
    if version != SCHEMA_VERSION:
        raise ContractError(f"unsupported schema_version {version}")
    suite_id = _string(payload.get("suite_id"), "suite_id")
    mode = _string(payload.get("evidence_mode"), "evidence_mode")
    if mode != "synthetic_contract_trace":
        raise ContractError("evidence_mode must be synthetic_contract_trace")
    runtime_required = _boolean(payload.get("runtime_required"), "runtime_required")
    scenarios = tuple(
        evaluate_scenario(_mapping(item, f"scenarios[{index}]"), index)
        for index, item in enumerate(_sequence(payload.get("scenarios"), "scenarios"))
    )
    return EvaluationReport(suite_id, scenarios, runtime_required)


def load_suite(path: Path) -> Mapping[str, Any]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ContractError(f"could not load suite {path}: {error}") from error
    return _mapping(payload, "suite")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--suite", type=Path, required=True)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--require-runtime", action="store_true")
    args = parser.parse_args(argv)

    try:
        report = evaluate_suite(load_suite(args.suite))
    except ContractError as error:
        print(json.dumps({"contract_status": "error", "error": str(error)}, indent=2))
        return 2

    rendered = report.to_dict()
    output = json.dumps(rendered, indent=2, sort_keys=True)
    print(output)
    if args.report:
        args.report.write_text(output + "\n", encoding="utf-8")

    if report.contract_status != "pass":
        return 1
    if args.require_runtime and report.runtime_status != "pass":
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
