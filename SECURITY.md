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
