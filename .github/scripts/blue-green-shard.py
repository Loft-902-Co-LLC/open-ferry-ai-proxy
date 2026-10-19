"""Not upstream's: split the blue-green tests between CI's runners.

Usage: blue-green-shard.py METADATA OS SHARD COUNT

METADATA is what `cargo metadata --no-deps --format-version 1 --locked`
prints, OS is the runner's (`Linux`, `Windows` or `macOS`, as RUNNER_OS gives it) and
SHARD counts from 1. Prints this shard's step outputs: `args`, the target
filters for its `cargo test`, and `doc`, whether it also runs the doc tests.
The whole plan goes to stderr.

Every test target of the three packages goes to exactly one shard. `--test`
runs a name's target in each package that has one, so a name is one item. The
unit tests (`--lib --bins`) and the doc tests are one more. Items go, longest
first, to the shard with the least time so far, so each runner gets about the
same time. Ties go by name, then to the lowest shard, so every runner works
out the same split.
"""

import json
import re
import sys

PACKAGES = ("open-ferry", "open-ferry-supervisor", "open-ferry-update")
UNITS = "(units)"

# Seconds each item took on CI's runners, from the blue-green job's logs before
# it was split, summed over the packages that have the target. The units also
# count what building the binaries as tests and the doc tests adds. A target
# missing here counts as DEFAULT, and the plan names it, so its time can be
# added.
DEFAULT = 30.0
WEIGHTS = {
    # Initial estimates from Linux; replace with macOS runner timings after its first run.
    "macOS": {
        "process_switch": 215.5,
        "process_crash": 152.4,
        "process_lost_channel": 132.7,
        "process_recovery": 123.9,
        "process_ladder": 84.7,
        "process_detect": 49.2,
        "process_mode": 33.7,
        "supervise": 29.9,
        "process_real": 23.5,
        "process_stop": 22.2,
        "process_endpoint": 15.1,
        UNITS: 52.0,
    },
    "Linux": {
        "process_switch": 215.5,
        "process_crash": 152.4,
        "process_lost_channel": 132.7,
        "process_recovery": 123.9,
        "process_ladder": 84.7,
        "process_detect": 49.2,
        "process_mode": 33.7,
        "supervise": 29.9,
        "process_real": 23.5,
        "process_stop": 22.2,
        "process_endpoint": 15.1,
        "fake_worker": 10.2,
        "process_install_self": 9.9,
        "process_driver": 7.2,
        "process_supervise": 2.6,
        "process_service_run": 2.4,
        "subcommands": 0.2,
        "migrate": 0.1,
        "endpoint": 0.0,
        "process_service_scm": 0.0,
        "process_system": 0.0,
        "startup": 0.0,
        "unix_process": 0.0,
        "windows_process": 0.0,
        "windows_sockets": 0.0,
        "worker_channel": 0.0,
        UNITS: 52.0,
    },
    "Windows": {
        "process_switch": 205.7,
        "process_crash": 148.4,
        "process_lost_channel": 106.0,
        "process_ladder": 55.8,
        "process_detect": 55.6,
        "process_recovery": 54.7,
        "process_endpoint": 43.6,
        "process_mode": 37.1,
        "process_stop": 30.0,
        "supervise": 29.2,
        "process_real": 27.9,
        "process_install_self": 22.1,
        "process_service_run": 11.3,
        "fake_worker": 11.0,
        "process_driver": 8.3,
        "process_supervise": 4.9,
        "subcommands": 3.4,
        "migrate": 3.3,
        "windows_process": 0.7,
        "windows_sockets": 0.4,
        "startup": 0.4,
        "worker_channel": 0.1,
        "endpoint": 0.0,
        "process_service_scm": 0.0,
        "process_system": 0.0,
        "unix_process": 0.0,
        UNITS: 144.0,
    },
}

# The names go into the job's command line, so only plain ones are taken.
NAME = re.compile(r"[A-Za-z0-9_-]+")


def main(argv):
    if len(argv) != 5:
        sys.exit(__doc__)
    path, runner_os = argv[1], argv[2]
    shard, count = int(argv[3]), int(argv[4])
    if runner_os not in WEIGHTS:
        sys.exit(f"no times for runner OS {runner_os!r}")
    if not 1 <= shard <= count:
        sys.exit(f"shard {shard} isn't between 1 and {count}")
    with open(path, encoding="utf-8") as file:
        metadata = json.load(file)
    found = {pkg["name"]: pkg for pkg in metadata["packages"] if pkg["name"] in PACKAGES}
    missing = [name for name in PACKAGES if name not in found]
    if missing:
        sys.exit("not in the metadata: " + ", ".join(missing))
    tests = sorted(
        {
            target["name"]
            for pkg in found.values()
            for target in pkg["targets"]
            if "test" in target["kind"] and target.get("test", True)
        }
    )
    odd = [name for name in tests if not NAME.fullmatch(name)]
    if odd:
        sys.exit("test target names this can't pass on: " + ", ".join(odd))

    weights = WEIGHTS[runner_os]
    items = [(weights.get(name, DEFAULT), name) for name in tests]
    items.append((weights[UNITS], UNITS))
    items.sort(key=lambda item: (-item[0], item[1]))
    loads = [0.0] * count
    plan = [[] for _ in range(count)]
    for weight, name in items:
        least = min(range(count), key=lambda i: (loads[i], i))
        loads[least] += weight
        plan[least].append(name)

    for i, names in enumerate(plan):
        print(f"shard {i + 1}: about {loads[i]:.0f} s: {' '.join(names)}", file=sys.stderr)
    for name in tests:
        if name not in weights:
            print(f"no time recorded for {name}; counted as {DEFAULT:.0f} s", file=sys.stderr)
    gone = sorted(set(weights) - set(tests) - {UNITS})
    if gone:
        print("times recorded for targets that are gone: " + ", ".join(gone), file=sys.stderr)

    mine = plan[shard - 1]
    if not mine:
        sys.exit(f"shard {shard} has no tests; use fewer shards")
    args = ["--lib", "--bins"] if UNITS in mine else []
    for name in sorted(name for name in mine if name != UNITS):
        args += ["--test", name]
    print("args=" + " ".join(args))
    print("doc=" + ("true" if UNITS in mine else "false"))


if __name__ == "__main__":
    main(sys.argv)
