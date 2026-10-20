"""Qualify from an unelevated Windows desktop; keep all evidence in target/.

Prerequisites: Python 3.12, Git for Windows,
Rust MSVC tools, and cargo-about 0.9.2. Run from a disposable clean worktree:
    py -3.12 -I .github/scripts/qualify/windows-local.py
No administrator, temporary account, machine service or hosted-logon probe.
"""

import argparse
import base64
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import urllib.request
import winreg
import zipfile


sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[3]
SCRIPTS = ROOT / ".github/scripts/qualify"
TARGET = "x86_64-pc-windows-msvc"
DATA = Path(os.environ["LOCALAPPDATA"]) / "open-ferry"
INSTALL = Path(os.environ["LOCALAPPDATA"]) / "Programs/open-ferry"
CONFIG = Path(os.environ["APPDATA"]) / "open-ferry"
START = Path(os.environ["APPDATA"]) / "Microsoft/Windows/Start Menu/Programs"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as stream:
        stream.write(value if isinstance(value, str) else json.dumps(value, indent=2) + "\n")


def invoke(args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, cwd=ROOT, **kwargs)


def powershell(script):
    script = "$ProgressPreference = 'SilentlyContinue'\n" + script
    encoded = base64.b64encode(script.encode("utf-16le")).decode("ascii")
    output = subprocess.check_output(
        ["powershell.exe", "-NoProfile", "-NonInteractive", "-EncodedCommand", encoded],
        text=True, encoding="utf-8", errors="strict")
    return json.loads(output)


def registry(path, names=None):
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, path) as key:
            values = {}
            for index in range(winreg.QueryInfoKey(key)[1]):
                name, value, kind = winreg.EnumValue(key, index)
                if names is None or name.lower() in names:
                    values[name] = {"type": kind, "value": value.hex() if isinstance(value, bytes) else value}
            return values
    except FileNotFoundError:
        return None


def tree(path):
    if not path.exists():
        return None
    result = {}
    for entry in [path, *sorted(path.rglob("*"))]:
        relative = str(entry.relative_to(path))
        info = entry.lstat()
        if info.st_file_attributes & 0x400:  # Never traverse or remove reparse points.
            raise RuntimeError(f"refusing a reparse point in the snapshot: {entry}")
        if entry.is_dir():
            result[relative] = {"directory": True}
        else:
            with entry.open("rb") as stream:
                result[relative] = {"size": info.st_size, "sha256": hashlib.file_digest(stream, "sha256").hexdigest()}
    return result


def snapshot():
    tasks = powershell("""
        $ErrorActionPreference = 'Stop'
        [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
        $items = @(Get-ScheduledTask | Where-Object {
            $_.TaskName -like '*ferry*' -or $_.TaskPath -like '*ferry*'
        } | Sort-Object TaskPath,TaskName | ForEach-Object {
            @{ name = $_.TaskName; path = $_.TaskPath;
               xml = (Export-ScheduledTask -TaskName $_.TaskName -TaskPath $_.TaskPath) }
        })
        ConvertTo-Json -InputObject $items -Depth 6
    """)
    start = {str(p.relative_to(START)): tree(p) for p in sorted(START.rglob("*"))
             if "open-ferry" in p.name.lower()}
    return {"path": registry("Environment", {"path"}),
            "run": registry(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            "tasks": tasks, "start_menu": start,
            "data": tree(DATA), "install": tree(INSTALL), "config": tree(CONFIG)}


def installed_processes():
    paths = powershell("""
        $ErrorActionPreference = 'Stop'
        [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
        $paths = @(Get-CimInstance Win32_Process -Filter "Name='open-ferry.exe'" |
            ForEach-Object { $_.ExecutablePath } | Where-Object { $_ })
        ConvertTo-Json -InputObject $paths
    """)
    return [path for path in paths if any(Path(path).is_relative_to(root) for root in (INSTALL, DATA))]


def preflight():
    require(os.name == "nt" and sys.version_info[:2] == (3, 12), "use Windows Python 3.12")
    require(not ctypes.windll.shell32.IsUserAnAdmin(), "refusing an elevated token")
    require(Path.cwd().resolve() == ROOT, "run from the repository root")
    require(not any(os.environ.get(name) for name in ("OPEN_FERRY_INSTALL_DIR", "OPEN_FERRY_INSTALL_CONFIG")),
            "unset install-directory/config overrides before local qualification")
    require(not INSTALL.exists() and not CONFIG.exists() and not shutil.which("open-ferry"),
            "refusing an existing open-ferry installation or config")
    if DATA.exists():
        require(DATA.is_dir() and all(p.name == "claude-cli" and p.is_dir() for p in DATA.iterdir()),
                "refusing existing open-ferry data or update records")
    require(not installed_processes(), "refusing an existing installed open-ferry process")
    before = snapshot()
    require(not before["tasks"] and not before["start_menu"], "refusing existing ferry tasks or Start-menu entries")
    require(not any("open-ferry" in str(value).lower() for value in (before["run"] or {}).values()),
            "refusing existing open-ferry Run entries")
    return before


def prepare_node(work, env):
    version = (ROOT / "dashboard/.nvmrc").read_text().strip()
    node = shutil.which("node.exe", path=env["PATH"])
    if (node and (Path(node).parent / "npm.cmd").is_file()
            and subprocess.check_output([node, "--version"], text=True).strip() == "v" + version):
        return Path(node).parent
    # Like setup-node in CI, use the repository's exact version. A portable
    # copy changes only this run's PATH, leaving the installed Node untouched.
    tools = work / "tools"
    tools.mkdir()
    name = f"node-v{version}-win-x64.zip"
    base = f"https://nodejs.org/dist/v{version}/"
    with urllib.request.urlopen(base + "SHASUMS256.txt", timeout=90) as response:
        sums = response.read().decode("ascii")
    expected = next(line.split()[0] for line in sums.splitlines() if line.split()[-1] == name)
    archive = tools / name
    with urllib.request.urlopen(base + name, timeout=90) as response, archive.open("wb") as output:
        shutil.copyfileobj(response, output)
    with archive.open("rb") as stream:
        require(hashlib.file_digest(stream, "sha256").hexdigest() == expected, "portable Node checksum mismatch")
    with zipfile.ZipFile(archive) as package:
        require(all((tools / member.filename).resolve().is_relative_to(tools.resolve())
                    for member in package.infolist()), "portable Node archive escaped its directory")
        package.extractall(tools)
    node_dir = tools / f"node-v{version}-win-x64"
    require(subprocess.check_output([str(node_dir / "node.exe"), "--version"], text=True).strip() == "v" + version,
            "portable Node version mismatch")
    env["PATH"] = str(node_dir) + os.pathsep + env["PATH"]
    write(work / "node-download.json", {"version": version, "archive": name, "sha256": expected})
    return node_dir


def prepare(work, env):
    node_dir = prepare_node(work, env)
    about = subprocess.check_output(["cargo", "about", "--version"], text=True).strip()
    require(about == "cargo-about 0.9.2", "install cargo-about 0.9.2 with --locked --features cli")
    python = work / "venv/Scripts/python.exe"
    invoke([sys.executable, "-I", "-m", "venv", work / "venv"])
    invoke([python, "-I", "-m", "pip", "install", "--disable-pip-version-check", "--require-hashes",
            "--only-binary=:all:", "-r", SCRIPTS / "requirements.txt"], env=env)
    subprocess.run([str(node_dir / "npm.cmd"), "ci", "--ignore-scripts"], cwd=ROOT / "dashboard", env=env, check=True)
    subprocess.run([str(node_dir / "npm.cmd"), "run", "build"], cwd=ROOT / "dashboard", env=env, check=True)
    invoke(["cargo", "fetch", "--locked"], env=env)
    # cargo-about rejects stdout redirection beneath any PowerShell ancestor.
    # Its supported output-file path preserves the same CI license data/rendering.
    license_json = work / "rust-licenses.json"
    invoke(["cargo", "about", "generate", "--frozen", "--fail", "--config", ROOT / "about.toml",
            "--manifest-path", ROOT / "crates/open-ferry/Cargo.toml", "--target", TARGET,
            "--format", "json", "--output-file", license_json], env=env)
    spec = importlib.util.spec_from_file_location("rust_licenses", ROOT / ".github/scripts/rust-licenses.py")
    licenses = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(licenses)
    write(ROOT / "rust-licenses" / f"{TARGET}.txt", licenses.render(TARGET, json.loads(license_json.read_bytes())))
    invoke([python, "-I", SCRIPTS / "test_run.py"], env=env)
    return python


def restore(before, work):
    """Use product service uninstall first, then remove only new install/data paths."""
    errors, removed = [], []
    exe = INSTALL / "open-ferry.exe"
    if exe.exists():
        result = subprocess.run([str(exe), "service", "uninstall"], capture_output=True, text=True, timeout=120)
        write(work / "service-uninstall.txt", result.stdout + result.stderr)
    # Do not erase evidence/files under a service that failed to stop.
    current_tasks = snapshot()["tasks"]
    require(not installed_processes() and current_tasks == before["tasks"],
            "product uninstall left a process or task; refusing file removal")
    for root, original in ((INSTALL, before["install"]), (DATA, before["data"]), (CONFIG, before["config"])):
        if not root.exists():
            continue
        require(not root.is_symlink() and not root.is_junction(), f"refusing linked cleanup root: {root}")
        candidates = [root] if original is None else [p for p in root.iterdir() if str(p.relative_to(root)) not in original]
        for entry in candidates:
            require(entry == root or entry.resolve().parent == root.resolve(), "cleanup escaped its named directory")
            require(entry.name != "claude-cli", "refusing to touch claude-cli")
            tree(entry)  # Refuse any reparse point before recursive removal.
            removed.append(str(entry))
            if entry.is_dir():
                shutil.rmtree(entry)
            else:
                entry.unlink()
    after = snapshot()
    write(work / "after.json", after)
    changed = [name for name in before if before[name] != after[name]]
    write(work / "restoration.json", {"matches": not changed, "changed": changed, "removed": removed, "errors": errors})
    require(not changed, "snapshot mismatch: " + ", ".join(changed))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reuse", type=Path, help="reuse candidates from a prior local attempt after fixing a harness bug")
    args = parser.parse_args()
    before = preflight()
    work = ROOT / "target/qualification" / time.strftime("local-%Y%m%d-%H%M%S")
    work.mkdir(parents=True)
    write(work / "before.json", before)
    write(work / "identity.txt", subprocess.check_output(["whoami", "/groups"], text=True))
    print(f"Local evidence: {work}", flush=True)
    env = dict(os.environ, RUNNER_OS="Windows", CARGO_BUILD_JOBS="4", CARGO_INCREMENTAL="0",
               OPEN_FERRY_REQUIRE_DASHBOARD="1", CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="-C target-feature=+crt-static")
    require(not env.get("RUSTFLAGS") and not env.get("CARGO_ENCODED_RUSTFLAGS"), "unset overriding Rust flags")
    env.pop("QUALIFY_FAULT", None)
    candidates = work / "candidates"
    result = {"passed": False, "errors": []}
    try:
        python = prepare(work, env)
        if args.reuse:
            source = args.reuse.resolve()
            require(source.is_relative_to(ROOT / "target"), "reuse must stay inside this worktree's target")
            # Keep the first attempt's diagnostics and config untouched. Copy
            # only public candidate inputs into a new, empty run directory.
            for relative in (Path(), Path("controls/untrusted-key"), Path("controls/bad-signature")):
                destination = candidates / relative
                shutil.copytree(source / relative / "mirror", destination / "mirror")
                shutil.copyfile(source / relative / "candidates.json", destination / "candidates.json")
        else:
            try:
                invoke([python, "-I", SCRIPTS / "build.py", candidates, "--controls"], env=env)
            finally:
                invoke(["git", "diff", "--exit-code", "--", "Cargo.toml", "Cargo.lock", "release-keys.pub"])
        runs = [candidates, *(candidates / "controls" / fault for fault in ("untrusted-key", "bad-signature"))]
        for number, candidate in enumerate(runs, 1):
            run_evidence = work / f"run-{number}"
            run_evidence.mkdir()
            write(run_evidence / "before.json", before)
            try:
                fault = json.loads((candidate / "candidates.json").read_text(encoding="utf-8"))["fault"]
                invoke([python, "-I", SCRIPTS / "run.py", candidate], env=dict(env, QUALIFY_FAULT=fault))
            finally:
                restore(before, run_evidence)
        result["passed"] = True
    except Exception as error:
        result["errors"].append(f"{type(error).__name__}: {error}")
        try:
            write(work / "system-load.json", powershell("""
                [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
                Get-CimInstance Win32_Processor | Select-Object Name,LoadPercentage,NumberOfLogicalProcessors |
                    ConvertTo-Json
            """))
        except Exception as load_error:
            result["errors"].append(f"load evidence: {load_error}")
    finally:
        try:
            restore(before, work)
        except Exception as error:
            result["passed"] = False
            result["errors"].append(f"restoration: {type(error).__name__}: {error}")
        write(work / "local-result.json", result)
    require(result["passed"], "; ".join(result["errors"]))
    print("PASS: lifecycle, both signature rejections, and matching pre-run snapshot", flush=True)


if __name__ == "__main__":
    main()
