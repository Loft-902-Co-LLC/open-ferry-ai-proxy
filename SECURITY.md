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

Each release's `SHA256SUMS` is signed with a [minisign](https://jedisct1.github.io/minisign/) key, as `SHA256SUMS.minisig`. Its trusted comment names the release, as in `open-ferry 0.1.0 SHA256SUMS`. The public key is in [`release-keys.pub`](release-keys.pub) and built into the binary, which checks the signature before it installs an update (see [docs/updates.md](docs/updates.md)); the private key never leaves a GitHub environment that the maintainer approves for each release. To check a download by hand, run `minisign -Vm SHA256SUMS -P <key>`, with `<key>` the key line from `release-keys.pub`, then check the archive against `SHA256SUMS` (`sha256sum --check --ignore-missing SHA256SUMS`). Report a signature that doesn't check out, or a key that differs from the one in this repository, as a vulnerability.

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
