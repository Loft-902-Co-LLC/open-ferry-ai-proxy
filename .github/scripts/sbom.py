#!/usr/bin/env python3
"""Write release CycloneDX JSON: sbom.py OUT_DIR [RUST_TARGET...].

Install cargo-cyclonedx 0.5.9 and @cyclonedx/cyclonedx-npm 6.0.1 first,
and run cargo fetch --locked. No compilation or dashboard/node_modules needed.
"""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)


def check_bom(path):
    bom = json.loads(path.read_text(encoding="utf-8"))
    if bom.get("bomFormat") != "CycloneDX" or bom.get("specVersion") != "1.5":
        sys.exit(f"Not CycloneDX 1.5: {path}")
    components = bom.get("components", [])
    if not components:
        sys.exit(f"Empty SBOM: {path}")
    print(f"{path.name}: {len(components)} components (root application separate)", flush=True)


def production_bom(bom, snapshot, target, env):
    # cargo metadata unifies workspace/dev features, so cargo-cyclonedx can
    # overinclude optional and test-only transitive packages. Cargo tree with
    # only normal/build edges gives the shipped package's actual release graph.
    tree = subprocess.check_output(
        [
            "cargo", "tree", "--locked", "--color", "never", "-p", "open-ferry", "--target", target,
            "--edges", "normal,build", "--prefix", "depth", "--format", "{p}",
        ],
        cwd=snapshot, env=env, text=True,
    )
    components = bom["components"]
    root = bom["metadata"]["component"]
    by_package = {(c["name"], c["version"]): c for c in [root, *components]}
    if len(by_package) != len(components) + 1:
        sys.exit("Ambiguous package name/version in generated SBOM")
    graph, parents = {}, []
    for line in tree.splitlines():
        match = re.fullmatch(r"(\d+)(\S+) v(\S+)(?: .*)?", line)
        if not match:
            sys.exit(f"Unexpected cargo tree line: {line}")
        depth, name, version = match.groups()
        depth = int(depth)
        component = by_package.get((name, version))
        if component is None:
            sys.exit(f"cargo-cyclonedx omitted {name} {version}")
        ref = component["bom-ref"]
        graph.setdefault(ref, set())
        if depth > len(parents):
            sys.exit("Unexpected cargo tree depth")
        parents[depth:] = []
        if parents:
            graph[parents[-1]].add(ref)
        elif ref != root["bom-ref"]:
            sys.exit("Unexpected cargo tree root")
        parents.append(ref)
    if root["bom-ref"] not in graph:
        sys.exit("Empty production dependency graph")
    bom["components"] = [c for c in components if c["bom-ref"] in graph]
    bom["dependencies"] = [
        {"ref": ref, "dependsOn": sorted(dependencies)}
        for ref, dependencies in sorted(graph.items())
    ]
    return bom


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    out = Path(sys.argv[1]).resolve()
    targets = sys.argv[2:] or TARGETS
    if any(target not in TARGETS for target in targets):
        sys.exit("Target must be one of the release targets")
    out.mkdir(parents=True, exist_ok=True)

    # cargo-cyclonedx emits files for EVERY workspace member, even with a
    # member's --manifest-path. Work in a disposable snapshot of tracked Rust
    # sources, including current lockfile edits, so it cannot dirty the tree.
    tracked = subprocess.check_output(
        ["git", "ls-files", "-z", "--", "Cargo.toml", "Cargo.lock", "crates", "tools"],
        cwd=ROOT,
    ).decode().split("\0")
    with tempfile.TemporaryDirectory(prefix="open-ferry-sbom-") as directory:
        snapshot = Path(directory)
        for relative in filter(None, tracked):
            destination = snapshot / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, destination)
        locked = (snapshot / "Cargo.lock").read_bytes()
        env = dict(os.environ, CARGO_NET_OFFLINE="true")
        for target in targets:
            subprocess.run(
                [
                    "cargo", "cyclonedx", "--manifest-path", "crates/open-ferry/Cargo.toml",
                    "--format", "json", "--spec-version", "1.5", "--target", target,
                    "--override-filename", "release-sbom",
                ],
                cwd=snapshot, env=env, check=True,
            )
            source = snapshot / "crates/open-ferry/release-sbom.json"
            bom = production_bom(json.loads(source.read_text(encoding="utf-8")), snapshot, target, env)
            # The tool has no --locked flag. Offline metadata plus this check
            # fails closed if it ever resolves anything outside Cargo.lock.
            if (snapshot / "Cargo.lock").read_bytes() != locked:
                sys.exit("cargo-cyclonedx changed Cargo.lock")
            path = out / f"open-ferry-sbom-rust-{target}.cdx.json"
            path.write_text(json.dumps(bom, indent=2) + "\n", encoding="utf-8", newline="\n")
            check_bom(path)

    npm_tool = shutil.which("cyclonedx-npm")
    if not npm_tool:
        sys.exit("Install @cyclonedx/cyclonedx-npm@6.0.1 first")
    path = out / "open-ferry-sbom-dashboard.cdx.json"
    subprocess.run(
        [
            npm_tool, "--package-lock-only", "--omit", "dev", "--flatten-components",
            "--spec-version", "1.5", "--output-format", "JSON", "--output-file", str(path),
            "--", "package.json",
        ],
        cwd=ROOT / "dashboard", check=True,
    )
    check_bom(path)


if __name__ == "__main__":
    main()
