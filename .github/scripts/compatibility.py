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
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
CATALOG = ROOT / ".github/scripts/compatibility-evidence.json"
DOCUMENT = ROOT / "docs/compatibility.md"
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("compatibility_rust", Path(__file__).with_name("compatibility_rust.py"))
rust = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rust)

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
    "claude-oauth": ("Claude", "OAuth sign-in; see scope limits"),
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
    "route-only": ("Scripted route dispatcher", "Provider credential not exercised"),
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

# Meaningful capabilities of each endpoint, independent of implementation
# or current coverage. Other dimensions are omitted, not called unsupported.
MEANINGFUL = {
    "chat": ("text", "streaming", "tools", "reasoning", "image-in", "image-out", "audio", "video", "tokens"),
    "completions": ("text", "streaming", "tokens"),
    "responses": ("text", "streaming", "tools", "reasoning", "image-in", "image-out", "audio", "video", "tokens"),
    "responses-ws": ("text", "streaming", "tools", "reasoning", "image-in", "image-out", "audio", "video", "tokens", "websocket"),
    "claude": ("text", "streaming", "tools", "reasoning", "image-in", "tokens"),
    "gemini": ("text", "streaming", "tools", "reasoning", "image-in", "image-out", "audio", "video", "tokens"),
    "interactions": ("text", "streaming", "tools", "reasoning", "image-in", "image-out", "audio", "video", "tokens"),
    "images": ("streaming", "image-in", "image-out"),
    "speech": ("audio",),
    "xai-tts": ("audio",),
    "videos": ("image-in", "video"),
    "xai-videos": ("image-in", "video"),
    "models": ("model-list",),
}

# Dedicated executor paths for the media endpoints (see the linked handler
# and provider executor modules in the document). Other protocols remain
# catalog/model-dependent and retain all real provider/credential rows.
ELIGIBLE = {
    "images": {"codex-oauth", "codex-key", "codex-siwc", "xai-key", "compat-key"},
    "speech": {"xai-key"},
    "xai-tts": {"xai-key"},
    "videos": {"xai-key"},
    "xai-videos": {"xai-key"},
}


def eligible(protocol, provider):
    return provider == "route-only" or provider in ELIGIBLE.get(protocol, set(PROVIDERS) - {"route-only"})


def accept_digest(identifier, path=CATALOG):
    catalog = load_catalog(path, check_digests=False)
    entry = next((entry for entry in catalog["evidence"] if entry["id"] == identifier), None)
    if entry is None:
        raise ValueError(f"Unknown evidence ID: {identifier}")
    # Replace just this field, leaving scopes, cells, dates and other bytes
    # intact. Initial catalog creation also uses this same normalized hash.
    source = path.read_text(encoding="utf-8")
    pattern = r'("id"\s*:\s*"' + re.escape(identifier) + r'"(?:(?!"id"\s*:).)*?"body_sha256"\s*:\s*")[0-9a-f]{64}(")'
    updated, count = re.subn(pattern, lambda match: match[1] + entry["_body_digest"] + match[2], source, count=1, flags=re.S)
    if count != 1:
        raise ValueError(f"Missing body_sha256 field for {identifier}")
    path.write_bytes(updated.encode("utf-8"))


def read_source(path: str) -> str:
    source = (ROOT / path).resolve()
    if not source.is_relative_to(ROOT):
        raise ValueError(f"Evidence source is outside this checkout: {path}")
    if not source.is_file():
        raise FileNotFoundError(f"Evidence source does not exist in this checkout: {path}")
    return source.read_text(encoding="utf-8")


def load_catalog(path: Path = CATALOG, *, check_digests=True) -> dict:
    catalog = json.loads(path.read_text(encoding="utf-8"))
    date.fromisoformat(catalog["reviewed"])
    identifiers = set()
    cells = {}
    resolver = rust.Resolver(read_source)
    for entry in catalog["evidence"]:
        identifier = entry["id"]
        if not re.fullmatch(r"[EU][0-9]+", identifier) or identifier in identifiers:
            raise ValueError(f"Invalid or duplicate evidence ID: {identifier}")
        identifiers.add(identifier)
        source = read_source(entry["path"])
        if entry["level"] in ("fixture-tested", "unsupported"):
            if entry["level"] == "fixture-tested" and not identifier.startswith("E"):
                raise ValueError(f"Fixture evidence needs an E ID: {identifier}")
            test = entry["test"]
            if not re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*(::[A-Za-z_][A-Za-z_0-9]*)*", test):
                raise ValueError(f"Invalid Rust test name: {test}")
            # Derive the package from the source path, rather than accepting
            # shell commands or an independently maintained package name.
            parts = Path(entry["path"]).parts
            if len(parts) < 4 or parts[0] != "crates" or parts[2] != "src":
                raise ValueError(f"Expected a crate's libtest source: {entry['path']}")
            entry["package"] = parts[1]
            body = resolver.body(f"crates/{parts[1]}/src/lib.rs", test, entry["path"])
            entry["_body_digest"] = rust.digest(body)
            if check_digests and entry.get("body_sha256") != entry["_body_digest"]:
                raise ValueError(f"{identifier}'s test changed: review its scope and cells, then run compatibility.py --accept {identifier}")
            if entry["level"] == "unsupported" and (not identifier.startswith("U") or entry["refusal"] not in source):
                raise ValueError(f"Missing refusal code for {identifier}")
        else:
            raise ValueError(f"No recorded live results; invalid evidence level: {entry['level']}")
        if not entry["cells"] or not entry["scope"].strip():
            raise ValueError(f"Evidence needs cells and a scope: {identifier}")
        for protocol, provider, capability in entry["cells"]:
            if protocol not in PROTOCOLS or provider not in PROVIDERS or capability not in MEANINGFUL[protocol] or not eligible(protocol, provider):
                raise ValueError(f"Unknown matrix dimension in {identifier}")
            cell = (protocol, provider, capability)
            if cell in cells and cells[cell] != entry["level"]:
                raise ValueError(f"Conflicting evidence levels for {cell}")
            cells[cell] = entry["level"]
    return catalog


def command(args: list[str], *, env=None) -> str:
    result = subprocess.run(args, cwd=ROOT, capture_output=True, text=True, encoding="utf-8", env=env)
    if result.returncode:
        raise ValueError(f"Command failed: {' '.join(args)}\n{result.stdout}\n{result.stderr}")
    return result.stdout


class Harnesses(dict):
    non_fresh = 0


def build_harnesses(catalog: dict) -> Harnesses:
    packages = {entry["package"] for entry in catalog["evidence"]}
    args = ["cargo", "test", "--workspace", "--locked", "--no-run", "--message-format=json"]
    print("Checking workspace test harnesses...", file=sys.stderr, flush=True)
    output = command(args)
    harnesses = Harnesses()
    for line in output.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and not message["fresh"]:
            harnesses.non_fresh += 1
        if message.get("reason") != "compiler-artifact" or not message.get("executable"):
            continue
        target = message["target"]
        if message.get("profile", {}).get("test"):
            package = target["name"].replace("_", "-")
            if (package in packages and "lib" in target["kind"]) or package == "open-ferry-parity":
                harnesses[package] = Path(message["executable"])
    if set(harnesses) != packages | {"open-ferry-parity"}:
        raise ValueError(f"Missing test harnesses: {(packages | {'open-ferry-parity'}) - set(harnesses)}")
    print(f"Cargo artifacts not fresh: {harnesses.non_fresh}", file=sys.stderr, flush=True)
    return harnesses


def validate_evidence(catalog: dict, harnesses: dict[str, Path]) -> None:
    packages = {entry["package"] for entry in catalog["evidence"]}
    listed = {
        package: set(re.findall(r"^(.+): test$", command([str(binary), "--list", "--format=terse"]), re.M))
        for package, binary in harnesses.items() if package in packages
    }
    ran = set()
    for entry in catalog["evidence"]:
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
    cells = {}
    for entry in catalog["evidence"]:
        for cell in entry["cells"]:
            cells.setdefault(tuple(cell), []).append(entry)
    return cells


def cell_counts(catalog: dict, *, scripted=False) -> Counter:
    counts = Counter({level: 0 for level in ("fixture-tested", "live-tested", "unsupported", "unknown")})
    counts.update(entries[0]["level"] for cell, entries in matrix(catalog).items() if (cell[1] == "route-only") == scripted)
    total = sum(len(MEANINGFUL[protocol]) for protocol in PROTOCOLS for provider in PROVIDERS if (provider == "route-only") == scripted and eligible(protocol, provider))
    counts["unknown"] = total - sum(counts.values())
    return counts


def load_parity(harnesses) -> list[dict]:
    with tempfile.TemporaryDirectory(prefix="compatibility-catalog-") as directory:
        path = Path(directory) / "suites.json"
        env = dict(os.environ, OPEN_FERRY_COMPATIBILITY_CATALOG=str(path))
        output = command([str(harnesses["open-ferry-parity"]), "--exact", "compatibility::tests::write_catalog_for_generator", "--nocapture"], env=env)
        if "1 passed; 0 failed; 0 ignored;" not in output:
            raise ValueError("Parity catalog transport test did not run")
        entries = json.loads(path.read_text(encoding="utf-8"))
    slugs = set()
    for entry in entries:
        if entry["slug"] in slugs or entry["cases"] <= 0:
            raise ValueError(f"Invalid parity suite metadata: {entry}")
        slugs.add(entry["slug"])
    if not entries:
        raise ValueError("Empty parity suite registry")
    return entries


def parity_cells(parity: list[dict]) -> dict:
    cells = {}
    for entry in parity:
        if entry["direction"] in ("reasoning", "model-list", "response-error"):
            continue
        cells.setdefault((entry["client_format"], entry["upstream_format"]), []).append(entry)
    return cells


def evidence_cell(entries: list[dict]) -> str:
    marker = "F" if entries[0]["level"] == "fixture-tested" else "U"
    return marker + " " + ", ".join(f"[{entry['id']}](#{entry['id'].lower()})" for entry in entries)


def render(catalog: dict, parity: list[dict]) -> str:
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
        "",
        "## Legend",
        "",
        "- **F [E…] — fixture-tested:** the exact linked Rust test compiles and passes offline in the required CI tests job; its scope says which assertions and boundary it exercises.",
        "- **P — parity-tested:** a maintained translation suite is exercised by the separate offline parity CI job. A passing comparison allows only identical/equivalent outputs or recorded known differences; see the translation section for timing and limits.",
        "- **live-tested:** requires a recorded real provider/client result. **There are no recorded live results or live-tested cells.**",
        "- **U [U…] — unsupported:** the linked code explicitly refuses the capability.",
        "- **? — unknown:** no evidence is mapped for this meaningful combination. Implementation or coverage of another credential is not proof.",
        "",
        "## What is fixture-tested",
        "",
        "Each fraction is tested capabilities / meaningful capabilities for that protocol and provider/credential row. These are fixture assertions, not tested application versions or general model support.",
        "",
        "| Client protocol / transport | Provider / credential: capabilities tested |",
        "|---|---|",
    ]
    for protocol, label in PROTOCOLS.items():
        summaries = []
        for provider, (name, credential) in PROVIDERS.items():
            tested = sum(
                cells.get((protocol, provider, capability), [{}])[0].get("level") == "fixture-tested"
                for capability in MEANINGFUL[protocol]
            )
            if tested:
                summaries.append(f"{name} / {credential}: **{tested}/{len(MEANINGFUL[protocol])}**")
        if summaries:
            lines.append(f"| {label} | " + "; ".join(summaries) + " |")
    absent = [label for protocol, label in PROTOCOLS.items() if not any(key[0] == protocol for key in cells)]
    lines.extend([
        "", "Protocols with no provider/credential fixture or refusal evidence yet: " + "; ".join(absent) + ".",
        "", "## Scope and client application versions", "",
        "Client application versions are **not tested yet**. Protocol fixtures, synthetic client-version headers, and documented minimum versions do not establish a Codex CLI, Claude Code, or other real application recipe.",
        "Future recorded recipes belong here with application/version, provider/model, credential type, protocol/transport, capabilities, artifact, and test date.",
        "",
        "Executor fixtures exercise the named client format and a fake upstream. Public-route fixtures say so in their scope.",
        "The **Scripted route dispatcher** row exercises a public route with fake dispatch; it does not exercise a provider account or provider credential. It is not promoted to any provider/credential row.",
        "Text includes the request/response behavior described by the evidence; tools and reasoning mean only the named tool/field/event assertions. Audio and video scopes state direction. Token counts may be local estimates.",
        "",
        "**Claude OAuth sign-in limits:** this route goes against Anthropic's terms; in testing only Haiku models worked. Accounts may be limited or suspended.",
        "The synthetic OAuth fixture does not establish real Sonnet/Opus access. See [Claude subscriptions](claude-subscription.md#the-claude-sign-in). `claude-cli` runs the user's installed Claude Code and stores no Claude Code token.",
        "", "## Counts and meaningful combinations", "",
        "For each protocol, counts use its meaningful capability columns and eligible real provider/credential rows, including hidden unknown rows. Scripted dispatch is counted separately.",
        "The dedicated [Images paths](../crates/open-ferry-server/src/handlers/images.rs) cover Codex, xAI and OpenAI-compatible executors; [speech](../crates/open-ferry-providers/src/xai/executor/speech.rs) and [video](../crates/open-ferry-providers/src/xai/executor/videos.rs) paths cover xAI API keys. Other endpoint/provider pairs retain the broad unknown rule because routing is catalog/model-dependent, not a fixed provider allowlist.",
        "A cell with several passing fixtures counts once. Dimensions outside that endpoint's set are omitted, not marked unsupported. For example, speech counts audio, and model discovery counts model list.",
        "", "| Provider/credential evidence level | Meaningful cells |", "|---|---:|",
    ])
    lines.extend(f"| {level} | {count} |" for level, count in counts.items())
    lines.extend([
        "", f"Total: **{sum(counts.values())}** meaningful real provider/credential capability cells. Scripted dispatch and translation evidence are counted separately.",
        "", "| Scripted route dispatch evidence level | Meaningful cells |", "|---|---:|",
        *[f"| {level} | {count} |" for level, count in cell_counts(catalog, scripted=True).items()],
        "", f"## Translation parity with CLIProxyAPI {version}", "",
        f"Pinned reference: **{version}**, commit `{commit}` ({pinned_date}); [pin](../UPSTREAM.md#pinned-reference). Catalog reviewed **{catalog['reviewed']}**.",
        "The parity section reads the current pin from UPSTREAM.md because parity CI compares against that pin; fixture tests below exercise Open Ferry code instead.",
        "",
        "**parity-tested** describes maintained suite coverage, not a newly recorded Go comparison for this checkout. A passing translator run compares the same inputs against the pinned Go implementation and permits only identical outputs, equivalence under recorded rules, or cases with recorded known differences; an unaccepted Different result fails the job. Build/setup errors also fail it.",
        "The separate config-save suite has its own recorded certification-failure baseline; it is outside this translation table.",
        "",
        "[PR parity CI](../.github/workflows/ci.yml) runs seed 1 with 25 random cases per generator only when translators, providers, core, the pin, parity tooling, manifests, or similar listed paths change; a docs-only PR does not trigger that comparison.",
        "[Scheduled parity](../.github/workflows/parity-scheduled.yml) runs nightly at 06:23 UTC, and manually, with 2,000 random cases per generator by default. The existing tests job checks metadata on PRs, merge groups and manual runs when its Rust job runs; pushes to main only warm test caches. Metadata export does not execute Go.",
        "",
        "This is translation evidence only: it proves no provider account, credential type, or public route, and fills none of the provider/credential cells.",
        "Format names are registry identifiers: openai/openai-chat mean Chat Completions, openai-responses means Responses, claude means Messages, gemini means generateContent, interactions means Gemini Interactions, and codex is the Codex backend's Responses format.",
        "Counts in links are maintained **hand-written cases**, including signature-relay cases, with seed 1 and zero random cases; CI's generated random cases are additional. Request compatibility variants are listed separately. The tool-input-error helper checks stream finalization/error presence, not a successful response.",
        "",
    ])
    translations = parity_cells(parity)
    clients = sorted({client for client, _ in translations})
    upstreams = sorted({provider for _, provider in translations})
    lines.extend([
        "| Client protocol | " + " | ".join(upstreams) + " |",
        "|---|" + "---|" * len(upstreams),
    ])
    directions = {"request": "Request", "request-compat": "Request (compat)", "streamed-response": "Streamed response", "whole-response": "Whole response"}
    for client in clients:
        values = []
        for provider in upstreams:
            entries = translations.get((client, provider), [])
            values.append("<br>".join(
                f"P {directions[entry['direction']]}: [{entry['slug']} ({entry['cases']})](#p-{entry['slug']})"
                for entry in entries
            ) or "? (no direct suite)")
        lines.append(f"| {client} | " + " | ".join(values) + " |")
    helpers = [entry for entry in parity if entry["direction"] in ("reasoning", "model-list", "response-error")]
    lines.extend(["", "Protocol helpers from the same registry:", "", "| Client operation | Upstream format | Evidence |", "|---|---|---|"])
    for entry in helpers:
        lines.append(f"| {entry['client_format']} | {entry['upstream_format']} | P {entry['direction']}: [{entry['slug']} ({entry['cases']})](#p-{entry['slug']}) |")
    parity_unknown = len(clients) * len(upstreams) - len(translations)
    lines.extend(["", f"Parity count: **{len(parity)} parity-tested suites**, covering **{len(translations)}** nonempty client/upstream translation cells and **{len(helpers)}** protocol-helper entries. The other **{parity_unknown}** direct-translation cells are unknown; they do not imply an indirect chain is unsupported."])
    lines.extend(["", "## Provider and route fixture details", "", "Only rows with fixture or refusal evidence are shown. Each table uses its meaningful capability set; absent provider/credential rows stay unknown."])
    for protocol, label in PROTOCOLS.items():
        providers = [provider for provider in PROVIDERS if any(key[:2] == (protocol, provider) for key in cells)]
        if not providers:
            continue
        capabilities = MEANINGFUL[protocol]
        lines.extend(["", f"### {label}", "", "| Provider | Credential | " + " | ".join(CAPABILITIES[capability] for capability in capabilities) + " |", "|---|---|" + "---|" * len(capabilities)])
        for provider in providers:
            name, credential = PROVIDERS[provider]
            values = [evidence_cell(cells[(protocol, provider, capability)]) if (protocol, provider, capability) in cells else "?" for capability in capabilities]
            lines.append(f"| {name} | {credential} | " + " | ".join(values) + " |")
        missing = [f"{name} / {credential}" for provider, (name, credential) in PROVIDERS.items() if provider not in providers and eligible(protocol, provider)]
        lines.extend(["", "No evidence yet: " + "; ".join(missing) + "."])
    lines.extend([
        "", "## Regeneration and checks", "",
        "Run from the repository root with Rust and Python 3.11+; no Go, credentials, live proxy, upstream checkout, or git history is needed:",
        "", "```sh", "python .github/scripts/compatibility.py", "python .github/scripts/compatibility.py --check", "python -I .github/scripts/test_compatibility.py", "python .github/scripts/compatibility.py --accept E03  # only after reviewing scope/cells", "```", "",
        "The [fixture mapping](../.github/scripts/compatibility-evidence.json) names exact tests; generation/checking verify their source declarations and compiled test names, and run each with one pass and zero ignored tests. Required CI checks committed document bytes in the existing tests job.",
        "The checker reuses cargo test --workspace --locked --no-run --message-format=json and reports non-fresh artifacts; no subset or normal binary build is used. A test in the already-built parity harness writes the JSON catalog to a temporary output path. This transport test is not evidence.",
        "Parity formats/directions/counts come from the actual registry. Intentional exclusions are explicitly named with reasons; every other unclassified key is an error, including newly added suites.",
        "Fixture identity is the Open Ferry source path, exact resolved test definition, reviewed function-body digest in the catalog, and review date. A small Rust lexer removes comments/whitespace outside literals and matches braces through strings, raw strings and chars. Edits outside the body or comments/formatting do not require review; changed body tokens fail until an explicit --accept ID records only that digest after scope/cell review. Missing tests or extraction failures cannot be accepted. Run --check afterwards; no whole-file digest, line anchor, workspace release version or generation time is used.",
        "", "## Evidence catalog", "",
        f"Fixture entries identify reviewed **Open Ferry test bodies** (body_sha256 in the linked catalog) and review date **{catalog['reviewed']}**, not an upstream version. This dates the mapping review, not a live run. The parity catalog separately uses the current upstream pin above.",
    ])
    for entry in catalog["evidence"]:
        lines.extend(["", f"### {entry['id']}", "", f"{entry['level']}; Open Ferry fixture (reviewed body revision in catalog); reviewed {catalog['reviewed']}.", ""])
        if entry["level"] == "fixture-tested":
            lines.append(f"Test: [`{entry['path']}::{entry['test']}`](../{entry['path']}).")
        else:
            lines.append(f"Refusal: [`{entry['path']}`](../{entry['path']}); `{entry['refusal']}`. Checked by `{entry['test']}` in the same file.")
        lines.extend(["", entry["scope"]])
    lines.extend(["", "### Parity suite evidence", "", "Each slug is selectable with `--suite SLUG` in a real offline comparison. Labels come from the [translator registry](../tools/parity/src/translator.rs) and its [Interactions families](../tools/parity/src/interactions/mod.rs).", "", "| Suite slug | Registry label | Hand-written cases |", "|---|---|---:|"])
    for entry in parity:
        lines.append(f"| <a id=\"p-{entry['slug']}\"></a>`{entry['slug']}` | `{entry['label']}` | {entry['cases']} |")
    lines.append("")
    return "\n".join(lines)


def check_document(expected: str, path: Path = DOCUMENT) -> None:
    if not path.is_file() or path.read_bytes() != expected.encode("utf-8"):
        raise ValueError("docs/compatibility.md is out of date; run python .github/scripts/compatibility.py")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail instead of writing an outdated document")
    parser.add_argument("--accept", metavar="ID", help="record only this test body's reviewed digest; does not run tests or change cells")
    args = parser.parse_args()
    try:
        if args.accept:
            if args.check:
                raise ValueError("--accept and --check must run separately")
            accept_digest(args.accept)
            print(f"Accepted reviewed test body for {args.accept}")
            return 0
        catalog = load_catalog()
        harnesses = build_harnesses(catalog)
        validate_evidence(catalog, harnesses)
        parity = load_parity(harnesses)
        output = render(catalog, parity)
        if args.check:
            check_document(output)
        else:
            DOCUMENT.write_bytes(output.encode("utf-8"))
        print(json.dumps({**cell_counts(catalog), "scripted-dispatch": dict(cell_counts(catalog, scripted=True)), "parity-tested-suites": len(parity), "parity-translation-cells": len(parity_cells(parity)), "non-fresh-artifacts": harnesses.non_fresh}))
        return 0
    except (ValueError, KeyError, OSError) as error:
        print(error, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
