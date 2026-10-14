"""Not upstream's: check a never-run release-seam-check executable.

Build: cargo build -p open-ferry-update --release --example release-seam-check
Check: python -I crates/open-ferry-update/tests/check_release_seams.py <executable>
The example keeps the disk APIs linked without invoking them. Do not enable
blue-green-test for the plain scan; enable it for --self-test. Check the executable, not rlib documentation.
"""
import pathlib
import sys

positive = len(sys.argv) == 3 and sys.argv[1] == "--self-test"
if not positive and len(sys.argv) != 2:
    raise SystemExit("usage: check_release_seams.py [--self-test] <executable>")
binary = pathlib.Path(sys.argv[-1]).read_bytes()
markers = (
    # The three journal.* prefixes are not retained as contiguous strings in
    # the optimized example, even with the feature. They cannot prove absence.
    b".blue-green-test",
    b"state.fail", b"atomic.landed", b"staging.cleanup", b"injected failure at", b".arrived", b".release", b".hold", b".failed",
)
if positive:
    missing = [marker.decode("ascii") for marker in markers if marker not in binary]
    if missing:
        raise SystemExit("positive control missing required markers: " + ", ".join(missing))
    print(f"release seam positive control: {len(markers)} markers present")
    raise SystemExit(0)
found = [marker.decode("ascii") for marker in markers if marker in binary]
if found:
    raise SystemExit("release contains test seam strings: " + ", ".join(found))
print(f"release seam check: {len(markers)} markers absent")
