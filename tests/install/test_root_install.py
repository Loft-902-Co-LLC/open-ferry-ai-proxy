"""Real-root installer tests. Only scratch files and loopback are used."""
import io
import os
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "install.sh"


@unittest.skipUnless(os.name == "posix" and os.geteuid() == 0, "requires real root")
class RootInstallTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(os.environ["OFP_TEST_SCRATCH"])
        # The container mount is supplied by the runner, but belongs to root
        # inside this isolated job. No machine installation paths are used.
        scratch.mkdir(parents=True, exist_ok=True)
        os.chown(scratch, 0, 0)
        scratch.chmod(0o755)
        self.work = Path(tempfile.mkdtemp(prefix="root-install-", dir=scratch))
        self.addCleanup(shutil.rmtree, self.work)
        self.marker = self.work / "marker"
        self.curl_home = self.work / "caller-home"
        self.curl_home.mkdir()
        (self.curl_home / ".curlrc").write_text(f'trace-ascii = "{self.marker}"\n')
        self.env = dict(os.environ)
        for name in list(self.env):
            if "proxy" in name.lower():
                del self.env[name]
        self.hostile = {
            "OPEN_FERRY_INSTALL_BASE_URL": "http://127.0.0.1:1/untrusted",
            "OPEN_FERRY_INSTALL_GH": str(self.work / "untrusted-gh"),
            "OPEN_FERRY_INSTALL_TEST_PRIVILEGED": "0",
            "FAKE_OPEN_FERRY_INSTALL_LOG": str(self.work / "fake-log"),
            "FAKE_ANYTHING": "ignored",
            "TAR_OPTIONS": f'--checkpoint=1 --checkpoint-action="exec=touch {self.marker}"',
            "CURL_HOME": str(self.curl_home),
            "WGETRC": str(self.work / "wgetrc"),
            "SSL_CERT_FILE": str(self.marker),
            "SSL_CERT_DIR": str(self.work),
            "CURL_CA_BUNDLE": str(self.marker),
            "PERL5OPT": "-MDefinitelyMissingInstallerModule",
            "PERL5LIB": str(self.work),
            "OPENSSL_CONF": str(self.work / "invalid-openssl"),
            "GH_TOKEN": "test-value-must-not-be-printed",
            "HOME": str(self.curl_home),
            "PATH": str(self.work),
        }
        self.env.update(self.hostile)

    def shell(self, code):
        result = subprocess.run(["/bin/sh", "-c", code], env=self.env,
                                text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, timeout=45)
        self.assertFalse(self.marker.exists(), result.stdout)
        self.assertNotIn(self.hostile["GH_TOKEN"], result.stdout)
        return result

    def helper(self, code):
        # Execute the actual bootstrap/helper definitions, not a test-only
        # privileged seam. No installer body or network request runs here.
        prefix = SCRIPT.read_text().split("# --- Options", 1)[0]
        return self.shell(prefix + "\n" + code)

    def test_root_names_dropped_variables_and_tools_receive_only_allow_list(self):
        result = self.helper("run env")
        self.assertEqual(result.returncode, 0, result.stdout)
        for name in self.hostile:
            if name not in ("PATH", "HOME"):
                self.assertIn(f"Ignoring {name} when running as root.", result.stdout)
                self.assertNotIn(f"\n{name}=", result.stdout)
        self.assertIn("PATH=/usr/sbin:/usr/bin:/sbin:/bin\n", result.stdout)
        home = next(line.split(":")[5] for line in Path("/etc/passwd").read_text().splitlines()
                    if line.split(":")[2] == "0")
        self.assertIn(f"HOME={home}\n", result.stdout)
        self.assertIn("LC_ALL=C\n", result.stdout)
        env_lines = [line for line in result.stdout.splitlines() if "=" in line]
        self.assertEqual({line.split("=", 1)[0] for line in env_lines}, {"PATH", "HOME", "LC_ALL"})
        self.env.update(https_proxy="http://127.0.0.1:9", NO_PROXY="127.0.0.1")
        proxies = self.helper("run env")
        self.assertEqual(proxies.returncode, 0, proxies.stdout)
        self.assertIn("https_proxy=http://127.0.0.1:9\n", proxies.stdout)
        self.assertIn("NO_PROXY=127.0.0.1\n", proxies.stdout)


    def test_root_tar_ignores_checkpoint_action_and_checksum_tools_ignore_hooks(self):
        archive = self.work / "archive.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            entry = tarfile.TarInfo("payload")
            entry.size = 32768
            tar.addfile(entry, io.BytesIO(b"x" * entry.size))
        result = self.helper(f"run tar -xzf {shlex.quote(str(archive))} -C {shlex.quote(str(self.work))}\n"
                             "if has shasum; then run shasum -a 256 " + shlex.quote(str(archive)) + "; fi\n"
                             "if has openssl; then run openssl dgst -sha256 " + shlex.quote(str(archive)) + "; fi")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertTrue((self.work / "payload").exists())

    def install(self, bin_dir):
        # A bound, non-listening loopback proxy guarantees the official URL
        # cannot reach any external host, even outside --network none.
        with socket.socket() as blocked:
            blocked.bind(("127.0.0.1", 0))
            self.env["https_proxy"] = f"http://127.0.0.1:{blocked.getsockname()[1]}"
            return self.shell("exec /bin/sh " + shlex.quote(str(SCRIPT)) +
                              " --system --target x86_64-unknown-linux-musl --version 0.1.0 --bin-dir " +
                              shlex.quote(str(bin_dir)) + " --config " + shlex.quote(str(self.work / "config.yaml")))

    def test_root_refuses_untrusted_bin_directory_or_ancestor_before_download(self):
        for kind in ("owner", "group-write", "world-write", "link"):
            for ancestor in (False, True):
                with self.subTest(kind=kind, ancestor=ancestor):
                    parent = self.work / f"{kind}-{ancestor}"
                    parent.mkdir()
                    directory = parent / "bin"
                    directory.mkdir()
                    bad = parent if ancestor else directory
                    if kind == "owner":
                        os.chown(bad, 12345, 12345)
                    elif kind == "link":
                        bad.rmdir() if not ancestor else shutil.rmtree(bad)
                        bad.symlink_to(self.curl_home, target_is_directory=True)
                    else:
                        bad.chmod(0o775 if kind == "group-write" else 0o757)
                    result = self.install(directory)
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn("refusing " + str(bad), result.stdout)
                    self.assertNotIn("couldn't download", result.stdout)
                    self.assertFalse((self.work / "config.yaml").exists())

    def test_root_failed_download_ignores_curl_config_and_cleans_staging(self):
        directory = self.work / "bin"
        result = self.install(directory)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("couldn't download SHA256SUMS", result.stdout)
        self.assertNotIn("from http://127.0.0.1:1/untrusted", result.stdout)
        self.assertEqual(list(directory.iterdir()), [])
        self.assertFalse((self.work / "config.yaml").exists())
