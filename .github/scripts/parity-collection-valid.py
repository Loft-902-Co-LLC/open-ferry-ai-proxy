#!/usr/bin/env python3
"""Check committed calibration inputs, without building or running either proxy."""

import argparse
import fnmatch
import json
from pathlib import Path, PurePosixPath
import subprocess
import sys


# One policy table: each row names the inputs and why their identity matters.
# A path belongs to the first row that matches it.
# The workspace crates the parity tool links, directly or through each other;
# a test derives this set from the Cargo manifests.
CLOSURE = ("crates/open-ferry-core", "crates/open-ferry-providers", "crates/open-ferry-translate")
GROUPS = (
    ("config", ("crates/open-ferry-core/src/config", "config.example.yaml")),  # Load/save/writer verdicts; the root example is a case.
    ("parity", ("tools/parity",)),  # Generators, classifiers, harnesses; exclude docs/baseline.
    ("build", ("Cargo.lock", "Cargo.toml", "*/Cargo.toml", "rust-toolchain*", ".cargo")),  # Dependencies/features/compiler.
    ("crates", CLOSURE),  # Other linked code, such as shared JSON and Go-compatibility helpers.
    ("upstream", ("reference/cliproxyapi",)),  # Exact upstream oracle gitlink.
)
PIN = "reference/cliproxyapi"
FINGERPRINT_KEYS = {"classifier_version", "upstream_pin", "generator_sha256"}


def git(root, *args):
    result = subprocess.run(
        ["git", "-C", str(root), *args], capture_output=True, encoding="utf-8"
    )
    if result.returncode:
        raise ValueError(result.stderr.strip() or "git command failed")
    return result.stdout.rstrip("\n")


def commit(root, rev):
    return git(root, "rev-parse", "--verify", "--end-of-options", rev + "^{commit}")


def matches(pattern, path):
    return path == pattern or path.startswith(pattern + "/") or fnmatch.fnmatchcase(path, pattern)


def included(group, path):
    # Documentation inside the code directories never reaches a verdict.
    if group not in {"parity", "crates"}:
        return True
    parts = PurePosixPath(path).parts[2:]
    return not (
        (group == "parity" and parts[0] == "baseline")
        or any(part.lower() in {"doc", "docs"} for part in parts[:-1])
        or PurePosixPath(path).name.lower().startswith("readme")
        or PurePosixPath(path).suffix.lower() in {".md", ".markdown"}
    )


def group_of(path):
    for name, patterns in GROUPS:
        if any(matches(pattern, path) for pattern in patterns):
            return name if included(name, path) else None
    return None


def snapshot(root, rev):
    # Inventory paths/modes, then resolve the actual blob/tree/gitlink identities
    # through rev-parse. Filtering leaves avoids hashing excluded parity data.
    entries = {}
    for entry in git(root, "ls-tree", "-r", "-z", rev).split("\0"):
        if entry:
            metadata, path = entry.split("\t", 1)
            entries[path] = metadata.split()[0]
    groups = {}
    for name, _ in GROUPS:
        paths = sorted(path for path in entries if group_of(path) == name)
        groups[name] = {}
        # Keep Windows command lines comfortably below their length limit.
        for start in range(0, len(paths), 50):
            batch = paths[start:start + 50]
            ids = git(root, "rev-parse", *(rev + ":" + path for path in batch)).splitlines()
            groups[name].update((path, (entries[path], oid)) for path, oid in zip(batch, ids))
    if groups["upstream"].get(PIN, (None,))[0] != "160000":
        raise ValueError(f"{rev}: missing upstream gitlink: {PIN}")
    return groups


def read_json(path):
    with Path(path).open(encoding="utf-8") as stream:
        return json.load(stream)


def check(root, producer, head, observations, manifest):
    pairs = [(path, producer) for path in observations]
    if manifest:
        records = read_json(manifest)
        if not isinstance(records, list) or not records:
            raise ValueError("manifest must be a nonempty list of {observation, producer} pairs")
        for record in records:
            if (not isinstance(record, dict) or set(record) != {"observation", "producer"}
                    or any(not isinstance(v, str) or not v for v in record.values())):
                raise ValueError("invalid manifest pair; expected {observation, producer} strings")
            pairs.append((str(Path(manifest).resolve().parent / record["observation"]), record["producer"]))
    head = commit(root, head)
    current = snapshot(root, head)
    pin = current["upstream"][PIN][1]
    producers = {}
    files = []
    fingerprint = None
    for path, rev in pairs or [(None, producer)]:
        sha = commit(root, rev)
        if sha not in producers:
            old = snapshot(root, sha)
            # --no-renames reports both old and new paths, even across groups.
            changed = git(root, "diff", "--no-ext-diff", "--no-renames", "--name-only", "-z", sha, head).split("\0")
            groups = []
            for name, _ in GROUPS:
                if old[name] != current[name]:
                    paths = sorted(p for p in changed if p in old[name] or p in current[name])
                    groups.append({"group": name, "paths": paths})
            producers[sha] = {"producer": sha, "status": "stale" if groups else "valid", "changes": groups}
        if path is None:
            continue
        observation = read_json(path)
        fp = observation.get("fingerprint") if isinstance(observation, dict) else None
        if (not isinstance(fp, dict) or not FINGERPRINT_KEYS <= fp.keys()
                or any(not isinstance(v, str) or not v for v in fp.values())):
            raise ValueError(f"{path}: invalid fingerprint")
        count = observation.get("non_certification")
        if type(count) is not int or count < 0:
            raise ValueError(f"{path}: invalid non_certification count")
        reasons = []
        if producers[sha]["status"] == "stale":
            reasons.append("producer inputs changed")
        if fp["upstream_pin"] != pin:
            reasons.append(f"upstream_pin mismatch: {fp['upstream_pin']} != {pin}")
        if count != 0:
            reasons.append(f"non_certification = {count}")
        if fingerprint is not None and fp != fingerprint:
            reasons.append("fingerprint differs from first observation")
        if fingerprint is None:
            fingerprint = fp
        files.append({"observation": str(path), "producer": sha,
                      "status": "stale" if reasons else "valid", "reasons": reasons})
    stale = any(p["status"] == "stale" for p in producers.values()) or any(f["status"] == "stale" for f in files)
    return {"status": "stale" if stale else "valid", "head": head,
            "upstream_pin": pin, "producers": list(producers.values()), "observations": files}


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise ValueError(message)


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    parser = Parser(description=__doc__)
    parser.add_argument("--producer", help="commit that produced the observations")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--observation", action="append", default=[])
    parser.add_argument("--manifest", help="JSON pairs; relative observation paths are relative to this file")
    parser.add_argument("--json", action="store_true")
    try:
        args = parser.parse_args(argv)
        if args.manifest and (args.producer or args.observation):
            parser.error("--manifest replaces --producer and --observation")
        if not args.manifest and not args.producer:
            parser.error("--producer is required unless --manifest is given")
        root = git(Path.cwd(), "rev-parse", "--show-toplevel")
        result = check(root, args.producer, args.head, args.observation, args.manifest)
    except (ValueError, OSError) as error:
        result = {"status": "error", "error": str(error)}
    if "--json" in argv:
        print(json.dumps(result, sort_keys=True))
    elif result["status"] == "error":
        print("error: " + result["error"])
    else:
        for producer in result["producers"]:
            print(f"producer {producer['producer']}: {producer['status']}")
            for group in producer["changes"]:
                print(f"  {group['group']}: {', '.join(group['paths'])}")
        for observation in result["observations"]:
            detail = "; ".join(observation["reasons"])
            print(f"{observation['observation']}: {observation['status']}" + (f" ({detail})" if detail else ""))
        print("collection: " + result["status"])
    return {"valid": 0, "stale": 1, "error": 2}[result["status"]]


if __name__ == "__main__":
    sys.exit(main())
