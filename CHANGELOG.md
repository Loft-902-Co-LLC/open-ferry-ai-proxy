# Changelog

All notable changes to open-ferry are recorded here. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and open-ferry follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0.0, a minor version may change behaviour.

Where open-ferry differs from CLIProxyAPI, which it ports, is in [UPSTREAM.md](UPSTREAM.md). How to move over is in the [migration guide](docs/migrating-from-cliproxyapi.md).

## [Unreleased]

Nothing has been released yet. This is what is in place today, ported from CLIProxyAPI v8.0.15.

### Added

- **CLIProxyAPI's `config.yaml` and auth directory, read as they are**, in both the legacy and the v8 layout. Changes to either are picked up without a restart.
- **Sign-ins** to Codex (`-codex-login`, or `-codex-device-login` with a device code) and to Claude (`-claude-login`).
- **The client APIs**, over HTTP and WebSocket:
  - OpenAI Chat Completions, legacy Completions and Responses;
  - Claude Messages;
  - the Gemini API's `/v1beta/models` routes, and Gemini Interactions;
  - the model list Codex clients fetch.
- **The providers**: Codex, Claude, Gemini, Gemini Interactions, Vertex AI, Meta (API keys and access tokens), xAI (API keys) and any OpenAI-compatible upstream. Requests are translated between the client's format and the provider's.
- **CLIProxyAPI's credential manager**: it picks an account for each request, retries on another, handles cooldowns and quota, applies model aliases, and refreshes tokens in the background.
- **The management API subset T3 Code uses, and more**, under `/v0/management` and `/v8/management`:
  - credential files and their state;
  - the OAuth logins;
  - reads of the config;
  - logs, usage and quota fetches.

  The config can't be changed through it yet.
- **Request logs, `main.log`, usage records, payload rules and saved cooldowns**, as CLIProxyAPI has them.
- **CLIProxyAPI's terminal UI** (`-tui`), as a client of a running server's management API (`-management-base-url`) or of one it starts (`-standalone`). Its OAuth tab offers Codex and Claude.
- **`-password`**, a management password for clients on the same machine, with CLIProxyAPI's `/keep-alive` endpoint.
- **A web dashboard** at `/dashboard/`, built into the binary, in place of CLIProxyAPI's downloaded control panel: `/management.html` sends the browser there. It signs in with the management key, and reads the management API and an API of its own under `/open-ferry/api/v1/` ([docs/dashboard-api.md](docs/dashboard-api.md)): usage, a search of the request logs, and client setup.
- **A usage ledger**, `open-ferry-usage.sqlite3` in the log directory, which keeps the usage records for the dashboard while `usage-statistics-enabled` is on: 90 days by default, with no prompt or answer text and no client key in clear. Costs are estimated from prices you enter.
- **Release binaries** for Linux (x86-64 and arm64), macOS (Intel and Apple silicon) and Windows (x86-64). Each release comes with `SHA256SUMS` and build provenance attestations.

[Unreleased]: https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/commits/main
