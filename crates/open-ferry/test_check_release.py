"""Not upstream's: the release positive control must contain every core hook."""
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest


class PositiveControl(unittest.TestCase):
    def check_artifact(self, content, feature=True):
        with tempfile.TemporaryDirectory() as root:
            root = pathlib.Path(root)
            artifact = root / "test-artifact"
            artifact.write_bytes(content)
            events = root / "cargo.json"
            events.write_text("\n".join(json.dumps(event) for event in (
                {"reason": "compiler-artifact", "target": {"name": "open-ferry", "kind": ["bin"]},
                 "profile": {"opt_level": "3", "test": False}, "features": ["blue-green-test"] if feature else [],
                 "executable": str(artifact)},
                {"reason": "build-finished", "success": True},
            )), encoding="utf-8")
            return subprocess.run(
                [sys.executable, "-I", str(pathlib.Path(__file__).with_name("check-release.py")),
                 *(["--self-test"] if feature else []), str(events)],
                capture_output=True, text=True, check=False,
            )

    WORKER = (
        b"events.log", b"test-marker", b"x-open-ferry-test-marker",
        b"handover.written", b"handover_failed", b"hold-load", b"load.before_loaded",
    )
    FLAGS = (b"test-data-dir", b"test-endpoint")
    REQUIRED = (
        b"refresh.after_rotate", b"refresh.before_save", b"persist.before", b"state.fail:",
        b".blue-green-test", *WORKER, *FLAGS,
    )

    def test_each_required_hook_is_required(self):
        for omitted in self.REQUIRED:
            with self.subTest(omitted=omitted):
                # A hook another one contains goes with it: `test-marker` is
                # in the marker header's name.
                kept = (hook for hook in self.REQUIRED if omitted not in hook)
                result = self.check_artifact(b"\n".join(kept))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(omitted.decode("ascii"), result.stderr)

    def test_all_required_hooks_prove_rejection(self):
        result = self.check_artifact(b"\n".join(self.REQUIRED))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("positive control rejected:", result.stdout)

    def test_each_worker_seam_and_test_flag_fails_a_feature_off_artifact(self):
        for text in (b".blue-green-test", *self.WORKER, *self.FLAGS):
            with self.subTest(text=text):
                result = self.check_artifact(b"nothing here\n" + text, feature=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(text.decode("ascii"), result.stderr)


if __name__ == "__main__":
    unittest.main()
