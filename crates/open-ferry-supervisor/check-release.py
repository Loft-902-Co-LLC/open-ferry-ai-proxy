"""Not upstream's: verify a no-feature release library contains no fault hooks.
Run with python -I check-release.py <cargo-build.json> after cargo build
--release -p open-ferry-supervisor --lib --no-default-features --message-format=json.
The input must be the output of that successful build, not a directory of old artifacts.
"""
import json
import pathlib
import sys

FORBIDDEN = (
    b"fail-load", b"fail-before-ready", b"exit-after", b"slow-ready",
    b"ignore-handover", b"hold-refresh", b"drop-channel", b"supervisor-panic",
    b"refresh.after_rotate", b"refresh.before_save", b"persist.before",
    b"handover.gate_closing", b"handover.gate_closed", b"lease.release",
    b"take_over.before_reload", b"rename.after_first", b"journal.before:",
    b"journal.after:", b"journal.fail:", b"state.fail:", b"disk.commit.before_replace",
    b"injected fault", b"required-test-cap",
    b"termination-timeout.hold", b"termination-timeout.arrived",
    b"startup.before_install", b"install.between_renames",
    b"startup.after_prepared_journal", b"startup.recovered_install_sync",
)
if len(sys.argv) != 2:
    raise SystemExit("usage: check-release.py <cargo-build.json>")
events = [json.loads(line) for line in pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines() if line.strip()]
if not events or events[-1].get("reason") != "build-finished" or not events[-1].get("success"):
    raise SystemExit("Cargo build did not finish successfully")
artifacts = []
for event in events:
    if event.get("reason") != "compiler-artifact" or event.get("target", {}).get("name") != "open_ferry_supervisor":
        continue
    if not event.get("profile", {}).get("opt_level") or event["profile"]["opt_level"] == "0":
        raise SystemExit("library was not built with release optimization")
    if "blue-green-test" in event.get("features", []):
        raise SystemExit("library was built with test hooks enabled")
    artifacts.extend(pathlib.Path(name) for name in event.get("filenames", []) if name.endswith(".rlib"))
if not artifacts:
    raise SystemExit("no release library found in this Cargo build")
for artifact in artifacts:
    content = artifact.read_bytes()
    found = [text.decode("ascii") for text in FORBIDDEN if text in content]
    if found:
        raise SystemExit(f"fault hooks found in {artifact.name}: {', '.join(found)}")
print(f"checked {len(artifacts)} release libraries: no fault/barrier strings")
