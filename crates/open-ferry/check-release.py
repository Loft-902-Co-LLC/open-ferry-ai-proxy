"""Not upstream's: verify a no-feature release binary contains no fault hooks.
Run with python -I check-release.py <cargo-build.json> after cargo build
--release -p open-ferry --bin open-ferry --no-default-features --message-format=json.
Positive control: build with --features blue-green-test, then run
python -I check-release.py --self-test <cargo-feature-build.json>.
The inputs must be successful builds' output, not directories of old artifacts.
Unit tests: python -I crates/open-ferry/test_check_release.py (from the repo root).
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
    b".blue-green-test", b"events.log", b"test-marker", b"x-open-ferry-test-marker",
    b"handover.written", b"handover_failed",
)

# What a feature build must carry, or the scan above proves nothing. The
# worker's own seams (`events.log`, `test-marker`, the marker header,
# `handover.written`, `handover_failed`) belong here too, but only once the
# binary's entry point calls the worker: until then the linker drops them from
# a feature build, and the scan for them above can't be shown to catch them.
REQUIRED = (
    b"refresh.after_rotate", b"refresh.before_save", b"persist.before", b"state.fail:",
    b".blue-green-test",
)


def scan(artifact):
    content = artifact.read_bytes()
    found = [text.decode("ascii") for text in FORBIDDEN if text in content]
    if found:
        raise ValueError(f"fault hooks found in {artifact.name}: {', '.join(found)}")


def main():
    positive = len(sys.argv) == 3 and sys.argv[1] == "--self-test"
    if not positive and len(sys.argv) != 2:
        raise SystemExit("usage: check-release.py [--self-test] <cargo-build.json>")
    events = [json.loads(line) for line in pathlib.Path(sys.argv[-1]).read_text(encoding="utf-8").splitlines() if line.strip()]
    if not events or events[-1].get("reason") != "build-finished" or not events[-1].get("success"):
        raise SystemExit("Cargo build did not finish successfully")
    artifacts = []
    for event in events:
        target = event.get("target", {})
        if event.get("reason") != "compiler-artifact" or target.get("name") != "open-ferry" or "bin" not in target.get("kind", []):
            continue
        if not event.get("profile", {}).get("opt_level") or event["profile"]["opt_level"] == "0":
            raise SystemExit("binary was not built with release optimization")
        if event["profile"].get("test"):
            raise SystemExit("artifact is a test harness, not the release binary")
        feature = "blue-green-test" in event.get("features", [])
        if feature != positive:
            raise SystemExit("positive control needs a feature build" if positive else "binary was built with test hooks enabled")
        if event.get("executable"):
            artifacts.append(pathlib.Path(event["executable"]))
    if not artifacts:
        raise SystemExit("no release binary found in this Cargo build")
    for artifact in artifacts:
        if positive:
            content = artifact.read_bytes()
            missing = [text.decode("ascii") for text in REQUIRED if text not in content]
            if missing:
                raise SystemExit(f"positive control missing required hooks: {', '.join(missing)}")
        try:
            scan(artifact)
        except ValueError as error:
            if not positive:
                raise SystemExit(str(error)) from error
            print(f"positive control rejected: {error}")
        else:
            if positive:
                raise SystemExit("positive control failed: feature artifact passed the string scan")
    if not positive:
        print(f"checked {len(artifacts)} release binaries: no fault/barrier strings")


if __name__ == "__main__":
    main()
