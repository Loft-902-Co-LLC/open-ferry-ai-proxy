"""Not upstream's: the release positive control must contain every core hook."""
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest


class PositiveControl(unittest.TestCase):
    def check_artifact(self, content):
        with tempfile.TemporaryDirectory() as root:
            root = pathlib.Path(root)
            artifact = root / "test-artifact"
            artifact.write_bytes(content)
            events = root / "cargo.json"
            events.write_text("\n".join(json.dumps(event) for event in (
                {"reason": "compiler-artifact", "target": {"name": "open-ferry", "kind": ["bin"]},
                 "profile": {"opt_level": "3", "test": False}, "features": ["blue-green-test"],
                 "executable": str(artifact)},
                {"reason": "build-finished", "success": True},
            )), encoding="utf-8")
            return subprocess.run(
                [sys.executable, "-I", str(pathlib.Path(__file__).with_name("check-release.py")),
                 "--self-test", str(events)], capture_output=True, text=True, check=False,
            )

    def test_each_required_hook_is_required(self):
        required = (b"refresh.after_rotate", b"refresh.before_save", b"persist.before", b"state.fail:")
        for omitted in required:
            with self.subTest(omitted=omitted):
                result = self.check_artifact(b"\n".join(hook for hook in required if hook != omitted))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(omitted.decode("ascii"), result.stderr)

    def test_all_required_hooks_prove_rejection(self):
        result = self.check_artifact(b"refresh.after_rotate\nrefresh.before_save\npersist.before\nstate.fail:")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("positive control rejected:", result.stdout)


if __name__ == "__main__":
    unittest.main()
