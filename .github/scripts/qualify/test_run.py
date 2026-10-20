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
import shutil
import subprocess
import sys
import tempfile
import traceback
import unittest
from unittest.mock import Mock, patch
from types import SimpleNamespace

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
                           {"id": 3, "name": "SHA256SUMS.minisig"},
                           {"id": 5, "name": "install.sh"},
                           {"id": 6, "name": "install.ps1"}], **changes}

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
        with patch.object(qualification, "command", side_effect=[Mock(returncode=1),
                          Mock(returncode=0, stdout="open-ferry 1.2.3 SHA256SUMS\n")]) as run:
            qualification.verify_release_signature(self.work, "1.2.3", ["other-key", self.key])
        self.assertEqual(run.call_count, 2)
        self.assertTrue(all(call.kwargs == {"check": False, "sensitive": True}
                            for call in run.call_args_list), "public-key arguments must stay out of diagnostics")
        self.assertEqual(run.call_args.args[0], ["minisign", "-V", "-H", "-Q", "-m", self.work / "SHA256SUMS", "-x",
                                               self.work / "SHA256SUMS.minisig", "-P", self.key])
        with patch.object(qualification, "command", return_value=Mock(returncode=1)), \
                self.assertRaisesRegex(RuntimeError, "signature does not verify"):
            qualification.verify_release_signature(self.work, "1.2.3", [self.key])

    def test_wrong_signed_comment_and_legacy_signature_are_refused(self):
        self.signature("1.2.4")
        with patch.object(qualification, "command", return_value=Mock(
                returncode=0, stdout="open-ferry 1.2.4 SHA256SUMS\n")) as run, \
                self.assertRaisesRegex(RuntimeError, "trusted comment"):
            qualification.verify_release_signature(self.work, "1.2.3", [self.key])
        run.assert_called_once()
        text = self.signature().splitlines()
        text[1] = base64.b64encode(b"Ed" + base64.b64decode(text[1])[2:]).decode()
        (self.work / "SHA256SUMS.minisig").write_text("\n".join(text), newline="\n")
        with self.assertRaisesRegex(RuntimeError, "prehashed"):
            qualification.verify_release_signature(self.work, "1.2.3", [self.key])

    def test_all_preconditions_must_pass_before_manifest_is_written(self):
        a, b = self.release(), self.release("1.2.4", id=4)
        script = b"released installer fixture\n"
        sums = "".join(f"{hashlib.sha256(script).hexdigest()}  {name}\n"
                       for name in ("install.sh", "install.ps1")).encode()
        def download_asset(args, **kwargs):
            name = next(asset["name"] for asset in a["assets"] if str(asset["id"]) == args[-1].split("/")[-1])
            kwargs["stdout"].write(sums if name == "SHA256SUMS" else script)
        with patch.object(qualification.shutil, "which", return_value="tool"), \
                patch.object(qualification, "command"), \
                patch.object(qualification, "gh_release", side_effect=[a, b, b]), \
                patch.object(qualification, "release_keys", return_value=[self.key]), \
                patch.object(qualification.subprocess, "run", side_effect=download_asset) as download, \
                patch.object(qualification, "verify_release_signature") as verify, \
                patch.object(qualification, "verify_release_installers", wraps=qualification.verify_release_installers) as installers:
            result = qualification.prepare_published("1.2.3,1.2.4", form="machine")
        self.assertEqual(result["mode"], "published")
        self.assertEqual(result["form"], "machine")
        self.assertEqual(download.call_count, 6)
        self.assertEqual(verify.call_count, 2)
        installers.assert_called_once_with(self.work / "published/1.2.3", "1.2.3")
        self.assertTrue((self.work / "candidates.json").exists())

    def test_both_released_installers_must_exist_and_match_signed_checksums(self):
        scripts = {"install.sh": b"shell installer\n", "install.ps1": b"powershell installer\n"}
        sums = "".join(f"{hashlib.sha256(data).hexdigest()}  {name}\n" for name, data in scripts.items())
        (self.work / "SHA256SUMS").write_text(sums, newline="\n")
        for name, data in scripts.items():
            (self.work / name).write_bytes(data)
        qualification.verify_release_installers(self.work, "1.2.3")
        for name, data in scripts.items():
            with self.subTest(name=name):
                release = self.release()
                release["assets"] = [a for a in release["assets"] if a["name"] != name]
                with self.assertRaisesRegex(RuntimeError, "missing " + qualification.re.escape(name)):
                    qualification.validate_release(release, "1.2.3", installers=True)
                (self.work / name).unlink()
                with self.assertRaisesRegex(RuntimeError, "missing signed installer"):
                    qualification.verify_release_installers(self.work, "1.2.3")
                (self.work / name).write_bytes(data + b"tampered")
                with self.assertRaisesRegex(RuntimeError, "SHA-256 differs"):
                    qualification.verify_release_installers(self.work, "1.2.3")
                (self.work / name).write_bytes(data)
                (self.work / "SHA256SUMS").write_text("\n".join(line for line in sums.splitlines()
                                                         if not line.endswith(name)) + "\n", newline="\n")
                with self.assertRaisesRegex(RuntimeError, "missing signed installer"):
                    qualification.verify_release_installers(self.work, "1.2.3")
                (self.work / "SHA256SUMS").write_text(sums, newline="\n")

    def test_real_minisign_verifies_comment_key_and_file(self):
        if not shutil.which("minisign"):
            if os.environ.get("QUALIFY_REQUIRE_MINISIGN") == "1":
                self.fail("QUALIFY_REQUIRE_MINISIGN=1 but minisign is not installed")
            self.skipTest("minisign is not installed")
        for name in ("fixture", "other"):
            subprocess.run(["minisign", "-G", "-W", "-p", str(self.work / (name + ".pub")),
                            "-s", str(self.work / (name + ".key"))], check=True,
                           stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=30)
        keys = qualification.release_keys(self.work / "fixture.pub")
        (self.work / "SHA256SUMS").write_bytes(b"test checksum fixture\n")
        def sign(comment):
            subprocess.run(["minisign", "-S", "-s", str(self.work / "fixture.key"),
                            "-m", str(self.work / "SHA256SUMS"), "-t", comment],
                           check=True, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                           stderr=subprocess.STDOUT, timeout=30)
        sign("open-ferry 1.2.3 SHA256SUMS")
        qualification.verify_release_signature(self.work, "1.2.3", keys)
        with self.assertRaisesRegex(RuntimeError, "signature does not verify"):
            qualification.verify_release_signature(self.work, "1.2.3", qualification.release_keys(self.work / "other.pub"))
        sign("open-ferry 1.2.4 SHA256SUMS")
        with self.assertRaisesRegex(RuntimeError, "trusted comment"):
            qualification.verify_release_signature(self.work, "1.2.3", keys)
        sign("open-ferry 1.2.3 SHA256SUMS")
        (self.work / "SHA256SUMS").write_bytes(b"tampered\n")
        with self.assertRaisesRegex(RuntimeError, "signature does not verify"):
            qualification.verify_release_signature(self.work, "1.2.3", keys)

    def test_ci_scopes_token_and_requires_real_minisign_before_preconditions(self):
        workflow = Path(__file__).resolve().parents[3] / ".github/workflows/qualify.yml"
        job = qualification.yaml.safe_load(workflow.read_text(encoding="utf-8"))["jobs"]["qualify"]
        self.assertNotIn("GH_TOKEN", job["env"])
        steps = job["steps"]
        token_steps = [step for step in steps if "GH_TOKEN" in step.get("env", {})]
        self.assertEqual(len(token_steps), 2)
        self.assertTrue(all(step["if"] == "inputs.published != ''" for step in token_steps))
        real_test = next(i for i, step in enumerate(steps)
                         if step.get("env", {}).get("QUALIFY_REQUIRE_MINISIGN") == "1")
        preconditions = next(i for i, step in enumerate(steps) if step.get("name", "").startswith("Published preconditions"))
        self.assertLess(real_test, preconditions)
        self.assertIn("test_run.py", steps[real_test]["run"])
        published = next(step for step in steps if step.get("name") == "Qualify the published service")
        self.assertIn("--preserve-env=GH_TOKEN,QUALIFY_FAULT", published["run"])

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

    def published_check(self):
        q = object.__new__(qualification.Qualification)
        q.a, q.b, q.service_started = "1.2.3", "1.2.4", 0
        q.candidates = {"mode": "published", "fault": "none"}
        q.load = Mock()
        q.logs = Mock(side_effect=lambda: qualification.write(self.work / "diagnostics/supervisor-worker.log", ""))
        status = {"running_version": q.a, "installed_version": q.a,
                  "next_check": "1970-01-01T00:04:10Z",
                  "supervisor": {"form": "supervised", "running_version": q.a, "phase": "idle"}}
        return q, status

    def test_check_started_must_still_meet_240_second_budget(self):
        q, status = self.published_check()
        clock = [200]
        def api(suffix="", method="GET"):
            if suffix == "/check":
                clock[0] = 241
                return {"check": "started"}
            return status
        q.api = Mock(side_effect=api)
        q.await_published_acceptance = Mock()
        with patch.object(qualification.time, "monotonic", side_effect=lambda: clock[0]), \
                patch.object(qualification.time, "time", return_value=200), \
                self.assertRaisesRegex(RuntimeError, "exceeded 240s"):
            q.check_after_flip()
        q.await_published_acceptance.assert_not_called()

    def test_on_time_check_now_acceptance_passes_and_records_next_check(self):
        q, status = self.published_check()
        accepted = {"supervisor": {"form": "supervised", "phase": "loading",
                                   "switch": {"how": "update", "to": q.b}}}
        q.api = Mock(side_effect=[status, {"check": "started"}, accepted])
        with patch.object(qualification.time, "monotonic", return_value=200), \
                patch.object(qualification.time, "time", return_value=200):
            q.check_after_flip()
        self.assertEqual(q.timer_deadline, 249)
        snapshot = json.loads((self.work / "diagnostics/before-check-now.json").read_text())
        self.assertEqual(snapshot["status"]["next_check"], status["next_check"])
        self.assertTrue((self.work / "diagnostics/check-now-switch-accepted-before-timer.json").exists())
        q.api.assert_any_call("/check", "POST")

    def test_late_acceptance_fails_each_deadline_even_if_poll_began_in_time(self):
        for deadline in (230, 245, 300):
            with self.subTest(deadline=deadline):
                q, _ = self.published_check()
                q.timer_deadline = deadline if deadline != 300 else 999
                clock = [200 if deadline == 245 else deadline - 10]
                def api():
                    clock[0] = deadline
                    return {"supervisor": {"form": "supervised", "phase": "loading"}}
                q.api = Mock(side_effect=api)
                with patch.object(qualification.time, "monotonic", side_effect=lambda: clock[0]), \
                        self.assertRaisesRegex(RuntimeError, "acceptance missed"):
                    q.await_published_acceptance()

    def test_refused_deferred_uncertain_or_ended_automatic_switch_is_red(self):
        messages = ("automatic switch could not start", "automatic switch deferred: another update is running",
                    "automatic switch deferred: no newer version is staged", "automatic switch deferred: error",
                    "no answer from the supervisor; it may have taken the automatic switch; see its status",
                    "automatic switch ended")
        for message in messages:
            with self.subTest(message=message):
                q, _ = self.published_check()
                q.timer_deadline = 249
                q.api = Mock(return_value={"supervisor": {"form": "supervised", "phase": "loading"}})
                q.logs.side_effect = lambda: qualification.write(self.work / "diagnostics/supervisor-worker.log",
                                                                 message + "\nautomatic switch accepted: 1.2.4\n")
                with patch.object(qualification.time, "monotonic", return_value=200), \
                        self.assertRaisesRegex(RuntimeError, "did not complete its automatic switch"):
                    q.await_published_acceptance()

    def test_staged_b_and_stale_or_idle_status_do_not_prove_acceptance(self):
        for supervisor in ({"phase": "idle"}, {"phase": "unknown"}, {"phase": "loading", "stale": True},
                           {"phase": "degraded"}, {"phase": "stopping"}, {"phase": "loading", "error": "unreachable"}):
            q, _ = self.published_check()
            q.timer_deadline = 249
            q.api = Mock(return_value={"staged_version": q.b,
                                      "supervisor": {"form": "supervised", **supervisor}})
            clock = [200]
            with self.subTest(supervisor=supervisor), \
                    patch.object(qualification.time, "monotonic", side_effect=lambda: clock[0]), \
                    patch.object(qualification.time, "sleep", side_effect=lambda _: clock.__setitem__(0, 246)), \
                    self.assertRaisesRegex(RuntimeError, "timed out"):
                q.await_published_acceptance()

    def install_fixture(self, system="Linux", form="user"):
        q = object.__new__(qualification.Qualification)
        q.a, q.b, q.os = "1.2.3", "1.2.4", system
        q.candidates = {"mode": "published", "form": form, "fault": "none"}
        q.env, q.port = {"GH_TOKEN": "fixture-token"}, 12345
        q.config, q.exe, q.data = self.work / "config.yaml", self.work / "exe", self.work / "data"
        q.data.mkdir(exist_ok=True)
        q.config.write_text("management: {secret-key: fixture-management}\naccess: {api-keys: [fixture-client]}\n"
                            "server: {}\noauth: {}\n", newline="\n")
        (q.data / "install-receipt.json").write_text(json.dumps({"version": q.a, "binary": str(q.exe)}), newline="\n")
        q.until = Mock(return_value={"trusts_release_key": True, "can_update_itself": True, "mode": "auto"})
        return q

    def test_published_constructor_strips_install_and_update_overrides(self):
        (self.work / "candidates.json").write_text(json.dumps({"mode": "published", "fault": "none",
                                                              "versions": ["1.2.3", "1.2.4"]}), newline="\n")
        overrides = {name: "injected" for name in ("OPEN_FERRY_INSTALL_BASE_URL", "OPEN_FERRY_UPDATE_BASE_URL",
                     "OPEN_FERRY_INSTALL_GH", "OPEN_FERRY_INSTALL_SYSTEM", "OPEN_FERRY_SELF_UPDATE")}
        with patch.dict(os.environ, overrides, clear=True), \
                patch.object(qualification.platform, "system", return_value="Linux"), \
                patch.object(qualification.Path, "home", return_value=self.work), \
                patch.object(qualification, "free_port", return_value=12345):
            q = qualification.Qualification()
        self.assertFalse(set(overrides) & set(q.env))

    def test_install_uses_as_published_script_explicit_version_and_attestation(self):
        for system in ("Linux", "Windows"):
            with self.subTest(system=system):
                q = self.install_fixture(system, "machine")
                q.prepare_root_auth, q.remove_root_auth = Mock(), Mock()
                def run(args, **kwargs):
                    if args[0] in ("sh", "powershell.exe"):
                        installer = self.work / "published" / q.a / ("install.ps1" if system == "Windows" else "install.sh")
                        self.assertIn(str(installer), args)
                        flag = "-Version" if system == "Windows" else "--version"
                        self.assertEqual(args[args.index(flag) + 1], q.a)
                        self.assertIn("-System" if system == "Windows" else "--system", args)
                        self.assertNotIn("-NoAttestation", args)
                        self.assertNotIn("--no-attestation", args)
                        self.assertNotIn("OPEN_FERRY_INSTALL_BASE_URL", kwargs["env"])
                        self.assertNotIn("OPEN_FERRY_UPDATE_BASE_URL", kwargs["env"])
                        return Mock(stdout="Checking the build provenance attestation with gh", returncode=0)
                    return Mock(stdout=f"open-ferry {q.a}" if args[-1] == "--version"
                                else "LocalSystem -supervise -data-dir", returncode=0)
                with patch.object(qualification, "command", side_effect=run):
                    q.install(None)
                self.assertFalse((q.config.parent / ".env").exists())
                self.assertTrue(json.loads((self.work / "diagnostics/attestation.json").read_text())["verified"])
                self.assertTrue(qualification.yaml.safe_load(q.config.read_text())["observability"]["logs"]["debug"])

    def test_successful_installer_without_attestation_is_red_before_service_start(self):
        for output in ("installed successfully", "Checking the build provenance attestation with gh; wasn't checked"):
            q = self.install_fixture()
            with self.subTest(output=output), patch.object(qualification, "command", return_value=Mock(stdout=output)), \
                    self.assertRaisesRegex(RuntimeError, "did not perform its required gh attestation"):
                q.install(None)
            q.until.assert_not_called()

    def test_root_credentials_removed_after_install_before_service_and_preserve_existing_config(self):
        for existing in ("new", "directory", "config"):
            with self.subTest(existing=existing):
                root = self.work / existing
                root.mkdir()
                auth = root / ".config/gh/hosts.yml"
                config = auth.parent / "config.yml"
                if existing != "new":
                    config.parent.mkdir(parents=True)
                if existing == "config":
                    config.write_text("original config\n", newline="\n")
                q = self.install_fixture(form="machine")
                events = []
                remove = q.remove_root_auth
                def removed():
                    remove()
                    events.append("removed")
                    self.assertFalse(auth.exists())
                q.remove_root_auth = removed
                def run(args, **kwargs):
                    if args[0] == "/usr/bin/env":
                        self.assertEqual(qualification.yaml.safe_load(auth.read_text())["github.com"]["oauth_token"], "fixture-token")
                        events.append("written")
                    if args[0] == "sh":
                        self.assertTrue(auth.exists())
                        events.append("install")
                        if existing != "config":
                            config.write_text("gh generated config\n", newline="\n")
                        return Mock(stdout="Checking the build provenance attestation with gh")
                    if "service" in args and "install" in args:
                        events.append("lifecycle")
                        self.assertFalse(auth.exists())
                    return Mock(stdout=f"open-ferry {q.a}" if args[-1] == "--version" else "-supervise -data-dir")
                with patch.dict(sys.modules, {"pwd": SimpleNamespace(getpwuid=lambda _: SimpleNamespace(pw_dir=str(root)))}), \
                        patch.object(qualification, "command", side_effect=run):
                    q.install(None)
                self.assertEqual(events, ["written", "install", "removed", "lifecycle"])
                q.remove_root_auth()  # The final cleanup backstop is idempotent.
                if existing == "config":
                    self.assertEqual(config.read_text(), "original config\n")
                elif existing == "directory":
                    self.assertTrue(auth.parent.is_dir())
                    self.assertFalse(config.exists())
                else:
                    self.assertFalse(auth.parent.exists())
                    self.assertFalse((root / ".config").exists())

    def test_failed_root_installer_removes_credentials_immediately(self):
        q = self.install_fixture(form="machine")
        root = self.work / "root"
        root.mkdir()
        auth = root / ".config/gh/hosts.yml"
        def run(args, **kwargs):
            if args[0] == "sh":
                self.assertTrue(auth.exists())
                raise RuntimeError("installer failed")
            return Mock(stdout="authenticated")
        with patch.dict(sys.modules, {"pwd": SimpleNamespace(getpwuid=lambda _: SimpleNamespace(pw_dir=str(root)))}), \
                patch.object(qualification, "command", side_effect=run), \
                self.assertRaisesRegex(RuntimeError, "installer failed"):
            q.install(None)
        self.assertFalse(auth.exists())
        self.assertFalse(auth.parent.exists())
        q.until.assert_not_called()

    def test_existing_root_credentials_are_never_replaced_or_removed(self):
        q = self.install_fixture(form="machine")
        auth = self.work / "root/.config/gh/hosts.yml"
        auth.parent.mkdir(parents=True)
        auth.write_text("original credentials\n", newline="\n")
        with patch.dict(sys.modules, {"pwd": SimpleNamespace(getpwuid=lambda _: SimpleNamespace(pw_dir=str(self.work / "root")))}), \
                self.assertRaisesRegex(RuntimeError, "refusing to replace"):
            q.prepare_root_auth()
        q.remove_root_auth()
        self.assertEqual(auth.read_text(), "original credentials\n")

    def test_root_cleanup_backstop_runs_before_other_lifecycle_cleanup(self):
        q, _ = self.published_check()
        q.os, q.data, q.installed_service = "Linux", self.work / "data", True
        events = []
        q.prerequisites = Mock(side_effect=RuntimeError("prerequisite failure"))
        q.remove_root_auth = Mock(side_effect=lambda: events.append("credentials"))
        q.logs = Mock(side_effect=lambda: events.append("logs"))
        q.uninstall = Mock(side_effect=lambda: events.append("uninstall"))
        q.load.finish.side_effect = lambda: events.append("load")
        with self.assertRaises(qualification.QualificationFailure):
            q.run()
        self.assertEqual(events, ["credentials", "load", "logs", "uninstall"])

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
