"""Regression checks for qualification races, error retention and fault controls."""

from collections import Counter
from contextlib import redirect_stdout
import base64
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import traceback
import unittest
from unittest.mock import Mock, patch

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey


sys.dont_write_bytecode = True
with patch.object(sys, "argv", [__file__, tempfile.gettempdir()]):
    spec = importlib.util.spec_from_file_location("qualification", Path(__file__).with_name("run.py"))
    qualification = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(qualification)
    spec = importlib.util.spec_from_file_location("candidate_build", Path(__file__).with_name("build.py"))
    candidate_build = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(candidate_build)
real_record = qualification.record


class QualificationControls(unittest.TestCase):
    fault = "bad-signature"

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.out = self.work / "diagnostics"
        self.out.mkdir()
        for name, value in (("WORK", self.work), ("OUT", self.out), ("record", Mock()),
                            ("ThreadingHTTPServer", Mock()), ("Load", Mock())):
            patcher = patch.object(qualification, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        patcher = patch.object(qualification.time, "sleep")
        patcher.start()
        self.addCleanup(patcher.stop)
        qualification.Mirror.requests = Counter({"/latest/download/SHA256SUMS.minisig": 1})
        self.q = object.__new__(qualification.Qualification)
        self.q.os = "Linux"
        self.q.a, self.q.b = "0.0.900", "0.0.901"
        self.q.candidates = {"versions": [self.q.a, self.q.b], "fault": self.fault,
                             "target": "x86_64-unknown-linux-gnu"}
        self.q.port, self.q.client = 12345, "unused-test-client"
        self.q.data = self.work / "data"
        self.q.load = None
        self.q.installed_service = False
        self.q.fault_confirmed = False
        self.q.prerequisites = Mock()
        self.q.install = Mock()
        self.q.logs = Mock()
        self.q.uninstall = Mock(side_effect=self.uninstall)
        self.q.api = Mock(side_effect=[{"check": "started"}, self.rejected_status()])
        self.q.until = Mock(side_effect=qualification.UpdateFailure(
            "update failed: " + qualification.SIGNATURE_ERRORS[self.fault]))
        self.load = qualification.Load.return_value
        self.load.snapshot.return_value = {"counts": {"baseline.requests": 4, "auto.requests": 4}}

    def uninstall(self):
        if self.q.load:
            load, self.q.load = self.q.load, None
            load.finish()
        self.q.installed_service = False

    def rejected_status(self):
        return {"last_error": qualification.SIGNATURE_ERRORS[self.fault], "last_result": "error",
                "checking": False, "running_version": self.q.a, "installed_version": self.q.a,
                "supervisor": {"form": "supervised", "running_version": self.q.a, "phase": "idle"}}

    def run_main(self):
        with patch.object(qualification, "Qualification", return_value=self.q):
            code = qualification.main()
        return code, json.loads((self.out / "result.json").read_text(encoding="utf-8"))

    def test_exact_signature_rejection_is_a_green_control(self):
        code, result = self.run_main()
        self.assertEqual(code, 0)
        self.assertTrue(result["passed"])
        self.assertTrue(result["fault_confirmed"])
        self.assertEqual(result["expected_failure"], qualification.SIGNATURE_ERRORS[self.fault])
        self.q.uninstall.assert_called_once()

    def test_unrelated_failure_is_red_in_fault_mode(self):
        self.q.until.side_effect = RuntimeError("service crashed")
        code, result = self.run_main()
        self.assertEqual(code, 1)
        self.assertFalse(result["passed"])
        self.assertEqual(result["error"], "RuntimeError: service crashed")

    def test_wrong_updater_error_is_red(self):
        self.q.until.side_effect = qualification.UpdateFailure("update failed: archive missing")
        code, result = self.run_main()
        self.assertEqual(code, 1)
        self.assertFalse(result["passed"])

    def test_other_signature_fault_is_red(self):
        other = "untrusted-key" if self.fault == "bad-signature" else "bad-signature"
        self.q.until.side_effect = qualification.UpdateFailure(
            "update failed: " + qualification.SIGNATURE_ERRORS[other])
        code, result = self.run_main()
        self.assertEqual(code, 1)
        self.assertFalse(result["passed"])

    def test_changed_a_staged_b_or_switch_is_red(self):
        for change in ({"running_version": self.q.b}, {"staged_version": self.q.b},
                       {"supervisor": {"last_switch": {"to": self.q.b}}}):
            with self.subTest(change=change):
                self.q.api.side_effect = [{"check": "started"}, self.rejected_status() | change]
                code, result = self.run_main()
                self.assertEqual(code, 1)
                self.assertFalse(result["passed"])

    def test_empty_load_is_red(self):
        self.load.snapshot.return_value = {"counts": {"baseline.requests": 4, "auto.requests": 0}}
        code, result = self.run_main()
        self.assertEqual(code, 1)
        self.assertIn("no requests covered auto", result["error"])

    def test_fetching_untrusted_b_archive_is_red(self):
        qualification.Mirror.requests[
            f"/download/v{self.q.b}/open-ferry-{self.q.b}-{self.q.candidates['target']}.tar.gz"] = 1
        code, result = self.run_main()
        self.assertEqual(code, 1)
        self.assertIn("untrusted B archive was requested", result["error"])

    def test_unexpected_switch_is_red(self):
        self.q.until.side_effect = None
        code, result = self.run_main()
        self.assertEqual(code, 1)
        self.assertIn("unexpectedly switched to B", result["error"])

    def test_load_cleanup_failure_cannot_turn_control_green(self):
        self.load.finish.side_effect = RuntimeError("load failed")
        code, result = self.run_main()
        self.assertEqual(code, 1)
        self.assertFalse(result["passed"])
        self.assertEqual(result["error"], "RuntimeError: load failed")

    def test_uninstall_failure_cannot_turn_control_green(self):
        self.q.uninstall.side_effect = RuntimeError("uninstall failed")
        code, result = self.run_main()
        self.assertEqual(code, 1)
        self.assertFalse(result["passed"])
        self.assertIn("uninstall failed", result["error"])

    def test_malformed_yaml_never_exposes_an_unregistered_key(self):
        fake_key = "fake-key-must-never-appear-74931"
        self.q.config = self.work / "config.yaml"
        self.q.config.write_text('management: [' + fake_key + ': "unterminated\n',
                                 encoding="utf-8", newline="\n")
        self.q.env = {}
        install = qualification.Qualification.install
        self.q.install.side_effect = lambda base: install(self.q, base)
        output = io.StringIO()
        with patch.object(qualification, "SECRETS", []), patch.object(qualification, "command"), \
                patch.object(qualification, "record", real_record), redirect_stdout(output):
            try:
                self.q.install("http://unused")
            except RuntimeError as error:
                self.assertEqual(str(error), "generated config is not valid YAML")
                self.assertNotIn(fake_key, "".join(traceback.format_exception(error)))
            else:
                self.fail("malformed YAML was accepted")
            code, result = self.run_main()
        self.assertEqual(code, 1)
        self.assertEqual(result["error"], "RuntimeError: generated config is not valid YAML")
        self.assertNotIn(fake_key, output.getvalue())
        for path in self.out.iterdir():
            self.assertNotIn(fake_key, path.read_text(encoding="utf-8"))

    def test_first_error_and_later_cleanup_errors_survive(self):
        self.q.load = self.load
        self.q.install.side_effect = ValueError("first failure")
        self.load.finish.side_effect = RuntimeError("load cleanup failed")
        self.q.logs.side_effect = OSError("log export failed")
        code, result = self.run_main()
        self.assertEqual(code, 1)
        self.assertEqual(result["error"], "ValueError: first failure")
        self.assertEqual([item["phase"] for item in result["errors"]],
                         ["lifecycle", "load cleanup", "diagnostics"])
        self.assertIn("load cleanup failed", result["errors"][1]["error"])
        qualification.ThreadingHTTPServer.return_value.shutdown.assert_called_once()

    def test_running_check_retries_until_post_flip_check_starts(self):
        self.q.load = self.load
        self.q.api.side_effect = [{"check": "running"}, {"check": "running"}, {"check": "started"}]
        self.q.check_after_flip()
        self.assertEqual(self.q.api.call_count, 3)

    def test_running_check_alone_times_out(self):
        self.q.load = self.load
        self.q.api.side_effect = None
        self.q.api.return_value = {"check": "running"}
        with patch.object(qualification.time, "monotonic", side_effect=[0, 1, 2, 91]):
            with self.assertRaisesRegex(RuntimeError, "timed out starting a new check"):
                self.q.check_after_flip()


class UntrustedKeyControls(QualificationControls):
    fault = "untrusted-key"


class CandidateSignatures(unittest.TestCase):
    def test_fault_modes_select_the_intended_verification_path(self):
        trusted, alternate = Ed25519PrivateKey.generate(), Ed25519PrivateKey.generate()
        key_id, data = os.urandom(8), b"qualification checksum fixture\n"
        for fault in ("none", "untrusted-key", "bad-signature"):
            for version in candidate_build.VERSIONS:
                with self.subTest(fault=fault, version=version), patch.object(
                        candidate_build.Ed25519PrivateKey, "generate", return_value=alternate):
                    text = candidate_build.candidate_signature(trusted, key_id, data, version, fault)
                    lines = text.splitlines()
                    packet, global_signature = base64.b64decode(lines[1]), base64.b64decode(lines[3])
                    self.assertEqual(packet[:2], b"ED")
                    self.assertEqual(len(packet), 74)
                    self.assertEqual(len(global_signature), 64)
                    comment = lines[2].removeprefix("trusted comment: ").encode("ascii")
                    rejected = fault != "none" and version == candidate_build.VERSIONS[1]
                    signer = alternate if rejected else trusted
                    signer.public_key().verify(packet[10:], hashlib.blake2b(data).digest())
                    signer.public_key().verify(global_signature, packet[10:] + comment)
                    if rejected:
                        with self.assertRaises(InvalidSignature):
                            trusted.public_key().verify(packet[10:], hashlib.blake2b(data).digest())
                    if rejected and fault == "untrusted-key":
                        self.assertNotEqual(packet[2:10], key_id)
                    else:
                        self.assertEqual(packet[2:10], key_id)


class ReservedPort(unittest.TestCase):
    def test_dynamic_selection_refuses_the_reserved_port(self):
        with patch.object(qualification.socket, "socket") as socket:
            socket.return_value.__enter__.return_value.getsockname.return_value = ("127.0.0.1", 8317)
            with self.assertRaisesRegex(RuntimeError, "reserved development port"):
                qualification.free_port()

    def test_windows_load_never_repeats_a_source_address_or_relaxes_timeout(self):
        load = object.__new__(qualification.Load)
        load.port = 54321
        load.lock = qualification.threading.Lock()
        load.next_source = 2
        with patch.object(qualification.platform, "system", return_value="Windows"), \
                patch.object(qualification.http.client, "HTTPConnection") as connection:
            for index in range(4):
                load.connection()
                connection.assert_called_with("127.0.0.1", 54321, timeout=5,
                                              source_address=(f"127.0.0.{index + 2}", 0))
            load.next_source = 255
            load.connection()
            connection.assert_called_with("127.0.0.1", 54321, timeout=5,
                                          source_address=("127.0.0.255", 0))
            load.connection()
            connection.assert_called_with("127.0.0.1", 54321, timeout=5,
                                          source_address=("127.0.1.0", 0))

    def test_posix_load_connection_is_unchanged(self):
        load = object.__new__(qualification.Load)
        load.port = 54321
        with patch.object(qualification.platform, "system", return_value="Linux"), \
                patch.object(qualification.http.client, "HTTPConnection") as connection:
            load.connection()
            connection.assert_called_once_with("127.0.0.1", 54321, timeout=5)


@unittest.skipUnless(os.name == "nt", "Windows desktop snapshot and cleanup")
class WindowsRestoration(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location("windows_local", Path(__file__).with_name("windows-local.py"))
        self.local = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.local)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in ("DATA", "INSTALL", "CONFIG"):
            setattr(self.local, name, self.root / name.lower())

    def test_elevated_session_is_refused_before_snapshot(self):
        local = self.local
        with patch.object(local.ctypes.windll.shell32, "IsUserAnAdmin", return_value=True), \
                patch.object(local, "snapshot") as snapshot:
            with self.assertRaisesRegex(RuntimeError, "elevated token"):
                local.preflight()
            snapshot.assert_not_called()

    def test_existing_install_is_refused_before_snapshot(self):
        local = self.local
        local.INSTALL.mkdir()
        with patch.object(local.ctypes.windll.shell32, "IsUserAnAdmin", return_value=False), \
                patch.object(local, "snapshot") as snapshot:
            with self.assertRaisesRegex(RuntimeError, "existing open-ferry installation"):
                local.preflight()
            snapshot.assert_not_called()

    def test_cleanup_preserves_existing_claude_cli_and_removes_only_new_data(self):
        local = self.local
        keep = local.DATA / "claude-cli/keep.txt"
        keep.parent.mkdir(parents=True)
        keep.write_text("unrelated user data", encoding="utf-8")
        before = {"data": local.tree(local.DATA), "install": None, "config": None, "tasks": []}
        (local.DATA / "versions").mkdir()
        (local.DATA / "update-state.json").write_text("{}", encoding="utf-8")
        local.INSTALL.mkdir()
        with patch.object(local, "snapshot", return_value=before), patch.object(local, "installed_processes", return_value=[]):
            local.restore(before, self.root / "evidence")
        self.assertEqual(keep.read_text(encoding="utf-8"), "unrelated user data")
        self.assertEqual(local.tree(local.DATA), before["data"])
        self.assertFalse(local.INSTALL.exists())
        result = json.loads((self.root / "evidence/restoration.json").read_text())
        self.assertTrue(result["matches"])
        self.assertFalse(any("claude-cli" in path for path in result["removed"]))

    def test_running_installed_process_prevents_file_removal(self):
        local = self.local
        local.INSTALL.mkdir()
        before = {"data": None, "install": None, "config": None, "tasks": []}
        with patch.object(local, "snapshot", return_value=before), patch.object(local, "installed_processes", return_value=["owned.exe"]):
            with self.assertRaisesRegex(RuntimeError, "left a process or task"):
                local.restore(before, self.root / "evidence")
        self.assertTrue(local.INSTALL.exists())


if __name__ == "__main__":
    unittest.main()
