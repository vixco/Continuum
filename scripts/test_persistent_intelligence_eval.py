#!/usr/bin/env python3
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Callable

MODULE_PATH = Path(__file__).with_name("persistent_intelligence_eval.py")
SPEC = importlib.util.spec_from_file_location("persistent_intelligence_eval", MODULE_PATH)
assert SPEC and SPEC.loader
module = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = module
SPEC.loader.exec_module(module)

ROOT = Path(__file__).resolve().parents[1]
SUITE_PATH = ROOT / "evals/persistent-intelligence/reference-suite.json"


def load_suite() -> dict:
    return json.loads(SUITE_PATH.read_text(encoding="utf-8"))


def scenario(payload: dict, scenario_id: str) -> dict:
    return next(item for item in payload["scenarios"] if item["id"] == scenario_id)


def scenario_result(report: object, scenario_id: str) -> object:
    return next(item for item in report.scenarios if item.scenario_id == scenario_id)


class ReferenceSuiteTests(unittest.TestCase):
    def test_reference_contract_passes_without_claiming_runtime_proof(self) -> None:
        report = module.evaluate_suite(load_suite())
        self.assertEqual(report.contract_status, "pass")
        self.assertEqual(report.runtime_status, "unsupported")
        self.assertTrue(report.exit_ok)
        self.assertIn("runtime pass is unavailable", report.to_dict()["claim"])

    def test_require_runtime_makes_contract_only_suite_non_green(self) -> None:
        report = module.evaluate_suite(load_suite(), require_runtime=True)
        self.assertEqual(report.runtime_status, "unsupported")
        self.assertFalse(report.exit_ok)

    def test_relabeling_fixtures_as_runtime_is_rejected(self) -> None:
        payload = load_suite()
        for item in payload["scenarios"]:
            item["evidence_mode"] = "runtime"
        with self.assertRaisesRegex(module.EvaluationError, "cannot claim runtime proof"):
            module.evaluate_suite(payload, require_runtime=True)

    def test_never_observe_cannot_be_promoted_to_durable_memory(self) -> None:
        payload = load_suite()
        scenario(payload, "testing-continuum")["durable_memories"][0][
            "sensitivity"
        ] = "never_observe"
        report = module.evaluate_suite(payload)
        result = scenario_result(report, "testing-continuum")
        self.assertTrue(
            any(
                check.dimension == "memory_precision" and not check.passed
                for check in result.checks
            )
        )

    def test_local_only_durable_memory_is_permitted_only_with_local_scope(self) -> None:
        payload = load_suite()
        memory = scenario(payload, "testing-continuum")["durable_memories"][0]
        memory.update(
            {
                "sensitivity": "local_only",
                "storage_scope": "local",
                "cloud_egress": False,
                "process_local_ephemeral_cache_entry_created": True,
                "reusable_cache_entry_created": False,
                "cross_scope_cache_entry_created": False,
            }
        )
        passing = scenario_result(
            module.evaluate_suite(payload), "testing-continuum"
        )
        self.assertTrue(
            any(
                check.dimension == "memory_precision" and check.passed
                for check in passing.checks
            )
        )

        forbidden_mutations: list[tuple[str, object]] = [
            ("cloud_egress", True),
            ("reusable_cache_entry_created", True),
            ("cross_scope_cache_entry_created", True),
        ]
        for field, value in forbidden_mutations:
            with self.subTest(field=field):
                candidate = load_suite()
                candidate_memory = scenario(candidate, "testing-continuum")[
                    "durable_memories"
                ][0]
                candidate_memory.update(
                    {
                        "sensitivity": "local_only",
                        "storage_scope": "local",
                        "cloud_egress": False,
                        "process_local_ephemeral_cache_entry_created": True,
                        "reusable_cache_entry_created": False,
                        "cross_scope_cache_entry_created": False,
                        field: value,
                    }
                )
                result = scenario_result(
                    module.evaluate_suite(candidate), "testing-continuum"
                )
                self.assertTrue(
                    any(
                        check.dimension == "memory_precision" and not check.passed
                        for check in result.checks
                    )
                )

    def test_unknown_evidence_reference_fails_provenance(self) -> None:
        payload = load_suite()
        scenario(payload, "testing-continuum")["claims"][0]["evidence_refs"] = [
            "missing"
        ]
        result = scenario_result(
            module.evaluate_suite(payload), "testing-continuum"
        )
        self.assertFalse(result.passed)
        self.assertTrue(
            any(
                check.dimension == "evidence_provenance" and not check.passed
                for check in result.checks
            )
        )

    def test_mixed_known_and_unknown_provenance_fails_without_crashing(self) -> None:
        payload = load_suite()
        scenario(payload, "testing-continuum")["claims"][0]["evidence_refs"] = [
            "tc-2",
            "missing",
        ]
        result = scenario_result(
            module.evaluate_suite(payload), "testing-continuum"
        )
        self.assertFalse(result.passed)
        self.assertTrue(
            any(
                check.dimension == "evidence_provenance" and not check.passed
                for check in result.checks
            )
        )

    def test_malformed_exporter_shapes_exit_two_without_traceback(self) -> None:
        def refs_as_string(payload: dict) -> None:
            scenario(payload, "testing-continuum")["claims"][0][
                "evidence_refs"
            ] = "tc-2"

        def topic_terms_as_string(payload: dict) -> None:
            scenario(payload, "school-research")["observations"][0][
                "topic_terms"
            ] = "consensus"

        def affected_capabilities_as_string(payload: dict) -> None:
            scenario(payload, "vision-failure")["health"][
                "affected_capabilities"
            ] = "vision_semantics"

        mutations: list[tuple[str, Callable[[dict], None]]] = [
            ("evidence_refs", refs_as_string),
            ("topic_terms", topic_terms_as_string),
            ("affected_capabilities", affected_capabilities_as_string),
        ]
        for name, mutate in mutations:
            with self.subTest(shape=name), tempfile.TemporaryDirectory() as directory:
                payload = load_suite()
                mutate(payload)
                suite_path = Path(directory) / "malformed.json"
                suite_path.write_text(json.dumps(payload), encoding="utf-8")
                stdout = io.StringIO()
                stderr = io.StringIO()
                with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(
                    stderr
                ):
                    code = module.main(["--suite", str(suite_path)])
                self.assertEqual(code, 2)
                self.assertIn("evaluation error:", stderr.getvalue())
                self.assertNotIn("Traceback", stderr.getvalue())

    def test_dedupe_cannot_trade_away_meaningful_change_recall(self) -> None:
        payload = load_suite()
        scenario(payload, "repeated-unchanged-frames")["metrics"][
            "meaningful_changes_retained"
        ] = 3
        result = scenario_result(
            module.evaluate_suite(payload), "repeated-unchanged-frames"
        )
        self.assertFalse(result.passed)
        self.assertTrue(
            any(
                check.dimension == "regression_safety" and not check.passed
                for check in result.checks
            )
        )

    def test_repair_success_requires_a_newer_healthy_probe(self) -> None:
        payload = load_suite()
        repair = scenario(payload, "vision-failure")["repair"]
        repair["verification_timestamp_ms"] = repair["action_timestamp_ms"]
        result = scenario_result(module.evaluate_suite(payload), "vision-failure")
        self.assertTrue(
            any(
                check.dimension == "repair_verification" and not check.passed
                for check in result.checks
            )
        )

    def test_secret_occurrence_anywhere_in_outputs_is_a_failure(self) -> None:
        payload = load_suite()
        privacy = scenario(payload, "privacy-sensitive-observation")
        privacy["outputs"]["logs"].append(privacy["sensitive_input"]["sentinel"])
        result = scenario_result(
            module.evaluate_suite(payload), "privacy-sensitive-observation"
        )
        self.assertTrue(
            any(
                check.dimension == "privacy" and not check.passed
                for check in result.checks
            )
        )

    def test_local_only_cache_may_be_ephemeral_but_not_reusable(self) -> None:
        payload = load_suite()
        passing = scenario_result(
            module.evaluate_suite(payload), "privacy-sensitive-observation"
        )
        self.assertTrue(
            any(
                check.dimension == "cache_correctness" and check.passed
                for check in passing.checks
            )
        )

        privacy = scenario(payload, "privacy-sensitive-observation")
        privacy["outputs"]["cache"]["reusable_entry_created"] = True
        failing = scenario_result(
            module.evaluate_suite(payload), "privacy-sensitive-observation"
        )
        self.assertTrue(
            any(
                check.dimension == "cache_correctness" and not check.passed
                for check in failing.checks
            )
        )

    def test_never_observe_requires_zero_local_artifacts(self) -> None:
        payload = load_suite()
        outputs = scenario(payload, "privacy-sensitive-observation")["outputs"]
        outputs["sensitivity"] = "never_observe"
        failing = scenario_result(
            module.evaluate_suite(payload), "privacy-sensitive-observation"
        )
        self.assertFalse(failing.passed)

        outputs["observation_record_created"] = False
        outputs["event_created"] = False
        outputs["content_hash_created"] = False
        outputs["durable_memories"] = []
        outputs["cloud_safe_output"]["included"] = False
        outputs["cache"] = {
            "process_local_ephemeral_entry_created": False,
            "reusable_entry_created": False,
            "exportable_entry_created": False,
            "cross_scope_entry_created": False,
        }
        passing = scenario_result(
            module.evaluate_suite(payload), "privacy-sensitive-observation"
        )
        self.assertTrue(passing.passed)

    def test_contradiction_must_invalidate_stale_cache(self) -> None:
        payload = load_suite()
        contradiction = scenario(payload, "contradictory-evidence")
        contradiction["cache"]["stale_value_served_after_contradiction"] = True
        result = scenario_result(
            module.evaluate_suite(payload), "contradictory-evidence"
        )
        self.assertTrue(
            any(
                check.dimension == "cache_correctness" and not check.passed
                for check in result.checks
            )
        )

    def test_supersession_requires_non_empty_provenance(self) -> None:
        payload = load_suite()
        scenario(payload, "contradictory-evidence")["memory_update"][
            "evidence_refs"
        ] = []
        result = scenario_result(
            module.evaluate_suite(payload), "contradictory-evidence"
        )
        self.assertTrue(
            any(
                check.dimension == "memory_precision" and not check.passed
                for check in result.checks
            )
        )

    def test_scroll_position_change_is_detected(self) -> None:
        payload = load_suite()
        scroll = scenario(payload, "chat-streaming-scroll")
        scroll["cases"][1]["after_scroll_top_px"] = 0
        result = scenario_result(
            module.evaluate_suite(payload), "chat-streaming-scroll"
        )
        self.assertFalse(result.passed)

    def test_baseline_comparison_detects_pass_to_fail_regression(self) -> None:
        baseline = module.evaluate_suite(load_suite()).to_dict()
        payload = load_suite()
        scenario(payload, "chat-streaming-scroll")["cases"][0][
            "after_distance_from_bottom_px"
        ] = 10
        candidate = module.evaluate_suite(payload)
        failures = module.compare_reports(candidate, baseline)
        self.assertEqual(len(failures), 1)
        self.assertIn("pass→fail", failures[0])

    def test_cli_writes_deterministic_report(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            report_path = Path(directory) / "report.json"
            with contextlib.redirect_stdout(io.StringIO()):
                code = module.main(
                    ["--suite", str(SUITE_PATH), "--report", str(report_path)]
                )
            self.assertEqual(code, 0)
            first = report_path.read_text(encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()):
                code = module.main(
                    ["--suite", str(SUITE_PATH), "--report", str(report_path)]
                )
            self.assertEqual(code, 0)
            self.assertEqual(first, report_path.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
