#!/usr/bin/env python3
"""Tests for install.sh and install.ps1, against a fake release.

  python3 tests/install/test_install.py [-v]

A server on 127.0.0.1 serves fake releases in GitHub's layout:
<base>/download/v<version>/<file>, and <base>/latest/download/<file>, which
redirects to the latest release's, as GitHub's does. Their archives hold a
fake open-ferry that records its arguments in a file and, for `init`, writes
a config. One release's SHA256SUMS doesn't match its archives. The scripts'
base URL (OPEN_FERRY_INSTALL_BASE_URL) is that server, and their GitHub CLI
(OPEN_FERRY_INSTALL_GH) is a fake that records its arguments, or a name that
isn't installed: nothing contacts GitHub.

install.sh runs under sh, and needs what it needs on a user's system: curl
or wget, tar, and sha256sum, shasum or openssl. On Windows, sh is Git
Bash's (run the tests from Git Bash), with the target given or the platform
faked, as install.sh refuses Windows itself. install.ps1 runs on Windows, under Windows
PowerShell and PowerShell 7, each that is installed; the fake open-ferry.exe
is compiled with the .NET Framework's csc. Elsewhere, install.ps1 is only
checked to refuse to run, when pwsh is installed.

Every install goes to a temporary directory the test passes, and HOME,
XDG_CONFIG_HOME, XDG_DATA_HOME, LOCALAPPDATA, APPDATA and the temporary
directory are redirected to temporary directories too, so the install
receipt is written in one.

Environment:
  INSTALL_TEST_EXPECT_TARGET  also run install.sh with this system's own
                              detection, and expect this target
  INSTALL_TEST_DEFAULT_PATHS  1: also run the scripts with neither an install
                              directory nor a config path, so that they use
                              their defaults under the redirected HOME,
                              LOCALAPPDATA and APPDATA. Off by default, so a
                              run on a developer's machine never depends on
                              that redirection; CI turns it on.
  INSTALL_TEST_SH             the sh for install.sh (default: sh on PATH);
                              empty to skip install.sh
  INSTALL_TEST_POWERSHELL     Windows PowerShell (default: powershell on
                              PATH); empty to skip it
  INSTALL_TEST_PWSH           PowerShell 7 (default: pwsh on PATH); empty to
                              skip it
"""

import collections
import functools
import hashlib
import http.server
import io
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import threading
import time
import unittest
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
INSTALL_SH = os.path.join(ROOT, "install.sh")
INSTALL_PS1 = os.path.join(ROOT, "install.ps1")
WINDOWS = os.name == "nt"
REPO = "Loft-902-Co-LLC/open-ferry-ai-proxy"

UNIX_TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)
WINDOWS_TARGET = "x86_64-pc-windows-msvc"
GNU = "x86_64-unknown-linux-gnu"

LATEST = "1.2.3"
OLDER = "1.1.0"  # its SHA256SUMS marks the names binary ("<hash> *<name>")
TAMPERED = "0.9.0"  # its SHA256SUMS doesn't match its archives
MISSING = "2.0.0"  # no such release
NO_GH = "open-ferry-test-no-gh"  # a GitHub CLI that isn't installed

EXPECT_TARGET = os.environ.get("INSTALL_TEST_EXPECT_TARGET", "")
DEFAULT_PATHS = os.environ.get("INSTALL_TEST_DEFAULT_PATHS") == "1"


def find_tool(variable, name, *fallbacks):
    value = os.environ.get(variable)
    if value is not None:
        return value or None
    found = shutil.which(name)
    if found:
        return found
    for fallback in fallbacks:
        if os.path.isfile(fallback):
            return fallback
    return None


PROGRAM_FILES = os.environ.get("ProgramFiles", r"C:\Program Files")
SH = find_tool("INSTALL_TEST_SH", "sh", os.path.join(PROGRAM_FILES, "Git", "usr", "bin", "sh.exe"))
if WINDOWS and SH and not os.path.isfile(os.path.join(os.path.dirname(os.path.abspath(SH)), "cygpath.exe")):
    # Git's bin\sh.exe only starts its usr\bin\sh.exe, which has Git's tools
    # beside it: use that one.
    beside = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(SH))), "usr", "bin", "sh.exe")
    if os.path.isfile(beside):
        SH = beside
POWERSHELL = find_tool("INSTALL_TEST_POWERSHELL", "powershell") if WINDOWS else None
PWSH = find_tool("INSTALL_TEST_PWSH", "pwsh")

# Git Bash's tools, after everything else on PATH, for an sh started from
# outside Git Bash.
SH_TOOLS = []
if WINDOWS and SH:
    usr_bin = os.path.dirname(os.path.abspath(SH))
    SH_TOOLS = [usr_bin, os.path.join(os.path.dirname(os.path.dirname(usr_bin)), "mingw64", "bin")]

# --- Fakes -------------------------------------------------------------------

FAKE_OPEN_FERRY_SH = r"""#!/bin/sh
# name: @NAME@
# A fake open-ferry for the install script tests. It records its arguments
# in FAKE_OPEN_FERRY_LOG, as a line of tab-separated fields after its name;
# for `init -config PATH` writes a config, or fails if FAKE_OPEN_FERRY_FAIL
# is set; and fails `update` if FAKE_OPEN_FERRY_UPDATE_FAIL is set.
{
  printf '%s' '@NAME@'
  for arg in "$@"; do
    printf '\t%s' "$arg"
  done
  printf '\n'
} >> "$FAKE_OPEN_FERRY_LOG"
if [ "${1:-}" = update ] && [ -n "${FAKE_OPEN_FERRY_UPDATE_FAIL:-}" ]; then
  echo "fake open-ferry: update failed" >&2
  exit 4
fi
if [ "${1:-}" = init ]; then
  if [ -n "${FAKE_OPEN_FERRY_FAIL:-}" ]; then
    echo "fake open-ferry: init failed" >&2
    exit 3
  fi
  if [ "${2:-}" = -config ] && [ -n "${3:-}" ]; then
    mkdir -p "$(dirname "$3")"
    printf 'fake-config: @NAME@\n' > "$3"
    echo "Wrote $3"
  fi
fi
"""

FAKE_OPEN_FERRY_CS = r"""
// A fake open-ferry.exe for the install script tests. It records its
// arguments in FAKE_OPEN_FERRY_LOG, as a line of tab-separated fields after
// its name; for `init -config PATH` writes a config, or fails if
// FAKE_OPEN_FERRY_FAIL is set; fails `update` if FAKE_OPEN_FERRY_UPDATE_FAIL
// is set; and for `sleep MS` sleeps, to stand for a running open-ferry.
using System;
using System.IO;
using System.Text;
using System.Threading;

public static class FakeOpenFerry
{
    public static int Main(string[] args)
    {
        string name = "@NAME@";
        string log = Environment.GetEnvironmentVariable("FAKE_OPEN_FERRY_LOG");
        if (!String.IsNullOrEmpty(log))
        {
            StringBuilder line = new StringBuilder(name);
            foreach (string arg in args)
            {
                line.Append('\t').Append(arg);
            }
            line.Append('\n');
            File.AppendAllText(log, line.ToString());
        }
        if (args.Length == 2 && args[0] == "sleep")
        {
            Thread.Sleep(Int32.Parse(args[1]));
            return 0;
        }
        if (args.Length >= 1 && args[0] == "update"
            && !String.IsNullOrEmpty(Environment.GetEnvironmentVariable("FAKE_OPEN_FERRY_UPDATE_FAIL")))
        {
            Console.Error.WriteLine("fake open-ferry: update failed");
            return 4;
        }
        if (args.Length >= 1 && args[0] == "init")
        {
            if (!String.IsNullOrEmpty(Environment.GetEnvironmentVariable("FAKE_OPEN_FERRY_FAIL")))
            {
                Console.Error.WriteLine("fake open-ferry: init failed");
                return 3;
            }
            if (args.Length >= 3 && args[1] == "-config")
            {
                Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(args[2])));
                File.WriteAllText(args[2], "fake-config: " + name + "\n");
                Console.WriteLine("Wrote " + args[2]);
            }
        }
        return 0;
    }
}
"""

FAKE_GH_SH = r"""#!/bin/sh
# A fake gh for the install script tests: it records its arguments in
# FAKE_GH_LOG and exits with FAKE_GH_EXIT (default 0).
{
  printf 'gh'
  for arg in "$@"; do
    printf '\t%s' "$arg"
  done
  printf '\n'
} >> "$FAKE_GH_LOG"
exit "${FAKE_GH_EXIT:-0}"
"""

FAKE_GH_CMD = r"""@echo off
rem A fake gh for the install script tests: it records its arguments in
rem FAKE_GH_LOG and exits with FAKE_GH_EXIT (default 0).
>>"%FAKE_GH_LOG%" echo gh %*
if not defined FAKE_GH_EXIT exit /b 0
exit /b %FAKE_GH_EXIT%
"""

FAKE_UNAME = r"""#!/bin/sh
# A fake uname for the install script tests: FAKE_UNAME_S and FAKE_UNAME_M.
case ${1:-} in
  -s) printf '%s\n' "$FAKE_UNAME_S" ;;
  -m) printf '%s\n' "$FAKE_UNAME_M" ;;
  *)
    echo "fake uname: unexpected arguments: $*" >&2
    exit 2
    ;;
esac
"""

FAKE_GETCONF = r"""#!/bin/sh
# A fake getconf for the install script tests: GNU_LIBC_VERSION is glibc
# FAKE_GLIBC, or unknown, as on musl, when FAKE_GLIBC is empty.
if [ "$*" = GNU_LIBC_VERSION ] && [ -n "${FAKE_GLIBC:-}" ]; then
  printf 'glibc %s\n' "$FAKE_GLIBC"
  exit 0
fi
echo "getconf: $*: unknown variable" >&2
exit 1
"""

FAKE_SYSCTL = r"""#!/bin/sh
# A fake sysctl for the install script tests: sysctl.proc_translated is
# FAKE_TRANSLATED, or unknown, as on an Intel Mac, when it is empty.
if [ "$*" = "-n sysctl.proc_translated" ] && [ -n "${FAKE_TRANSLATED:-}" ]; then
  printf '%s\n' "$FAKE_TRANSLATED"
  exit 0
fi
echo "sysctl: unknown oid" >&2
exit 1
"""

# --- Helpers -----------------------------------------------------------------

Result = collections.namedtuple("Result", "code output")


def long_path(path):
    """The path without 8.3 short names, which a runner's TEMP may hold."""
    if not WINDOWS:
        return path
    import ctypes

    buffer = ctypes.create_unicode_buffer(32768)
    length = ctypes.windll.kernel32.GetLongPathNameW(path, buffer, len(buffer))
    return buffer.value if 0 < length < len(buffer) else path


def sh_path(path):
    """The path as sh writes it: on Windows, Git Bash's form, as cygpath
    gives it for the temporary directory (which may be /tmp), so that it
    matches PATH as sh converts it."""
    if not WINDOWS:
        return path
    if BASE_SH and (path == BASE or path.startswith(BASE + os.sep)):
        return BASE_SH + path[len(BASE):].replace("\\", "/")
    drive, rest = os.path.splitdrive(path)
    return "/" + drive[0].lower() + rest.replace("\\", "/")


def find_base_sh(base):
    """BASE as Git Bash names it, from Git's cygpath."""
    if not (WINDOWS and SH):
        return None
    cygpath = os.path.join(os.path.dirname(os.path.abspath(SH)), "cygpath.exe")
    if not os.path.isfile(cygpath):
        return None
    result = subprocess.run([cygpath, "-u", base], capture_output=True, text=True)
    return result.stdout.strip() if result.returncode == 0 and result.stdout.strip() else None


def ps_quote(text):
    return "'" + text.replace("'", "''") + "'"


def write_file(path, text, newline="\n", mode=0o755):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline=newline) as f:
        f.write(text)
    os.chmod(path, mode)


def read_text(path):
    with open(path, encoding="utf-8", newline="") as f:
        return f.read()


def read_log(path):
    """The lines a fake recorded, each split into its tab-separated fields."""
    if not os.path.exists(path):
        return []
    return [line.split("\t") for line in read_text(path).splitlines() if line]


def lines_of(output):
    return [line.rstrip() for line in output.splitlines()]


RECEIPT_TIME = r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z"


def read_receipt(path):
    """The install receipt at path, which must be UTF-8 without a BOM, the
    only thing in its directory, and have exactly the receipt's fields."""
    with open(path, "rb") as f:
        data = f.read()
    assert not data.startswith(b"\xef\xbb\xbf"), "the receipt has a BOM"
    receipt = json.loads(data.decode("utf-8"))
    assert os.listdir(os.path.dirname(path)) == ["install-receipt.json"], os.listdir(os.path.dirname(path))
    assert sorted(receipt) == ["binary", "format", "installed_at", "installer", "target", "version"], receipt
    assert re.fullmatch(RECEIPT_TIME, receipt.pop("installed_at")), receipt
    return receipt


def sha256_file(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def clean_env():
    """This process's environment without anything that could steer a script
    to a real release, a real gh, a proxy or a real config."""
    dropped = {
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "PSMODULEPATH",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    }
    env = {}
    for key, value in os.environ.items():
        upper = key.upper()
        if upper in dropped or upper.startswith(("OPEN_FERRY_", "FAKE_", "INSTALL_TEST_")):
            continue
        env[key] = value
    return env


def run(command, env, timeout=180):
    process = subprocess.run(command, env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout)
    output = process.stdout.decode("utf-8", "replace") + process.stderr.decode("utf-8", "replace")
    return Result(process.returncode, output)


def wait_for(predicate, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.1)
    return False


def stop(process):
    """Stops a process this test started, and only that."""
    if process.poll() is None:
        process.kill()
    process.wait(timeout=30)


# --- The fake releases -------------------------------------------------------


def archive_name(version, target):
    extension = ".zip" if "-windows-" in target else ".tar.gz"
    return f"open-ferry-{version}-{target}{extension}"


def top_dir(version, target):
    return f"open-ferry-{version}-{target}"


# The files an archive holds besides the binary, as the release workflow
# packs them.
EXTRA_FILES = (
    ("LICENSE", b"MIT License\n"),
    ("README.md", b"# open-ferry\n"),
    ("config.example.yaml", b"server:\n  host: \"\"\n  port: 8317\n"),
    ("licenses/rust-third-party-licenses.txt", b"fake licenses\n"),
)


def write_tar(path, version, target):
    top = top_dir(version, target)
    members = [(f"{top}/open-ferry", FAKE_OPEN_FERRY_SH.replace("@NAME@", top).encode(), 0o755)]
    members += [(f"{top}/{name}", data, 0o644) for name, data in EXTRA_FILES]
    with tarfile.open(path, "w:gz") as tar:
        for directory in (top, f"{top}/licenses"):
            info = tarfile.TarInfo(directory)
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            tar.addfile(info)
        for name, data, mode in members:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = mode
            info.uid = info.gid = 1000
            tar.addfile(info, io.BytesIO(data))


def write_zip(path, version, target, exe):
    top = top_dir(version, target)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(f"{top}/open-ferry.exe", exe)
        for name, data in EXTRA_FILES:
            archive.writestr(f"{top}/{name}", data)


def find_csc():
    windir = os.environ.get("WINDIR", r"C:\Windows")
    for framework in ("Framework64", "Framework"):
        csc = os.path.join(windir, "Microsoft.NET", framework, "v4.0.30319", "csc.exe")
        if os.path.isfile(csc):
            return csc
    raise RuntimeError("the .NET Framework's csc.exe, which builds the fake open-ferry.exe, isn't installed")


def compile_fake_exe(directory, name):
    source = os.path.join(directory, name + ".cs")
    output = os.path.join(directory, name + ".exe")
    write_file(source, FAKE_OPEN_FERRY_CS.replace("@NAME@", name), mode=0o644)
    result = run([find_csc(), "/nologo", "/target:exe", "/out:" + output, source], dict(os.environ))
    if result.code != 0:
        raise RuntimeError("csc couldn't build the fake open-ferry.exe:\n" + result.output)
    with open(output, "rb") as f:
        return f.read()


def build_release(root, version, exe, binary_mode=False, tampered=False):
    directory = os.path.join(root, "download", "v" + version)
    os.makedirs(directory)
    names = []
    for target in UNIX_TARGETS:
        name = archive_name(version, target)
        write_tar(os.path.join(directory, name), version, target)
        names.append(name)
    name = archive_name(version, WINDOWS_TARGET)
    write_zip(os.path.join(directory, name), version, WINDOWS_TARGET, exe)
    names.append(name)
    # The scripts are release assets too, listed in SHA256SUMS.
    for script in (INSTALL_SH, INSTALL_PS1):
        name = os.path.basename(script)
        shutil.copyfile(script, os.path.join(directory, name))
        names.append(name)
    lines = []
    for name in sorted(names):
        digest = sha256_file(os.path.join(directory, name))
        if tampered:
            digest = hashlib.sha256(b"not the archive: " + name.encode()).hexdigest()
        lines.append(f"{digest} {'*' if binary_mode else ' '}{name}\n")
    write_file(os.path.join(directory, "SHA256SUMS"), "".join(lines), mode=0o644)


class ReleaseHandler(http.server.SimpleHTTPRequestHandler):
    """Serves the release directory, with GitHub's redirect from
    <base>/latest/download/<file> to the latest release's file."""

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        for base, version in self.server.latest.items():
            prefix = f"/{base}/latest/download/"
            if path.startswith(prefix):
                host, port = self.server.server_address[:2]
                self.send_response(302)
                self.send_header("Location", f"http://{host}:{port}/{base}/download/v{version}/{path[len(prefix):]}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
        super().do_GET()

    def guess_type(self, path):
        # As GitHub serves release assets.
        return "application/octet-stream"

    def log_message(self, format, *args):
        pass


class ReleaseServer:
    def __init__(self, root, latest):
        self.httpd = http.server.ThreadingHTTPServer(
            ("127.0.0.1", 0), functools.partial(ReleaseHandler, directory=root)
        )
        self.httpd.latest = latest
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    def url(self, base):
        return f"http://127.0.0.1:{self.port}/{base}"

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


BASE = None
BASE_SH = None  # BASE as sh names it, on Windows
SERVER = None
EXES = {}  # the fake open-ferry.exe of each Windows archive, by its name
FAKE_BIN = None  # the fake uname, getconf and sysctl
FAKE_GH = None  # the fake gh: gh, and gh.cmd for install.ps1


def setUpModule():
    global BASE, BASE_SH, SERVER, FAKE_BIN, FAKE_GH
    BASE = long_path(tempfile.mkdtemp(prefix="dist-install-"))
    try:
        BASE_SH = find_base_sh(BASE)
        fakes = os.path.join(BASE, "fakes")
        FAKE_BIN = os.path.join(fakes, "bin")
        write_file(os.path.join(FAKE_BIN, "uname"), FAKE_UNAME)
        write_file(os.path.join(FAKE_BIN, "getconf"), FAKE_GETCONF)
        write_file(os.path.join(FAKE_BIN, "sysctl"), FAKE_SYSCTL)
        FAKE_GH = os.path.join(fakes, "gh")
        write_file(os.path.join(FAKE_GH, "gh"), FAKE_GH_SH)
        write_file(os.path.join(FAKE_GH, "gh.cmd"), FAKE_GH_CMD, newline="\r\n")

        for version in (LATEST, OLDER):
            name = top_dir(version, WINDOWS_TARGET)
            if WINDOWS:
                EXES[name] = compile_fake_exe(fakes, name)
            else:
                EXES[name] = b"not a Windows program: " + name.encode() + b"\n"
        latest_exe = EXES[top_dir(LATEST, WINDOWS_TARGET)]
        older_exe = EXES[top_dir(OLDER, WINDOWS_TARGET)]

        releases = os.path.join(BASE, "releases")
        # "good": the latest release is LATEST; "broken": it is TAMPERED.
        good = os.path.join(releases, "good")
        build_release(good, LATEST, latest_exe)
        build_release(good, OLDER, older_exe, binary_mode=True)
        build_release(good, TAMPERED, latest_exe, tampered=True)
        build_release(os.path.join(releases, "broken"), TAMPERED, latest_exe, tampered=True)
        SERVER = ReleaseServer(releases, {"good": LATEST, "broken": TAMPERED})
    except BaseException:
        shutil.rmtree(BASE, ignore_errors=True)
        raise


def tearDownModule():
    if SERVER is not None:
        SERVER.close()
    if BASE is not None:
        shutil.rmtree(BASE, ignore_errors=True)


class Case(unittest.TestCase):
    def make_work(self):
        work = tempfile.mkdtemp(prefix="test-", dir=BASE)
        self.addCleanup(shutil.rmtree, work, True)
        return work

    def assertExit(self, result, code):
        self.assertEqual(result.code, code, f"exit code {result.code}, not {code}; output:\n{result.output}")


# --- install.sh --------------------------------------------------------------


@unittest.skipIf(SH is None, "no sh")
class InstallShTests(Case):
    def setUp(self):
        self.work = self.make_work()
        self.home = os.path.join(self.work, "home")
        self.tmp = os.path.join(self.work, "tmp")
        os.makedirs(self.home)
        os.makedirs(self.tmp)
        # Spaces, to check the scripts' quoting.
        self.bin_dir = os.path.join(self.work, "bin dir")
        self.config = os.path.join(self.work, "config dir", "open-ferry", "config.yaml")
        self.log = os.path.join(self.work, "open-ferry.log")
        self.gh_log = os.path.join(self.work, "gh.log")

    def env(self, path_first=(), **extra):
        env = clean_env()
        env.update(
            HOME=sh_path(self.home),
            TMPDIR=sh_path(self.tmp),
            OPEN_FERRY_INSTALL_BASE_URL=SERVER.url("good"),
            OPEN_FERRY_INSTALL_GH=NO_GH,
            FAKE_OPEN_FERRY_LOG=sh_path(self.log),
            FAKE_GH_LOG=sh_path(self.gh_log),
        )
        for key, value in extra.items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
        env["PATH"] = os.pathsep.join([*path_first, os.environ.get("PATH", ""), *SH_TOOLS])
        return env

    def run_sh(self, *args, path_first=(), **extra):
        return run([SH, sh_path(INSTALL_SH), *args], self.env(path_first, **extra))

    def install_args(self, *more):
        return ["--bin-dir", sh_path(self.bin_dir), "--config", sh_path(self.config), *more]

    def binary(self):
        return os.path.join(self.bin_dir, "open-ferry")

    def installed_name(self):
        match = re.search(r"^# name: (\S+)$", read_text(self.binary()), re.M)
        return match.group(1) if match else None

    def assertInstalled(self, version, target):
        name = top_dir(version, target)
        self.assertEqual(read_text(self.binary()), FAKE_OPEN_FERRY_SH.replace("@NAME@", name))
        if not WINDOWS:
            self.assertTrue(os.access(self.binary(), os.X_OK))
        # Only the binary: nothing staged is left behind.
        self.assertEqual(os.listdir(self.bin_dir), ["open-ferry"])
        return name

    def assertNothingInstalled(self):
        self.assertFalse(os.path.exists(self.bin_dir))
        self.assertFalse(os.path.exists(self.config))
        self.assertEqual(read_log(self.log), [])
        self.assertFalse(os.path.exists(self.receipt()))

    def receipt(self, data_home=None):
        return os.path.join(data_home or os.path.join(self.home, ".local", "share"), "open-ferry", "install-receipt.json")

    def assertReceipt(self, version, binary, data_home=None):
        self.assertEqual(
            read_receipt(self.receipt(data_home)),
            {"format": 1, "installer": "install.sh", "version": version, "binary": binary, "target": GNU},
        )

    def reset(self):
        for path in (self.bin_dir, os.path.dirname(os.path.dirname(self.config))):
            shutil.rmtree(path, ignore_errors=True)
        for path in (self.log, self.gh_log):
            if os.path.exists(path):
                os.remove(path)

    def test_installs_the_latest_release(self):
        result = self.run_sh("--target", GNU, *self.install_args())
        self.assertExit(result, 0)
        name = self.assertInstalled(LATEST, GNU)
        self.assertEqual(read_log(self.log), [[name, "init", "-config", sh_path(self.config)]])
        self.assertEqual(read_text(self.config), f"fake-config: {name}\n")
        output = result.output
        lines = lines_of(output)
        self.assertIn(f"Downloading open-ferry {LATEST} for {GNU}...", lines)
        self.assertIn(f"Checked {name}.tar.gz against SHA256SUMS.", lines)
        self.assertIn(
            "The build provenance attestation wasn't checked: gh, the GitHub CLI, isn't installed."
            " The SHA256SUMS check passed.",
            lines,
        )
        binary = sh_path(self.bin_dir) + "/open-ferry"
        self.assertIn(f"Installed open-ferry {LATEST} as {binary}.", lines)
        self.assertIn(f"Wrote {sh_path(self.config)}", lines)
        self.assertIn(f"{sh_path(self.bin_dir)} isn't on your PATH.", output)
        self.assertIn(f'  export PATH="{sh_path(self.bin_dir)}:$PATH"', lines)
        config = f'"{sh_path(self.config)}"'
        self.assertIn(f'  Start it:            "{binary}" -config {config}', lines)
        self.assertIn(f'  Or run it at login:  "{binary}" service install -config {config}', lines)
        self.assertIn(
            "  Open the dashboard:  http://127.0.0.1:8317/dashboard/ and sign in with the management key above",
            lines,
        )
        self.assertIn(f'  Check the setup:     "{binary}" check -config {config}', lines)
        self.assertEqual(
            lines[-1], f'open-ferry keeps itself up to date. To turn that off: "{binary}" update -mode off -config {config}'
        )
        self.assertEqual(os.listdir(self.tmp), [], "the temporary directory is left behind")
        self.assertReceipt(LATEST, binary)

    def test_installs_a_given_version(self):
        for flags in (["--version", OLDER], ["--version", "v" + OLDER], ["--version=" + OLDER]):
            with self.subTest(flags=flags):
                self.reset()
                result = self.run_sh("--target", GNU, *flags, *self.install_args())
                self.assertExit(result, 0)
                name = self.assertInstalled(OLDER, GNU)
                self.assertEqual(read_log(self.log), [[name, "init", "-config", sh_path(self.config)]])

    def test_refuses_an_archive_that_does_not_match(self):
        cases = (
            ("--version", ["--version", TAMPERED], SERVER.url("good")),
            ("latest", [], SERVER.url("broken")),
        )
        for label, flags, base_url in cases:
            with self.subTest(label):
                self.reset()
                result = self.run_sh(
                    "--target", GNU, *flags, *self.install_args(), OPEN_FERRY_INSTALL_BASE_URL=base_url
                )
                self.assertExit(result, 1)
                self.assertRegex(
                    result.output,
                    rf"install\.sh: error: open-ferry-{TAMPERED}-{GNU}\.tar\.gz doesn't match SHA256SUMS"
                    r" \(expected [0-9a-f]{64}, got [0-9a-f]{64}\): not installing it",
                )
                self.assertNothingInstalled()

    def test_refuses_a_release_that_does_not_exist(self):
        result = self.run_sh("--target", GNU, "--version", MISSING, *self.install_args())
        self.assertExit(result, 1)
        self.assertIn(f"couldn't download SHA256SUMS for open-ferry {MISSING}: is there a release v{MISSING}?", result.output)
        self.assertNothingInstalled()

    def test_refuses_bad_usage(self):
        cases = (
            ["--bogus"],
            ["--version"],
            ["--version", "1.2"],
            ["--version", "1.2.3.4"],
            ["--bin-dir="],
            ["--target", WINDOWS_TARGET],
            ["--target", "riscv64gc-unknown-linux-gnu"],
        )
        for args in cases:
            with self.subTest(args=args):
                result = self.run_sh(*args, "--config", sh_path(self.config))
                self.assertExit(result, 2)
                self.assertIn("Run install.sh --help for the options.", result.output)
                self.assertNothingInstalled()

    def test_help(self):
        result = self.run_sh("--help")
        self.assertExit(result, 0)
        self.assertIn("Usage: install.sh [options]", result.output)
        self.assertIn("--no-attestation", result.output)

    def test_checks_the_attestation_with_gh(self):
        result = self.run_sh("--target", GNU, *self.install_args(), OPEN_FERRY_INSTALL_GH=sh_path(os.path.join(FAKE_GH, "gh")))
        self.assertExit(result, 0)
        name = self.assertInstalled(LATEST, GNU)
        calls = read_log(self.gh_log)
        self.assertEqual(len(calls), 1, calls)
        self.assertEqual(calls[0][:3], ["gh", "attestation", "verify"])
        self.assertTrue(calls[0][3].endswith(f"/{name}.tar.gz"), calls)
        self.assertEqual(calls[0][4:], ["--repo", REPO])
        self.assertIn("Checking the build provenance attestation with gh...", lines_of(result.output))
        self.assertNotIn("wasn't checked", result.output)

    def test_refuses_an_archive_gh_does_not_verify(self):
        result = self.run_sh(
            "--target", GNU, *self.install_args(),
            OPEN_FERRY_INSTALL_GH=sh_path(os.path.join(FAKE_GH, "gh")), FAKE_GH_EXIT="1",
        )
        self.assertExit(result, 1)
        self.assertIn(f"gh couldn't verify open-ferry-{LATEST}-{GNU}.tar.gz's attestation: not installing it.", result.output)
        self.assertIn("--no-attestation", result.output)
        self.assertNothingInstalled()

    def test_no_attestation(self):
        result = self.run_sh(
            "--target", GNU, *self.install_args("--no-attestation"),
            OPEN_FERRY_INSTALL_GH=sh_path(os.path.join(FAKE_GH, "gh")),
        )
        self.assertExit(result, 0)
        self.assertInstalled(LATEST, GNU)
        self.assertEqual(read_log(self.gh_log), [])
        self.assertIn("Not checking the build provenance attestation (--no-attestation).", lines_of(result.output))

    def test_keeps_an_existing_config(self):
        write_file(self.config, "mine: true\n", mode=0o600)
        result = self.run_sh("--target", GNU, *self.install_args())
        self.assertExit(result, 0)
        self.assertInstalled(LATEST, GNU)
        self.assertEqual(read_text(self.config), "mine: true\n")
        self.assertEqual(read_log(self.log), [], "open-ferry init ran")
        lines = lines_of(result.output)
        self.assertIn(f"Keeping your config at {sh_path(self.config)}.", lines)
        self.assertIn(
            "  Open the dashboard:  http://127.0.0.1:<port>/dashboard/, with your config's port (8317 by default)",
            lines,
        )
        self.assertTrue(lines[-1].startswith("open-ferry keeps itself up to date, unless your config says otherwise."))

    def test_replaces_an_installed_binary(self):
        self.assertExit(self.run_sh("--target", GNU, "--version", OLDER, *self.install_args()), 0)
        self.assertInstalled(OLDER, GNU)
        result = self.run_sh("--target", GNU, *self.install_args())
        self.assertExit(result, 0)
        self.assertInstalled(LATEST, GNU)
        self.assertIn(f"Keeping your config at {sh_path(self.config)}.", lines_of(result.output))
        self.assertReceipt(LATEST, sh_path(self.bin_dir) + "/open-ferry")

    def test_sets_the_update_mode(self):
        config = sh_path(self.config)
        off = f'To turn that off: "{sh_path(self.binary())}" update -mode off -config "{config}"'
        on = f'Automatic updates are off. To turn them on: "{sh_path(self.binary())}" update -mode auto -config "{config}"'
        cases = (
            # options, OPEN_FERRY_INSTALL_SELF_UPDATE, the mode set, the last line
            (["--no-auto-update"], None, "off", on),
            ([], "off", "off", on),
            ([], "notify", "notify", f"open-ferry says when a release is out, but doesn't install it. {off}"),
            ([], "auto", "auto", f"open-ferry keeps itself up to date. {off}"),
            (["--no-auto-update"], "auto", "off", on),
            ([], "", None, f"open-ferry keeps itself up to date. {off}"),
        )
        for flags, variable, mode, last in cases:
            with self.subTest(flags=flags, variable=variable):
                self.reset()
                result = self.run_sh(
                    "--target", GNU, *self.install_args(*flags), OPEN_FERRY_INSTALL_SELF_UPDATE=variable
                )
                self.assertExit(result, 0)
                name = self.assertInstalled(LATEST, GNU)
                calls = [[name, "init", "-config", config]]
                if mode:
                    calls.append([name, "update", "-mode", mode, "-config", config])
                self.assertEqual(read_log(self.log), calls)
                self.assertEqual(lines_of(result.output)[-1], last)

        # With a config kept, only the mode is set.
        self.reset()
        write_file(self.config, "mine: true\n", mode=0o600)
        result = self.run_sh("--target", GNU, *self.install_args("--no-auto-update"))
        self.assertExit(result, 0)
        name = self.assertInstalled(LATEST, GNU)
        self.assertEqual(read_log(self.log), [[name, "update", "-mode", "off", "-config", config]])

    def test_refuses_a_bad_update_mode(self):
        result = self.run_sh("--target", GNU, *self.install_args(), OPEN_FERRY_INSTALL_SELF_UPDATE="sometimes")
        self.assertExit(result, 2)
        self.assertIn('OPEN_FERRY_INSTALL_SELF_UPDATE is "sometimes": use off, notify or auto', result.output)
        self.assertNothingInstalled()

    def test_fails_when_setting_the_update_mode_fails(self):
        result = self.run_sh("--target", GNU, *self.install_args("--no-auto-update"), FAKE_OPEN_FERRY_UPDATE_FAIL="1")
        self.assertExit(result, 1)
        self.assertIn("fake open-ferry: update failed", result.output)
        self.assertIn(
            f"open-ferry update couldn't set self-update.mode to off in {sh_path(self.config)}", result.output
        )

    def test_writes_the_receipt_under_xdg_data_home(self):
        xdg = os.path.join(self.work, "xdg data")
        binary = sh_path(self.bin_dir) + "/open-ferry"
        result = self.run_sh("--target", GNU, *self.install_args(), XDG_DATA_HOME=sh_path(xdg))
        self.assertExit(result, 0)
        self.assertReceipt(LATEST, binary, data_home=xdg)
        self.assertFalse(os.path.exists(self.receipt()))
        # Not absolute, so not counted, as open-ferry doesn't.
        result = self.run_sh("--target", GNU, "--version", OLDER, *self.install_args(), XDG_DATA_HOME="relative/xdg")
        self.assertExit(result, 0)
        self.assertReceipt(OLDER, binary)

    def test_says_when_it_cannot_write_the_receipt(self):
        # A file where the data directory goes.
        blocker = os.path.join(self.home, ".local", "share", "open-ferry")
        write_file(blocker, "not a directory\n", mode=0o644)
        result = self.run_sh("--target", GNU, *self.install_args())
        self.assertExit(result, 0)
        self.assertInstalled(LATEST, GNU)
        receipt = sh_path(self.home) + "/.local/share/open-ferry/install-receipt.json"
        lines = lines_of(result.output)
        self.assertIn(
            f"Couldn't write the install receipt {receipt}, so open-ferry won't update itself;"
            " it will say when a release is out.",
            lines,
        )
        self.assertTrue(lines[-1].startswith("open-ferry says when a release is out. To turn that off: "), lines[-1])
        self.assertEqual(read_text(blocker), "not a directory\n")

    @unittest.skipIf(WINDOWS, "Windows paths can't hold a double quote")
    def test_escapes_the_receipt(self):
        self.bin_dir = os.path.join(self.work, 'bin "q" ' + chr(92) + " dir")
        result = self.run_sh("--target", GNU, *self.install_args())
        self.assertExit(result, 0)
        self.assertReceipt(LATEST, self.bin_dir + "/open-ferry")

    def test_says_nothing_about_path_when_the_directory_is_on_it(self):
        result = self.run_sh("--target", GNU, *self.install_args(), path_first=[self.bin_dir])
        self.assertExit(result, 0)
        self.assertNotIn("isn't on your PATH", result.output)
        lines = lines_of(result.output)
        self.assertIn(f'  Start it:            open-ferry -config "{sh_path(self.config)}"', lines)
        self.assertIn(f'  Or run it at login:  open-ferry service install -config "{sh_path(self.config)}"', lines)

    def test_fails_when_init_fails(self):
        result = self.run_sh("--target", GNU, *self.install_args(), FAKE_OPEN_FERRY_FAIL="1")
        self.assertExit(result, 1)
        self.assertIn("fake open-ferry: init failed", result.output)
        self.assertIn(f"open-ferry init couldn't write {sh_path(self.config)}", result.output)

    def test_runs_piped_to_sh(self):
        url = SERVER.url("good") + "/latest/download/install.sh"
        script = (
            'url=$1; shift; '
            'if command -v curl >/dev/null 2>&1; then curl -fsSL "$url"; else wget -qO- "$url"; fi | sh -s -- "$@"'
        )
        result = run([SH, "-c", script, "sh", url, "--target", GNU, *self.install_args()], self.env())
        self.assertExit(result, 0)
        self.assertInstalled(LATEST, GNU)

    # uname -s, uname -m, glibc, Rosetta, the target, the note on it
    DETECTIONS = (
        ("Linux", "x86_64", "2.39", "", "x86_64-unknown-linux-gnu", None),
        ("Linux", "x86_64", "2.31", "", "x86_64-unknown-linux-gnu", None),
        ("Linux", "x86_64", "2.28", "", "x86_64-unknown-linux-musl",
         "This system's glibc, 2.28, is older than 2.31, so this is the static (musl) build."),
        ("Linux", "x86_64", "2.4", "", "x86_64-unknown-linux-musl", "is older than 2.31"),
        ("Linux", "amd64", "2.36", "", "x86_64-unknown-linux-gnu", None),
        ("Linux", "x86_64", "", "", "x86_64-unknown-linux-musl",
         "This system has no glibc, so this is the static (musl) build."),
        ("Linux", "aarch64", "2.35", "", "aarch64-unknown-linux-gnu", None),
        ("Linux", "aarch64", "", "", "aarch64-unknown-linux-musl", "has no glibc"),
        ("Darwin", "arm64", "", "", "aarch64-apple-darwin", None),
        ("Darwin", "x86_64", "", "", "x86_64-apple-darwin", None),
        ("Darwin", "x86_64", "", "0", "x86_64-apple-darwin", None),
        ("Darwin", "x86_64", "", "1", "aarch64-apple-darwin",
         "This shell runs under Rosetta, so this is the Apple silicon build."),
    )

    def test_detects_the_target(self):
        for system, machine, glibc, translated, target, note in self.DETECTIONS:
            with self.subTest(system=system, machine=machine, glibc=glibc, translated=translated):
                self.reset()
                result = self.run_sh(
                    *self.install_args("--no-attestation"),
                    path_first=[FAKE_BIN],
                    FAKE_UNAME_S=system, FAKE_UNAME_M=machine, FAKE_GLIBC=glibc, FAKE_TRANSLATED=translated,
                )
                self.assertExit(result, 0)
                self.assertInstalled(LATEST, target)
                self.assertIn(f"Downloading open-ferry {LATEST} for {target}...", lines_of(result.output))
                if note:
                    self.assertIn(note, result.output)
                else:
                    self.assertNotIn("this is the", result.output)

    REFUSALS = (
        ("MINGW64_NT-10.0-26100", "x86_64",
         f"on Windows, use install.ps1: irm https://github.com/{REPO}/releases/latest/download/install.ps1 | iex"),
        ("MSYS_NT-10.0-26100", "x86_64", "on Windows, use install.ps1"),
        ("CYGWIN_NT-10.0", "x86_64", "on Windows, use install.ps1"),
        ("Linux", "riscv64", "there's no open-ferry build for riscv64 processors"),
        ("Linux", "armv7l", "there's no open-ferry build for armv7l processors"),
        ("FreeBSD", "amd64", "there's no open-ferry build for FreeBSD"),
    )

    def test_refuses_platforms_without_a_build(self):
        for system, machine, message in self.REFUSALS:
            with self.subTest(system=system, machine=machine):
                result = self.run_sh(
                    *self.install_args(), path_first=[FAKE_BIN],
                    FAKE_UNAME_S=system, FAKE_UNAME_M=machine, FAKE_GLIBC="2.39", FAKE_TRANSLATED="",
                )
                self.assertExit(result, 1)
                self.assertIn(message, result.output)
                self.assertNothingInstalled()

    @unittest.skipUnless(EXPECT_TARGET, "INSTALL_TEST_EXPECT_TARGET isn't set")
    def test_detects_this_system(self):
        result = self.run_sh(*self.install_args("--no-attestation"))
        self.assertExit(result, 0)
        self.assertInstalled(LATEST, EXPECT_TARGET)

    @unittest.skipUnless(DEFAULT_PATHS, "INSTALL_TEST_DEFAULT_PATHS isn't 1")
    def test_default_paths(self):
        xdg = os.path.join(self.work, "xdg")
        cases = (
            # XDG_CONFIG_HOME, the config directory it makes
            (None, os.path.join(self.home, ".config")),
            (sh_path(xdg), xdg),
            # Not absolute, so not counted, as open-ferry init doesn't.
            ("relative/xdg", os.path.join(self.home, ".config")),
        )
        for xdg_config_home, config_home in cases:
            with self.subTest(XDG_CONFIG_HOME=xdg_config_home):
                shutil.rmtree(self.home, ignore_errors=True)
                shutil.rmtree(xdg, ignore_errors=True)
                os.makedirs(self.home)
                result = self.run_sh("--target", GNU, "--no-attestation", XDG_CONFIG_HOME=xdg_config_home)
                self.assertExit(result, 0)
                binary = os.path.join(self.home, ".local", "bin", "open-ferry")
                config = os.path.join(config_home, "open-ferry", "config.yaml")
                name = top_dir(LATEST, GNU)
                self.assertEqual(read_text(binary), FAKE_OPEN_FERRY_SH.replace("@NAME@", name))
                self.assertEqual(read_log(self.log)[-1], [name, "init", "-config", sh_path(config)])
                self.assertTrue(os.path.isfile(config))
                lines = lines_of(result.output)
                self.assertIn(f'  Start it:            {sh_path(binary)} -config "{sh_path(config)}"', lines)
                self.assertIn(f'  Or run it at login:  {sh_path(binary)} service install -config "{sh_path(config)}"', lines)
                self.assertReceipt(LATEST, sh_path(binary))


# --- install.ps1 -------------------------------------------------------------


class InstallPs1Cases:
    """install.ps1's tests, run under each PowerShell (shell)."""

    shell = None

    def setUp(self):
        if not WINDOWS:
            self.skipTest("install.ps1 runs on Windows")
        if not self.shell:
            self.skipTest("not installed")
        self.work = self.make_work()
        # Spaces and a quote, to check the script's quoting.
        self.install_dir = os.path.join(self.work, "Programs dir", "open-ferry")
        self.config = os.path.join(self.work, "App Data", "Matt's open-ferry", "config.yaml")
        self.exe = os.path.join(self.install_dir, "open-ferry.exe")
        self.log = os.path.join(self.work, "open-ferry.log")
        self.gh_log = os.path.join(self.work, "gh.log")
        self.local = os.path.join(self.work, "LocalAppData")
        self.roaming = os.path.join(self.work, "AppData")
        self.tmp = os.path.join(self.work, "tmp")
        for path in (self.local, self.roaming, self.tmp):
            os.makedirs(path)

    def env(self, **extra):
        env = clean_env()
        env.update(
            LOCALAPPDATA=self.local,
            APPDATA=self.roaming,
            TEMP=self.tmp,
            TMP=self.tmp,
            OPEN_FERRY_INSTALL_BASE_URL=SERVER.url("good"),
            OPEN_FERRY_INSTALL_GH=NO_GH,
            FAKE_OPEN_FERRY_LOG=self.log,
            FAKE_GH_LOG=self.gh_log,
        )
        for key, value in extra.items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
        return env

    def shell_args(self):
        return [self.shell, "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"]

    def run_file(self, *args, **extra):
        return run([*self.shell_args(), "-File", INSTALL_PS1, *args], self.env(**extra))

    def run_command(self, command, **extra):
        return run([*self.shell_args(), "-Command", command], self.env(**extra))

    def install_args(self, *more):
        return ["-InstallDir", self.install_dir, "-ConfigPath", self.config, *more]

    def script_url(self):
        return SERVER.url("good") + "/latest/download/install.ps1"

    def assertInstalled(self, version, install_dir=None):
        install_dir = install_dir or self.install_dir
        name = top_dir(version, WINDOWS_TARGET)
        with open(os.path.join(install_dir, "open-ferry.exe"), "rb") as f:
            self.assertTrue(f.read() == EXES[name], f"open-ferry.exe isn't {name}'s")
        self.assertEqual(
            sorted(os.listdir(install_dir)),
            ["LICENSE", "README.md", "config.example.yaml", "licenses", "open-ferry.exe"],
        )
        self.assertEqual(os.listdir(os.path.join(install_dir, "licenses")), ["rust-third-party-licenses.txt"])
        self.assertEqual(
            [entry for entry in os.listdir(self.tmp) if entry.startswith("open-ferry-install-")],
            [],
            "the temporary directory is left behind",
        )
        return name

    def assertNothingInstalled(self):
        self.assertFalse(os.path.exists(self.install_dir))
        self.assertFalse(os.path.exists(self.config))
        self.assertEqual(read_log(self.log), [])
        self.assertFalse(os.path.exists(self.receipt()))

    def receipt(self):
        return os.path.join(self.local, "open-ferry", "install-receipt.json")

    def assertReceipt(self, version, exe=None):
        self.assertEqual(
            read_receipt(self.receipt()),
            {"format": 1, "installer": "install.ps1", "version": version, "binary": exe or self.exe, "target": WINDOWS_TARGET},
        )

    def test_installs_the_latest_release(self):
        result = self.run_file(*self.install_args())
        self.assertExit(result, 0)
        name = self.assertInstalled(LATEST)
        self.assertEqual(read_log(self.log), [[name, "init", "-config", self.config]])
        self.assertEqual(read_text(self.config), f"fake-config: {name}\n")
        lines = lines_of(result.output)
        self.assertIn(f"Downloading open-ferry {LATEST} for {WINDOWS_TARGET}...", lines)
        self.assertIn(f"Checked {name}.zip against SHA256SUMS.", lines)
        self.assertIn(
            "The build provenance attestation wasn't checked: gh, the GitHub CLI, isn't installed."
            " The SHA256SUMS check passed.",
            lines,
        )
        self.assertIn(f"Installed open-ferry {LATEST} as {self.exe}.", lines)
        self.assertIn(f"Wrote {self.config}", lines)
        self.assertIn(f"{self.install_dir} isn't on your PATH.", result.output)
        self.assertIn(
            "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User')"
            f" + ';' + {ps_quote(self.install_dir)}, 'User')",
            lines,
        )
        exe = "& " + ps_quote(self.exe)
        config = ps_quote(self.config)
        self.assertIn(f"  Start it:            {exe} -config {config}", lines)
        self.assertIn(f"  Or run it at login:  {exe} service install -config {config}", lines)
        self.assertIn(
            "  Open the dashboard:  http://127.0.0.1:8317/dashboard/ and sign in with the management key above",
            lines,
        )
        self.assertIn(f"  Check the setup:     {exe} check -config {config}", lines)
        self.assertEqual(lines[-1], f"open-ferry keeps itself up to date. To turn that off: {exe} update -mode off -config {config}")
        self.assertReceipt(LATEST)

    def test_installs_a_given_version(self):
        for version in (OLDER, "v" + OLDER):
            with self.subTest(version=version):
                shutil.rmtree(self.install_dir, ignore_errors=True)
                result = self.run_file("-Version", version, *self.install_args())
                self.assertExit(result, 0)
                self.assertInstalled(OLDER)

    def test_refuses_an_archive_that_does_not_match(self):
        cases = (
            ("-Version", ["-Version", TAMPERED], SERVER.url("good")),
            ("latest", [], SERVER.url("broken")),
        )
        for label, flags, base_url in cases:
            with self.subTest(label):
                result = self.run_file(*flags, *self.install_args(), OPEN_FERRY_INSTALL_BASE_URL=base_url)
                self.assertExit(result, 1)
                self.assertRegex(
                    result.output,
                    rf"install\.ps1: error: open-ferry-{TAMPERED}-{WINDOWS_TARGET}\.zip doesn't match SHA256SUMS"
                    r" \(expected [0-9a-f]{64}, got [0-9a-f]{64}\): not installing it",
                )
                self.assertNothingInstalled()

    def test_refuses_a_release_that_does_not_exist(self):
        result = self.run_file("-Version", MISSING, *self.install_args())
        self.assertExit(result, 1)
        self.assertIn(f"Couldn't download SHA256SUMS for open-ferry {MISSING}: is there a release v{MISSING}?", result.output)
        self.assertNothingInstalled()

    def test_refuses_a_bad_version(self):
        for version in ("1.2", "1.2.3.4", "latest"):
            with self.subTest(version=version):
                result = self.run_file("-Version", version, *self.install_args())
                self.assertExit(result, 1)
                self.assertIn(f"Not a version: {version} (such as 0.1.0)", result.output)
                self.assertNothingInstalled()

    def test_checks_the_attestation_with_gh(self):
        result = self.run_file(*self.install_args(), OPEN_FERRY_INSTALL_GH=os.path.join(FAKE_GH, "gh.cmd"))
        self.assertExit(result, 0)
        name = self.assertInstalled(LATEST)
        calls = read_text(self.gh_log).splitlines()
        self.assertEqual(len(calls), 1, calls)
        self.assertRegex(calls[0], rf"^gh attestation verify \S*\\{re.escape(name)}\.zip --repo {REPO}$")
        self.assertIn("Checking the build provenance attestation with gh...", lines_of(result.output))
        self.assertNotIn("wasn't checked", result.output)

    def test_refuses_an_archive_gh_does_not_verify(self):
        result = self.run_file(
            *self.install_args(), OPEN_FERRY_INSTALL_GH=os.path.join(FAKE_GH, "gh.cmd"), FAKE_GH_EXIT="1"
        )
        self.assertExit(result, 1)
        self.assertIn(f"gh couldn't verify open-ferry-{LATEST}-{WINDOWS_TARGET}.zip's attestation: not installing it.", result.output)
        self.assertIn("-NoAttestation", result.output)
        self.assertNothingInstalled()

    def test_no_attestation(self):
        result = self.run_file(
            *self.install_args("-NoAttestation"), OPEN_FERRY_INSTALL_GH=os.path.join(FAKE_GH, "gh.cmd")
        )
        self.assertExit(result, 0)
        self.assertInstalled(LATEST)
        self.assertFalse(os.path.exists(self.gh_log))
        self.assertIn("Not checking the build provenance attestation (-NoAttestation).", lines_of(result.output))

    def test_keeps_an_existing_config(self):
        write_file(self.config, "mine: true\n", mode=0o600)
        result = self.run_file(*self.install_args())
        self.assertExit(result, 0)
        self.assertInstalled(LATEST)
        self.assertEqual(read_text(self.config), "mine: true\n")
        self.assertEqual(read_log(self.log), [], "open-ferry init ran")
        lines = lines_of(result.output)
        self.assertIn(f"Keeping your config at {self.config}.", lines)
        self.assertTrue(lines[-1].startswith("open-ferry keeps itself up to date, unless your config says otherwise."))

    def test_sets_the_update_mode(self):
        exe = "& " + ps_quote(self.exe)
        config = ps_quote(self.config)
        off = f"To turn that off: {exe} update -mode off -config {config}"
        on = f"Automatic updates are off. To turn them on: {exe} update -mode auto -config {config}"
        cases = (
            # options, OPEN_FERRY_INSTALL_SELF_UPDATE, the mode set, the last line
            (["-NoAutoUpdate"], None, "off", on),
            ([], "off", "off", on),
            ([], "notify", "notify", f"open-ferry says when a release is out, but doesn't install it. {off}"),
            ([], "auto", "auto", f"open-ferry keeps itself up to date. {off}"),
            (["-NoAutoUpdate"], "auto", "off", on),
        )
        for flags, variable, mode, last in cases:
            with self.subTest(flags=flags, variable=variable):
                shutil.rmtree(self.install_dir, ignore_errors=True)
                shutil.rmtree(os.path.dirname(self.config), ignore_errors=True)
                if os.path.exists(self.log):
                    os.remove(self.log)
                result = self.run_file(*self.install_args(*flags), OPEN_FERRY_INSTALL_SELF_UPDATE=variable)
                self.assertExit(result, 0)
                name = self.assertInstalled(LATEST)
                self.assertEqual(
                    read_log(self.log),
                    [[name, "init", "-config", self.config], [name, "update", "-mode", mode, "-config", self.config]],
                )
                self.assertEqual(lines_of(result.output)[-1], last)

        # The environment variable is the form for irm | iex; with a config
        # kept, only the mode is set.
        os.remove(self.log)
        result = self.run_command(
            f"irm {self.script_url()} | iex",
            OPEN_FERRY_INSTALL_DIR=self.install_dir,
            OPEN_FERRY_INSTALL_CONFIG=self.config,
            OPEN_FERRY_INSTALL_SELF_UPDATE="off",
        )
        self.assertExit(result, 0)
        name = self.assertInstalled(LATEST)
        self.assertEqual(read_log(self.log), [[name, "update", "-mode", "off", "-config", self.config]])

    def test_refuses_a_bad_update_mode(self):
        result = self.run_file(*self.install_args(), OPEN_FERRY_INSTALL_SELF_UPDATE="sometimes")
        self.assertExit(result, 1)
        self.assertIn("OPEN_FERRY_INSTALL_SELF_UPDATE is 'sometimes': use off, notify or auto", result.output)
        self.assertNothingInstalled()

    def test_fails_when_setting_the_update_mode_fails(self):
        result = self.run_file(*self.install_args("-NoAutoUpdate"), FAKE_OPEN_FERRY_UPDATE_FAIL="1")
        self.assertExit(result, 1)
        self.assertIn("fake open-ferry: update failed", result.output)
        self.assertIn(f"open-ferry update couldn't set self-update.mode to off in {self.config}", result.output)

    def test_says_when_it_cannot_write_the_receipt(self):
        # A file where the data directory goes.
        blocker = os.path.join(self.local, "open-ferry")
        write_file(blocker, "not a directory\n", mode=0o644)
        result = self.run_file(*self.install_args())
        self.assertExit(result, 0)
        self.assertInstalled(LATEST)
        lines = lines_of(result.output)
        self.assertIn(
            "Couldn't write the install receipt in %LOCALAPPDATA%" + chr(92) + "open-ferry, so open-ferry won't"
            " update itself; it will say when a release is out.",
            lines,
        )
        self.assertTrue(lines[-1].startswith("open-ferry says when a release is out. To turn that off: "), lines[-1])
        self.assertEqual(read_text(blocker), "not a directory\n")

    def test_replaces_a_running_open_ferry(self):
        self.assertExit(self.run_file("-Version", OLDER, *self.install_args()), 0)
        self.assertInstalled(OLDER)
        running = subprocess.Popen(
            [self.exe, "sleep", "120000"],
            env=self.env(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.addCleanup(stop, running)
        self.assertTrue(
            wait_for(lambda: any(line[1:2] == ["sleep"] for line in read_log(self.log))),
            "the fake open-ferry didn't start",
        )

        def aside():
            return [entry for entry in os.listdir(self.install_dir) if entry.endswith(".old")]

        # Twice while it runs: the first install moves the running binary
        # aside, and the second can't delete it yet but installs anyway.
        for _ in range(2):
            result = self.run_file(*self.install_args())
            self.assertExit(result, 0)
            self.assertIn("An older open-ferry.exe is still running: restart it to run this one.", lines_of(result.output))
            self.assertEqual(len(aside()), 1, os.listdir(self.install_dir))
            with open(self.exe, "rb") as f:
                self.assertTrue(f.read() == EXES[top_dir(LATEST, WINDOWS_TARGET)])

        stop(running)
        result = self.run_file(*self.install_args())
        self.assertExit(result, 0)
        self.assertNotIn("still running", result.output)
        self.assertInstalled(LATEST)

    def test_runs_from_irm_and_iex(self):
        result = self.run_command(
            f"irm {self.script_url()} | iex; Write-Host 'The session goes on.'",
            OPEN_FERRY_INSTALL_DIR=self.install_dir,
            OPEN_FERRY_INSTALL_CONFIG=self.config,
        )
        self.assertExit(result, 0)
        name = self.assertInstalled(LATEST)
        self.assertEqual(read_log(self.log), [[name, "init", "-config", self.config]])
        self.assertIn("The session goes on.", lines_of(result.output))

    def test_a_failure_under_iex_leaves_the_session_open(self):
        result = self.run_command(
            f"irm {self.script_url()} | iex; Write-Host 'The session goes on.'",
            OPEN_FERRY_INSTALL_BASE_URL=SERVER.url("broken"),
            OPEN_FERRY_INSTALL_DIR=self.install_dir,
            OPEN_FERRY_INSTALL_CONFIG=self.config,
        )
        self.assertIn("doesn't match SHA256SUMS", result.output)
        self.assertIn("The session goes on.", lines_of(result.output))
        self.assertNothingInstalled()

    def test_runs_from_irm_as_a_script_block_with_options(self):
        result = self.run_command(
            f"& ([scriptblock]::Create((irm {self.script_url()}))) -Version {OLDER}"
            f" -InstallDir {ps_quote(self.install_dir)} -ConfigPath {ps_quote(self.config)} -NoAttestation"
        )
        self.assertExit(result, 0)
        self.assertInstalled(OLDER)
        self.assertIn("Not checking the build provenance attestation (-NoAttestation).", lines_of(result.output))

    def test_options_win_over_the_environment(self):
        other = os.path.join(self.work, "other")
        result = self.run_file(
            *self.install_args(),
            OPEN_FERRY_INSTALL_DIR=other,
            OPEN_FERRY_INSTALL_CONFIG=os.path.join(other, "config.yaml"),
        )
        self.assertExit(result, 0)
        self.assertInstalled(LATEST)
        self.assertFalse(os.path.exists(other))

    def test_says_nothing_about_path_when_the_directory_is_on_it(self):
        result = self.run_file(*self.install_args(), PATH=self.install_dir + "\\;" + os.environ["PATH"])
        self.assertExit(result, 0)
        self.assertNotIn("isn't on your PATH", result.output)
        lines = lines_of(result.output)
        self.assertIn(f"  Start it:            open-ferry -config {ps_quote(self.config)}", lines)
        self.assertIn(f"  Or run it at login:  open-ferry service install -config {ps_quote(self.config)}", lines)

    def test_fails_when_init_fails(self):
        result = self.run_file(*self.install_args(), FAKE_OPEN_FERRY_FAIL="1")
        self.assertExit(result, 1)
        self.assertIn("fake open-ferry: init failed", result.output)
        self.assertIn(f"open-ferry init couldn't write {self.config}", result.output)

    def test_architectures(self):
        cases = (
            # PROCESSOR_ARCHITECTURE, PROCESSOR_ARCHITEW6432, the exit code, a line
            ("AMD64", None, 0, None),
            ("x86", "AMD64", 0, None),  # a 32-bit PowerShell on 64-bit Windows
            ("ARM64", None, 0, "this is the x86_64 build, which Windows 11 runs under emulation"),
            ("x86", None, 1, "There's no open-ferry build for x86 processors"),
        )
        for arch, wow, code, message in cases:
            with self.subTest(arch=arch, wow=wow):
                shutil.rmtree(self.install_dir, ignore_errors=True)
                result = self.run_file(
                    *self.install_args(), PROCESSOR_ARCHITECTURE=arch, PROCESSOR_ARCHITEW6432=wow
                )
                self.assertExit(result, code)
                if message:
                    self.assertIn(message, result.output)
                if code == 0:
                    self.assertInstalled(LATEST)
                else:
                    self.assertFalse(os.path.exists(self.install_dir))

    @unittest.skipUnless(DEFAULT_PATHS, "INSTALL_TEST_DEFAULT_PATHS isn't 1")
    def test_default_paths(self):
        install_dir = os.path.join(self.local, "Programs", "open-ferry")
        exe = os.path.join(install_dir, "open-ferry.exe")
        config = os.path.join(self.roaming, "open-ferry", "config.yaml")
        for label in ("file", "iex"):
            with self.subTest(label):
                shutil.rmtree(install_dir, ignore_errors=True)
                shutil.rmtree(os.path.dirname(config), ignore_errors=True)
                if label == "file":
                    result = self.run_file()
                else:
                    result = self.run_command(f"irm {self.script_url()} | iex")
                self.assertExit(result, 0)
                name = self.assertInstalled(LATEST, install_dir)
                self.assertEqual(read_text(config), f"fake-config: {name}\n")
                lines = lines_of(result.output)
                self.assertIn(f"  Start it:            & {ps_quote(exe)} -config {ps_quote(config)}", lines)
                self.assertIn(f"  Or run it at login:  & {ps_quote(exe)} service install -config {ps_quote(config)}", lines)
                self.assertReceipt(LATEST, exe)


class WindowsPowerShellTests(InstallPs1Cases, Case):
    shell = POWERSHELL


class PowerShell7Tests(InstallPs1Cases, Case):
    shell = PWSH


@unittest.skipIf(WINDOWS or not PWSH, "for pwsh outside Windows")
class InstallPs1ElsewhereTests(Case):
    def test_refuses_to_run(self):
        work = self.make_work()
        env = clean_env()
        env.update(
            OPEN_FERRY_INSTALL_BASE_URL=SERVER.url("good"),
            OPEN_FERRY_INSTALL_GH=NO_GH,
            OPEN_FERRY_INSTALL_DIR=os.path.join(work, "bin"),
            OPEN_FERRY_INSTALL_CONFIG=os.path.join(work, "config.yaml"),
        )
        result = run([PWSH, "-NoLogo", "-NoProfile", "-NonInteractive", "-File", INSTALL_PS1], env)
        self.assertExit(result, 1)
        self.assertIn("install.ps1 is for Windows. On Linux and macOS, use install.sh", result.output)
        self.assertEqual(os.listdir(work), [])


if __name__ == "__main__":
    unittest.main()
