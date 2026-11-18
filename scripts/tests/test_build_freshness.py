"""Stored source timestamps must remain bounded, exact, and independent of file mtimes."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "build_tools", Path(__file__).resolve().parents[1] / "build_tools.py"
)
tools = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(tools)


class FreshnessReceiptTests(unittest.TestCase):
    def setUp(self):
        root = tools.cache_root() / "test-fixtures"
        root.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=root)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.receipt = self.root / "source-freshness.json"

    def test_receipt_round_trip_and_absence(self):
        self.assertIsNone(tools.read_source_receipt(self.receipt))
        tools.write_source_receipt(self.receipt, "a" * 64, 123)
        self.assertEqual(tools.read_source_receipt(self.receipt),
                         {"version": 1, "sourceDigest": "a" * 64, "mtimeNs": 123})
        self.assertEqual(list(self.root.glob(".source-freshness.*")), [])

    def test_invalid_receipt_never_supplies_a_reusable_timestamp(self):
        valid = {"version": 1, "sourceDigest": "a" * 64, "mtimeNs": 123}
        invalid = [None, [], {**valid, "version": 2}]
        invalid += [{**valid, "mtimeNs": stamp} for stamp in [None, True, 0, -1, 1.5, 2**63]]
        invalid += [{**valid, "sourceDigest": digest} for digest in [None, "a" * 63, "a" * 65, "A" * 64, "g" * 64]]
        for value in invalid:
            with self.subTest(value=value):
                self.receipt.write_text(json.dumps(value))
                with self.assertRaisesRegex(ValueError, "Invalid source freshness receipt"):
                    tools.read_source_receipt(self.receipt)

    def test_oversized_receipt_is_rejected_before_parsing(self):
        self.receipt.write_text(" " * 4097)
        with self.assertRaisesRegex(ValueError, "byte limit"):
            tools.read_source_receipt(self.receipt)

    def test_timestamp_scan_rejects_symlinks_and_excess_metadata(self):
        artifact = self.root / "artifact"
        artifact.write_bytes(b"fixture")
        link = self.root / "link"
        link.symlink_to(artifact)
        with self.assertRaisesRegex(ValueError, "must not be symlinks"):
            tools.latest_artifact_timestamp(self.root)
        link.unlink()
        self.assertEqual(tools.latest_artifact_timestamp(self.root), artifact.stat().st_mtime_ns)
        with patch.object(tools, "MAX_BUILD_CACHE_ENTRIES", 0):
            with self.assertRaisesRegex(ValueError, "metadata bound"):
                tools.latest_artifact_timestamp(self.root)

    def test_failed_atomic_replace_preserves_previous_receipt(self):
        tools.write_source_receipt(self.receipt, "a" * 64, 123)
        original = self.receipt.read_bytes()
        with patch.object(tools.os, "replace", side_effect=OSError("fixture failure")):
            with self.assertRaises(OSError):
                tools.write_source_receipt(self.receipt, "b" * 64, 456)
        self.assertEqual(self.receipt.read_bytes(), original)
        self.assertEqual(list(self.root.glob(".source-freshness.*")), [])


if __name__ == "__main__":
    unittest.main()
