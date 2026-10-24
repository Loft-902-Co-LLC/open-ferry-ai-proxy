"""Not upstream's: the release library scanner and its positive control."""
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest


def archive(*members):
    result = b"!<arch>\n"
    for name, data in members:
        result += (name.ljust(16) + b"0".ljust(12) + b"0".ljust(6) + b"0".ljust(6)
                   + b"100644".ljust(8) + str(len(data)).encode("ascii").ljust(10) + b"`\n")
        result += data + (b"\n" if len(data) % 2 else b"")
    return result


class PositiveControl(unittest.TestCase):
    REQUIRED = (
        b"startup.before_install", b"install.between_renames",
        b"startup.after_prepared_journal", b"startup.recovered_install_sync",
        b"test_endpoint_location",
    )

    def check_artifact(self, content, feature=True, *, artifact_feature=None, test=False, opt_level="3", artifacts=True, raw_archive=False):
        with tempfile.TemporaryDirectory() as root:
            root = pathlib.Path(root)
            artifact = root / "test-artifact.rlib"
            artifact.write_bytes(content if raw_archive else archive((b"code.o/", b"\x64\x86" + content)))
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

    def test_machine_code_objects_with_short_and_long_names_pass(self):
        long_name = b"open_ferry_supervisor.long_name.rcgu.o"
        bsd_name = long_name + b"\0" * 3
        archives = (
            archive((b"code.o/", b"\x64\x86machine code")),
            archive((b"//", b"other-name/\n" + long_name + b"/\n"), (b"/12", b"\x7fELFmachine code")),
            archive((b"//", long_name + b"\0"), (b"/0", b"\x64\x86machine code")),
            archive((b"#1/" + str(len(bsd_name)).encode("ascii"), bsd_name + b"\xcf\xfa\xed\xfemachine code")),
            archive((b"bitcode.o/", b"BC\xc0\xdebitcode"), (b"code.o/", b"\x64\x86machine code")),
        )
        for content in archives:
            with self.subTest(content=content):
                result = self.check_artifact(content, feature=False, raw_archive=True)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_bitcode_only_is_rejected_before_either_string_scan(self):
        content = archive((b"code.o/", b"BC\xc0\xde" + b"\n".join(self.REQUIRED)),
                          (b"other.o/", b"BC\xc0\xdemore bitcode"))
        for feature in (False, True):
            with self.subTest(feature=feature):
                result = self.check_artifact(content, feature=feature, raw_archive=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("build it with CARGO_PROFILE_RELEASE_LTO=off", result.stderr)

    def test_wrapped_bitcode_is_rejected_for_bsd_and_gnu_names(self):
        long_name = b"open_ferry_supervisor.long_name.rcgu.o"
        data = b"\xde\xc0\x17\x0b" + b"\n".join(self.REQUIRED)
        archives = (
            archive((b"//", long_name + b"/\n"), (b"/0", data)),
            archive((b"#1/" + str(len(long_name)).encode("ascii"), long_name + data)),
        )
        for content in archives:
            for feature in (False, True):
                with self.subTest(content=content, feature=feature):
                    result = self.check_artifact(content, feature=feature, raw_archive=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("build it with CARGO_PROFILE_RELEASE_LTO=off", result.stderr)

    def test_metadata_and_symbols_do_not_count_as_machine_code(self):
        archives = (
            archive((b"lib.rmeta/", b"metadata"), (b"/", b"symbols")),
            archive((b"lib.rmeta/", b"metadata"), (b"code.o/", b"BC\xc0\xdebitcode")),
        )
        for content in archives:
            with self.subTest(content=content):
                result = self.check_artifact(content, feature=False, raw_archive=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("no object members" if b"code.o" not in content else "LLVM bitcode", result.stderr)

    def test_invalid_or_truncated_archives_are_rejected(self):
        valid = archive((b"code.o/", b"\x64\x86machine code"))
        archives = (
            b"not an archive", valid[:-2], valid[:8] + valid[8:67] + b"?" + valid[68:],
            archive((b"/0", b"\x64\x86machine code")),
            archive((b"#1/99", b"short")),
        )
        for content in archives:
            with self.subTest(content=content):
                result = self.check_artifact(content, feature=False, raw_archive=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("Traceback", result.stderr)


if __name__ == "__main__":
    unittest.main()
