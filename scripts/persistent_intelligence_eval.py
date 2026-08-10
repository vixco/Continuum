#!/usr/bin/env python3
"""Deterministic cross-layer evaluation for Continuum persistent intelligence.

The evaluator consumes normalized, synthetic contract evidence. It never
relabels fixture prose as runtime proof. Every scenario uses explicit
invariants or a documented unit-bearing metric.
"""
from __future__ import annotations

import argparse
import json
import math
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping, Sequence

SCHEMA_VERSION = 1
VALID_EVIDENCE_MODES = {"contract_fixture"}
VALID_SENSITIVITY = {"cloud_allowed", "local_only", "never_observe"}
VALID_DIMENSIONS = {
    "perception_latency",
    "semantic_relevance",
    "temporal_coherence",
    "historical_retrieval",
    "confidence_calibration",
    "evidence_provenance",
    "memory_precision",
    "privacy",
    "cache_correctness",
    "failure_explanation",
    "repair_verification",
    "ui_truthfulness",
    "regression_safety",
}


class EvaluationError(ValueError):
    """Raised when a suite is malformed or cannot be evaluated safely."""


@dataclass(frozen=True)
class CheckResult:
    dimension: str
    check: str
    passed: bool
    explanation: str
    metric: Mapping[str, Any] | None = None

    def to_dict(self) -> dict[str, Any]:
        result: dict[str, Any] = {
            "dimension": self.dimension,
            "check": self.check,
            "status": "pass" if self.passed else "fail",
            "explanation": self.explanation,
        }
        if self.metric is not None:
            result["metric"] = dict(self.metric)
        return result


@dataclass
class ScenarioResult:
    scenario_id: str
    title: str
    evidence_mode: str
    checks: list[CheckResult] = field(default_factory=list)

    @property
    def passed(self) -> bool:
        return bool(self.checks) and all(check.passed for check in self.checks)

    def to_dict(self) -> dict[str, Any]:
        return {
            "scenario_id": self.scenario_id,
            "title": self.title,
            "evidence_mode": self.evidence_mode,
            "status": "pass" if self.passed else "fail",
            "dimensions": sorted({check.dimension for check in self.checks}),
            "checks": [check.to_dict() for check in self.checks],
        }


@dataclass(frozen=True)
class EvaluationReport:
    suite_id: str
    synthetic_only: bool
    scenarios: tuple[ScenarioResult, ...]
    runtime_required: bool
    regression_failures: tuple[str, ...] = ()

    @property
    def contract_status(self) -> str:
        return (
            "pass"
            if self.scenarios and all(scenario.passed for scenario in self.scenarios)
            else "fail"
        )

    @property
    def runtime_status(self) -> str:
        return "fail" if self.contract_status == "fail" else "unsupported"

    @property
    def exit_ok(self) -> bool:
        if self.contract_status != "pass" or self.regression_failures:
            return False
        # Schema v1 intentionally cannot produce runtime proof. Keep the release
        # gate red until adapter-backed artifacts use a separate validated path.
        return not self.runtime_required

    def to_dict(self) -> dict[str, Any]:
        dimension_counts: dict[str, Counter[str]] = defaultdict(Counter)
        mode_counts: Counter[str] = Counter()
        for scenario in self.scenarios:
            mode_counts[scenario.evidence_mode] += 1
            for check in scenario.checks:
                dimension_counts[check.dimension][
                    "pass" if check.passed else "fail"
                ] += 1

        return {
            "schema_version": SCHEMA_VERSION,
            "suite_id": self.suite_id,
            "synthetic_only": self.synthetic_only,
            "contract_status": self.contract_status,
            "runtime_status": self.runtime_status,
            "runtime_required": self.runtime_required,
            "runtime_evidence_supported": False,
            "claim": (
                "synthetic contract invariants failed"
                if self.contract_status == "fail"
                else "synthetic contract invariants pass; runtime adapters are not implemented and runtime pass is unavailable"
            ),
            "evidence_modes": dict(sorted(mode_counts.items())),
            "dimension_summary": {
                dimension: dict(sorted(counts.items()))
                for dimension, counts in sorted(dimension_counts.items())
            },
            "regression_failures": list(self.regression_failures),
            "scenarios": [scenario.to_dict() for scenario in self.scenarios],
        }


def _is_number(value: Any) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
    )


def _check(
    dimension: str,
    check: str,
    passed: bool,
    explanation: str,
    *,
    metric: Mapping[str, Any] | None = None,
) -> CheckResult:
    if dimension not in VALID_DIMENSIONS:
        raise EvaluationError(f"unknown dimension {dimension!r}")
    return CheckResult(dimension, check, bool(passed), explanation, metric)


def _require_mapping(value: Any, where: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise EvaluationError(f"{where} must be an object")
    return value


def _require_sequence(value: Any, where: str) -> list[Any]:
    if not isinstance(value, list):
        raise EvaluationError(f"{where} must be an array")
    return value


def _require_string(value: Any, where: str) -> str:
    if not isinstance(value, str) or not value:
        raise EvaluationError(f"{where} must be a non-empty string")
    return value


def _require_string_sequence(
    value: Any, where: str, *, allow_empty: bool = False
) -> list[str]:
    raw = _require_sequence(value, where)
    if not allow_empty and not raw:
        raise EvaluationError(f"{where} must not be empty")
    result: list[str] = []
    for index, item in enumerate(raw):
        if not isinstance(item, str) or not item:
            raise EvaluationError(f"{where}[{index}] must be a non-empty string")
        result.append(item)
    return result


def _require_bool(value: Any, where: str) -> bool:
    if not isinstance(value, bool):
        raise EvaluationError(f"{where} must be a boolean")
    return value


def _require_sensitivity(value: Any, where: str) -> str:
    sensitivity = _require_string(value, where)
    if sensitivity not in VALID_SENSITIVITY:
        raise EvaluationError(
            f"{where} must be one of {sorted(VALID_SENSITIVITY)}; got {sensitivity!r}"
        )
    return sensitivity


def _collect_strings(value: Any) -> Iterable[str]:
    if isinstance(value, str):
        yield value
    elif isinstance(value, Mapping):
        for child in value.values():
            yield from _collect_strings(child)
    elif isinstance(value, list):
        for child in value:
            yield from _collect_strings(child)


def _observation_index(
    scenario: Mapping[str, Any], *, required: bool = True
) -> dict[str, Mapping[str, Any]]:
    raw_observations = scenario.get("observations")
    if raw_observations is None and not required:
        return {}
    observations = _require_sequence(raw_observations, "observations")
    result: dict[str, Mapping[str, Any]] = {}
    previous_ts: float | None = None
    for index, raw in enumerate(observations):
        observation = _require_mapping(raw, f"observations[{index}]")
        observation_id = _require_string(
            observation.get("id"), f"observations[{index}].id"
        )
        timestamp_ms = observation.get("timestamp_ms")
        if observation_id in result:
            raise EvaluationError(f"duplicate observation id {observation_id!r}")
        if not _is_number(timestamp_ms):
            raise EvaluationError(
                f"observation {observation_id!r} has invalid timestamp_ms"
            )
        timestamp = float(timestamp_ms)
        if previous_ts is not None and timestamp < previous_ts:
            raise EvaluationError("observations must be chronological")
        previous_ts = timestamp
        result[observation_id] = observation
    return result


def _claims_with_valid_provenance(
    scenario: Mapping[str, Any], observations: Mapping[str, Mapping[str, Any]]
) -> tuple[bool, str, dict[str, Mapping[str, Any]]]:
    claims_raw = _require_sequence(scenario.get("claims"), "claims")
    claims: dict[str, Mapping[str, Any]] = {}
    problems: list[str] = []
    for index, raw in enumerate(claims_raw):
        claim = _require_mapping(raw, f"claims[{index}]")
        claim_id = _require_string(claim.get("id"), f"claims[{index}].id")
        if claim_id in claims:
            problems.append(f"duplicate claim id {claim_id}")
            continue
        confidence = claim.get("confidence")
        if not _is_number(confidence) or not 0.0 <= float(confidence) <= 1.0:
            problems.append(f"claim {claim_id} has invalid confidence")
        refs_value = claim.get("evidence_refs")
        if not isinstance(refs_value, list):
            raise EvaluationError(f"claim {claim_id}.evidence_refs must be an array")
        refs = _require_string_sequence(
            refs_value, f"claim {claim_id}.evidence_refs", allow_empty=True
        )
        if not refs:
            problems.append(f"claim {claim_id} has no evidence")
        unknown = [ref for ref in refs if ref not in observations]
        if unknown:
            problems.append(f"claim {claim_id} references unknown evidence {unknown}")
        claims[claim_id] = claim
    message = "; ".join(problems) if problems else "every claim cites known observations"
    return not problems, message, claims


def _memory_record_policy_ok(
    memory: Mapping[str, Any], observations: Mapping[str, Mapping[str, Any]], where: str
) -> tuple[bool, str]:
    sensitivity = _require_sensitivity(memory.get("sensitivity"), f"{where}.sensitivity")
    refs = _require_string_sequence(
        memory.get("evidence_refs"), f"{where}.evidence_refs", allow_empty=True
    )
    storage_scope = _require_string(memory.get("storage_scope"), f"{where}.storage_scope")
    cloud_egress = _require_bool(memory.get("cloud_egress"), f"{where}.cloud_egress")
    process_local_ephemeral = _require_bool(
        memory.get("process_local_ephemeral_cache_entry_created"),
        f"{where}.process_local_ephemeral_cache_entry_created",
    )
    reusable_cache = _require_bool(
        memory.get("reusable_cache_entry_created"),
        f"{where}.reusable_cache_entry_created",
    )
    cross_scope_cache = _require_bool(
        memory.get("cross_scope_cache_entry_created"),
        f"{where}.cross_scope_cache_entry_created",
    )
    lifecycle_state = _require_string(
        memory.get("lifecycle_state"), f"{where}.lifecycle_state"
    )

    base_ok = (
        memory.get("salience_warranted") is True
        and bool(refs)
        and all(ref in observations for ref in refs)
        and lifecycle_state in {"candidate", "confirmed"}
    )
    if sensitivity == "never_observe":
        return False, "never_observe cannot create a durable memory record"
    if sensitivity == "local_only":
        local_only_ok = (
            storage_scope == "local"
            and cloud_egress is False
            and reusable_cache is False
            and cross_scope_cache is False
        )
        return (
            base_ok and local_only_ok,
            "local_only may remain in local lifecycle storage and process-local ephemeral reuse, but never cloud egress or reusable/cross-scope cache",
        )
    # cloud_allowed is eligible for normal policy-controlled egress. Cache flags
    # are still explicit so adapters cannot hide an ambiguous cache disposition.
    cloud_allowed_ok = storage_scope in {"local", "cloud_safe"}
    _ = process_local_ephemeral
    return (
        base_ok and cloud_allowed_ok,
        "cloud_allowed memory remains subject to salience, lifecycle, provenance, and explicit storage scope",
    )


def _evaluate_testing(scenario: Mapping[str, Any]) -> list[CheckResult]:
    observations = _observation_index(scenario)
    provenance_ok, provenance_message, claims = _claims_with_valid_provenance(
        scenario, observations
    )
    expected = {
        "current_activity": "testing_debugging",
        "earlier_activity": "editing_bug_notes",
        "project": "continuum",
    }
    semantic_ok = all(
        claims.get(key, {}).get("value") == value for key, value in expected.items()
    )

    current_claim = claims.get("current_activity", {})
    current_refs_value = current_claim.get("evidence_refs", [])
    if not isinstance(current_refs_value, list):
        raise EvaluationError("claim current_activity.evidence_refs must be an array")
    current_refs = _require_string_sequence(
        current_refs_value,
        "claim current_activity.evidence_refs",
        allow_empty=True,
    )
    known_current_refs = [ref for ref in current_refs if ref in observations]
    source_types = {
        observations[ref].get("source")
        for ref in known_current_refs
        if isinstance(observations[ref].get("source"), str)
    }
    latest_current_ts = max(
        (float(observations[ref]["timestamp_ms"]) for ref in known_current_refs),
        default=None,
    )
    temporal_ok = (
        len(source_types) >= 2
        and latest_current_ts is not None
        and any(
            float(observation["timestamp_ms"]) < latest_current_ts
            for observation in observations.values()
        )
    )

    memories = _require_sequence(
        scenario.get("durable_memories", []), "durable_memories"
    )
    memory_problems: list[str] = []
    for index, raw in enumerate(memories):
        memory = _require_mapping(raw, f"durable_memories[{index}]")
        ok, message = _memory_record_policy_ok(
            memory, observations, f"durable_memories[{index}]"
        )
        if not ok:
            memory_problems.append(f"memory {index}: {message}")
    memory_ok = not memory_problems

    return [
        _check(
            "semantic_relevance",
            "answers identify the current activity and project",
            semantic_ok,
            "expected testing/debugging of Continuum without a stronger unsupported claim",
        ),
        _check(
            "historical_retrieval",
            "earlier related activity remains retrievable",
            claims.get("earlier_activity", {}).get("value") == "editing_bug_notes",
            "the earlier synthetic bug-notes edit is represented as prior context",
        ),
        _check(
            "temporal_coherence",
            "related evidence is synthesized across time and sources",
            temporal_ok,
            "a coherent session needs chronological evidence from at least two source types",
        ),
        _check(
            "evidence_provenance",
            "all claims cite known observations",
            provenance_ok,
            provenance_message,
        ),
        _check(
            "memory_precision",
            "durable memory follows salience, provenance, lifecycle, and sensitivity policy",
            memory_ok,
            (
                "; ".join(memory_problems)
                if memory_problems
                else "cloud_allowed may use policy-controlled egress; local_only remains local and non-reusable; never_observe creates no record"
            ),
            metric={"durable_memories": len(memories), "unit": "records"},
        ),
    ]


def _evaluate_research(scenario: Mapping[str, Any]) -> list[CheckResult]:
    observations = _observation_index(scenario)
    sessions = {observation.get("session_id") for observation in observations.values()}
    applications = {
        observation.get("application") for observation in observations.values()
    }
    topic_terms: Counter[str] = Counter()
    for observation_id, observation in observations.items():
        terms = _require_string_sequence(
            observation.get("topic_terms"),
            f"observation {observation_id}.topic_terms",
            allow_empty=True,
        )
        for term in terms:
            topic_terms[term.casefold()] += 1
    coherent = len(sessions) == 1 and None not in sessions
    cross_app = {"browser", "pdf_reader", "notes"}.issubset(applications)
    repeated_topic = bool(topic_terms) and max(topic_terms.values()) >= 3
    provenance_ok, message, claims = _claims_with_valid_provenance(
        scenario, observations
    )
    session_claim_ok = (
        claims.get("research_session", {}).get("value") == "school_research"
    )
    return [
        _check(
            "temporal_coherence",
            "browser, PDF, and notes events form one session",
            coherent and cross_app and repeated_topic and session_claim_ok,
            "one session id, all three application roles, and a topic repeated across at least three observations",
        ),
        _check(
            "semantic_relevance",
            "session label reflects the repeated research topic",
            session_claim_ok and repeated_topic,
            "the label is grounded in repeated terms rather than an isolated application switch",
        ),
        _check(
            "evidence_provenance",
            "research claim cites observations",
            provenance_ok,
            message,
        ),
    ]


def _require_non_negative_int(value: Any, where: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise EvaluationError(f"{where} must be a non-negative integer")
    return value


def _evaluate_repeated_frames(scenario: Mapping[str, Any]) -> list[CheckResult]:
    metrics = _require_mapping(scenario.get("metrics"), "metrics")
    total = _require_non_negative_int(metrics.get("eligible_frames"), "metrics.eligible_frames")
    inferences = _require_non_negative_int(
        metrics.get("semantic_inferences"), "metrics.semantic_inferences"
    )
    meaningful = _require_non_negative_int(
        metrics.get("meaningful_changes"), "metrics.meaningful_changes"
    )
    retained = _require_non_negative_int(
        metrics.get("meaningful_changes_retained"),
        "metrics.meaningful_changes_retained",
    )
    cache_hits = _require_non_negative_int(metrics.get("cache_hits"), "metrics.cache_hits")

    if total == 0:
        collapse = 0.0
        hit_rate = 0.0
    else:
        duplicate_frames = max(total - meaningful, 0)
        duplicate_inferences = max(inferences - meaningful, 0)
        collapse = (
            1.0
            if duplicate_frames == 0
            else 1.0 - duplicate_inferences / duplicate_frames
        )
        hit_rate = cache_hits / total
    no_loss = retained == meaningful
    bounded = inferences <= meaningful + math.ceil(max(total - meaningful, 0) * 0.10)
    accounting_ok = total > 0 and cache_hits + inferences == total

    return [
        _check(
            "cache_correctness",
            "unchanged/minimally changed frames collapse without hiding changes",
            collapse >= 0.90 and no_loss and bounded,
            "at most one semantic inference per ten duplicate frames, while every labeled meaningful change is retained",
            metric={
                "duplicate_collapse": round(collapse, 6),
                "minimum": 0.90,
                "meaningful_changes": meaningful,
                "meaningful_changes_retained": retained,
                "unit": "ratio_and_frames",
            },
        ),
        _check(
            "perception_latency",
            "cache behavior is observable rather than inferred from missing work",
            accounting_ok,
            "every eligible frame must be accounted for by a cache hit or semantic inference; schema v1 does not claim measured runtime milliseconds",
            metric={"cache_hit_rate": round(hit_rate, 6), "unit": "ratio"},
        ),
        _check(
            "regression_safety",
            "meaningful visual changes have zero loss",
            no_loss,
            "meaningful-change recall is an invariant, not an average score",
            metric={"expected": meaningful, "retained": retained, "unit": "frames"},
        ),
    ]


def _evaluate_vision_failure(scenario: Mapping[str, Any]) -> list[CheckResult]:
    health = _require_mapping(scenario.get("health"), "health")
    repair = _require_mapping(scenario.get("repair"), "repair")
    cause = health.get("probable_cause")
    affected = _require_string_sequence(
        health.get("affected_capabilities"), "health.affected_capabilities"
    )
    unaffected = _require_string_sequence(
        health.get("unaffected_capabilities"), "health.unaffected_capabilities"
    )
    degraded = health.get("status") == "degraded"
    clear_scope = isinstance(cause, str) and bool(cause.strip()) and bool(affected)
    safe_degrade = (
        degraded
        and "capture" in unaffected
        and "vision_semantics" in affected
    )

    policy = repair.get("policy_decision")
    action_at = repair.get("action_timestamp_ms")
    probe_at = repair.get("verification_timestamp_ms")
    probe = repair.get("verification_status")
    success_reported = repair.get("success_reported") is True
    policy_ok = (
        policy in {"allow", "ask"}
        and repair.get("permission_bypassed") is False
    )
    verified = (
        _is_number(action_at)
        and _is_number(probe_at)
        and float(probe_at) > float(action_at)
        and probe == "healthy"
        and success_reported
    )
    return [
        _check(
            "failure_explanation",
            "health exposes cause, scope, and safe degradation",
            clear_scope and safe_degrade,
            "capture remains available while unavailable vision semantics are explicitly degraded",
        ),
        _check(
            "repair_verification",
            "repair follows policy and reports success only after a healthy post-action probe",
            policy_ok and verified,
            "verification must be newer than the action and permission boundaries may not be bypassed",
        ),
        _check(
            "ui_truthfulness",
            "degraded state is not presented as healthy",
            health.get("ui_status") == "degraded"
            and health.get("ui_source") == "runtime",
            "the UI status must be runtime-backed and match the health state",
        ),
    ]


def _privacy_memory_ok(memory: Mapping[str, Any], where: str) -> bool:
    sensitivity = _require_sensitivity(memory.get("sensitivity"), f"{where}.sensitivity")
    storage_scope = _require_string(memory.get("storage_scope"), f"{where}.storage_scope")
    lifecycle_state = _require_string(
        memory.get("lifecycle_state"), f"{where}.lifecycle_state"
    )
    cloud_egress = _require_bool(memory.get("cloud_egress"), f"{where}.cloud_egress")
    automatic = _require_bool(memory.get("automatic"), f"{where}.automatic")
    reusable = _require_bool(
        memory.get("reusable_cache_entry_created"),
        f"{where}.reusable_cache_entry_created",
    )
    exportable = _require_bool(
        memory.get("exportable_cache_entry_created"),
        f"{where}.exportable_cache_entry_created",
    )
    return (
        sensitivity == "local_only"
        and storage_scope == "local"
        and lifecycle_state in {"candidate", "confirmed"}
        and memory.get("salience_warranted") is True
        and cloud_egress is False
        and automatic is False
        and reusable is False
        and exportable is False
    )


def _evaluate_privacy(scenario: Mapping[str, Any]) -> list[CheckResult]:
    sensitive_input = _require_mapping(
        scenario.get("sensitive_input"), "sensitive_input"
    )
    sentinel = _require_string(
        sensitive_input.get("sentinel"), "sensitive_input.sentinel"
    )
    outputs = _require_mapping(scenario.get("outputs"), "outputs")
    sensitivity = _require_sensitivity(outputs.get("sensitivity"), "outputs.sensitivity")
    redacted = _require_bool(
        outputs.get("redaction_applied"), "outputs.redaction_applied"
    )
    observation_record_created = _require_bool(
        outputs.get("observation_record_created"),
        "outputs.observation_record_created",
    )
    event_created = _require_bool(outputs.get("event_created"), "outputs.event_created")
    content_hash_created = _require_bool(
        outputs.get("content_hash_created"), "outputs.content_hash_created"
    )
    memories = _require_sequence(
        outputs.get("durable_memories", []), "outputs.durable_memories"
    )
    injection = _require_mapping(
        outputs.get("prompt_injection"), "outputs.prompt_injection"
    )
    cache = _require_mapping(outputs.get("cache"), "outputs.cache")
    process_local_ephemeral = _require_bool(
        cache.get("process_local_ephemeral_entry_created"),
        "outputs.cache.process_local_ephemeral_entry_created",
    )
    reusable = _require_bool(
        cache.get("reusable_entry_created"),
        "outputs.cache.reusable_entry_created",
    )
    exportable = _require_bool(
        cache.get("exportable_entry_created"),
        "outputs.cache.exportable_entry_created",
    )
    cross_scope = _require_bool(
        cache.get("cross_scope_entry_created"),
        "outputs.cache.cross_scope_entry_created",
    )
    cloud_safe = _require_mapping(
        outputs.get("cloud_safe_output"), "outputs.cloud_safe_output"
    )
    cloud_included = _require_bool(
        cloud_safe.get("included"), "outputs.cloud_safe_output.included"
    )
    _require_sequence(
        cloud_safe.get("records"), "outputs.cloud_safe_output.records"
    )

    leaked_locations = [text for text in _collect_strings(outputs) if sentinel in text]
    memory_policy_ok = all(
        _privacy_memory_ok(
            _require_mapping(raw, f"outputs.durable_memories[{index}]"),
            f"outputs.durable_memories[{index}]",
        )
        for index, raw in enumerate(memories)
    )
    no_automatic_memory = all(
        isinstance(raw, Mapping) and raw.get("automatic") is False for raw in memories
    )
    untrusted = (
        injection.get("input_trust") == "untrusted"
        and injection.get("privilege_elevated") is False
    )

    if sensitivity == "local_only":
        disposition_ok = (
            redacted
            and cloud_included is False
            and memory_policy_ok
            and no_automatic_memory
        )
        cache_ok = reusable is False and exportable is False and cross_scope is False
        # Process-local ephemeral reuse is allowed for local_only evidence. It is
        # explicit here so a reusable/exportable cache cannot masquerade as local.
        _ = process_local_ephemeral
    elif sensitivity == "never_observe":
        disposition_ok = (
            observation_record_created is False
            and event_created is False
            and content_hash_created is False
            and not memories
            and cloud_included is False
        )
        cache_ok = (
            process_local_ephemeral is False
            and reusable is False
            and exportable is False
            and cross_scope is False
        )
    else:
        # This scenario is intentionally privacy-sensitive, so a cloud_allowed
        # disposition is a contract failure rather than a malformed token.
        disposition_ok = False
        cache_ok = False

    return [
        _check(
            "privacy",
            "sensitive observation follows canonical retention and egress policy",
            not leaked_locations and disposition_ok,
            "never_observe creates no event/hash/cache/memory; local_only may remain locally under lifecycle rules but never enters cloud-safe output",
            metric={
                "raw_secret_occurrences": len(leaked_locations),
                "maximum": 0,
                "unit": "occurrences",
            },
        ),
        _check(
            "confidence_calibration",
            "observed screen text remains untrusted input",
            untrusted,
            "prompt-like text may not elevate authority or policy privileges",
        ),
        _check(
            "cache_correctness",
            "cache disposition distinguishes ephemeral local reuse from reusable/exportable reuse",
            cache_ok,
            "local_only may use bounded process-local ephemeral reuse only; never_observe creates no cache artifact",
        ),
    ]


def _evaluate_contradiction(scenario: Mapping[str, Any]) -> list[CheckResult]:
    observations = _observation_index(scenario)
    provenance_ok, message, claims = _claims_with_valid_provenance(
        scenario, observations
    )
    initial = claims.get("initial_activity", {})
    revised = claims.get("revised_activity", {})
    initial_conf = initial.get("confidence")
    revised_conf = revised.get("confidence")
    confidence_ok = (
        _is_number(initial_conf)
        and _is_number(revised_conf)
        and float(revised_conf) >= float(initial_conf)
        and revised.get("value") != initial.get("value")
    )
    cache = _require_mapping(scenario.get("cache"), "cache")
    cache_ok = (
        cache.get("stale_key_invalidated") is True
        and cache.get("stale_value_served_after_contradiction") is False
        and cache.get("invalidation_reason") == "contradictory_evidence"
    )
    memory = _require_mapping(scenario.get("memory_update"), "memory_update")
    memory_refs = _require_string_sequence(
        memory.get("evidence_refs"),
        "memory_update.evidence_refs",
        allow_empty=True,
    )
    memory_ok = (
        memory.get("overwrite_in_place") is False
        and memory.get("status") == "superseded"
        and memory.get("supersedes") == initial.get("id")
        and bool(memory_refs)
        and all(ref in observations for ref in memory_refs)
    )
    return [
        _check(
            "confidence_calibration",
            "confidence and conclusion update when stronger contradictory evidence arrives",
            confidence_ok,
            "the revised claim differs and is at least as confident because it cites newer stronger evidence",
        ),
        _check(
            "evidence_provenance",
            "both claims cite source evidence",
            provenance_ok,
            message,
        ),
        _check(
            "cache_correctness",
            "contradiction invalidates the stale conclusion before reuse",
            cache_ok,
            "no stale cached value may be served after the contradiction is observed",
        ),
        _check(
            "memory_precision",
            "durable knowledge is superseded with provenance instead of silently overwritten",
            memory_ok,
            "supersession must be visible and linked to the prior claim and a non-empty set of new evidence",
        ),
    ]


def _evaluate_chat_scroll(scenario: Mapping[str, Any]) -> list[CheckResult]:
    cases = _require_sequence(scenario.get("cases"), "cases")
    indexed: dict[str, Mapping[str, Any]] = {}
    for index, raw in enumerate(cases):
        case = _require_mapping(raw, f"cases[{index}]")
        name = _require_string(case.get("name"), f"cases[{index}].name")
        if name in indexed:
            raise EvaluationError(f"duplicate chat scroll case {name!r}")
        indexed[name] = case

    at_bottom = indexed.get("stream_when_at_bottom", {})
    scrolled = indexed.get("stream_when_user_scrolled_up", {})
    no_top_jump = indexed.get("stream_never_jumps_to_top", {})
    bottom_ok = at_bottom.get("after_distance_from_bottom_px") == 0
    preserved_ok = (
        scrolled.get("before_scroll_top_px") == scrolled.get("after_scroll_top_px")
        and scrolled.get("auto_scrolled") is False
    )
    minimum_scroll_top = no_top_jump.get("minimum_scroll_top_px", 0)
    top_ok = _is_number(minimum_scroll_top) and float(minimum_scroll_top) > 0
    return [
        _check(
            "ui_truthfulness",
            "streaming scroll behavior matches user intent",
            bottom_ok and preserved_ok and top_ok,
            "stick to bottom only when already at bottom; preserve a user's reading position; never jump to top",
        ),
        _check(
            "regression_safety",
            "all three scroll invariants are exercised",
            {
                "stream_when_at_bottom",
                "stream_when_user_scrolled_up",
                "stream_never_jumps_to_top",
            }.issubset(indexed),
            "the regression gate is incomplete unless each interaction state has evidence",
            metric={
                "required_cases": 3,
                "observed_cases": len(indexed),
                "unit": "cases",
            },
        ),
    ]


EVALUATORS: dict[str, Callable[[Mapping[str, Any]], list[CheckResult]]] = {
    "testing-continuum": _evaluate_testing,
    "school-research": _evaluate_research,
    "repeated-unchanged-frames": _evaluate_repeated_frames,
    "vision-failure": _evaluate_vision_failure,
    "privacy-sensitive-observation": _evaluate_privacy,
    "contradictory-evidence": _evaluate_contradiction,
    "chat-streaming-scroll": _evaluate_chat_scroll,
}


def evaluate_suite(
    payload: Mapping[str, Any], *, require_runtime: bool = False
) -> EvaluationReport:
    if payload.get("schema_version") != SCHEMA_VERSION:
        raise EvaluationError(
            f"schema_version must be {SCHEMA_VERSION}; got {payload.get('schema_version')!r}"
        )
    suite_id = _require_string(payload.get("suite_id"), "suite_id")
    synthetic_only = payload.get("synthetic_only") is True
    if not synthetic_only:
        raise EvaluationError("committed suites must declare synthetic_only=true")

    raw_scenarios = _require_sequence(payload.get("scenarios"), "scenarios")
    by_id: dict[str, Mapping[str, Any]] = {}
    for index, raw in enumerate(raw_scenarios):
        scenario = _require_mapping(raw, f"scenarios[{index}]")
        scenario_id = _require_string(
            scenario.get("id"), f"scenarios[{index}].id"
        )
        if scenario_id in by_id:
            raise EvaluationError(f"duplicate scenario id {scenario_id!r}")
        by_id[scenario_id] = scenario

    missing = sorted(set(EVALUATORS) - set(by_id))
    unexpected = sorted(set(by_id) - set(EVALUATORS))
    if missing or unexpected:
        raise EvaluationError(
            f"scenario registry mismatch: missing={missing}, unexpected={unexpected}"
        )

    results: list[ScenarioResult] = []
    for scenario_id, evaluator in EVALUATORS.items():
        scenario = by_id[scenario_id]
        title = _require_string(scenario.get("title"), f"scenario {scenario_id}.title")
        mode = scenario.get("evidence_mode")
        if mode not in VALID_EVIDENCE_MODES:
            raise EvaluationError(
                f"scenario {scenario_id!r} has unsupported evidence_mode {mode!r}; "
                "schema v1 accepts contract_fixture only and cannot claim runtime proof"
            )
        try:
            checks = evaluator(scenario)
        except EvaluationError:
            raise
        except (IndexError, KeyError, TypeError, ValueError) as error:
            raise EvaluationError(
                f"scenario {scenario_id!r} contains malformed exporter evidence: {error}"
            ) from error
        results.append(ScenarioResult(scenario_id, title, str(mode), checks))

    return EvaluationReport(suite_id, synthetic_only, tuple(results), require_runtime)


def compare_reports(
    candidate: EvaluationReport, baseline_payload: Mapping[str, Any]
) -> tuple[str, ...]:
    baseline_scenarios = {
        scenario.get("scenario_id"): scenario
        for scenario in _require_sequence(
            baseline_payload.get("scenarios"), "baseline.scenarios"
        )
        if isinstance(scenario, Mapping)
    }
    failures: list[str] = []
    for scenario in candidate.scenarios:
        baseline = baseline_scenarios.get(scenario.scenario_id)
        if not isinstance(baseline, Mapping):
            continue
        raw_checks = _require_sequence(
            baseline.get("checks", []),
            f"baseline scenario {scenario.scenario_id}.checks",
        )
        baseline_checks = {
            (check.get("dimension"), check.get("check")): check.get("status")
            for check in raw_checks
            if isinstance(check, Mapping)
        }
        for check in scenario.checks:
            key = (check.dimension, check.check)
            if baseline_checks.get(key) == "pass" and not check.passed:
                failures.append(
                    f"{scenario.scenario_id}: {check.dimension}/{check.check} regressed pass→fail"
                )
    return tuple(failures)


def load_json(path: Path) -> Mapping[str, Any]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise EvaluationError(f"could not load {path}: {error}") from error
    return _require_mapping(payload, str(path))


def write_report(path: Path, report: EvaluationReport) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(report.to_dict(), indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--suite", type=Path, required=True, help="Synthetic contract-fixture suite JSON"
    )
    parser.add_argument("--report", type=Path, help="Write a deterministic JSON report")
    parser.add_argument(
        "--baseline",
        type=Path,
        help="Fail on pass-to-fail regressions against a prior report",
    )
    parser.add_argument(
        "--require-runtime",
        action="store_true",
        help="Reserved release gate: schema v1 always fails because runtime adapters are not implemented",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    try:
        report = evaluate_suite(
            load_json(args.suite), require_runtime=args.require_runtime
        )
        if args.baseline:
            failures = compare_reports(report, load_json(args.baseline))
            report = EvaluationReport(
                report.suite_id,
                report.synthetic_only,
                report.scenarios,
                report.runtime_required,
                failures,
            )
        if args.report:
            write_report(args.report, report)
        print(json.dumps(report.to_dict(), indent=2, sort_keys=True))
        return 0 if report.exit_ok else 1
    except EvaluationError as error:
        print(f"evaluation error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
