"""Not upstream's: check a never-run release-seam-check executable.

Build: cargo build -p open-ferry-update --release --example release-seam-check
Check: python -I crates/open-ferry-update/tests/check_release_seams.py <executable>
The example keeps the disk APIs linked without invoking them. Do not enable
blue-green-test for this build. Check the executable, not rlib documentation.
"""
import pathlib
import sys

binary = pathlib.Path(sys.argv[1]).read_bytes()
markers = (
    b".blue-green-test", b"journal.before", b"journal.after", b"journal.fail",
    b"state.fail", b"atomic.landed", b"staging.cleanup", b"injected failure at", b".arrived", b".release", b".hold", b".failed",
)
found = [marker.decode("ascii") for marker in markers if marker in binary]
if found:
    raise SystemExit("release contains test seam strings: " + ", ".join(found))
print(f"release seam check: {len(markers)} markers absent")
