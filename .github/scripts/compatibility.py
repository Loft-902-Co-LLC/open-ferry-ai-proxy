#!/usr/bin/env python3
"""Generate docs/compatibility.md from compatibility-evidence.json.

The small reviewed map connects capabilities to existing Rust fixture tests;
it is not a list of general support claims. Both generation and --check build
the mapped libtest harnesses, find each exact test, and run it. Zero tests,
ignored tests, failures, and missing source declarations are errors. CI runs
test_compatibility.py in the existing tests job, after the workspace tests.
No Go checkout, provider credentials, live proxy, or git history is needed.
"""

from __future__ import annotations

import argparse
from collections import Counter
from datetime import date
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]
CATALOG = ROOT / ".github/scripts/compatibility-evidence.json"
DOCUMENT = ROOT / "docs/compatibility.md"

PROTOCOLS = {
    "chat": "OpenAI Chat Completions / HTTP",
    "completions": "Legacy Completions / HTTP",
    "responses": "OpenAI Responses / HTTP",
    "responses-ws": "OpenAI Responses / WebSocket",
    "claude": "Claude Messages / HTTP",
    "gemini": "Gemini generateContent / HTTP",
    "interactions": "Gemini Interactions / HTTP",
    "images": "OpenAI Images / HTTP",
    "speech": "OpenAI Speech / HTTP",
    "xai-tts": "xAI TTS / HTTP",
    "videos": "OpenAI Videos / HTTP",
    "xai-videos": "xAI Videos / HTTP",
    "models": "Model discovery / HTTP",
}
PROVIDERS = {
    "codex-oauth": ("Codex", "OAuth"),
    "codex-key": ("Codex", "API key"),
    "codex-siwc": ("Codex", "SIWC nested OAuth"),
    "claude-key": ("Claude", "API key"),
    "claude-oauth": ("Claude", "OAuth sign-in; see limits above"),
    "claude-cli": ("Claude via claude-cli", "Installed Claude Code session"),
    "gemini-key": ("Gemini", "API key"),
    "interactions-key": ("Gemini via Interactions executor", "API key"),
    "vertex-key": ("Vertex AI", "API key"),
    "vertex-sa": ("Vertex AI", "Service account"),
    "vertex-token": ("Vertex AI", "Access token"),
    "meta-key": ("Meta", "API key"),
    "meta-token": ("Meta", "Access token"),
    "xai-key": ("xAI", "API key"),
    "compat-key": ("OpenAI-compatible upstream", "API key"),
}
CAPABILITIES = {
    "text": "Text",
    "streaming": "Streaming",
    "tools": "Tool calls",
    "reasoning": "Reasoning",
    "image-in": "Images in",
    "image-out": "Images out",
    "audio": "Audio",
    "video": "Video",
    "model-list": "Model list",
    "tokens": "Token counting",
    "websocket": "WebSocket",
}


def read_source(path: str) -> str:
    source = (ROOT / path).resolve()
    if not source.is_relative_to(ROOT) or not source.is_file():
        raise ValueError(f"Evidence source does not exist in this checkout: {path}")
    return source.read_text(encoding="utf-8")


def load_catalog(path: Path = CATALOG) -> dict:
    catalog = json.loads(path.read_text(encoding="utf-8"))
    date.fromisoformat(catalog["reviewed"])
    identifiers = set()
    cells = set()
    for entry in catalog["evidence"]:
        identifier = entry["id"]
        if not re.fullmatch(r"[EU][0-9]+", identifier) or identifier in identifiers:
            raise ValueError(f"Invalid or duplicate evidence ID: {identifier}")
        identifiers.add(identifier)
        source = read_source(entry["path"])
        if entry["level"] == "fixture-tested":
            if not identifier.startswith("E"):
                raise ValueError(f"Fixture evidence needs an E ID: {identifier}")
            test = entry["test"]
            if not re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*(::[A-Za-z_][A-Za-z_0-9]*)*", test):
                raise ValueError(f"Invalid Rust test name: {test}")
            if not re.search(r"\bfn\s+" + re.escape(test.split("::")[-1]) + r"\s*\(", source):
                raise ValueError(f"Evidence test does not exist in {entry['path']}: {test}")
            # Derive the package from the source path, rather than accepting
            # shell commands or an independently maintained package name.
            parts = Path(entry["path"]).parts
            if len(parts) < 4 or parts[0] != "crates" or parts[2] != "src":
                raise ValueError(f"Expected a crate's libtest source: {entry['path']}")
            entry["package"] = parts[1]
        elif entry["level"] == "unsupported":
            if not identifier.startswith("U") or entry["refusal"] not in source:
                raise ValueError(f"Missing refusal code for {identifier}")
        else:
            raise ValueError(f"No recorded live results; invalid evidence level: {entry['level']}")
        if not entry["cells"] or not entry["scope"].strip():
            raise ValueError(f"Evidence needs cells and a scope: {identifier}")
        for protocol, provider, capability in entry["cells"]:
            if protocol not in PROTOCOLS or provider not in PROVIDERS or capability not in CAPABILITIES:
                raise ValueError(f"Unknown matrix dimension in {identifier}")
            cell = (protocol, provider, capability)
            if cell in cells:
                raise ValueError(f"Duplicate evidence assignment for {cell}")
            cells.add(cell)
    return catalog


def command(args: list[str]) -> str:
    result = subprocess.run(args, cwd=ROOT, capture_output=True, text=True, encoding="utf-8")
    if result.returncode:
        raise ValueError(f"Command failed: {' '.join(args)}\n{result.stdout}\n{result.stderr}")
    return result.stdout


def build_harnesses(catalog: dict) -> dict[str, Path]:
    packages = sorted({entry["package"] for entry in catalog["evidence"] if entry["level"] == "fixture-tested"})
    args = ["cargo", "test", "--locked", "--lib", "--no-run", "--message-format=json"]
    for package in packages:
        args.extend(["-p", package])
    print("Building fixture test harnesses...", file=sys.stderr, flush=True)
    output = command(args)
    harnesses = {}
    for line in output.splitlines():
        message = json.loads(line)
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        target = message["target"]
        if message.get("profile", {}).get("test") and "lib" in target["kind"]:
            package = target["name"].replace("_", "-")
            if package in packages:
                harnesses[package] = Path(message["executable"])
    if set(harnesses) != set(packages):
        raise ValueError(f"Missing test harnesses: {set(packages) - set(harnesses)}")
    return harnesses


def validate_evidence(catalog: dict, harnesses: dict[str, Path]) -> None:
    listed = {
        package: set(re.findall(r"^(.+): test$", command([str(binary), "--list", "--format=terse"]), re.M))
        for package, binary in harnesses.items()
    }
    ran = set()
    for entry in catalog["evidence"]:
        if entry["level"] != "fixture-tested":
            continue
        package, test = entry["package"], entry["test"]
        if test not in listed.get(package, set()):
            raise ValueError(f"Evidence test does not exist in compiled harness: {package}::{test}")
        if (package, test) in ran:
            continue
        output = command([str(harnesses[package]), "--exact", test, "--nocapture", "--color=never"])
        if not re.search(r"test result: ok\. 1 passed; 0 failed; 0 ignored;", output):
            raise ValueError(f"Evidence test did not run exactly once (possibly ignored): {test}\n{output}")
        ran.add((package, test))
    print(f"Verified {len(ran)} exact Rust fixture tests.", file=sys.stderr, flush=True)


def matrix(catalog: dict) -> dict:
    return {
        tuple(cell): entry
        for entry in catalog["evidence"]
        for cell in entry["cells"]
    }


def cell_counts(catalog: dict) -> Counter:
    counts = Counter({level: 0 for level in ("fixture-tested", "live-tested", "unsupported", "unknown")})
    counts.update(entry["level"] for entry in matrix(catalog).values())
    counts["unknown"] = len(PROTOCOLS) * len(PROVIDERS) * len(CAPABILITIES) - sum(counts.values())
    return counts


def render(catalog: dict) -> str:
    cells = matrix(catalog)
    counts = cell_counts(catalog)
    upstream = read_source("UPSTREAM.md")
    pin = re.search(r"\| Version \| `([^`]+)` \|\s*\| Commit \| `([0-9a-f]{40})` \((\d{4}-\d{2}-\d{2})\)", upstream)
    if not pin:
        raise ValueError("Cannot read the upstream pin in UPSTREAM.md")
    version, commit, pinned_date = pin.groups()
    lines = [
        "# Compatibility evidence",
        "",
        "<!-- Written by .github/scripts/compatibility.py; don't edit by hand. -->",
        "",
        "Open Ferry is a credited Rust port of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI).",
        f"The pinned reference is **{version}**, commit `{commit}` ({pinned_date}); see [UPSTREAM.md](../UPSTREAM.md#pinned-reference).",
        "",
        "This matrix describes **client protocols and transports**, providers, credential types, and individual capabilities separately.",
        "Evidence is deliberately narrow: executor fixtures exercise the named client format and a fake upstream, without a real client application or provider account.",
        "They do not establish every model, every request shape, or the public HTTP/WebSocket route. Evidence that exercises a server route says so explicitly.",
        "A provider's upstream WebSocket test alone does not prove a client's WebSocket transport.",
        "",
        "## Legend and scope",
        "",
        "- **F [E…] — fixture-tested**: the linked, exact Rust test is run offline by the generator's check in required CI. Each reference identifies its source, fixture revision, review date, and the assertions it covers.",
        "- **live-tested**: requires a recorded result from a real provider and client. **There are no recorded live results, and no live-tested cells.**",
        "- **U [U…] — unsupported**: explicit refusal code is linked and checked for existence. A refusal is not a successful capability test.",
        "- **? — unknown**: no evidence mapped for this exact combination. Implementation, translator parity, or support for another credential type does not promote it to tested.",
        "",
        "Text means text request/response handling; streaming means incremental protocol events; tools means only the tool behavior described by its evidence.",
        "Reasoning means the named reasoning fields or events, not model quality. Images in/out are separate; audio and video evidence specifies its direction.",
        "Token counting may be local estimation; its evidence states which. Unknown also covers dimensions that are not meaningful for an endpoint; it is not a claim of refusal.",
        "",
        "**Claude OAuth sign-in limits:** this route goes against Anthropic's terms; in testing only Haiku models worked. Accounts may be limited or suspended.",
        "The synthetic OAuth fixture below does not establish real Sonnet/Opus access. See [Claude subscriptions](claude-subscription.md#the-claude-sign-in).",
        "`claude-cli` instead runs the user's installed Claude Code and stores no Claude Code token.",
        "",
        "## Client application versions",
        "",
        "Client application versions are **not tested yet**. No Codex CLI, Claude Code, or other application version is claimed by these protocol fixtures.",
        "A future recorded client recipe should add application/version, provider/model, credential type, protocol/transport, capabilities, evidence artifact, and test date here.",
        "A documented minimum version or a synthetic User-Agent is not a tested client recipe.",
        "",
        "## Cell counts",
        "",
        "Counts cover capability cells in the protocol tables only, including unknown combinations.",
        "",
        "| Evidence level | Cells |",
        "|---|---:|",
    ]
    lines.extend(f"| {level} | {count} |" for level, count in counts.items())
    lines.extend(["", f"Total: {sum(counts.values())} cells ({len(PROTOCOLS)} protocol/transport entries × {len(PROVIDERS)} provider/credential entries × {len(CAPABILITIES)} capabilities)."])
    for protocol, label in PROTOCOLS.items():
        lines.extend(["", f"## {label}", "", "| Provider | Credential | " + " | ".join(CAPABILITIES.values()) + " |", "|---|---|" + "---|" * len(CAPABILITIES)])
        for provider, (name, credential) in PROVIDERS.items():
            values = []
            for capability in CAPABILITIES:
                evidence = cells.get((protocol, provider, capability))
                if evidence is None:
                    values.append("?")
                else:
                    identifier = evidence["id"]
                    marker = "F" if evidence["level"] == "fixture-tested" else "U"
                    values.append(f"{marker} [{identifier}](#{identifier.lower()})")
            lines.append(f"| {name} | {credential} | " + " | ".join(values) + " |")
    lines.extend([
        "", "## Evidence catalog", "",
        f"Catalog review date: **{catalog['reviewed']}** (the mapping review, not a live run or a generation timestamp).",
        "Fixture versions below are SHA-256 digests of the linked Rust source file after LF normalization, so they also work in shallow CI checkouts.",
        "The review date stays fixed until the mapping is reviewed; source changes update the fixture revision and require regeneration.",
        "The upstream pin above identifies the port's reference, not an executed parity comparison for these provider fixtures.",
    ])
    for entry in catalog["evidence"]:
        source = read_source(entry["path"])
        digest = hashlib.sha256(source.encode("utf-8")).hexdigest()
        needle = f"fn {entry['test'].split('::')[-1]}(" if entry["level"] == "fixture-tested" else entry["refusal"]
        line = source[:source.index(needle)].count("\n") + 1
        lines.extend(["", f"### {entry['id']}", "", f"{entry['level']}; reviewed {catalog['reviewed']}; fixture revision `sha256:{digest}`.", ""])
        if entry["level"] == "fixture-tested":
            lines.append(f"Test: [`{entry['path']}::{entry['test']}`](../{entry['path']}#L{line}).")
        else:
            lines.append(f"Refusal: [`{entry['path']}`](../{entry['path']}#L{line}); `{entry['refusal']}`.")
        lines.extend(["", entry["scope"]])
    lines.extend([
        "", "## Regeneration and checks", "",
        "Run from the repository root (Rust and Python 3.11+; no Go or upstream checkout needed):", "",
        "```sh", "python .github/scripts/compatibility.py", "python .github/scripts/compatibility.py --check", "python -I .github/scripts/test_compatibility.py", "```", "",
        "The small hand-kept mapping is [compatibility-evidence.json](../.github/scripts/compatibility-evidence.json).",
        "Generation and checking validate each source declaration, compile the referenced crate's test harness, require its exact test name, and run it with one pass and zero ignored tests.",
        "Missing, renamed, ignored, failing, or conditionally absent tests fail the check. Explicit refusal snippets must still exist, and every dimension and evidence ID is validated.",
        "Required CI runs these checks in the existing tests job, alongside the workspace tests; it also checks the committed document byte for byte.",
        "Translation-only parity suites are not promoted to provider/credential/transport evidence. Their separate results remain in the [parity tool](../tools/parity/README.md).",
        "Extending coverage means reviewing and mapping an actual test, then regenerating; implemented combinations without mapped assertions remain unknown.", "",
    ])
    return "\n".join(lines)


def check_document(expected: str, path: Path = DOCUMENT) -> None:
    if not path.is_file() or path.read_bytes() != expected.encode("utf-8"):
        raise ValueError("docs/compatibility.md is out of date; run python .github/scripts/compatibility.py")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail instead of writing an outdated document")
    args = parser.parse_args()
    try:
        catalog = load_catalog()
        validate_evidence(catalog, build_harnesses(catalog))
        output = render(catalog)
        if args.check:
            check_document(output)
        else:
            DOCUMENT.write_bytes(output.encode("utf-8"))
        print(json.dumps(dict(cell_counts(catalog))))
        return 0
    except (ValueError, KeyError, OSError) as error:
        print(error, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
