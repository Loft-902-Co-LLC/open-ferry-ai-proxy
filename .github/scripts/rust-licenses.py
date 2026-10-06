#!/usr/bin/env python3
"""Writes the licenses of the Rust crates built into open-ferry.

Usage: rust-licenses.py OUT_DIR [TARGET...]

For each target (by default, every target the release workflow builds),
runs cargo-about on the open-ferry package's dependency graph for that
target, normal and build dependencies alike, and writes OUT_DIR/TARGET.txt.
The file holds:
- each license text cargo-about found, with the crates it covers;
- a crate's own license files, as it ships them, where cargo-about fell back
  to a license's standard text for it, so that their copyright lines are kept;
- every NOTICE file at the root of a crate, whatever its license.

cargo-about takes its settings from about.toml. It fails when a crate's
license can't be met from the accepted list there, and this script then
fails too. It runs with --frozen: fetch the crates first with
`cargo fetch --locked`; nothing is fetched from the crates' repositories.
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# The release workflow's targets (.github/workflows/release.yml).
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)

# License and notice files at the root of a crate, by name.
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|copyright)([._-].*)?$", re.IGNORECASE)
NOTICE_FILE = re.compile(r"^notices?([._-].*)?$", re.IGNORECASE)

RULE = "-" * 78


def about(target):
    """cargo-about's JSON for open-ferry's build for target."""
    command = [
        "cargo",
        "about",
        "generate",
        "--frozen",
        "--fail",
        "--config",
        str(ROOT / "about.toml"),
        "--manifest-path",
        str(ROOT / "crates" / "open-ferry" / "Cargo.toml"),
        "--target",
        target,
        "--format",
        "json",
    ]
    result = subprocess.run(command, cwd=ROOT, stdout=subprocess.PIPE, check=False)
    if result.returncode != 0:
        sys.exit(f"cargo-about failed for {target} (exit code {result.returncode})")
    return json.loads(result.stdout)


def crate_key(krate):
    return (krate["name"], krate["version"])


def crate_line(krate):
    line = f"{krate['name']} {krate['version']}"
    if krate.get("repository"):
        line += f" <{krate['repository']}>"
    return line


def read_text(path):
    text = path.read_text(encoding="utf-8", errors="replace")
    return text.replace("\r\n", "\n").strip()


def root_files(krate, pattern):
    """The files at the root of krate's source whose names match pattern."""
    root = Path(krate["manifest_path"]).parent
    return sorted(
        entry for entry in root.iterdir() if entry.is_file() and pattern.match(entry.name)
    )


def render(target, data):
    crates = {crate_key(c["package"]): c for c in data["crates"]}
    if not crates:
        sys.exit(f"cargo-about listed no crates for {target}")

    licenses = sorted(
        data["licenses"],
        key=lambda lic: (
            lic["id"],
            sorted(crate_key(used["crate"]) for used in lic["used_by"]),
        ),
    )
    # Crates for which cargo-about used a license's standard text, having not
    # matched the crate's own license file to it.
    standard_text = set()
    for lic in licenses:
        if not lic.get("source_path"):
            standard_text.update(crate_key(used["crate"]) for used in lic["used_by"])

    lines = [
        f"open-ferry: third-party Rust crates ({target})",
        "",
        f"The open-ferry binary for {target} is built from the {len(crates)} Rust",
        "crates below, besides open-ferry's own, which are under its LICENSE. Each",
        "license text is listed with the crates it covers. Where a crate offers a",
        "choice of licenses, the one used is listed.",
        "",
        "Licenses:",
    ]
    for entry in sorted(data["overview"], key=lambda o: (-o["count"], o["id"])):
        lines.append(f"  {entry['id']}: {entry['count']}")
    lines.append("")

    for lic in licenses:
        lines += [RULE, f"{lic['name']} ({lic['id']})", "", "Used by:"]
        users = sorted({crate_key(u["crate"]): u["crate"] for u in lic["used_by"]}.items())
        lines += [f"  {crate_line(krate)}" for _, krate in users]
        lines.append("")
        if not lic.get("source_path"):
            lines += [
                "The license's standard text. The license files these crates ship",
                "are reproduced as they are under \"License files\" below.",
                "",
            ]
        lines += [lic["text"].replace("\r\n", "\n").strip(), ""]

    if standard_text:
        lines += [
            RULE,
            "License files",
            "",
            "The license files of the crates above whose license was given in its",
            "standard text, as the crates ship them.",
            "",
        ]
        for key in sorted(standard_text & crates.keys()):
            krate = crates[key]["package"]
            files = root_files(krate, LICENSE_FILE)
            lines += [RULE, crate_line(krate), f"License: {crates[key]['license']}", ""]
            if not files:
                lines += ["The crate ships no license file.", ""]
            for path in files:
                lines += [f"[{path.name}]", "", read_text(path), ""]

    notices = [
        (key, path)
        for key, entry in sorted(crates.items())
        for path in root_files(entry["package"], NOTICE_FILE)
    ]
    if notices:
        lines += [
            RULE,
            "NOTICE files",
            "",
            "The NOTICE files of the crates above, as they ship them.",
            "",
        ]
        for key, path in notices:
            lines += [RULE, crate_line(crates[key]["package"]), f"[{path.name}]", ""]
            lines += [read_text(path), ""]

    lines += [RULE, ""]
    return "\n".join(lines)


def main(argv):
    if len(argv) < 2:
        sys.exit(__doc__)
    out_dir = Path(argv[1])
    targets = argv[2:] or list(TARGETS)
    out_dir.mkdir(parents=True, exist_ok=True)
    for target in targets:
        text = render(target, about(target))
        path = out_dir / f"{target}.txt"
        with open(path, "w", encoding="utf-8", newline="\n") as file:
            file.write(text)
        print(f"{path}: {len(text)} bytes", file=sys.stderr)


if __name__ == "__main__":
    main(sys.argv)
