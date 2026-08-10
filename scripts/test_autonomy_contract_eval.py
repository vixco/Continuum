import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import autonomy_contract_eval as evaluator


ROOT = Path(__file__).resolve().parents[1]
REFERENCE = ROOT / "evals" / "autonomy" / "reference-suite.json"


class AutonomyContractEvaluatorTests(unittest.TestCase):
    def reference(self):
        return json.loads(REFERENCE.read_text(encoding="utf-8"))

    def test_reference_suite_passes_contract_but_not_runtime(self):
        report = evaluator.evaluate_suite(self.reference())
        self.assertEqual(report.contract_status, "pass")
        self.assertEqual(report.runtime_status, "unsupported")
        self.assertTrue(report.runtime_required)

    def test_mutation_without_authorization_fails(self):
        payload = self.reference()
        scenario = payload["scenarios"][0]
        scenario["events"] = [
            event
            for event in scenario["events"]
            if event["type"] != "policy_authorized"
        ]
        report = evaluator.evaluate_suite(payload)
        result = report.scenarios[0]
        self.assertFalse(result.passed)
        self.assertIn(
            "capability_authorized",
            {violation.invariant for violation in result.violations},
        )

    def test_unknown_mutation_replay_fails(self):
        payload = self.reference()
        scenario = payload["scenarios"][2]
        terminal = scenario["events"].pop()
        scenario["events"].extend(
            [
                {
                    "seq": 9,
                    "at_ms": 1015,
                    "type": "step_checkpointed",
                    "step_id": "send-message",
                    "attempt": 2,
                },
                {
                    "seq": 10,
                    "at_ms": 1016,
                    "type": "step_dispatched",
                    "step_id": "send-message",
                    "attempt": 2,
                    "capability": "composio.send",
                    "target": "synthetic:recipient",
                    "risk": "write",
                    "idempotent": False,
                },
            ]
        )
        terminal["seq"] = 11
        terminal["at_ms"] = 1020
        scenario["events"].append(terminal)
        report = evaluator.evaluate_suite(payload)
        result = report.scenarios[2]
        self.assertIn(
            "unknown_mutation_never_replayed",
            {violation.invariant for violation in result.violations},
        )

    def test_scope_expansion_dispatch_fails(self):
        payload = self.reference()
        scenario = payload["scenarios"][0]
        dispatch = next(
            event for event in scenario["events"] if event["type"] == "step_dispatched"
        )
        dispatch["target"] = "synthetic:unapproved-target"
        report = evaluator.evaluate_suite(payload)
        self.assertIn(
            "scope_non_expansion",
            {violation.invariant for violation in report.scenarios[0].violations},
        )

    def test_completed_mutation_without_verification_fails(self):
        payload = self.reference()
        scenario = payload["scenarios"][0]
        scenario["events"] = [
            event
            for event in scenario["events"]
            if event["type"] != "postcondition_verified"
        ]
        for seq, event in enumerate(scenario["events"], start=1):
            event["seq"] = seq
        report = evaluator.evaluate_suite(payload)
        self.assertIn(
            "completion_requires_verified_postcondition",
            {violation.invariant for violation in report.scenarios[0].violations},
        )

    def test_secret_like_fixture_is_rejected(self):
        payload = self.reference()
        payload["scenarios"][0]["events"][0]["note"] = "password=synthetic"
        report = evaluator.evaluate_suite(payload)
        self.assertIn(
            "public_safe_fixture",
            {violation.invariant for violation in report.scenarios[0].violations},
        )

    def test_cli_writes_a_sanitized_report(self):
        with tempfile.TemporaryDirectory() as directory:
            report_path = Path(directory) / "report.json"
            status = evaluator.main(
                [
                    "--suite",
                    str(REFERENCE),
                    "--report",
                    str(report_path),
                ]
            )
            self.assertEqual(status, 0)
            payload = json.loads(report_path.read_text(encoding="utf-8"))
            self.assertEqual(payload["contract_status"], "pass")
            self.assertFalse(payload["runtime_evidence_supported"])

    def test_runtime_gate_stays_red(self):
        status = evaluator.main(
            [
                "--suite",
                str(REFERENCE),
                "--require-runtime",
            ]
        )
        self.assertEqual(status, 1)


if __name__ == "__main__":
    unittest.main()
