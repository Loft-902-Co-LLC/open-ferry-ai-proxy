"""Not upstream's: drop the workspace's own crates from the target directory.

Usage: clean-target.py METADATA

METADATA is what `cargo metadata --no-deps --format-version 1 --locked`
prints. CI caches the target directory for the dependencies only, so before a
save this removes, in each profile directory (a directory with a .fingerprint
in it, under the target directory or a target triple's directory in it):
- everything but deps, build and .fingerprint: the copies of the workspace's
  binaries and libraries, examples, incremental;
- in .fingerprint and build, the workspace packages' entries;
- in deps, the files of the workspace's targets.
Anything else in the target directory goes too (doc, tmp, scratch). Cargo
names those entries after the package or target, a hyphen and a 16-digit hex
hash, so only an exact name of the workspace's matches.
"""

import json
import re
import shutil
import sys
from pathlib import Path

KEEP = {"deps", "build", ".fingerprint"}


def remove(path):
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink()


def profiles(target):
    for child in target.iterdir():
        if not child.is_dir():
            continue
        if (child / ".fingerprint").is_dir():
            yield child
            continue
        for grandchild in child.iterdir():
            if grandchild.is_dir() and (grandchild / ".fingerprint").is_dir():
                yield grandchild


def main(argv):
    if len(argv) != 2:
        sys.exit(__doc__)
    with open(argv[1], encoding="utf-8") as file:
        metadata = json.load(file)
    target = Path(metadata["target_directory"])
    if not target.is_dir():
        print(f"no target directory at {target}")
        return
    members = set(metadata["workspace_members"])
    packages = [pkg for pkg in metadata["packages"] if pkg["id"] in members]
    package_names = {pkg["name"] for pkg in packages}
    target_names = {t["name"].replace("-", "_") for pkg in packages for t in pkg["targets"]}

    def alternatives(names):
        return "|".join(re.escape(name) for name in sorted(names, key=len, reverse=True))

    package_entry = re.compile(rf"(?:{alternatives(package_names)})-[0-9a-f]{{16}}")
    target_file = re.compile(rf"(?:lib)?(?:{alternatives(target_names)})-[0-9a-f]{{16}}(?:\..*)?")

    keep = set(profiles(target))
    kept_parents = {profile.parent for profile in keep}
    removed = 0
    for profile in keep:
        for entry in profile.iterdir():
            if entry.name not in KEEP:
                remove(entry)
                removed += 1
        for name in (".fingerprint", "build"):
            directory = profile / name
            if directory.is_dir():
                for entry in directory.iterdir():
                    if package_entry.fullmatch(entry.name):
                        remove(entry)
                        removed += 1
        deps = profile / "deps"
        if deps.is_dir():
            for entry in deps.iterdir():
                if target_file.fullmatch(entry.name):
                    remove(entry)
                    removed += 1
    # What's left around the profile directories: doc, tmp, the tests'
    # scratch directories.
    for parent in kept_parents | {target}:
        for entry in parent.iterdir():
            if entry not in keep and entry not in kept_parents and entry.name != "CACHEDIR.TAG":
                remove(entry)
                removed += 1
    print(f"kept {', '.join(sorted(str(p.relative_to(target)) for p in keep)) or 'nothing'}; removed {removed} entries")


if __name__ == "__main__":
    main(sys.argv)
