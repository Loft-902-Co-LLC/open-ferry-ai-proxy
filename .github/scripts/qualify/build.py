"""Build two native, no-seam releases. The throwaway private key exists only in RAM.

Run from the checkout with python -I. Never upload the checkout or the mirror:
only the explicitly selected diagnostics directory is an artifact.
"""

import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey


ROOT = Path.cwd()
WORK = Path(sys.argv[1]).resolve()
VERSIONS = ("0.0.900", "0.0.901")


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as stream:
        stream.write(text)


def run(command, **kwargs):
    subprocess.run(command, check=True, **kwargs)


def sign(private, key_id, data, version):
    # https://jedisct1.github.io/minisign/#signature-format
    signature = private.sign(hashlib.blake2b(data).digest())
    comment = f"open-ferry {version} SHA256SUMS".encode("ascii")
    global_signature = private.sign(signature + comment)
    encode = lambda value: base64.b64encode(value).decode("ascii")
    return (
        "untrusted comment: throwaway qualification signature\n"
        + encode(b"ED" + key_id + signature)
        + "\ntrusted comment: " + comment.decode("ascii") + "\n"
        + encode(global_signature) + "\n"
    )


def candidate_signature(private, key_id, data, version, fault):
    signer, signer_id = private, key_id
    if version == VERSIONS[1] and fault != "none":
        signer = Ed25519PrivateKey.generate()
        if fault == "untrusted-key":
            signer_id = os.urandom(8)
            while signer_id == key_id:
                signer_id = os.urandom(8)
        elif fault != "bad-signature":
            raise RuntimeError("unknown qualification fault")
        # bad-signature retains the trusted ID but signs both the prehash and
        # trusted comment with the other key. The format stays valid; the real
        # verifier selects the trusted key and rejects the Ed25519 signature.
    return sign(signer, signer_id, data, version)


def package_shell():
    if os.name != "nt":
        return os.environ.get("QUALIFY_BASH", "bash")
    # PATH's bash.exe may be WSL. Resolve Git for Windows explicitly.
    git = shutil.which("git.exe")
    shell = Path(os.environ.get("QUALIFY_BASH", "") or
                 (Path(git).resolve().parents[1] / "bin/bash.exe" if git else ""))
    if not shell.is_absolute() or not shell.is_file():
        raise RuntimeError("set QUALIFY_BASH to the full Git for Windows bin/bash.exe path")
    return str(shell)


def main():
    WORK.mkdir(parents=True, exist_ok=False)
    diagnostics = WORK / "diagnostics"
    diagnostics.mkdir()
    host = subprocess.check_output(["rustc", "-vV"], text=True)
    target = re.search(r"^host: (.+)$", host, re.MULTILINE)[1]
    expected = {
        "Linux": "x86_64-unknown-linux-gnu",
        "Windows": "x86_64-pc-windows-msvc",
        "macOS": "aarch64-apple-darwin",
    }[os.environ["RUNNER_OS"]]
    if target != expected:
        raise RuntimeError(f"qualification must build natively: {target} != {expected}")

    originals = {name: (ROOT / name).read_bytes()
                 for name in ("Cargo.toml", "Cargo.lock", "release-keys.pub")}
    manifest = originals["Cargo.toml"].decode("utf-8")
    lock = originals["Cargo.lock"].decode("utf-8")
    workspace = tomllib.loads(manifest)["workspace"]
    local_names = set()
    for pattern in workspace["members"]:
        for member in ROOT.glob(pattern):
            package = tomllib.loads((member / "Cargo.toml").read_text(encoding="utf-8"))["package"]
            if isinstance(package.get("version"), dict) and package["version"].get("workspace"):
                local_names.add(package["name"])

    private = Ed25519PrivateKey.generate()
    key_id = os.urandom(8)
    public = base64.b64encode(b"Ed" + key_id + private.public_key().public_bytes_raw()).decode("ascii")
    try:
        write(ROOT / "release-keys.pub", "# Throwaway qualification key, never commit.\n" + public + "\n")
        for version in VERSIONS:
            # Only workspace-owned versions change; every registry checksum stays locked.
            changed, count = re.subn(
                r'(\[workspace\.package\]\s*\nversion = ")[^"]+(".*)',
                lambda match: match[1] + version + match[2], manifest, count=1,
            )
            if count != 1:
                raise RuntimeError("could not identify the workspace version")
            write(ROOT / "Cargo.toml", changed)
            blocks = lock.split("[[package]]")
            changed_names = set()
            for index, block in enumerate(blocks[1:], 1):
                package = tomllib.loads("[[package]]" + block)["package"][0]
                if package["name"] in local_names and "source" not in package:
                    blocks[index], count = re.subn(r'^version = "[^"]+"',
                                                  f'version = "{version}"', block, count=1,
                                                  flags=re.MULTILINE)
                    if count != 1:
                        raise RuntimeError("could not rewrite a workspace lock version")
                    changed_names.add(package["name"])
            if changed_names != local_names:
                raise RuntimeError("workspace packages and lock entries disagree")
            write(ROOT / "Cargo.lock", "[[package]]".join(blocks))
            env = dict(os.environ, VERSION=version, TARGET=target)
            events = diagnostics / f"build-{version}.jsonl"
            with events.open("w", encoding="utf-8", newline="\n") as output:
                run(["cargo", "build", "--release", "--locked", "-p", "open-ferry",
                     "--target", target, "--message-format=json"], env=env, stdout=output)
            run([sys.executable, "-I", "crates/open-ferry/check-release.py", str(events)])
            # Packaging emits a release step output. Keep qualification's output
            # separate, including when this script runs outside GitHub Actions.
            with tempfile.TemporaryDirectory(prefix="open-ferry-package-") as scratch:
                env["GITHUB_OUTPUT"] = str(Path(scratch) / "output")
                run([package_shell(), ".github/scripts/package-release.sh"], env=env)
            suffix = ".zip" if os.name == "nt" else ".tar.gz"
            archive = f"open-ferry-{version}-{target}{suffix}"
            destination = WORK / "mirror" / "download" / f"v{version}"
            destination.mkdir(parents=True)
            shutil.move(str(ROOT / "dist" / archive), destination / archive)
            for script in ("install.sh", "install.ps1"):
                shutil.copyfile(ROOT / script, destination / script)
            sums = "".join(
                hashlib.sha256((destination / name).read_bytes()).hexdigest() + "  " + name + "\n"
                for name in sorted((archive, "install.sh", "install.ps1"))
            )
            write(destination / "SHA256SUMS", sums)
            write(destination / "SHA256SUMS.minisig", candidate_signature(
                private, key_id, sums.encode(), version, os.environ.get("QUALIFY_FAULT", "none")))
            print(f"Packaged native candidate {version} for {target}", flush=True)
        write(WORK / "candidates.json", json.dumps({
            "versions": VERSIONS, "target": target,
            "fault": os.environ.get("QUALIFY_FAULT", "none"),
            "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        }, indent=2) + "\n")
        if "--controls" in sys.argv[2:]:
            if os.environ.get("QUALIFY_FAULT", "none") != "none":
                raise RuntimeError("--controls requires an unfaulted primary mirror")
            # Reuse these exact binaries while the trusted signing key is still
            # in memory; never persist it just to prepare the negative controls.
            for fault in ("untrusted-key", "bad-signature"):
                control = WORK / "controls" / fault
                shutil.copytree(WORK / "mirror", control / "mirror")
                destination = control / "mirror/download" / f"v{VERSIONS[1]}"
                write(destination / "SHA256SUMS.minisig", candidate_signature(
                    private, key_id, (destination / "SHA256SUMS").read_bytes(), VERSIONS[1], fault))
                candidates = json.loads((WORK / "candidates.json").read_text(encoding="utf-8"))
                candidates["fault"] = fault
                write(control / "candidates.json", json.dumps(candidates, indent=2) + "\n")
    finally:
        # Restore even on a failed build. No changed keys or manifests survive the step.
        for name, data in originals.items():
            (ROOT / name).write_bytes(data)


if __name__ == "__main__":
    main()
