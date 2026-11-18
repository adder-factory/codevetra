"""Keep measurement failures and ranking evidence intact across runner refactors."""

import copy
import importlib.util
import io
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from contextlib import redirect_stdout


SPEC = importlib.util.spec_from_file_location(
    "evaluate_retrieval", Path(__file__).resolve().parents[1] / "evaluate-retrieval.py"
)
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)
TARGET = {"path": "src/worker.rs", "symbol": "run"}
CASE = {"id": "worker", "split": "held_out", "query": "run the worker", "targets": [TARGET]}


class EvidenceClient:
    def __init__(self):
        self.calls = []
        self.statuses = 0
        self.fresh = True
        self.final_generation = "original"
        self.result = {
            "freshness": "current",
            "evidence": {
                "generationId": "original", "semanticReadiness": "ready",
                "items": [{"path": "src/unrelated.rs", "name": "run"},
                          {"path": "src/worker.rs", "qualified_name": "run"}],
            },
        }
        self.labels = [{"path": "src/worker.rs", "name": "run"}]

    def request(self, method, params):
        self.calls.append((method, params))
        return {}

    def send(self, message):
        self.calls.append((message["method"], {}))

    def tool(self, name, arguments):
        self.calls.append((name, arguments))
        if name == "codevetra_status":
            self.statuses += 1
            generation = "original" if self.statuses == 1 else self.final_generation
            return {"project": {"fresh": self.fresh,
                                "snapshot": {"current": {"generation_id": generation}}}}
        if arguments["by"] == "name":
            return {"items": self.labels}
        return copy.deepcopy(self.result)


class EvaluationTests(unittest.TestCase):
    def setUp(self):
        self.client = EvidenceClient()
        self.args = SimpleNamespace(project=Path("."), repeat=2, limit=5, label="fixture")
        self.report = {"complete": False, "observations": []}
        self.evaluation = runner.Evaluation(self.client, self.args, self.report)

    def run_cases(self, cases):
        with redirect_stdout(io.StringIO()):
            self.evaluation.run(cases)

    def test_complete_run_checks_labels_and_generation_then_records_exact_ranks(self):
        self.run_cases([CASE])
        self.assertTrue(self.report["complete"])
        self.assertEqual(self.report["target_coverage"], {"worker": True})
        observations = self.report["observations"]
        self.assertEqual([row["first_correct_rank"] for row in observations], [2, 2, 2, 2])
        self.assertEqual([row["mode"] for row in observations], ["bm25", "hybrid"] * 2)
        self.assertEqual([row["repetition"] for row in observations], [0, 0, 1, 1])
        self.assertEqual(self.client.statuses, 2)
        searches = [args for name, args in self.client.calls if name == "codevetra_find"]
        self.assertEqual([args["by"] for args in searches], ["name"] + ["bm25", "hybrid"] * 3)
        self.assertEqual(searches[0]["limit"], 100)
        self.assertTrue(all(args["lowTokens"] for args in searches))

    def test_missing_labels_stop_before_warmup_and_allowed_absence_is_recorded(self):
        self.client.labels = []
        with self.assertRaisesRegex(ValueError, "target labels absent"):
            self.run_cases([CASE])
        self.assertFalse(self.report["complete"])
        self.assertEqual(self.report["observations"], [])
        self.assertEqual(len(self.client.calls), 5)
        self.client.statuses = 0
        self.run_cases([{**CASE, "allow_unindexed": True}])
        self.assertEqual(self.report["target_coverage"], {"worker": False})

    def test_stale_initial_snapshot_is_rejected(self):
        self.client.fresh = False
        with self.assertRaisesRegex(ValueError, "fresh source snapshot"):
            self.run_cases([CASE])
        self.assertFalse(self.report["complete"])
        self.assertEqual(self.report["observations"], [])

    def test_changed_or_stale_observation_is_not_counted(self):
        self.evaluation.initialize()
        original = copy.deepcopy(self.client.result)
        for key, value in [("freshness", "stale"), ("generationId", "replacement")]:
            with self.subTest(key=key):
                self.client.result = copy.deepcopy(original)
                destination = self.client.result if key == "freshness" else self.client.result["evidence"]
                destination[key] = value
                with self.assertRaisesRegex(ValueError, "source or generation changed"):
                    self.evaluation.observe(CASE, "bm25", 0)
                self.assertEqual(self.report["observations"], [])

    def test_degraded_hybrid_preserves_completed_bm25_without_claiming_completion(self):
        self.client.result["evidence"]["semanticReadiness"] = "coverage_incomplete"
        with self.assertRaisesRegex(ValueError, "complete semantic readiness"):
            self.run_cases([CASE])
        self.assertFalse(self.report["complete"])
        self.assertEqual([row["mode"] for row in self.report["observations"]], ["bm25"])

    def test_final_generation_change_preserves_observations_but_fails_completion(self):
        self.client.final_generation = "replacement"
        with self.assertRaisesRegex(ValueError, "changed source generation"):
            self.run_cases([CASE])
        self.assertFalse(self.report["complete"])
        self.assertEqual(len(self.report["observations"]), 4)

    def test_report_keeps_partial_evidence_and_refuses_overwrite(self):
        self.client.result["evidence"]["semanticReadiness"] = "unavailable"
        with self.assertRaises(ValueError):
            self.run_cases([CASE])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            runner.write_report(path, self.report)
            saved = json.loads(path.read_text())
            self.assertFalse(saved["complete"])
            self.assertEqual(saved["summary"]["bm25/all"]["mean_reciprocal_rank"], 0.5)
            self.assertEqual(saved["summary"]["hybrid/all"], {"count": 0})
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(FileExistsError):
                runner.write_report(path, self.report)


class ManifestTests(unittest.TestCase):
    def test_valid_manifest_returns_original_bytes_for_hashing(self):
        encoded = json.dumps({"schema_version": 1, "cases": [CASE]}).encode()
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cases.json"
            path.write_bytes(encoded)
            cases, original = runner.read_cases(path)
        self.assertEqual(cases, [CASE])
        self.assertEqual(original, encoded)

    def test_manifest_and_utf8_query_limits_remain_enforced(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cases.json"
            path.write_bytes(b" " * (runner.MAX_MANIFEST_BYTES + 1))
            with self.assertRaisesRegex(ValueError, "too large"):
                runner.read_cases(path)
        runner.validate_case({**CASE, "query": "é" * 2048})
        with self.assertRaisesRegex(ValueError, "query is invalid"):
            runner.validate_case({**CASE, "query": "é" * 2049})


if __name__ == "__main__":
    unittest.main()
