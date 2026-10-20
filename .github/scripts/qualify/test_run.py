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
        with patch.object(qualification, "Qualification", return_value=self.q), \
                patch.object(sys, "argv", [__file__, str(self.work)]):
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


class PublishedPreconditions(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        (self.work / "diagnostics").mkdir()
        self.private = Ed25519PrivateKey.generate()
        self.key_id = b"12345678"
        self.key = base64.b64encode(b"Ed" + self.key_id + self.private.public_key().public_bytes_raw()).decode()
        for name, value in (("WORK", self.work), ("OUT", self.work / "diagnostics"), ("record", Mock())):
            patcher = patch.object(qualification, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    @staticmethod
    def release(version="1.2.3", **changes):
        return {"id": 1, "tag_name": "v" + version, "draft": False, "prerelease": False,
                "published_at": "2026-10-20T00:00:00Z",
                "assets": [{"id": 2, "name": "SHA256SUMS"},
                           {"id": 3, "name": "SHA256SUMS.minisig"}], **changes}

    def test_stable_versions_are_normalized_and_compared_numerically(self):
        self.assertEqual(qualification.published_versions(" v1.9.9, 1.10.0 "), ("1.9.9", "1.10.0"))
        self.assertEqual(qualification.published_versions("9.9.9,10.0.0"), ("9.9.9", "10.0.0"))
        for value in ("1.2.3", "1.2.3,1.2.4,1.2.5", "1.2.3,1.2.3", "1.10.0,1.9.9",
                      "1.2.3-rc.1,1.2.4", "1.2.3,2.0.0+build", "01.2.3,2.0.0", "-1.2.3,2.0.0"):
            with self.subTest(value=value), self.assertRaisesRegex(RuntimeError, "published"):
                qualification.published_versions(value)

    def test_fault_and_preflight_combinations_are_refused_before_api_or_install(self):
        with patch.object(qualification, "gh_release") as api, \
                patch.object(qualification, "Qualification") as lifecycle:
            for fault in qualification.SIGNATURE_ERRORS:
                with self.subTest(fault=fault), self.assertRaisesRegex(RuntimeError, "refuses.*fault"):
                    qualification.prepare_published("1.2.3,1.2.4", fault)
            with self.assertRaisesRegex(RuntimeError, "refuses preflight_only"):
                qualification.prepare_published("1.2.3,1.2.4", preflight_only=True)
            api.assert_not_called()
            lifecycle.assert_not_called()

    def test_release_metadata_must_be_published_and_stable(self):
        qualification.validate_release(self.release(), "1.2.3")
        for changes in ({"draft": True}, {"prerelease": True}, {"published_at": None},
                        {"tag_name": "v1.2.4"}, {"draft": None}):
            with self.subTest(changes=changes), self.assertRaisesRegex(RuntimeError, "must be published"):
                qualification.validate_release(self.release(**changes), "1.2.3")
        for missing in ("SHA256SUMS", "SHA256SUMS.minisig"):
            release = self.release()
            release["assets"] = [a for a in release["assets"] if a["name"] != missing]
            with self.assertRaisesRegex(RuntimeError, "missing " + missing):
                qualification.validate_release(release, "1.2.3")

    def test_missing_release_is_clear_and_never_constructs_a_lifecycle(self):
        with patch.object(qualification, "command", return_value=Mock(returncode=1)), \
                self.assertRaisesRegex(RuntimeError, "release v9.9.8 unavailable.*release must exist"):
            qualification.gh_release("tags/v9.9.8", "release v9.9.8")
        with patch.object(qualification.shutil, "which", return_value="tool"), \
                patch.object(qualification, "command"), \
                patch.object(qualification, "gh_release", side_effect=RuntimeError("release v9.9.8 unavailable")), \
                patch.object(qualification, "Qualification") as lifecycle, \
                patch.object(sys, "argv", [__file__, str(self.work), "--published", "9.9.8,9.9.9",
                                           "--preconditions-only"]):
            self.assertEqual(qualification.main(), 1)
            lifecycle.assert_not_called()
        result = json.loads((self.work / "diagnostics/result.json").read_text())
        self.assertFalse(result["passed"])
        self.assertEqual(result["phase"], "preconditions")
        self.assertFalse((self.work / "candidates.json").exists())

    def test_b_must_be_repository_latest(self):
        for latest in (self.release(), self.release("1.2.4", id=77)):
            with self.subTest(latest=latest), \
                    patch.object(qualification.shutil, "which", return_value="tool"), \
                    patch.object(qualification, "command"), \
                    patch.object(qualification, "gh_release", side_effect=[self.release(),
                                 self.release("1.2.4", id=4), latest]), \
                    self.assertRaisesRegex(RuntimeError, "latest must be B"):
                qualification.prepare_published("1.2.3,1.2.4")

    def test_checked_out_keys_are_required_and_limited(self):
        path = self.work / "keys.pub"
        path.write_text("# heading\nuntrusted comment: key\n\n  " + self.key + " \n", newline="\n")
        self.assertEqual(qualification.release_keys(path), [self.key])
        for value in ("# none\n", "invalid", (self.key + "\n") * 3):
            path.write_text(value, newline="\n")
            with self.assertRaisesRegex(RuntimeError, "release-keys.pub|public key"):
                qualification.release_keys(path)

    def signature(self, version="1.2.3"):
        data = b"checksum fixture\n"
        (self.work / "SHA256SUMS").write_bytes(data)
        signature = candidate_build.sign(self.private, self.key_id, data, version)
        (self.work / "SHA256SUMS.minisig").write_text(signature, newline="\n")
        return signature

    def test_signature_uses_minisign_and_each_checkout_key(self):
        self.signature()
        with patch.object(qualification, "command", side_effect=[Mock(returncode=1), Mock(returncode=0)]) as run:
            qualification.verify_release_signature(self.work, "1.2.3", ["other-key", self.key])
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args.args[0], ["minisign", "-Vm", self.work / "SHA256SUMS", "-x",
                                               self.work / "SHA256SUMS.minisig", "-P", self.key])
        with patch.object(qualification, "command", return_value=Mock(returncode=1)), \
                self.assertRaisesRegex(RuntimeError, "signature does not verify"):
            qualification.verify_release_signature(self.work, "1.2.3", [self.key])

    def test_wrong_signed_comment_and_legacy_signature_are_refused(self):
        self.signature("1.2.4")
        with patch.object(qualification, "command") as run, \
                self.assertRaisesRegex(RuntimeError, "trusted comment"):
            qualification.verify_release_signature(self.work, "1.2.3", [self.key])
        run.assert_not_called()
        text = self.signature().splitlines()
        text[1] = base64.b64encode(b"Ed" + base64.b64decode(text[1])[2:]).decode()
        (self.work / "SHA256SUMS.minisig").write_text("\n".join(text), newline="\n")
        with self.assertRaisesRegex(RuntimeError, "prehashed"):
            qualification.verify_release_signature(self.work, "1.2.3", [self.key])

    def test_all_preconditions_must_pass_before_manifest_is_written(self):
        a, b = self.release(), self.release("1.2.4", id=4)
        with patch.object(qualification.shutil, "which", return_value="tool"), \
                patch.object(qualification, "command"), \
                patch.object(qualification, "gh_release", side_effect=[a, b, b]), \
                patch.object(qualification, "release_keys", return_value=[self.key]), \
                patch.object(qualification.subprocess, "run") as download, \
                patch.object(qualification, "verify_release_signature") as verify:
            result = qualification.prepare_published("1.2.3,1.2.4", form="machine")
        self.assertEqual(result["mode"], "published")
        self.assertEqual(result["form"], "machine")
        self.assertEqual(download.call_count, 4)
        self.assertEqual(verify.call_count, 2)
        self.assertTrue((self.work / "candidates.json").exists())

    def test_timer_deadline_and_unrequested_switch_are_red(self):
        q = object.__new__(qualification.Qualification)
        q.a, q.b, q.service_started = "1.2.3", "1.2.4", 10
        q.candidates = {"mode": "published"}
        q.api = Mock()
        with patch.object(qualification.time, "monotonic", return_value=250), \
                self.assertRaisesRegex(RuntimeError, "exceeded 240s"):
            q.before_published_check()
        q.api.assert_not_called()
        q.api.return_value = {"running_version": q.b}
        with patch.object(qualification.time, "monotonic", return_value=20), \
                self.assertRaisesRegex(RuntimeError, "before the requested check-now"):
            q.before_published_check()

    def test_published_lifecycle_uses_no_mirror_and_keeps_all_load_phases(self):
        q = object.__new__(qualification.Qualification)
        q.a, q.b, q.os = "1.2.3", "1.2.4", "Linux"
        q.candidates = {"mode": "published", "form": "machine", "fault": "none"}
        q.port, q.client = 12345, "unused-client"
        q.exe, q.config, q.data = self.work / "exe", self.work / "config.yaml", self.work / "data"
        q.env, q.load, q.installed_service, q.fault_confirmed = {}, None, False, False
        q.prerequisites, q.install, q.check_after_flip, q.until = Mock(), Mock(), Mock(), Mock()
        q.logs = lambda: qualification.write(qualification.OUT / "supervisor-worker.log",
                                              "automatic switch accepted: 1.2.4\n")
        def uninstall():
            if q.load:
                q.load.finish()
                q.load = None
        q.uninstall = Mock(side_effect=uninstall)
        with patch.object(qualification, "ThreadingHTTPServer") as mirror, \
                patch.object(qualification, "Load") as load, \
                patch.object(qualification, "command", side_effect=[Mock(stdout="open-ferry 1.2.4"),
                             Mock(stdout="rollback complete"), Mock(stdout="open-ferry 1.2.3")]):
            load.return_value.snapshot.return_value = {"counts": {"baseline.requests": 4,
                                                       "auto.requests": 4, "rollback.requests": 4}}
            q.run()
            mirror.assert_not_called()
            q.install.assert_called_once_with(None)
            q.check_after_flip.assert_called_once()
            load.return_value.baseline_ready.wait.assert_called_once_with(timeout=30)
            self.assertEqual([call.args[0] for call in load.return_value.set_phase.call_args_list],
                             ["auto", "rollback"])
            load.return_value.finish.assert_called_once()
        self.assertEqual(q.service_args("uninstall"), [q.exe, "service", "uninstall", "-system"])


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
