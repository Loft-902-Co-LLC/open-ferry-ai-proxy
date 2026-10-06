# open-ferry-ai-proxy

A Rust port of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) with a built-in UI and first-class WebSocket support.

> [!NOTE]
> **Credit where it's due.** open-ferry-ai-proxy is a port of CLIProxyAPI by
> [Luis Pater](https://github.com/luispater) and [Router-For.ME](https://github.com/router-for-me),
> and of the work of its 260+ contributors. Its request translation, routing, and
> management API are modelled on CLIProxyAPI's, and it reads CLIProxyAPI's config and
> auth files. This project is **not affiliated with or endorsed by** CLIProxyAPI,
> Router-For.ME, OpenAI, or Anthropic. See [UPSTREAM.md](UPSTREAM.md) for exactly what
> was ported from where.

## Status

**Pre-alpha.** This README describes what we're building, not all of what exists. `open-ferry` reads CLIProxyAPI's `config.yaml` and auth directory, signs in to Codex and Claude with their OAuth logins (`-codex-login`, `-codex-device-login`, `-claude-login`), and serves Chat Completions, legacy Completions, Claude Messages, and OpenAI Responses over HTTP and WebSocket, with upstream's routing, errors, streaming and keep-alives. It serves the Gemini API's `/v1beta/models` routes too (model lists, `generateContent`, `streamGenerateContent` and `countTokens`), and the model list Codex clients fetch, from upstream's Codex client catalog. With `xai-api-key` credentials it serves the video endpoints too, xAI's own `/v1/videos` routes and OpenAI's `/openai/v1/videos`, and streams the finished video without writing it to disk. It serves the images endpoints, `/v1/images/generations` and `/v1/images/edits`, through Codex's image generation tool, xAI's Grok Imagine image models and the image models of `openai-compatibility` providers, and reads an edit's uploaded images in memory, never writing them to disk. It also calls the OpenAI-compatible upstreams of the config (`openai-compatibility`), any provider that speaks Chat Completions, with their API keys, headers, proxies and model aliases, and Gemini and Vertex AI, with the config's `gemini-api-key` and `vertex-api-key` entries and Vertex AI service accounts from the auth directory. Requests to Codex and to the OpenAI-compatible upstreams carry the thinking setting a model suffix such as `gpt-5.5(high)` or the client's request asks for, checked against the model's levels. Behind the server, upstream's credential manager picks an account per request, with its retries, cooldowns and model aliases, refreshes tokens in the background, and follows changes to the config and auth files. With the config's `codex.model-level-cooling`, a Codex usage limit cools only the model, and with `codex.stream-bootstrap-buffering`, a Codex stream that reports an overload before it starts fails over to another account. It also serves the part of CLIProxyAPI's `/v0/management` API that T3 Code's hub uses, with upstream's key checks and bans: `auth-files` (and `auth-files/models`), `api-call` and `reset-quota`, under their `/v8/management` names too, and more: the credential files and state, the OAuth logins, the config reads and writes (a save keeps the file's comments and takes effect before the answer), `logs`, the usage queue and `quota/fetch`. The plugin routes and other providers' ones aren't ported ([UPSTREAM.md](UPSTREAM.md#not-ported)). Behind them, `request-log` and the error logs write each request's log to the log directory, `logging-to-file` writes `main.log`, `usage-statistics-enabled` queues a usage record for each call, the config's `payload` rules and `disable-image-generation` apply, and `save-cooldown-status` keeps cooldowns across restarts, all as upstream does (see [UPSTREAM.md](UPSTREAM.md#observability)). With `-tui` it runs upstream's terminal management UI, as a client of a running server's management API or, with `-standalone`, of a server it runs itself ([UPSTREAM.md](UPSTREAM.md#the-tui)). A web dashboard is built into the binary, at `/dashboard/` ([below](#dashboard)). The translators between Codex and three client formats, Claude Messages, OpenAI Responses and OpenAI Chat Completions, are ported in both directions, as are those between Claude and the two OpenAI formats, along with upstream's checks of every provider's reasoning signatures. So are the translators for upstreams that only speak OpenAI Chat Completions, for Claude Messages and OpenAI Responses clients, the passthrough for Chat Completions clients, the translators from Gemini `generateContent` clients to Codex, Claude and Chat Completions upstreams, and the translators for Gemini, Claude Messages, Chat Completions and OpenAI Responses clients to Gemini upstreams. For Claude clients of Codex, which drop Codex's encrypted reasoning, it keeps each turn's reasoning and tool calls in memory by the session the client names and puts them back in that session's next request, as upstream does. With `client.codex.optimize-multi-agent-v2`, a Codex client's multi-agent v2 requests can go to other upstreams, with the models it may delegate to listed in `spawn_agent`, and with `codex.orphan-delegation-compatibility`, a Codex sub-agent's delegation outputs without their call become user messages; a `codex-api-key` model marked `is-compat` gets requests in Codex's plain Responses dialect. The translators are checked against upstream's ([UPSTREAM.md](UPSTREAM.md#checking-parity)).

## Install

Coming from CLIProxyAPI? Read the [migration guide](docs/migrating-from-cliproxyapi.md) first: it covers what carries over and what doesn't.

No release has been published yet. Until the first one, [build from source](#build-from-source).

### Download a release

Each [release](https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases) has an archive for each platform:

| Platform | Archive |
|---|---|
| Linux, x86-64 | `open-ferry-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux, arm64 | `open-ferry-<version>-aarch64-unknown-linux-gnu.tar.gz` |
| macOS, Intel | `open-ferry-<version>-x86_64-apple-darwin.tar.gz` |
| macOS, Apple silicon | `open-ferry-<version>-aarch64-apple-darwin.tar.gz` |
| Windows, x86-64 | `open-ferry-<version>-x86_64-pc-windows-msvc.zip` |

- **Linux:** the binaries need glibc 2.31 or newer, as on Debian 11, Ubuntu 20.04, Fedora 32, RHEL 9 and Amazon Linux 2023, or later. On older systems, build from source.
- **macOS:** the binaries need macOS 11 or newer.

Each archive holds the `open-ferry` binary (`open-ferry.exe` on Windows), an example config, `config.example.yaml`, this README and the licenses. Put the binary on your `PATH`. Run it in the directory that holds your `config.yaml`, or point to the file with `-config`. For a new setup, copy `config.example.yaml` to `config.yaml` and replace its example `api-keys` with keys of your own: until you do, the proxy refuses service. Then add your upstream credentials, from the commented examples in the file or by signing in.

The binaries aren't code-signed. If macOS refuses to open one you downloaded with a browser, verify it as below, then remove the quarantine flag with `xattr -d com.apple.quarantine open-ferry`.

### Verify the download

Each release has a `SHA256SUMS` file and a build provenance attestation for each archive, both made by the [release workflow](.github/workflows/release.yml). Check both before you run the binary.

To check the checksum, download `SHA256SUMS` into the same directory as the archive, then run:

```sh
sha256sum --check --ignore-missing SHA256SUMS               # Linux
shasum -a 256 --check --ignore-missing SHA256SUMS           # macOS
```

On Windows, in PowerShell, this prints `True` when the archive matches:

```powershell
$archive = "open-ferry-<version>-x86_64-pc-windows-msvc.zip"
(Get-FileHash $archive).Hash -eq (Select-String -Path SHA256SUMS -SimpleMatch $archive).Line.Split(" ")[0]
```

To check the attestation, use the [GitHub CLI](https://cli.github.com/). It confirms that the archive was built by this repository's GitHub Actions, and from which commit:

```sh
gh attestation verify open-ferry-<version>-<target>.tar.gz --repo Loft-902-Co-LLC/open-ferry-ai-proxy
```

### Build from source

You need the Rust toolchain ([rustup](https://rustup.rs/)) and a C compiler, because rustls's crypto library, aws-lc-rs, and SQLite, which the usage ledger is kept in, are compiled from C:
- on Linux, `gcc` or `clang`;
- on macOS, the Xcode Command Line Tools;
- on Windows, the Visual Studio Build Tools with the C++ workload.

On some systems the build also asks for CMake. Then run:

```sh
git clone https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy.git
cd open-ferry-ai-proxy
cargo build --release --locked -p open-ferry
```

The binary is `target/release/open-ferry`. `rust-toolchain.toml` picks the toolchain. The example config, [`config.example.yaml`](config.example.yaml), is at the root of the repository; set it up as [above](#download-a-release).

To build the [dashboard](#dashboard) in, build its app first, with [Node.js](https://nodejs.org/) 22.22.2 or newer, then open-ferry:

```sh
cd dashboard
npm ci
npm run build
cd ..
cargo build --release --locked -p open-ferry
```

Without the app the build works all the same, and so do the proxy, the management API and the dashboard's own API, but `/dashboard/` shows only a page saying the app wasn't built in, and how to build it. The release binaries have it built in.

## Dashboard

open-ferry serves a web dashboard of its own at `/dashboard/` on the proxy's port, such as `http://localhost:<port>/dashboard/`, and CLIProxyAPI's `/management.html` sends the browser there. It is built into the binary and fetches nothing from elsewhere. Sign in with the management key, the config's `management.secret-key` or `MANAGEMENT_PASSWORD`, or on the same machine with the `-password` open-ferry was started with; the dashboard keeps it for that browser tab only. As with the management API, it answers only on the same machine unless `management.allow-remote` is on, which `MANAGEMENT_PASSWORD` implies, and `management.disable-control-panel` turns it off. A config in the legacy layout has these keys under `remote-management:`, which is still read.

It shows each credential's state, with the reason and what to do about it, and from there you can upload credential files, sign in to Claude or Codex, turn a credential off or on, reset its cooldown, check its quota, or delete it. Its Settings page has the common settings as a form, checked as you type and reviewed against the server before each save, along with the client API keys and `config.yaml` in an editor that shows a diff before saving. A save keeps the file's comments and the settings the page doesn't show, and takes effect at once. On a first run, the Overview replaces CLIProxyAPI's example client keys, which keep the proxy in safe mode, with a new key in one step, and its client setups use that key. Besides the management API, the dashboard reads an API of its own ([docs/dashboard-api.md](docs/dashboard-api.md)): usage and estimated cost per model, credential and client key, from a local SQLite ledger; a search of the request logs; and ready-made setups for common clients. The ledger is `open-ferry-usage.sqlite3` in the log directory. It records calls while `usage-statistics-enabled` is on, for 90 days by default, with no prompts or answers and client keys only masked, and estimates costs from the prices you enter.

## Roadmap

[ROADMAP.md](ROADMAP.md) has the full plan.

- **v1:** parity with the latest CLIProxyAPI release for the providers we support, the image and video endpoints, a web dashboard much easier to use than CLIProxyAPI's, and a TUI.
- **v2:** the Realtime API over WebSocket and WebRTC.
- **v3:** plugins, if a survey of the plugins people actually use shows they're worth it.

Throughout:
- **Drop-in config:** we read CLIProxyAPI's `config.yaml` and auth directory, so switching is cheap.
- **T3 Code compatible:** we serve the `/v0/management` subset T3 Code's CLIProxyAPI hub uses.
- **WebSockets done carefully:**
  - one account per connection;
  - keepalive pings;
  - a clean fallback to HTTP;
  - an honest `previous_response_not_found` when state is lost.
- **One binary:** the web UI is embedded and served from the proxy.

## Non-goals

- **Client impersonation.** Requests use each provider's official OAuth flow and its documented protocol headers. We don't spoof TLS fingerprints, generate synthetic user IDs, or otherwise disguise proxied traffic. So the CLIProxyAPI providers and sign-ins that only work by posing as another company's app aren't ported ([ROADMAP.md](ROADMAP.md#not-planned)).
- **Every CLIProxyAPI feature.** What we leave out, and why, is listed in [UPSTREAM.md](UPSTREAM.md) and [ROADMAP.md](ROADMAP.md).

## Layout

| Path | Purpose |
|---|---|
| `crates/open-ferry` | Binary: CLI and server entry point |
| `crates/open-ferry-core` | Config, credentials and their store, the model registry, and the credential manager that routes calls |
| `crates/open-ferry-providers` | The Codex and Claude OAuth logins and executors, the OpenAI-compatible executor, and the Gemini and Vertex AI executors |
| `crates/open-ferry-translate` | Format translators between OpenAI, Anthropic and Gemini. Pure functions with no I/O, publishable on its own |
| `crates/open-ferry-server` | HTTP and WebSocket handlers for `/v1/*` and the Gemini API's `/v1beta/*` |
| `crates/open-ferry-management` | CLIProxyAPI-compatible `/v0/management` API |
| `crates/open-ferry-dashboard` | The dashboard: serves the app, its API under `/open-ferry/api/v1/`, and the usage ledger |
| `crates/open-ferry-tui` | The terminal management UI (`-tui`), a client of the management API |
| `dashboard` | The dashboard app, in TypeScript and React, built with Vite into the binary |
| `reference/cliproxyapi` | Upstream source, pinned as a git submodule. Used as the spec and for comparison tests; never compiled in |

## A word on terms of service

This tool lets you use your own subscription credentials through a local proxy. Whether that's allowed depends on each provider's terms, and some users of similar tools have had accounts restricted. It's your account and your call; read the terms first.

## License

MIT. See [LICENSE](LICENSE). The Luis Pater and Router-For.ME copyright lines cover the portions ported from CLIProxyAPI (MIT); its original license is reproduced verbatim in [licenses/CLIProxyAPI-LICENSE](licenses/CLIProxyAPI-LICENSE). Small parts of Go's standard library, gin, protobuf-go, gjson, sjson, gorilla/websocket and godotenv are ported too, under their BSD and MIT licenses, as are the parts of Bubble Tea, Bubbles, Lip Gloss, Charm's x/ansi and x/cellbuf, and termenv that the TUI draws and edits text with, under the MIT license. yaml.v3 is ported too (its decoding rules for the config loader, and its scanner, parser and emitter for the config writer), under the MIT license for the parts it took from libyaml and the Apache License 2.0 with its NOTICE for the rest. All are in [licenses/](licenses). The release archives' `licenses/` also holds `rust-third-party-licenses.txt` and `dashboard-third-party-licenses.txt`, the licenses of the Rust crates and npm packages built into the binary.
