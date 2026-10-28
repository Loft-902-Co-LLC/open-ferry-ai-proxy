#!/usr/bin/env python3
"""Required-CI checks for the compatibility document and its real evidence."""

import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("compatibility", Path(__file__).with_name("compatibility.py"))
compatibility = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compatibility)


class CompatibilityTests(unittest.TestCase):
    def setUp(self):
        self.catalog = compatibility.load_catalog()

    def test_catalog_names_exist_and_run(self):
        compatibility.validate_evidence(self.catalog, compatibility.build_harnesses(self.catalog))

    def test_generated_document_is_current(self):
        compatibility.check_document(compatibility.render(self.catalog))

    def test_hand_edited_cell_is_rejected(self):
        expected = compatibility.render(self.catalog)
        changed = expected.replace("F [E01](#e01)", "?", 1)
        self.assertNotEqual(changed, expected)
        with tempfile.TemporaryDirectory(prefix="compatibility-test-") as directory:
            path = Path(directory) / "compatibility.md"
            path.write_bytes(changed.encode("utf-8"))
            with self.assertRaisesRegex(ValueError, "out of date"):
                compatibility.check_document(expected, path)

    def test_missing_source_test_is_rejected(self):
        catalog = copy.deepcopy(self.catalog)
        catalog["evidence"][0]["test"] = "codex::executor::tests::this_test_does_not_exist"
        with tempfile.TemporaryDirectory(prefix="compatibility-test-") as directory:
            path = Path(directory) / "evidence.json"
            path.write_text(json.dumps(catalog), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "Evidence test does not exist"):
                compatibility.load_catalog(path)

    def test_missing_compiled_test_is_rejected(self):
        # A declaration in a source file does not establish that cfg/module
        # wiring includes it in this platform's compiled test harness.
        catalog = copy.deepcopy(self.catalog)
        catalog["evidence"] = [catalog["evidence"][0]]
        catalog["evidence"][0]["test"] = "not_compiled::execute_fills_empty_output_from_items"
        with self.assertRaisesRegex(ValueError, "does not exist in compiled harness"):
            compatibility.validate_evidence(catalog, compatibility.build_harnesses(self.catalog))

    def test_ignored_or_zero_tests_cannot_prove_a_cell(self):
        catalog = copy.deepcopy(self.catalog)
        catalog["evidence"] = [catalog["evidence"][0]]
        test = catalog["evidence"][0]["test"]
        for result in [
            "test result: ok. 0 passed; 0 failed; 1 ignored; 0 filtered out;",
            "test result: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out;",
        ]:
            with self.subTest(result=result):
                with patch.object(compatibility, "command", side_effect=[f"{test}: test\n", result]):
                    with self.assertRaisesRegex(ValueError, "did not run exactly once"):
                        compatibility.validate_evidence(catalog, {"open-ferry-providers": Path("unused")})

    def test_failing_evidence_command_is_rejected(self):
        with patch.object(compatibility.subprocess, "run", return_value=subprocess.CompletedProcess(["fixture"], 101, "test failed", "")):
            with self.assertRaisesRegex(ValueError, "Command failed"):
                compatibility.command(["fixture"])

    def test_counts_cover_all_dimensions_without_live_claims(self):
        counts = compatibility.cell_counts(self.catalog)
        self.assertEqual(counts["live-tested"], 0)
        self.assertGreater(counts["unknown"], 0)
        self.assertEqual(sum(counts.values()), len(compatibility.PROTOCOLS) * len(compatibility.PROVIDERS) * len(compatibility.CAPABILITIES))


if __name__ == "__main__":
    unittest.main()
