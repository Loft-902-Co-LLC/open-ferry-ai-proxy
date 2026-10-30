"""CLI tests using real temporary git histories; no Rust or Go builds."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("parity-collection-valid.py").resolve()


class CollectionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q")
        self.write(".git/info/exclude", "seed.json\nsecond.json\nmanifest.json\ncollection/\n")
        self.git("config", "core.autocrlf", "false")
        for path in ("crates/open-ferry-core/src/config/save.rs", "tools/parity/src/main.rs",
                     "Cargo.lock", "Cargo.toml"):
            self.write(path, "initial\n")
        self.save()
        self.pin = self.git("rev-parse", "HEAD")
        self.git("update-index", "--add", "--cacheinfo", f"160000,{self.pin},reference/cliproxyapi")
        self.save(stage=False)
        self.producer = self.git("rev-parse", "HEAD")
        self.observation = {
            "fingerprint": {"classifier_version": "4", "generator_sha256": "hash", "upstream_pin": self.pin},
            "non_certification": 0,
        }
        # Leave observation files untracked, outside the selected paths.
        self.write_json("seed.json", self.observation)

    def git(self, *args):
        result = subprocess.run(["git", "-C", str(self.root), *args],
                                capture_output=True, text=True)
        if result.returncode:
            self.fail(result.stdout + result.stderr)
        return result.stdout.strip()

    def write(self, path, content):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8", newline="\n")

    def write_json(self, path, content):
        self.write(path, json.dumps(content))

    def save(self, stage=True):
        if stage:
            self.git("add", "--all", "--", ".", ":(exclude)reference/cliproxyapi")
        self.git("-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")

    def run_check(self, *args, code=0):
        result = subprocess.run([sys.executable, "-I", str(SCRIPT), "--json", *args],
                                cwd=self.root, capture_output=True, text=True)
        self.assertEqual(result.returncode, code, result.stdout + result.stderr)
        self.assertEqual(len(result.stdout.splitlines()), 1)
        return json.loads(result.stdout)

    def check(self, code=0, *args):
        return self.run_check("--producer", self.producer, "--observation", "seed.json", *args, code=code)

    def test_valid_and_excluded_changes(self):
        self.assertEqual(self.check()["status"], "valid")
        for path in ("tools/parity/baseline/config-save.json", "tools/parity/README.md",
                     "tools/parity/docs/guide.json", "tools/parity/src/notes.rst", "other.rs"):
            self.write(path, "irrelevant\n")
        self.save()
        self.assertEqual(self.check()["status"], "valid")

    def test_each_group_and_changed_paths(self):
        for group, path in (
            ("config", "crates/open-ferry-core/src/config/save.rs"),
            ("parity", "tools/parity/src/main.rs"),
            ("build", "Cargo.toml"), ("build", "Cargo.lock"),
            ("build", "rust-toolchain.toml"),
        ):
            with self.subTest(path=path):
                self.git("reset", "--hard", self.producer)
                self.write(path, "changed\n")
                self.save()
                result = self.check(1)
                self.assertEqual(result["producers"][0]["changes"], [{"group": group, "paths": [path]}])

    def test_deleted_and_renamed_inputs(self):
        self.git("mv", "tools/parity/src/main.rs", "tools/parity/src/renamed.rs")
        self.save()
        self.assertEqual(self.check(1)["producers"][0]["changes"][0]["paths"],
                         ["tools/parity/src/main.rs", "tools/parity/src/renamed.rs"])
        self.git("reset", "--hard", self.producer)
        self.git("rm", "Cargo.lock")
        self.save()
        self.assertEqual(self.check(1)["producers"][0]["changes"],
                         [{"group": "build", "paths": ["Cargo.lock"]}])

    def test_pin_mismatch_and_upstream_change(self):
        self.observation["fingerprint"]["upstream_pin"] = "wrong"
        self.write_json("seed.json", self.observation)
        self.assertIn("upstream_pin mismatch", self.check(1)["observations"][0]["reasons"][0])
        self.observation["fingerprint"]["upstream_pin"] = self.pin
        self.write_json("seed.json", self.observation)
        self.git("update-index", "--cacheinfo", f"160000,{self.producer},reference/cliproxyapi")
        self.save(stage=False)
        result = self.check(1)
        self.assertEqual(result["producers"][0]["changes"][0]["group"], "upstream")

    def test_non_certification_and_fingerprint_consistency(self):
        self.observation["non_certification"] = 1
        self.write_json("seed.json", self.observation)
        self.assertIn("non_certification = 1", self.check(1)["observations"][0]["reasons"])
        self.observation["non_certification"] = 0
        self.observation["fingerprint"]["generator_sha256"] = "other"
        self.write_json("second.json", self.observation)
        result = self.check(1, "--observation", "second.json")
        self.assertIn("fingerprint differs from first observation", result["observations"][1]["reasons"])

    def test_manifest_relative_paths_and_mixed_producers(self):
        self.write("tools/parity/README.md", "docs\n")
        self.save()
        later = self.git("rev-parse", "HEAD")
        self.write_json("collection/manifest.json", [
            {"observation": "../seed.json", "producer": self.producer},
            {"observation": "../seed.json", "producer": later},
        ])
        result = self.run_check("--manifest", "collection/manifest.json")
        self.assertEqual(len(result["producers"]), 2)
        self.write("crates/open-ferry-core/src/config/save.rs", "changed\n")
        self.save()
        latest = self.git("rev-parse", "HEAD")
        self.write_json("collection/manifest.json", [
            {"observation": "../seed.json", "producer": self.producer},
            {"observation": "../seed.json", "producer": latest},
        ])
        result = self.run_check("--manifest", "collection/manifest.json", code=1)
        self.assertEqual([p["status"] for p in result["producers"]], ["stale", "valid"])

    def test_input_errors(self):
        for args in ((), ("--producer", "missing"),
                     ("--manifest", "absent.json"),
                     ("--manifest", "seed.json", "--producer", self.producer),
                     ("--producer", self.producer, "--head", "missing"),
                     ("--producer", self.producer, "--observation", "absent.json")):
            with self.subTest(args=args):
                self.assertEqual(self.run_check(*args, code=2)["status"], "error")
        for value in ([], {}, {"fingerprint": {}}, {**self.observation, "non_certification": False}):
            self.write_json("seed.json", value)
            self.check(2)
        self.write("seed.json", "{bad json")
        self.check(2)
        self.write_json("manifest.json", [])
        self.run_check("--manifest", "manifest.json", code=2)

    def test_no_observations_and_text_output(self):
        self.assertEqual(self.run_check("--producer", self.producer)["status"], "valid")
        result = subprocess.run([sys.executable, "-I", str(SCRIPT), "--producer", self.producer,
                                 "--observation", "seed.json"], cwd=self.root, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0)
        self.assertIn("seed.json: valid", result.stdout)
        self.assertIn("collection: valid", result.stdout)


if __name__ == "__main__":
    unittest.main()
