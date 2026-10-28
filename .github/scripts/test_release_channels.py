"""Exercise the real gh/docker argument paths against stateful fake CLIs."""

import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("release_channels", Path(__file__).with_name("release_channels.py"))
channels = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(channels)


def release(version, **changes):
    result = {"id": version, "tag_name": "v" + version, "name": "open-ferry " + version,
              "draft": False, "prerelease": True, "published_at": "2026-10-27T00:00:00Z",
              "body": "Original release notes", "assets": [{"id": 42, "name": "SHA256SUMS"}]}
    result.update(changes)
    return result


def digest(version):
    return "sha256:" + hashlib.sha256(version.encode()).hexdigest()


class FakeCLIs:
    """Fake gh and docker processes, not mock policy decisions; no network."""

    def __init__(self, releases, latest=None, pointer=None):
        self.releases = copy.deepcopy(releases)
        self.latest = latest
        self.has_pointer = pointer is not None
        self.pointer = None if pointer is None else (pointer + "\n").encode()
        self.images = {v: digest(v) for r in releases if (v := channels.release_version(r))}
        if latest:
            self.images["latest"] = self.images[latest]
        if pointer:
            self.images["preview"] = self.images.get(pointer, digest(pointer))
        self.calls = []
        self.edits = []
        self.notes = []

    def find(self, version):
        return next(r for r in self.releases if r["tag_name"] == "v" + version)

    def run(self, args, **kwargs):
        self.calls.append(args)
        self.assert_safe(kwargs)
        try:
            result = self.gh(args[1:]) if args[0] == "gh" else self.docker(args[1:])
        except FileNotFoundError:
            return subprocess.CompletedProcess(args, 1, b"", b"gh: Not Found (HTTP 404)")
        if not isinstance(result, bytes):
            result = json.dumps(result).encode()
        return subprocess.CompletedProcess(args, 0, result, b"")

    @staticmethod
    def assert_safe(kwargs):
        assert not kwargs.get("shell"), "Never run inputs in a shell"

    def gh(self, args):
        if args[0] == "api":
            if "--paginate" in args:
                # Deliberately split into pages to test flattening.
                return [self.releases[:1], self.releases[1:]]
            endpoint = args[1].split("/releases", 1)[1]
            if endpoint == "/latest":
                if self.latest is None:
                    raise FileNotFoundError()
                return self.find(self.latest)
            if endpoint == "/tags/channel-preview":
                if not self.has_pointer:
                    raise FileNotFoundError()
                return {"draft": False, "prerelease": True, "assets": [] if self.pointer is None else
                        [{"id": 99, "name": "VERSION", "size": len(self.pointer)}]}
            if endpoint == "/assets/99":
                return self.pointer
            raise AssertionError(args)
        assert args[0] == "release", args
        action, tag = args[1:3]
        if action == "create":
            assert tag == "channel-preview" and "--prerelease" in args and "--latest=false" in args
            self.has_pointer = True
        elif action == "upload":
            assert tag == "channel-preview" and "--clobber" in args
            self.pointer = Path(args[3]).read_bytes()
        elif action == "delete-asset":
            assert tag == "channel-preview" and args[3:] == ["VERSION", "--yes"]
            self.pointer = None
        elif action == "edit":
            target = self.find(tag[1:])
            self.edits.append(args)
            if "--latest" in args:
                self.latest = tag[1:]
            if "--prerelease=false" in args:
                target["prerelease"] = False
            if "--prerelease" in args:
                target["prerelease"] = True
                if self.latest == tag[1:]:
                    self.latest = None
            if "--title" in args:
                target["name"] = args[args.index("--title") + 1]
            if "--notes-file" in args:
                notes = Path(args[args.index("--notes-file") + 1]).read_text(encoding="utf-8")
                self.notes.append(notes)
                target["body"] = notes
        else:
            raise AssertionError(args)
        return b""

    def docker(self, args):
        assert args[:2] == ["buildx", "imagetools"], args
        if args[2] == "inspect":
            tag = args[3].rsplit(":", 1)[1]
            if tag not in self.images:
                # Docker absence must use a registry error, not gh's 404.
                return b""
            return self.images[tag].encode()
        assert args[2] == "create" and "--prefer-index=false" in args, args
        tag = args[args.index("--tag") + 1].rsplit(":", 1)[1]
        self.images[tag] = args[-1].split("@", 1)[1]
        return b""


class ChannelTests(unittest.TestCase):
    def setUp(self):
        self.a = release("1.0.0", prerelease=False)
        self.b = release("1.1.0")
        self.c = release("1.2.0")

    def apply(self, fake, operation, version, reason=""):
        def runner(args, **kwargs):
            if args[:4] == ["docker", "buildx", "imagetools", "inspect"]:
                tag = args[4].rsplit(":", 1)[1]
                if tag not in fake.images:
                    return subprocess.CompletedProcess(args, 1, b"", b"manifest unknown")
            return fake.run(args, **kwargs)
        with patch.object(channels.subprocess, "run", side_effect=runner), \
                patch.object(channels.urllib.request, "urlopen", side_effect=AssertionError("No network")), \
                patch("sys.stdout", new_callable=io.StringIO) as output:
            channels.apply(operation, version, reason, channels.GitHub("owner/repo"), channels.Images())
        return output.getvalue()

    def test_semver_precedence(self):
        versions = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta",
                    "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0", "1.9.0", "1.10.0", "2.0.0"]
        self.assertEqual(sorted(reversed(versions), key=channels.version_key), versions)

    def test_malformed_versions_rejected_before_gh(self):
        values = ["", "v1.2.3", "01.2.3", "1.2", "1.2.3.4", "1.2.3+meta", "1.2.3-",
                  "1.2.3-a..b", "1.2.3-01", "1.2.3\n", "1.2.3;echo x", "$(whoami)",
                  "1.2.3-ä", "1.2.3-" + "x" * 58, "--help"]
        for value in values:
            for operation in ("publish", "Promote", "Withdraw"):
                with self.subTest(value=value, operation=operation), \
                        patch.object(channels.subprocess, "run") as run, self.assertRaises(ValueError):
                    channels.apply(operation, value, "reason", channels.GitHub("owner/repo"), channels.Images())
                run.assert_not_called()
        channels.version_key("1.2.3-" + "x" * 57)

    def test_publish_creates_pointer_as_prerelease_only(self):
        fake = FakeCLIs([self.a, self.b], latest="1.0.0")
        self.apply(fake, "publish", "1.1.0")
        self.assertEqual(fake.pointer, b"1.1.0\n")
        self.assertEqual(fake.images["preview"], digest("1.1.0"))
        self.assertEqual(fake.latest, "1.0.0")
        self.assertEqual(fake.images["latest"], digest("1.0.0"))
        self.assertFalse(fake.edits)

    def test_publish_only_moves_forward_by_semver(self):
        for newer, older in (("1.10.0", "1.9.0"), ("1.0.0", "1.0.0-rc.1"),
                             ("1.0.0-beta.11", "1.0.0-beta.2")):
            fake = FakeCLIs([release(newer), release(older)], pointer=newer)
            self.apply(fake, "publish", older)
            self.assertEqual(fake.pointer, (newer + "\n").encode())
            self.assertFalse(any(c[:3] == ["gh", "release", "upload"] for c in fake.calls))

    def test_publish_updates_existing_pointer(self):
        fake = FakeCLIs([self.a, self.b, self.c], latest="1.0.0", pointer="1.1.0")
        self.apply(fake, "publish", "1.2.0")
        self.assertEqual(fake.pointer, b"1.2.0\n")
        self.assertFalse(any(c[:3] == ["gh", "release", "create"] for c in fake.calls))

    def test_publish_equal_repairs_image_without_changing_build(self):
        fake = FakeCLIs([self.a, self.b], latest="1.0.0", pointer="1.1.0")
        fake.images["preview"] = digest("1.0.0")
        self.apply(fake, "publish", "1.1.0")
        self.assertEqual(fake.images["preview"], fake.images["1.1.0"])

    def test_publish_withdrawn_does_nothing(self):
        fake = FakeCLIs([self.a, release("1.1.0", name="open-ferry 1.1.0 (WITHDRAWN)")], latest="1.0.0")
        self.apply(fake, "publish", "1.1.0")
        self.assertFalse(fake.has_pointer)
        self.assertFalse(any(c[0] == "docker" for c in fake.calls))

    def test_promote_refusals(self):
        for changes in ({"draft": True}, {"published_at": None}, {"prerelease": False},
                        {"name": "open-ferry 1.1.0 (withdrawn)"}):
            with self.subTest(changes=changes):
                fake = FakeCLIs([self.a, release("1.1.0", **changes)], latest="1.0.0")
                with self.assertRaises(ValueError):
                    self.apply(fake, "Promote", "1.1.0")
                self.assertFalse(fake.edits)
        for older in ("0.9.0", "1.0.0-rc.1"):
            with self.subTest(older=older):
                fake = FakeCLIs([self.a, release(older)], latest="1.0.0")
                with self.assertRaisesRegex(ValueError, "newer than latest"):
                    self.apply(fake, "Promote", older)

    def test_promote_equal_to_latest_is_refused(self):
        with self.assertRaisesRegex(ValueError, "newer than latest"):
            channels.decide("Promote", "1.0.0", [release("1.0.0")], self.a, None)

    def test_promote_same_build_no_soak_requirement(self):
        fake = FakeCLIs([self.a, self.b], latest="1.0.0", pointer="1.1.0")
        output = self.apply(fake, "Promote", "1.1.0")
        self.assertIn("published for", output)
        self.assertIn("no minimum soak time", output)
        self.assertEqual(fake.latest, "1.1.0")
        self.assertFalse(fake.find("1.1.0")["prerelease"])
        self.assertEqual(fake.images["latest"], fake.images["1.1.0"])
        self.assertEqual(fake.pointer, b"1.1.0\n")

    def test_first_release_can_be_promoted(self):
        fake = FakeCLIs([self.b], pointer="1.1.0")
        self.apply(fake, "Promote", "1.1.0")
        self.assertEqual(fake.latest, "1.1.0")

    def test_withdraw_latest_preserves_newer_preview(self):
        b = release("1.1.0", prerelease=False)
        fake = FakeCLIs([self.a, b, self.c], latest="1.1.0", pointer="1.2.0")
        self.apply(fake, "Withdraw", "1.1.0", "broken")
        self.assertEqual(fake.latest, "1.0.0")
        self.assertEqual(fake.images["latest"], digest("1.0.0"))
        self.assertEqual(fake.pointer, b"1.2.0\n")
        self.assertEqual(fake.images["preview"], digest("1.2.0"))
        self.assertTrue(channels.withdrawn(fake.find("1.1.0")))
        self.assertTrue(fake.find("1.1.0")["prerelease"])
        self.assertEqual(fake.find("1.1.0")["assets"], b["assets"])

    def test_withdraw_pointer_selects_newest_remaining_preview(self):
        fake = FakeCLIs([self.a, self.b, self.c], latest="1.0.0", pointer="1.2.0")
        self.apply(fake, "Withdraw", "1.2.0", "broken")
        self.assertEqual(fake.pointer, b"1.1.0\n")
        self.assertEqual(fake.images["preview"], digest("1.1.0"))
        self.assertEqual(fake.latest, "1.0.0")

    def test_withdraw_pointer_removes_redundant_version(self):
        fake = FakeCLIs([self.a, self.b], latest="1.0.0", pointer="1.1.0")
        self.apply(fake, "Withdraw", "1.1.0", "broken")
        self.assertIsNone(fake.pointer)
        self.assertTrue(fake.has_pointer)
        self.assertEqual(fake.images["preview"], digest("1.0.0"))

    def test_withdraw_both(self):
        b = release("1.1.0", prerelease=False)
        fake = FakeCLIs([self.a, b], latest="1.1.0", pointer="1.1.0")
        self.apply(fake, "Withdraw", "1.1.0", "broken")
        self.assertEqual(fake.latest, "1.0.0")
        self.assertIsNone(fake.pointer)
        for tag in ("latest", "preview"):
            self.assertEqual(fake.images[tag], digest("1.0.0"))
        # Point clients away before marking the withdrawn release.
        self.assertEqual(fake.edits[0][2], "v1.0.0")
        self.assertEqual(fake.edits[-1][2], "v1.1.0")

    def test_withdraw_fallback_uses_semver_not_list_order_or_dates(self):
        stable = release("1.10.0", prerelease=False, published_at="2025-01-01T00:00:00Z")
        bad = release("2.0.0", prerelease=False)
        fake = FakeCLIs([release("1.9.0", prerelease=False), bad, stable,
                        release("1.11.0", prerelease=False, name="open-ferry 1.11.0 (withdrawn)"),
                        release("1.12.0", prerelease=False, draft=True)],
                       latest="2.0.0", pointer="2.0.0")
        self.apply(fake, "Withdraw", "2.0.0", "broken")
        self.assertEqual(fake.latest, "1.10.0")
        self.assertIsNone(fake.pointer)

    def test_withdraw_retry_is_safe_and_title_suffix_is_not_duplicated(self):
        fake = FakeCLIs([self.a, self.b], latest="1.0.0", pointer="1.1.0")
        self.apply(fake, "Withdraw", "1.1.0", "broken")
        self.apply(fake, "Withdraw", "1.1.0", "still broken")
        self.assertEqual(fake.find("1.1.0")["name"].count(" (withdrawn)"), 1)
        self.assertIsNone(fake.pointer)
        self.assertEqual(fake.latest, "1.0.0")

    def test_withdraw_both_with_only_good_prerelease_left(self):
        fake = FakeCLIs([self.a, self.b], latest="1.0.0", pointer="1.0.0")
        with patch.object(channels.Images, "disable") as disable:
            self.apply(fake, "Withdraw", "1.0.0", "broken")
        disable.assert_called_once_with("latest")
        self.assertIsNone(fake.latest)
        self.assertEqual(fake.pointer, b"1.1.0\n")
        self.assertEqual(fake.images["preview"], digest("1.1.0"))

    def test_withdraw_no_good_release_disables_aliases_retains_version(self):
        fake = FakeCLIs([self.a], latest="1.0.0", pointer="1.0.0")
        disabled = []
        with patch.object(channels.Images, "disable", side_effect=disabled.append):
            self.apply(fake, "Withdraw", "1.0.0", "broken")
        self.assertIsNone(fake.latest)
        self.assertIsNone(fake.pointer)
        self.assertEqual(disabled, ["latest", "preview"])
        self.assertEqual(fake.images["1.0.0"], digest("1.0.0"))

    def test_withdraw_no_older_stable_keeps_good_preview(self):
        fake = FakeCLIs([self.a, self.b], latest="1.0.0", pointer="1.1.0")
        with patch.object(channels.Images, "disable") as disable:
            self.apply(fake, "Withdraw", "1.0.0", "broken")
        disable.assert_called_once_with("latest")
        self.assertEqual(fake.pointer, b"1.1.0\n")

    def test_withdraw_skips_withdrawn_drafts_and_nonversion_releases(self):
        releases = [self.a, self.b, self.c, release("1.3.0", draft=True),
                    release("1.4.0", name="open-ferry 1.4.0 (withdrawn)"),
                    release("1.5.0", published_at=None), release("1.6.0", tag_name="channel-other")]
        fake = FakeCLIs(releases, latest="1.0.0", pointer="1.2.0")
        self.apply(fake, "Withdraw", "1.2.0", "broken")
        self.assertEqual(fake.pointer, b"1.1.0\n")

    def test_withdraw_does_not_move_aliases_at_unrelated_digests(self):
        fake = FakeCLIs([self.a, self.b, self.c], latest="1.0.0", pointer="1.2.0")
        self.apply(fake, "Withdraw", "1.1.0", "broken")
        self.assertFalse(any(c[:4] == ["docker", "buildx", "imagetools", "create"] for c in fake.calls))

    def test_reason_goes_through_file_as_inert_text(self):
        fake = FakeCLIs([self.a, self.b], latest="1.0.0", pointer="1.1.0")
        reason = '$(whoami) `code` "quotes"\n::error::text\n```\n<script>alert(1)</script>'
        self.apply(fake, "Withdraw", "1.1.0", reason)
        self.assertIn("````text\n" + reason + "\n````", fake.notes[0])
        self.assertTrue(fake.notes[0].endswith("Original release notes"))
        self.assertFalse(any(reason in c for c in fake.calls))

    def test_reason_validation_before_gh(self):
        for reason in ("", "  ", "hello\x00", "hello\r", "x" * 4001, "hello\x1b"):
            with self.subTest(reason=reason), patch.object(channels.subprocess, "run") as run, \
                    self.assertRaises(ValueError):
                channels.apply("Withdraw", "1.0.0", reason, channels.GitHub("owner/repo"), channels.Images())
            run.assert_not_called()

    def test_missing_release_refused(self):
        fake = FakeCLIs([self.a], latest="1.0.0")
        with self.assertRaisesRegex(ValueError, "does not exist"):
            self.apply(fake, "Promote", "1.1.0")

    def test_entrypoint_ignores_pointer_release_without_network(self):
        with patch.dict(os.environ, {"CHANNEL_EVENT": "release", "CHANNEL_TAG": "channel-preview"}), \
                patch.object(channels, "apply") as apply, patch("sys.stdout", new_callable=io.StringIO):
            channels.main()
        apply.assert_not_called()

    def test_entrypoint_refuses_dispatch_from_other_branch(self):
        with patch.dict(os.environ, {"CHANNEL_EVENT": "workflow_dispatch", "GITHUB_REF": "refs/heads/other"}), \
                patch.object(channels, "apply") as apply, self.assertRaisesRegex(ValueError, "main"):
            channels.main()
        apply.assert_not_called()

    def test_digest_copy_must_preserve_digest(self):
        with patch.object(channels, "command") as command, \
                patch.object(channels.Images, "digest", return_value=digest("other")), \
                self.assertRaisesRegex(RuntimeError, "changed the image digest"):
            channels.Images().move("preview", digest("1.1.0"))
        self.assertIn(channels.IMAGE + "@" + digest("1.1.0"), command.call_args.args[0])

    def test_missing_image_prevents_metadata_mutations(self):
        fake = FakeCLIs([self.a, self.b], latest="1.0.0")
        del fake.images["1.1.0"]
        with self.assertRaises(RuntimeError):
            self.apply(fake, "publish", "1.1.0")
        self.assertFalse(fake.has_pointer)
        self.assertFalse(fake.edits)

    def test_only_404_means_absent(self):
        for error in (b"gh: forbidden (HTTP 403)", b"connection timed out"):
            with self.subTest(error=error), patch.object(channels.subprocess, "run", return_value=
                    subprocess.CompletedProcess([], 1, b"", error)), self.assertRaises(RuntimeError):
                channels.GitHub("owner/repo").snapshot()

    def test_registry_auth_failure_is_not_absence(self):
        with patch.object(channels.subprocess, "run", return_value=
                subprocess.CompletedProcess([], 1, b"", b"unauthorized: repository not found (403)")), \
                self.assertRaises(RuntimeError):
            channels.Images().digest("preview", optional=True)

    def test_malformed_pointer_refused(self):
        for value in (b"1.1.0", b"1.1.0\n\n", b"01.1.0\n", b"1.1.0\x00\n", b"x" * 65):
            with self.subTest(value=value):
                fake = FakeCLIs([self.a, self.b], latest="1.0.0", pointer="1.1.0")
                fake.pointer = value
                with self.assertRaises(ValueError):
                    self.apply(fake, "publish", "1.1.0")

    def test_empty_index_transport_preserves_versioned_manifests(self):
        requests = []
        expected = "sha256:" + hashlib.sha256(
            b'{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}').hexdigest()

        def transport(request, **kwargs):
            requests.append(request)
            return io.BytesIO(b'{"token":"fake-token"}' if len(requests) == 1 else b"")

        with patch.dict(os.environ, {"GITHUB_ACTOR": "actor", "GH_TOKEN": "fake-secret"}), \
                patch.object(channels.urllib.request, "urlopen", side_effect=transport), \
                patch.object(channels.Images, "digest", return_value=expected), \
                patch("sys.stdout", new_callable=io.StringIO):
            channels.Images().disable("latest")
        self.assertEqual(requests[1].method, "PUT")
        self.assertTrue(requests[1].full_url.endswith("/manifests/latest"))
        self.assertEqual(json.loads(requests[1].data)["manifests"], [])
        self.assertNotIn("fake-secret", requests[1].full_url)


if __name__ == "__main__":
    unittest.main()
