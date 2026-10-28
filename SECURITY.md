# Security policy

open-ferry handles API keys, OAuth tokens and the traffic of everyone who uses it, so we take reports seriously.

## Reporting a vulnerability

**Please don't open a public issue.** Report privately through GitHub instead: go to the repository's **Security** tab and choose **Report a vulnerability**. That opens a private advisory that only the maintainer can see.

Please include:
- what an attacker can do, and what they need to do it (for example, network access to the proxy, a client API key, or a management key);
- steps or a test case that reproduce it;
- the commit or release you tested.

**Never include real secrets in a report.** Use made-up keys and tokens.

We aim to acknowledge a report within a week. Once a fix is out, we publish the advisory and credit you, unless you'd rather not be named.

## Supported versions

open-ferry is pre-release. Fixes go to `main`, and to the latest release once releases begin.

## Signed releases

Each release's `SHA256SUMS` is signed with a [minisign](https://jedisct1.github.io/minisign/) key, as `SHA256SUMS.minisig`. Its trusted comment names the release, as in `open-ferry 0.1.0 SHA256SUMS`. The public key is in [`release-keys.pub`](release-keys.pub) and built into the binary, which checks the signature before it installs an update (see [docs/updates.md](docs/updates.md)). The signing job receives the private key from a GitHub environment that the maintainer approves for each release; the maintainer keeps a recovery backup as described in [RELEASING.md](RELEASING.md#the-release-key). To check a download by hand, run `minisign -V -H -m SHA256SUMS -P <key>`, with `<key>` the key line from `release-keys.pub`, then check the archive against `SHA256SUMS` (`sha256sum --check --ignore-missing SHA256SUMS`). Report a signature that doesn't check out, or a key that differs from the one in this repository, as a vulnerability.

Releases also include CycloneDX 1.5 JSON software bills of materials: `open-ferry-sbom-rust-<target>.cdx.json` for each binary target, and `open-ferry-sbom-dashboard.cdx.json` for the production npm dependencies embedded in every binary. Rust SBOMs include normal and build dependencies, with build dependencies marked excluded from runtime scope; test dependencies are omitted. Target dependency sets differ, so use the file for your target together with the dashboard file. All eight files are release assets listed in the same signed `SHA256SUMS` and have build provenance attestations. Verify the signature and their hashes just as for an archive.

## Dependencies

[Supply chain](.github/workflows/supply-chain.yml) checks the full `Cargo.lock` against the current RustSec database with cargo-audit 0.22.2, treating advisory warnings as failures, and checks the dashboard's production lockfile dependencies with `npm audit --omit=dev --audit-level=low` (all severities). CI calls it when Cargo manifests, either lockfile, dashboard package manifests, or the tooling change; a daily run checks for newly published advisories. These jobs are outside the required `check` aggregate, including on the merge queue. Required-check policy remains the owner's responsibility. License compliance continues to use `about.toml` and cargo-about.

Disposition each finding in a reviewed change: prefer a small safe dependency update; otherwise record the advisory ID, an evidence-backed reachability or risk reason, and a calendar review date. Rust ignores live in [`.cargo/audit.toml`](.cargo/audit.toml), with that reason and date beside each ID; remove or reassess them by that date. There are currently no Rust or npm exceptions. The npm check has no allowlist: fix findings before it passes; any future exception needs an explicit advisory-specific filter with the same evidence and review date. Do not suppress findings by raising the severity threshold or disabling the check. Review daily failures even without a dependency PR, and re-run the checks after an update or disposition.

Run locally with `cargo install cargo-audit --version 0.22.2 --locked`, `cargo audit --file Cargo.lock --deny warnings`, and, in `dashboard/`, `npm audit --omit=dev --audit-level=low`. Neither audit requires compiling the application or installing the dashboard.

The 2026-10-28 baseline audit found three certificate trust advisories, all fixed with lockfile-only patch updates: [RUSTSEC-2026-0382](https://rustsec.org/advisories/RUSTSEC-2026-0382.html), `rustls-webpki` 0.103.15 to 0.103.16; [RUSTSEC-2026-0381](https://rustsec.org/advisories/RUSTSEC-2026-0381.html), `webpki-roots` 1.0.9 to 1.0.10; and [RUSTSEC-2026-0383](https://rustsec.org/advisories/RUSTSEC-2026-0383.html), `webpki-root-certs` 1.0.9 to 1.0.10. The repeat Rust audit and production npm audit found no remaining findings; no exception or review-date waiver was needed.

## Scope

**In scope:**
- the code in this repository;
- leaks of credentials or keys through logs, files on disk, error messages or responses;
- bypasses of client API keys or the management key;
- request smuggling or SSRF through the proxy;
- unsafe handling of config and auth files.

**Out of scope:**
- **Bugs in CLIProxyAPI itself.** Report those [upstream](https://github.com/router-for-me/CLIProxyAPI). If they affect open-ferry too, tell us as well.
- **Providers' terms of service.** Whether using a subscription through a proxy is allowed is between you and the provider.
