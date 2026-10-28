#!/usr/bin/env python3
"""Required-CI checks for the compatibility document and its real evidence."""

import copy
import importlib.util
import io
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
    @classmethod
    def setUpClass(cls):
        cls.catalog = compatibility.load_catalog()
        cls.harnesses = compatibility.build_harnesses(cls.catalog)
        cls.parity = compatibility.load_parity(cls.harnesses)

    def setUp(self):
        self.catalog = copy.deepcopy(type(self).catalog)

    def test_catalog_names_exist_and_run(self):
        compatibility.validate_evidence(self.catalog, self.harnesses)

    def test_generated_document_is_current(self):
        compatibility.check_document(compatibility.render(self.catalog, self.parity))

    def test_hand_edited_cell_is_rejected(self):
        expected = compatibility.render(self.catalog, self.parity)
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
            compatibility.validate_evidence(catalog, self.harnesses)

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
        self.assertEqual(sum(counts.values()), sum(len(compatibility.MEANINGFUL[p]) for p in compatibility.PROTOCOLS for provider in compatibility.PROVIDERS if provider != "route-only" and compatibility.eligible(p, provider)))
        self.assertEqual(sum(compatibility.cell_counts(self.catalog, scripted=True).values()), sum(map(len, compatibility.MEANINGFUL.values())))

    def test_unrelated_source_comment_does_not_change_document(self):
        expected = compatibility.render(self.catalog, self.parity)
        mapped_path = self.catalog["evidence"][0]["path"]
        original_read = compatibility.read_source

        def commented(path):
            source = original_read(path)
            return "// Unrelated comment above every mapped test.\n" + source if path == mapped_path else source

        with patch.object(compatibility, "read_source", side_effect=commented):
            changed_catalog = compatibility.load_catalog()
            self.assertEqual(expected, compatibility.render(changed_catalog, self.parity))

    def test_suite_slug_change_makes_document_stale(self):
        changed = copy.deepcopy(self.parity)
        changed[0]["slug"] += "-renamed"
        expected = compatibility.render(self.catalog, self.parity)
        self.assertNotEqual(expected, compatibility.render(self.catalog, changed))
        with tempfile.TemporaryDirectory(prefix="compatibility-test-") as directory:
            path = Path(directory) / "compatibility.md"
            path.write_bytes(expected.encode("utf-8"))
            with self.assertRaisesRegex(ValueError, "out of date"):
                compatibility.check_document(compatibility.render(self.catalog, changed), path)

    def test_body_edit_requires_accept_and_accept_updates_only_its_digest(self):
        entry = next(entry for entry in self.catalog["evidence"] if entry["id"] == "E03")
        original_read = compatibility.read_source

        def edited(path):
            source = original_read(path)
            return source.replace("assert_eq!(response.payload, MESSAGE.as_bytes());", "", 1) if path == entry["path"] else source

        with tempfile.TemporaryDirectory(prefix="compatibility-review-") as directory:
            path = Path(directory) / "catalog.json"
            original = compatibility.CATALOG.read_bytes()
            path.write_bytes(original)
            with patch.object(compatibility, "read_source", side_effect=edited):
                with self.assertRaisesRegex(ValueError, "E03's test changed.*--accept E03"):
                    compatibility.load_catalog(path)
                with patch.object(sys, "argv", ["compatibility.py", "--check"]), patch.object(sys, "stderr", new_callable=io.StringIO) as errors:
                    self.assertEqual(compatibility.main(), 1)
                    self.assertIn("E03's test changed", errors.getvalue())
                compatibility.accept_digest("E03", path)
                accepted = compatibility.load_catalog(path)
            before = json.loads(original)
            after = json.loads(path.read_bytes())
            changed = next(entry for entry in after["evidence"] if entry["id"] == "E03")
            reviewed = next(entry for entry in before["evidence"] if entry["id"] == "E03")
            self.assertNotEqual(reviewed["body_sha256"], changed["body_sha256"])
            changed["body_sha256"] = reviewed["body_sha256"]
            self.assertEqual(before, after)
            self.assertEqual(compatibility.render(self.catalog, self.parity), compatibility.render(accepted, self.parity))

    def test_body_comment_and_whitespace_do_not_require_review(self):
        entry = self.catalog["evidence"][0]
        original_read = compatibility.read_source
        body = compatibility.rust.Resolver(original_read).body(f"crates/{entry['package']}/src/lib.rs", entry["test"], entry["path"])
        position = body[0][1]

        def edited(path):
            source = original_read(path)
            return source[:position] + " /* review-neutral */ \n    " + source[position:] if path == entry["path"] else source

        with patch.object(compatibility, "read_source", side_effect=edited):
            self.assertEqual(self.catalog, compatibility.load_catalog())

    def test_same_leaf_name_in_wrong_module_or_file_is_rejected(self):
        entry = self.catalog["evidence"][0]
        resolver = compatibility.rust.Resolver(compatibility.read_source)
        with self.assertRaisesRegex(ValueError, "does not exist uniquely"):
            resolver.body(f"crates/{entry['package']}/src/lib.rs", "wrong::" + entry["test"].split("::")[-1], entry["path"])
        with self.assertRaisesRegex(ValueError, "not a test defined in"):
            resolver.body(f"crates/{entry['package']}/src/lib.rs", entry["test"], "some/other/tests.rs")


class RustBodyTests(unittest.TestCase):
    def test_literals_nested_comments_chars_and_lifetimes(self):
        source = r'''#[test] fn sample() {
            let a = "} // string /* not a comment */ ";
            let b = br###"{ /* raw */ \" }"###;
            let c = '}'; let d = '\u{7d}'; let e = b'{';
            /* outer { /* nested } */ } */
            let value: &'static str = "a b";
            assert!(true);
        }'''
        declaration = list(compatibility.rust.declarations(compatibility.rust.tokens(source)))[0]
        self.assertEqual(declaration[:2], ("fn", "sample"))
        body = declaration[3]
        commented = source.replace("assert!(true)", " /* } */ assert! ( true )")
        changed = list(compatibility.rust.declarations(compatibility.rust.tokens(commented)))[0][3]
        self.assertEqual(compatibility.rust.digest(body), compatibility.rust.digest(changed))
        changed = source.replace('"a b"', '"ab"')
        self.assertNotEqual(compatibility.rust.digest(body), compatibility.rust.digest(list(compatibility.rust.declarations(compatibility.rust.tokens(changed)))[0][3]))

    def test_malformed_body_fails_loudly(self):
        for source in ['#[test] fn sample() {', '#[test] fn sample() { r##"unterminated', '/* unterminated']:
            with self.subTest(source=source), self.assertRaises(ValueError):
                list(compatibility.rust.declarations(compatibility.rust.tokens(source)))


if __name__ == "__main__":
    unittest.main()
