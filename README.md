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

**Pre-alpha.** This README describes what we're building, not all of what exists. `open-ferry` reads CLIProxyAPI's `config.yaml` and auth directory, signs in to Codex and Claude with their OAuth logins (`-codex-login`, `-codex-device-login`, `-claude-login`), and serves Chat Completions, legacy Completions, Claude Messages, and OpenAI Responses over HTTP and WebSocket, with upstream's routing, errors, streaming and keep-alives. The Claude sign-in goes against Anthropic's terms and, in our testing, gets only the Haiku models ([why](docs/claude-subscription.md#the-claude-sign-in)). It serves the Gemini API's `/v1beta/models` routes too (model lists, `generateContent`, `streamGenerateContent` and `countTokens`), and the model list Codex clients fetch, from upstream's Codex client catalog. With `xai-api-key` credentials it serves the video endpoints too, xAI's own `/v1/videos` routes and OpenAI's `/openai/v1/videos`, and streams the finished video without writing it to disk. It serves OpenAI's speech endpoint, `/v1/audio/speech`, and xAI's `/v1/tts`, through xAI's Grok TTS models (`grok-tts`, `grok-voice-tts-1.0`). It serves the images endpoints, `/v1/images/generations` and `/v1/images/edits`, through Codex's image generation tool, xAI's Grok Imagine image models and the image models of `openai-compatibility` providers, and reads an edit's uploaded images in memory, never writing them to disk. Through Codex, the provider picks the image's size: it gets the request's `size` as sent, yet in our testing `1024x1024` came back as 1536x1024 and as 1254x1254. The answer's `size` gives the real one. It also calls the OpenAI-compatible upstreams of the config (`openai-compatibility`), any provider that speaks Chat Completions, with their API keys, headers, proxies and model aliases, and Gemini and Vertex AI, with the config's `gemini-api-key` and `vertex-api-key` entries and Vertex AI service accounts from the auth directory. With the config's `claude-cli` entries, it serves Claude models through your own installed Claude Code, which signs itself in to your subscription and makes the requests to Anthropic ([docs/claude-subscription.md](docs/claude-subscription.md)). Requests to Codex and to the OpenAI-compatible upstreams carry the thinking setting a model suffix such as `gpt-5.5(high)` or the client's request asks for, checked against the model's levels. Behind the server, upstream's credential manager picks an account per request, with its retries, cooldowns and model aliases, refreshes tokens in the background, and follows changes to the config and auth files. With the config's `codex.model-level-cooling`, a Codex usage limit cools only the model, and with `codex.stream-bootstrap-buffering`, a Codex stream that reports an overload before it starts fails over to another account. It also serves the part of CLIProxyAPI's `/v0/management` API that T3 Code's hub uses, with upstream's key checks and bans: `auth-files` (and `auth-files/models`), `api-call` and `reset-quota`, under their `/v8/management` names too, and more: the credential files and state, the OAuth logins, the config reads and writes (a save keeps the file's comments and takes effect before the answer), `logs`, the usage queue and `quota/fetch`. The plugin routes and other providers' ones aren't ported ([UPSTREAM.md](UPSTREAM.md#not-ported)). Behind them, `request-log` and the error logs write each request's log to the log directory, `logging-to-file` writes `main.log`, `usage-statistics-enabled` queues a usage record for each call, the config's `payload` rules and `disable-image-generation` apply, and `save-cooldown-status` keeps cooldowns across restarts, all as upstream does (see [UPSTREAM.md](UPSTREAM.md#observability)). With `-tui` it runs upstream's terminal management UI, as a client of a running server's management API or, with `-standalone`, of a server it runs itself ([UPSTREAM.md](UPSTREAM.md#the-tui)). A web dashboard is built into the binary, at `/dashboard/` ([below](#dashboard)). The translators between Codex and three client formats, Claude Messages, OpenAI Responses and OpenAI Chat Completions, are ported in both directions, as are those between Claude and the two OpenAI formats, along with upstream's checks of every provider's reasoning signatures. So are the translators for upstreams that only speak OpenAI Chat Completions, for Claude Messages and OpenAI Responses clients, the passthrough for Chat Completions clients, the translators from Gemini `generateContent` clients to Codex, Claude and Chat Completions upstreams, and the translators for Gemini, Claude Messages, Chat Completions and OpenAI Responses clients to Gemini upstreams. For Claude clients of Codex, which drop Codex's encrypted reasoning, it keeps each turn's reasoning and tool calls in memory by the session the client names and puts them back in that session's next request, as upstream does. With `client.codex.optimize-multi-agent-v2`, a Codex client's multi-agent v2 requests can go to other upstreams, with the models it may delegate to listed in `spawn_agent`, and with `codex.orphan-delegation-compatibility`, a Codex sub-agent's delegation outputs without their call become user messages; a `codex-api-key` model marked `is-compat` gets requests in Codex's plain Responses dialect. The translators are checked against upstream's ([UPSTREAM.md](UPSTREAM.md#checking-parity)).

## Install

Coming from CLIProxyAPI? Read the [migration guide](docs/migrating-from-cliproxyapi.md) first: it covers what carries over and what doesn't.

No release has been published yet. Until the first one, [build from source](#build-from-source): the install scripts, the container image and the archives below all come from a release.

### Install script

On Linux or macOS:

```sh
curl -fsSL https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.sh | sh
```

On Windows, in PowerShell (Windows PowerShell 5.1 or PowerShell 7):

```powershell
irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1 | iex
```

The script:

- downloads the release's archive for your system, and refuses it unless it matches the release's `SHA256SUMS`. On Linux it picks the static (musl) build on Alpine, on systems without glibc and on those with a glibc older than 2.31, and the glibc build everywhere else;
- if the [GitHub CLI](https://cli.github.com/) is installed, checks the archive's build provenance attestation with `gh attestation verify`, and refuses an archive that fails it. Without `gh`, it says that the attestation wasn't checked;
- installs the binary as `~/.local/bin/open-ferry` (`%LOCALAPPDATA%\Programs\open-ferry\open-ferry.exe` on Windows), replacing an older one;
- if there's no config where `open-ferry init` writes one by default ([below](#download-a-release)), runs `open-ferry init`, which prints the new keys once. A config that's already there is kept as it is;
- prints how to start open-ferry with that config, run it at login with `open-ferry service install`, open the dashboard, and check the setup with `open-ferry check`.

It changes no shell profile and no `PATH`. When the install directory isn't on your `PATH`, it says how to add it.

To read the script before you run it, download it, read it, then run the file:

```sh
curl -fsSLO https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.sh
less install.sh
sh install.sh
```

```powershell
irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1 -OutFile install.ps1
notepad install.ps1
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

Windows PowerShell runs no script files by default; `-ExecutionPolicy Bypass` lets it run this one, for that run only. Each release lists both scripts in its `SHA256SUMS` and has an attestation for each, so you can [check them](#verify-the-download) as you would an archive. Each release also has its own copy of the scripts, at `https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/download/v<version>/install.sh` (and `install.ps1`).

The scripts take options. Piped to `sh`, pass them after `sh -s --`; in PowerShell, make the script a script block and pass them after it:

```sh
curl -fsSL https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.sh | sh -s -- --version 0.1.0
```

```powershell
& ([scriptblock]::Create((irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1))) -Version 0.1.0
```

| `install.sh` | `install.ps1` | What it does |
|---|---|---|
| `--version VERSION` | `-Version VERSION` | Install this version rather than the latest release |
| `--bin-dir DIR` | `-InstallDir DIR` | Install the binary in `DIR` |
| `--config PATH` | `-ConfigPath PATH` | The config to keep, or to write when there's none |
| `--target TARGET` | | Install the build for this target rather than this system's, such as `x86_64-unknown-linux-musl` |
| `--no-attestation` | `-NoAttestation` | Don't check the attestation, even when `gh` is installed. The `SHA256SUMS` check still runs |

To download from a mirror instead of GitHub, set `OPEN_FERRY_INSTALL_BASE_URL` to its address, in place of `https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases`. The mirror needs GitHub's layout: `<base>/download/v<version>/<file>` for each release, and `<base>/latest/download/<file>` for the latest. The top of each script lists the rest of its settings.

### Run it in a container

Each release has a container image for linux/amd64 and linux/arm64, `ghcr.io/loft-902-co-llc/open-ferry`, tagged with the version, such as `0.1.0`, and, unless the release is a pre-release, `latest`. It holds the release's static binary on Alpine, at the paths of CLIProxyAPI's image: the config is `/CLIProxyAPI/config.yaml`, the auth directory `/root/.cli-proxy-api`, the logs `/CLIProxyAPI/logs`, and the port 8317. The image has a build provenance attestation too:

```sh
gh attestation verify oci://ghcr.io/loft-902-co-llc/open-ferry:<version> --repo Loft-902-Co-LLC/open-ferry-ai-proxy
```

To run it with Docker Compose, put [`docker-compose.yml`](docker-compose.yml) in a new directory, and in that directory write a config with new keys:

```sh
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/out" \
  ghcr.io/loft-902-co-llc/open-ferry:latest open-ferry init -config /out/config.yaml -host ""
```

`init` prints the keys once; the command it prints to start the proxy is for inside the container, which Compose starts for you. `--user` makes the file yours rather than root's; leave it out with Docker Desktop on Windows. Then, for the dashboard, set `allow-remote: true` under `management:` in `config.yaml` (see below), and start it:

```sh
docker compose up -d
```

The dashboard is then at `http://127.0.0.1:8317/dashboard/`. Without Compose, the same container is:

```sh
docker run -d --name open-ferry --restart unless-stopped -p 127.0.0.1:8317:8317 \
  -v "$PWD/config.yaml:/CLIProxyAPI/config.yaml" \
  -v "$PWD/auths:/root/.cli-proxy-api" \
  -v "$PWD/logs:/CLIProxyAPI/logs" \
  ghcr.io/loft-902-co-llc/open-ferry:latest
```

What's different in a container:

- **The host.** `-host ""` leaves the config's `server.host` empty, as the template has it, so open-ferry listens on every interface of the container. That's what a container needs: Docker forwards the published port to the container's own address, not to its loopback. The Compose file publishes the port on the host's 127.0.0.1 only; to serve other machines, publish it on another address.
- **The dashboard.** Requests from the host reach the container through Docker's gateway, not its loopback, so the dashboard and the management API turn them away (403) unless `management.allow-remote` is `true`. Every management request still needs the management key.
- **A management port of its own.** To keep the management API and the dashboard off the proxy's port, set `separate-address: ":8318"` under `management:` (every interface of the container, as for `server.host`), keep `allow-remote: true`, and publish 8318 on the host's 127.0.0.1 only: `-p 127.0.0.1:8318:8318`, or the commented line in `docker-compose.yml`. The dashboard is then at `http://127.0.0.1:8318/dashboard/`, and only the proxy's port, 8317, need be published on another address.
- **Signing in.** The Codex and Claude sign-ins wait for the browser on the container's own loopback, which no published port reaches. Sign in from the dashboard, and paste the address the browser lands on into its sign-in dialog; use the device login, `docker compose exec open-ferry open-ferry -codex-device-login`; or copy existing auth files into `./auths`.
- **Saving the config.** With `config.yaml` mounted on its own, as above, open-ferry can't replace the file in one step, so a save from the dashboard or the management API writes over it in place, as CLIProxyAPI does, and leaves the backup, `config.yaml.bak`, inside the container, where it goes when the container does. To have each save replace the file in one step and keep the backup beside it, mount a directory that holds `config.yaml` instead; `docker-compose.yml` shows how.
- **The time zone** is UTC. For local times in the logs, set `TZ`, such as `TZ=Europe/London`.

To upgrade, run `docker compose pull && docker compose up -d`. To pin a version, set `OPEN_FERRY_IMAGE=ghcr.io/loft-902-co-llc/open-ferry:<version>`. Coming from CLIProxyAPI's image? The [migration guide](docs/migrating-from-cliproxyapi.md#docker-compose) says what to change in its Compose file.

### Download a release

Each [release](https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases) has an archive for each platform:

| Platform | Archive |
|---|---|
| Linux, x86-64 | `open-ferry-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux, arm64 | `open-ferry-<version>-aarch64-unknown-linux-gnu.tar.gz` |
| Linux, x86-64, static | `open-ferry-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux, arm64, static | `open-ferry-<version>-aarch64-unknown-linux-musl.tar.gz` |
| macOS, Intel | `open-ferry-<version>-x86_64-apple-darwin.tar.gz` |
| macOS, Apple silicon | `open-ferry-<version>-aarch64-apple-darwin.tar.gz` |
| Windows, x86-64 | `open-ferry-<version>-x86_64-pc-windows-msvc.zip` |

- **Linux:** the `linux-gnu` binaries need glibc 2.31 or newer, as on Debian 11, Ubuntu 20.04, Fedora 32, RHEL 9 and Amazon Linux 2023, or later. The static `linux-musl` binaries need no C library, so they run on Alpine and on older systems too.
- **macOS:** the binaries need macOS 11 or newer.

Each archive holds the `open-ferry` binary (`open-ferry.exe` on Windows), an example config, `config.example.yaml`, this README and the licenses. Put the binary on your `PATH`. Run it in the directory that holds your `config.yaml`, or point to the file with `-config`. For a new setup, write a config with new keys, check it, and start the proxy with it:

```sh
open-ferry init
open-ferry check -config <the path init printed>
open-ferry -config <the path init printed>
```

`init` writes `config.example.yaml` with a new client key and management key, listening on 127.0.0.1 only, to `~/.config/open-ferry/config.yaml` (`%APPDATA%\open-ferry\config.yaml` on Windows), and prints the keys once. Then add your upstream credentials, from the commented examples in the file, by signing in, or in the dashboard. To run the proxy in the background, started when you log in, run `open-ferry service install`. [docs/cli.md](docs/cli.md) has the details of `init`, `check` and `service`.

The binaries aren't code-signed. If macOS refuses to open one you downloaded with a browser, verify it as below, then remove the quarantine flag with `xattr -d com.apple.quarantine open-ferry`.

### Verify the download

Each release has a `SHA256SUMS` file and a build provenance attestation for each archive and install script, both made by the [release workflow](.github/workflows/release.yml). Check both before you run the binary. The install scripts check both for you, the attestation when `gh` is installed.

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

To keep the management API, the dashboard and its API off the proxy's port altogether, set `management.separate-address` to an address of their own, such as `127.0.0.1:8318`: they are then served there alone, and the proxy's port answers every one of their paths with a 404, so a client that reaches the proxy finds no management route at all. That address doesn't serve the proxy. Both use `server.tls`, and a change takes a restart. This setting is open-ferry's own: CLIProxyAPI ignores it and serves its management API on its port.

It shows each credential's state, with the reason and what to do about it, and from there you can upload credential files, sign in to Claude or Codex, turn a credential off or on, reset its cooldown, check its quota, or delete it. Signing in to Claude goes against Anthropic's terms and puts the account at risk ([why](docs/claude-subscription.md#the-claude-sign-in)). Its Settings page has the common settings as a form, checked as you type and reviewed against the server before each save, along with the client API keys and `config.yaml` in an editor that shows a diff before saving. A save keeps the file's comments and the settings the page doesn't show, and takes effect at once. It writes the file as CLIProxyAPI's writer does, so the first save can move keys, change the indentation and drop quotes a value doesn't need; the file as it was before each save is kept as `config.yaml.bak`. On a first run, the Overview replaces CLIProxyAPI's example client keys, which keep the proxy in safe mode, with a new key in one step, and its client setups use that key. Besides the management API, the dashboard reads an API of its own ([docs/dashboard-api.md](docs/dashboard-api.md)): usage and estimated cost per model, credential and client key, from a local SQLite ledger; a search of the request logs; and ready-made setups for common clients. The ledger is `open-ferry-usage.sqlite3` in the log directory. It records calls while `usage-statistics-enabled` is on, for 90 days by default, with no prompts or answers and client keys only masked, and estimates costs from the prices you enter.

When a model fails on a credential with an overload or another error that should pass (HTTP 408, 500, 502 to 504, or 520 to 526), it rests on that credential for 60 seconds, or as long as the provider asks, as in CLIProxyAPI. With only one credential for that model, its requests get a 503 until then, and the Credentials page shows the credential as resting, with its cooldown to reset. A second credential avoids it, and so does a shorter `routing.cooldown.transient-error-cooldown-seconds` (-1 turns these rests off).

## Parity with CLIProxyAPI

`tools/parity` checks open-ferry against the CLIProxyAPI release it ports. It builds small Go programs from a CLIProxyAPI checkout, sends the same input through upstream's code and through ours, and compares what comes out. Its suites cover every translator ported so far, requests and responses, streamed and not, and other code whose output has to match upstream's, such as the reasoning signature checks, the model registry and the config writer. Each suite runs hand-written cases and random ones made from a seed. A case is *identical* (the same JSON, key order and number text included), *equivalent* (it differs only by a deviation we document), *known* (a hand-written case marked as a known difference) or *different*, and a clean run has no different cases. The deviations, and what we leave out on purpose, are in [UPSTREAM.md](UPSTREAM.md#deliberately-not-ported); [tools/parity/README.md](tools/parity/README.md) describes each suite.

<!-- parity-summary:start -->
<!-- Written by tools/parity's --summary option. Don't edit it by hand: rerun the tool. -->
Against CLIProxyAPI v8.0.20 (commit `0f96f568e4db`), built with go1.26.4, with `--random 1000 --seed 13`: 6,424 hand-written and 110,650 random cases in all.

| Suites | Cases | Identical | Equivalent | Known | Different |
|---:|---:|---:|---:|---:|---:|
| 108 | 117,074 | 106,124 | 10,848 | 102 | 0 |

<details>
<summary>Each suite</summary>

| Suite | Cases | Identical | Equivalent | Known | Different |
|---|---:|---:|---:|---:|---:|
| Claude → Codex request | 1,016 | 463 | 551 | 2 | 0 |
| Codex → Claude response, streaming | 1,012 | 974 | 38 | 0 | 0 |
| Codex → Claude response, non-streaming | 1,009 | 1,009 | 0 | 0 | 0 |
| Responses → Codex request | 1,034 | 1,032 | 0 | 2 | 0 |
| Codex → Responses response, streaming | 1,013 | 1,012 | 0 | 1 | 0 |
| Codex → Responses response, non-streaming | 1,013 | 1,013 | 0 | 0 | 0 |
| Chat Completions → Codex request | 1,038 | 999 | 36 | 3 | 0 |
| Codex → Chat Completions response, streaming | 1,024 | 991 | 32 | 1 | 0 |
| Codex → Chat Completions response, non-streaming | 1,012 | 980 | 32 | 0 | 0 |
| Claude → Codex request, compatibility mode | 1,016 | 463 | 551 | 2 | 0 |
| Chat Completions → Claude request | 1,062 | 182 | 878 | 2 | 0 |
| Chat Completions → Claude request, compatibility mode | 1,062 | 182 | 878 | 2 | 0 |
| Claude → Chat Completions response, streaming | 1,009 | 1,009 | 0 | 0 | 0 |
| Claude → Chat Completions response, non-streaming | 1,519 | 1,519 | 0 | 0 | 0 |
| Responses → Claude request | 1,098 | 139 | 957 | 2 | 0 |
| Responses → Claude request, compatibility mode | 1,098 | 139 | 957 | 2 | 0 |
| Claude → Responses response, streaming | 1,018 | 1,010 | 8 | 0 | 0 |
| Claude → Responses response, non-streaming | 1,527 | 1,515 | 12 | 0 | 0 |
| Responses → Chat Completions request | 1,165 | 729 | 428 | 8 | 0 |
| Chat Completions → Responses response, streaming | 1,078 | 1,074 | 4 | 0 | 0 |
| Chat Completions → Responses response, non-streaming | 1,040 | 1,030 | 10 | 0 | 0 |
| Claude → Chat Completions request | 1,058 | 564 | 490 | 4 | 0 |
| Claude → Chat Completions request, compatibility mode | 1,058 | 564 | 490 | 4 | 0 |
| Chat Completions → Claude response, streaming | 1,072 | 1,072 | 0 | 0 | 0 |
| Chat Completions → Claude response, non-streaming | 1,033 | 1,033 | 0 | 0 | 0 |
| Chat Completions passthrough request | 1,056 | 1,056 | 0 | 0 | 0 |
| Chat Completions passthrough response, streaming | 1,051 | 1,051 | 0 | 0 | 0 |
| Chat Completions passthrough response, non-streaming | 1,031 | 1,031 | 0 | 0 | 0 |
| Gemini → Codex request | 1,018 | 712 | 306 | 0 | 0 |
| Codex → Gemini response, streaming | 1,013 | 609 | 404 | 0 | 0 |
| Codex → Gemini response, non-streaming | 1,012 | 660 | 352 | 0 | 0 |
| Gemini → Claude request | 1,018 | 50 | 968 | 0 | 0 |
| Claude → Gemini response, streaming | 1,009 | 1,009 | 0 | 0 | 0 |
| Claude → Gemini response, non-streaming | 1,019 | 1,019 | 0 | 0 | 0 |
| Gemini → Chat Completions request | 1,018 | 820 | 198 | 0 | 0 |
| Chat Completions → Gemini response, streaming | 1,051 | 1,051 | 0 | 0 | 0 |
| Chat Completions → Gemini response, non-streaming | 1,031 | 1,012 | 19 | 0 | 0 |
| Signature checks and replay decisions | 1,057 | 970 | 86 | 1 | 0 |
| Claude Messages signature sanitizers | 1,012 | 1,004 | 8 | 0 | 0 |
| Gemini thought signature sanitizer and validators | 1,006 | 1,006 | 0 | 0 | 0 |
| Translator registry, requests | 2,584 | 2,201 | 357 | 26 | 0 |
| Translator registry, streaming responses | 2,470 | 2,431 | 36 | 3 | 0 |
| Translator registry, non-streaming responses | 2,226 | 2,175 | 50 | 1 | 0 |
| Translator registry, lookups and token counts | 1,108 | 1,108 | 0 | 0 | 0 |
| Completions → Chat Completions request | 1,040 | 921 | 115 | 4 | 0 |
| Chat Completions → Completions response | 1,035 | 945 | 86 | 4 | 0 |
| Chat Completions → Completions stream chunks | 1,019 | 813 | 204 | 2 | 0 |
| Gemini → Gemini request | 1,026 | 1,025 | 1 | 0 | 0 |
| Gemini passthrough response, streaming | 1,023 | 1,023 | 0 | 0 | 0 |
| Gemini passthrough response, non-streaming | 1,028 | 1,028 | 0 | 0 | 0 |
| Claude → Gemini request | 1,039 | 991 | 47 | 1 | 0 |
| Claude → Gemini request, compatibility mode | 1,039 | 991 | 47 | 1 | 0 |
| Gemini → Claude response, streaming | 1,034 | 1,012 | 22 | 0 | 0 |
| Gemini → Claude response, non-streaming | 1,028 | 1,028 | 0 | 0 | 0 |
| Chat Completions → Gemini request | 1,059 | 1,012 | 46 | 1 | 0 |
| Gemini → Chat Completions response, streaming | 1,030 | 1,005 | 25 | 0 | 0 |
| Gemini → Chat Completions response, non-streaming | 1,027 | 959 | 68 | 0 | 0 |
| Thinking settings for Codex and Responses | 1,031 | 1,030 | 0 | 1 | 0 |
| Thinking settings for Chat Completions | 1,017 | 1,017 | 0 | 0 | 0 |
| Responses → Gemini request | 1,037 | 1,036 | 1 | 0 | 0 |
| Gemini → Responses response, streaming | 1,049 | 1,048 | 0 | 1 | 0 |
| Gemini → Responses response, non-streaming | 1,025 | 1,024 | 0 | 1 | 0 |
| Codex client model list | 1,016 | 1,016 | 0 | 0 | 0 |
| Codex multi-agent v2 tools readied | 1,027 | 1,027 | 0 | 0 | 0 |
| Codex multi-agent v2 request optimized | 1,015 | 1,015 | 0 | 0 | 0 |
| Codex agent messages for other formats | 1,011 | 1,011 | 0 | 0 | 0 |
| Codex orphan delegation outputs | 1,020 | 1,019 | 0 | 1 | 0 |
| Codex multi-agent v2 namespace restored | 1,016 | 1,015 | 0 | 1 | 0 |
| Payload rules applied | 1,197 | 1,196 | 0 | 1 | 0 |
| Usage parsed from upstream responses | 1,112 | 1,112 | 0 | 0 | 0 |
| First-token events | 1,177 | 1,177 | 0 | 0 | 0 |
| Sessions requests name | 1,093 | 1,091 | 0 | 2 | 0 |
| Session identities derived | 1,046 | 1,045 | 0 | 1 | 0 |
| Config change details | 1,064 | 1,064 | 0 | 0 | 0 |
| Config file writes | 1,088 | 1,073 | 13 | 2 | 0 |
| Quota snapshots of response headers | 1,028 | 1,027 | 0 | 1 | 0 |
| Claude → Interactions request | 1,035 | 994 | 41 | 0 | 0 |
| Claude → Interactions request, compatibility mode | 1,035 | 997 | 38 | 0 | 0 |
| Interactions → Claude response, streaming | 1,015 | 1,014 | 1 | 0 | 0 |
| Interactions → Claude response, non-streaming | 1,030 | 1,030 | 0 | 0 | 0 |
| Interactions → Claude request | 1,045 | 994 | 51 | 0 | 0 |
| Claude → Interactions response, streaming | 1,010 | 1,010 | 0 | 0 | 0 |
| Claude → Interactions response, non-streaming | 1,014 | 1,014 | 0 | 0 | 0 |
| Chat Completions → Interactions request | 1,012 | 990 | 22 | 0 | 0 |
| Interactions → Chat Completions response, streaming | 1,012 | 992 | 20 | 0 | 0 |
| Interactions → Chat Completions response, non-streaming | 1,009 | 855 | 154 | 0 | 0 |
| Interactions → Chat Completions request | 1,008 | 899 | 109 | 0 | 0 |
| Chat Completions → Interactions response, streaming | 1,007 | 1,007 | 0 | 0 | 0 |
| Chat Completions → Interactions response, non-streaming | 1,006 | 990 | 16 | 0 | 0 |
| Responses → Interactions request | 1,026 | 1,011 | 10 | 5 | 0 |
| Interactions → Responses request | 1,011 | 896 | 114 | 1 | 0 |
| Interactions → Responses response, streaming | 1,013 | 1,013 | 0 | 0 | 0 |
| Interactions → Responses response, non-streaming | 1,010 | 921 | 89 | 0 | 0 |
| Interactions → Responses tool input error (FinalizeToolInput) | 1,013 | 1,013 | 0 | 0 | 0 |
| Responses → Interactions response, streaming | 1,005 | 1,005 | 0 | 0 | 0 |
| Responses → Interactions response, non-streaming | 1,004 | 1,004 | 0 | 0 | 0 |
| Interactions → Codex request | 1,042 | 830 | 209 | 3 | 0 |
| Codex → Interactions stream | 1,021 | 1,020 | 0 | 1 | 0 |
| Codex → Interactions non-stream | 1,014 | 1,013 | 0 | 1 | 0 |
| Interactions → Gemini request | 1,144 | 1,103 | 41 | 0 | 0 |
| Gemini → Interactions response, streaming | 1,020 | 961 | 59 | 0 | 0 |
| Gemini → Interactions response, non-streaming | 1,019 | 1,005 | 14 | 0 | 0 |
| Gemini → Interactions request | 1,084 | 1,072 | 12 | 0 | 0 |
| Interactions → Gemini response, streaming | 1,011 | 1,000 | 11 | 0 | 0 |
| Interactions → Gemini response, non-streaming | 1,016 | 990 | 26 | 0 | 0 |
| Interactions passthrough request | 1,006 | 1,006 | 0 | 0 | 0 |
| Interactions passthrough response, streaming | 1,002 | 1,002 | 0 | 0 | 0 |
| Interactions passthrough response, non-streaming | 1,005 | 1,005 | 0 | 0 | 0 |

</details>
<!-- parity-summary:end -->

To rerun it and update the summary above, use Go 1.26 ([UPSTREAM.md](UPSTREAM.md#checking-parity) says how to get it) and a CLIProxyAPI checkout at the pinned tag beside this one:

```sh
git clone --branch v8.0.20 https://github.com/router-for-me/CLIProxyAPI ../CLIProxyAPI
cargo run --release -p open-ferry-parity -- --upstream ../CLIProxyAPI --go go1.26.4 --random 1000 --seed 13 --summary README.md
```

Parity says nothing about speed: [docs/benchmarks.md](docs/benchmarks.md) compares the two proxies in front of the same fake upstream, on a rented cloud machine with 8 vCPUs. In its latest run, against CLIProxyAPI v8.0.20, open-ferry added 2.6 to 6.6 times less time to a coding agent's long conversation, spent less CPU time per request, reused its connections to the upstream, and started in 15.5 ms against 23.9 ms. On short requests the two were close, with CLIProxyAPI adding a little less time at 16 clients at once, and under load CLIProxyAPI used a little less memory. The page has every number, and how to rerun it.

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
| `crates/open-ferry-providers` | The Codex and Claude OAuth logins and executors, the `claude-cli` executor, which runs your own Claude Code, the OpenAI-compatible executor, and the Gemini and Vertex AI executors |
| `crates/open-ferry-translate` | Format translators between OpenAI, Anthropic and Gemini. Pure functions with no I/O, publishable on its own |
| `crates/open-ferry-server` | HTTP and WebSocket handlers for `/v1/*` and the Gemini API's `/v1beta/*` |
| `crates/open-ferry-management` | CLIProxyAPI-compatible `/v0/management` API |
| `crates/open-ferry-dashboard` | The dashboard: serves the app, its API under `/open-ferry/api/v1/`, and the usage ledger |
| `crates/open-ferry-tui` | The terminal management UI (`-tui`), a client of the management API |
| `dashboard` | The dashboard app, in TypeScript and React, built with Vite into the binary |
| `reference/cliproxyapi` | Upstream source, pinned as a git submodule. Used as the spec and for comparison tests; never compiled in |

## A word on terms of service

This tool lets you use your own subscription credentials through a local proxy. Whether that's allowed depends on each provider's terms, and some users of similar tools have had accounts restricted. It's your account and your call; read the terms first. For Claude subscriptions, see [docs/claude-subscription.md](docs/claude-subscription.md).

## License

MIT. See [LICENSE](LICENSE). The Luis Pater and Router-For.ME copyright lines cover the portions ported from CLIProxyAPI (MIT); its original license is reproduced verbatim in [licenses/CLIProxyAPI-LICENSE](licenses/CLIProxyAPI-LICENSE). Small parts of Go's standard library, gin, protobuf-go, gjson, sjson, gorilla/websocket and godotenv are ported too, under their BSD and MIT licenses, as are the parts of Bubble Tea, Bubbles, Lip Gloss, Charm's x/ansi and x/cellbuf, and termenv that the TUI draws and edits text with, under the MIT license. yaml.v3 is ported too (its decoding rules for the config loader, and its scanner, parser and emitter for the config writer), under the MIT license for the parts it took from libyaml and the Apache License 2.0 with its NOTICE for the rest. All are in [licenses/](licenses). The release archives' `licenses/` also holds `rust-third-party-licenses.txt` and `dashboard-third-party-licenses.txt`, the licenses of the Rust crates and npm packages built into the binary.
