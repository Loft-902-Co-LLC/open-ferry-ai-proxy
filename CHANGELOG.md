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
- **The images endpoints**, `/v1/images/generations` and `/v1/images/edits`: the Codex image generation tool's models (`gpt-image-1.5`, `gpt-image-2` and the `gpt-image-2.5` ones) through Codex credentials, on `gpt-5.4-mini` or the model `gpt-image-2-base-model` names; xAI's Grok Imagine image models through `xai-api-key` credentials, which now list them; and the image models of `openai-compatibility` providers. An edit can be a `multipart/form-data` form, read in memory up to the body limit and never written to disk. `disable-image-generation: true` turns both endpoints away.
- **The video endpoints**, served by xAI's Grok Imagine Video for `xai-api-key` credentials: xAI's own `/v1/videos` routes, and OpenAI's `/openai/v1/videos` with a video's status and its content. Later calls about a video go to the key that made it, remembered in memory for `video-result-auth-cache-ttl` (three hours by default). The video is streamed to the client, never written to disk.
- **The providers**: Codex, Claude, Gemini, Gemini Interactions, Vertex AI, Meta (API keys and access tokens), xAI (API keys) and any OpenAI-compatible upstream. Requests are translated between the client's format and the provider's.
- **CLIProxyAPI's credential manager**: it picks an account for each request, retries on another, handles cooldowns and quota, applies model aliases, and refreshes tokens in the background.
- **The management API subset T3 Code uses, and more**, under `/v0/management` and `/v8/management`:
  - credential files and their state;
  - the OAuth logins;
  - reads of the config;
  - logs, usage and quota fetches.
- **Config writes through the management API**: the settings, the client and provider key lists, the OAuth lists and `config.yaml` under `/v0/management`, and the writes of `config`, `config.yaml` and a path in the config under `/v8/management`, including turning a config API key off or on. A save keeps the file's comments, its key order and the settings open-ferry doesn't type, replaces the file in one step with the previous one kept as `config.yaml.bak`, and takes effect before the request is answered. A save that fails changes nothing.
- **Request logs, `main.log`, usage records, payload rules and saved cooldowns**, as CLIProxyAPI has them.
- **CLIProxyAPI's terminal UI** (`-tui`), as a client of a running server's management API (`-management-base-url`) or of one it starts (`-standalone`). Its OAuth tab offers Codex and Claude.
- **`-password`**, a management password for clients on the same machine, with CLIProxyAPI's `/keep-alive` endpoint.
- **A web dashboard** at `/dashboard/`, built into the binary, in place of CLIProxyAPI's downloaded control panel: `/management.html` sends the browser there. It signs in with the management key, and reads the management API and an API of its own under `/open-ferry/api/v1/` ([docs/dashboard-api.md](docs/dashboard-api.md)). It shows each credential's state, with the reason and what to do, and uploads, signs in, turns off, resets and deletes credentials; it also shows usage, searches the request logs, and gives client setups. Its Settings page edits the common settings, the client API keys and `config.yaml`, with a review before each save, and on a first run its Overview replaces the example client keys with a new one in one step, taking the proxy out of safe mode.
- **A usage ledger**, `open-ferry-usage.sqlite3` in the log directory, which keeps the usage records for the dashboard while `usage-statistics-enabled` is on: 90 days by default, with no prompt or answer text and no client key in clear. Costs are estimated from prices you enter.
- **Release binaries** for Linux (x86-64 and arm64), macOS (Intel and Apple silicon) and Windows (x86-64). Each release comes with `SHA256SUMS` and build provenance attestations.

[Unreleased]: https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/commits/main
