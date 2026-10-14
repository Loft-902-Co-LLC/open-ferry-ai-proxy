"""Not upstream's: the release library scanner and its positive control."""
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest


class PositiveControl(unittest.TestCase):
    REQUIRED = (
        b"startup.before_install", b"install.between_renames",
        b"startup.after_prepared_journal", b"startup.recovered_install_sync",
        b"test_endpoint_location",
    )

    def check_artifact(self, content, feature=True, *, artifact_feature=None, test=False, opt_level="3", artifacts=True):
        with tempfile.TemporaryDirectory() as root:
            root = pathlib.Path(root)
            artifact = root / "test-artifact.rlib"
            artifact.write_bytes(content)
            events = root / "cargo.json"
            events.write_text("\n".join(json.dumps(event) for event in (
                {"reason": "compiler-artifact", "target": {"name": "open_ferry_supervisor", "kind": ["lib"]},
                 "profile": {"opt_level": opt_level, "test": test},
                 "features": ["blue-green-test"] if (feature if artifact_feature is None else artifact_feature) else [],
                 "filenames": [str(artifact)] if artifacts else []},
                {"reason": "build-finished", "success": True},
            )), encoding="utf-8")
            return subprocess.run(
                [sys.executable, "-I", str(pathlib.Path(__file__).with_name("check-release.py")),
                 *(["--self-test"] if feature else []), str(events)],
                capture_output=True, text=True, check=False,
            )

    def test_each_required_hook_is_required(self):
        for omitted in self.REQUIRED:
            with self.subTest(omitted=omitted):
                result = self.check_artifact(b"\n".join(hook for hook in self.REQUIRED if hook != omitted))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(omitted.decode("ascii"), result.stderr)

    def test_all_required_hooks_prove_rejection(self):
        result = self.check_artifact(b"\n".join(self.REQUIRED))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("positive control rejected:", result.stdout)

    def test_each_hook_fails_a_feature_off_artifact(self):
        for hook in (*self.REQUIRED, b"hold-load", b"load.before_loaded"):
            with self.subTest(hook=hook):
                result = self.check_artifact(hook, feature=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(hook.decode("ascii"), result.stderr)

    def test_a_feature_off_artifact_without_hooks_passes(self):
        result = self.check_artifact(b"nothing here", feature=False)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_feature_state_must_match_the_scan(self):
        for feature in (False, True):
            with self.subTest(feature=feature):
                result = self.check_artifact(b"\n".join(self.REQUIRED), feature=feature, artifact_feature=not feature)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("feature build" if feature else "test hooks enabled", result.stderr)

    def test_a_test_profile_is_rejected(self):
        result = self.check_artifact(b"\n".join(self.REQUIRED), test=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("test harness", result.stderr)

    def test_an_unoptimized_artifact_is_rejected(self):
        result = self.check_artifact(b"\n".join(self.REQUIRED), opt_level="0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("release optimization", result.stderr)

    def test_zero_artifacts_are_rejected(self):
        result = self.check_artifact(b"\n".join(self.REQUIRED), artifacts=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no release library", result.stderr)


if __name__ == "__main__":
    unittest.main()
