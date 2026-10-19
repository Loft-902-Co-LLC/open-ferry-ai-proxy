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

**Pre-alpha.** This README describes what we're building, not all of what exists. `open-ferry` reads CLIProxyAPI's `config.yaml` and auth directory, signs in to Codex and Claude with their OAuth logins (`-codex-login`, `-codex-device-login`, `-claude-login`), binds Sign in with ChatGPT to an existing Codex credential with [`-siwc-bind FILE`](docs/cli.md#binding-sign-in-with-chatgpt-to-a-codex-credential) or the authenticated [management bind mode](docs/dashboard-api.md#sign-in-with-chatgpt-binding), and serves Chat Completions, legacy Completions, Claude Messages, and OpenAI Responses over HTTP and WebSocket, with upstream's routing, errors, streaming and keep-alives. The Claude sign-in goes against Anthropic's terms and, in our testing, gets only the Haiku models ([why](docs/claude-subscription.md#the-claude-sign-in)). It serves the Gemini API's `/v1beta/models` routes too (model lists, `generateContent`, `streamGenerateContent` and `countTokens`), and the model list Codex clients fetch, from upstream's Codex client catalog. With `xai-api-key` credentials it serves the video endpoints too, xAI's own `/v1/videos` routes and OpenAI's `/openai/v1/videos`, and streams the finished video without writing it to disk. It serves OpenAI's speech endpoint, `/v1/audio/speech`, and xAI's `/v1/tts`, through xAI's Grok TTS models (`grok-tts`, `grok-voice-tts-1.0`). It serves the images endpoints, `/v1/images/generations` and `/v1/images/edits`, through Codex's image generation tool, xAI's Grok Imagine image models and the image models of `openai-compatibility` providers, and reads an edit's uploaded images in memory, never writing them to disk. Through Codex, the provider picks the image's size: it gets the request's `size` as sent, yet in our testing `1024x1024` came back as 1536x1024 and as 1254x1254. The answer's `size` gives the real one. It also calls the OpenAI-compatible upstreams of the config (`openai-compatibility`), any provider that speaks Chat Completions, with their API keys, headers, proxies and model aliases, and Gemini and Vertex AI, with the config's `gemini-api-key` and `vertex-api-key` entries and Vertex AI service accounts from the auth directory. With the config's `claude-cli` entries, it serves Claude models through your own installed Claude Code, which signs itself in to your subscription and makes the requests to Anthropic ([docs/claude-subscription.md](docs/claude-subscription.md)). Requests to Codex and to the OpenAI-compatible upstreams carry the thinking setting a model suffix such as `gpt-5.5(high)` or the client's request asks for, checked against the model's levels. Behind the server, upstream's credential manager picks an account per request, with its retries, cooldowns and model aliases, refreshes tokens in the background, and follows changes to the config and auth files. With the config's `codex.model-level-cooling`, a Codex usage limit cools only the model, and with `codex.stream-bootstrap-buffering`, a Codex stream that reports an overload before it starts fails over to another account. It also serves the part of CLIProxyAPI's `/v0/management` API that T3 Code's hub uses, with upstream's key checks and bans: `auth-files` (and `auth-files/models`), `api-call` and `reset-quota`, under their `/v8/management` names too, and more: the credential files and state, the OAuth logins, the config reads and writes (a save keeps the file's comments and takes effect before the answer), `logs`, the usage queue and `quota/fetch`. The plugin routes and other providers' ones aren't ported ([UPSTREAM.md](UPSTREAM.md#not-ported)). Behind them, `request-log` and the error logs write each request's log to the log directory, `logging-to-file` writes `main.log`, `usage-statistics-enabled` queues a usage record for each call, the config's `payload` rules and `disable-image-generation` apply, and `save-cooldown-status` keeps cooldowns across restarts, all as upstream does (see [UPSTREAM.md](UPSTREAM.md#observability)). With `-tui` it runs upstream's terminal management UI, as a client of a running server's management API or, with `-standalone`, of a server it runs itself ([UPSTREAM.md](UPSTREAM.md#the-tui)). A web dashboard is built into the binary, at `/dashboard/` ([below](#dashboard)). The translators between Codex and three client formats, Claude Messages, OpenAI Responses and OpenAI Chat Completions, are ported in both directions, as are those between Claude and the two OpenAI formats, along with upstream's checks of every provider's reasoning signatures. So are the translators for upstreams that only speak OpenAI Chat Completions, for Claude Messages and OpenAI Responses clients, the passthrough for Chat Completions clients, the translators from Gemini `generateContent` clients to Codex, Claude and Chat Completions upstreams, and the translators for Gemini, Claude Messages, Chat Completions and OpenAI Responses clients to Gemini upstreams. For Claude clients of Codex, which drop Codex's encrypted reasoning, it keeps each turn's reasoning and tool calls in memory by the session the client names and puts them back in that session's next request, as upstream does. With `client.codex.optimize-multi-agent-v2`, a Codex client's multi-agent v2 requests can go to other upstreams, with the models it may delegate to listed in `spawn_agent`, and with `codex.orphan-delegation-compatibility`, a Codex sub-agent's delegation outputs without their call become user messages; a `codex-api-key` model marked `is-compat` gets requests in Codex's plain Responses dialect. The translators are checked against upstream's ([UPSTREAM.md](UPSTREAM.md#checking-parity)).

## Install

Coming from CLIProxyAPI? Install open-ferry, then run `open-ferry migrate`, which switches your setup over on its config and credentials, and `open-ferry migrate -undo` switches back. The install scripts offer to run it when they find CLIProxyAPI. The [migration guide](docs/migrating-from-cliproxyapi.md) covers what carries over and what doesn't.

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
- looks for CLIProxyAPI with `open-ferry migrate -json`, which changes nothing. When it finds it, it says so and asks `Switch to open-ferry now? [y/N]`; on yes, it runs `open-ferry migrate -yes`, which switches CLIProxyAPI's setup to open-ferry on its own config and credentials ([migration guide](docs/migrating-from-cliproxyapi.md#switching-over)). With no one at a terminal to ask, it prints the command to run instead. `install.sh` asks when its output is a terminal and it can open `/dev/tty`, so piped from `curl` too; `install.ps1` asks unless PowerShell runs `-NonInteractive`;
- if there's no config where `open-ferry init` writes one by default ([below](#download-a-release)), and it didn't find CLIProxyAPI, runs `open-ferry init`, which prints the new keys once. A config that's already there is kept as it is. When it found CLIProxyAPI, it writes no config, as one would compete with CLIProxyAPI's for its port;
- prints how to start open-ferry with that config, run it at login with `open-ferry service install`, open the dashboard, and check the setup with `open-ferry check`. After a switch, it prints how to check the setup and open the dashboard on CLIProxyAPI's config, and how to switch back.

It changes no shell profile or saved `PATH`. When the install directory isn't on your `PATH`, it says how to add it.

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
| `--system` | `-System` | Install for the whole machine on Linux or Windows; needs root or an elevated terminal |
| `--version VERSION` | `-Version VERSION` | Install this version rather than the latest release |
| `--bin-dir DIR` | `-InstallDir DIR` | Install the binary in `DIR` |
| `--config PATH` | `-ConfigPath PATH` | The config to keep, or to write when there's none |
| `--target TARGET` | | Install the build for this target rather than this system's, such as `x86_64-unknown-linux-musl` |
| `--no-attestation` | `-NoAttestation` | Don't check the attestation, even when `gh` is installed. The `SHA256SUMS` check still runs |
| `--no-auto-update` | `-NoAutoUpdate` | Install with automatic updates off (`self-update.mode: off` in the config). The default `auto` mode checks for releases: a supervised service switches to each new one by itself, and the plain form stages it for `open-ferry update` to install; [docs/updates.md](docs/updates.md) describes updates and every way to turn those checks off |
| `--migrate` | `-Migrate` | When CLIProxyAPI is found, switch it to open-ferry without asking, as for an unattended install. The script fails (exit code 1) when it can't be switched, or the switch fails; with no CLIProxyAPI, it only says so |
| `--no-migrate` | `-NoMigrate` | Don't look for CLIProxyAPI |

`OPEN_FERRY_INSTALL_MIGRATE=yes` does what `--migrate` and `-Migrate` do, and `OPEN_FERRY_INSTALL_MIGRATE=no` what `--no-migrate` and `-NoMigrate` do; an option given wins over it.

For a machine install, run `sudo sh install.sh --system` on Linux or `powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -System` (or `pwsh -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -System`) from an elevated Windows terminal. The binary goes to `/usr/local/bin/open-ferry` or `C:\Program Files\open-ferry\open-ferry.exe`; the starting config goes to `/etc/open-ferry/config.yaml` or `C:\ProgramData\open-ferry\config.yaml`. Windows finds Program Files and ProgramData from the machine, rather than environment overrides. The machine data and auth directories are private to root, or SYSTEM and Administrators on Windows. The scripts skip the migration offer. To start at boot, use `sudo open-ferry service install -system -config /etc/open-ferry/config.yaml` on Linux, or `open-ferry service install -system -config C:\ProgramData\open-ferry\config.yaml` from an elevated Windows terminal. See [system services](docs/cli.md#a-service-for-the-whole-machine).

Account installs run without sudo or elevation. Inside a Linux container, root may install for its own account; the installer prints a note. Machine installs on macOS are not supported yet.

On Windows machines where every terminal is elevated (UAC off, the built-in Administrator, Windows Server), account installs and user services are refused; use the `-NoProfile` machine install command above.

Under an elevated Windows `-System` run, `OPEN_FERRY_INSTALL_BASE_URL`, `OPEN_FERRY_INSTALL_GH`, `OPEN_FERRY_INSTALL_DIR` and `OPEN_FERRY_INSTALL_CONFIG` are ignored with a notice. Choose paths with `-InstallDir` and `-ConfigPath`; downloads use the official GitHub releases, and download, verification and unpacking take place in a new staging directory inside the install directory, removed on success or failure.

When `install.sh` runs as real root, it ignores `OPEN_FERRY_INSTALL_GH`, `OPEN_FERRY_INSTALL_BASE_URL`, `OPEN_FERRY_INSTALL_TEST_PRIVILEGED` and every `FAKE_*` variable, printing only their names. Every external program gets a clean environment containing root's home, `LC_ALL=C`, proxy settings and `PATH=/usr/sbin:/usr/bin:/sbin:/bin`; caller tool settings are dropped with their names printed, and containers are detected by marker files. A `gh` installed only under `/usr/local` is skipped. Windows disables module autoloading and imports shipped PowerShell modules by full path; start with `-NoProfile`, since a profile that already ran cannot be undone. Both scripts disable their test overrides under real privilege; an elevated Windows run also ignores every `FAKE_*` variable.

Before downloading, a privileged run checks the install directory and every directory above it for trusted ownership, no links and permissions that keep other accounts from changing it. Download and unpacking use a checked private staging directory inside it; Unix creates missing directories with mode 0755 and staging with mode 0700, and Windows uses protected ACLs for Administrators and SYSTEM.

For an unprivileged install, to download from a mirror instead of GitHub, set `OPEN_FERRY_INSTALL_BASE_URL` to its address, in place of `https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases`. The mirror needs GitHub's layout: `<base>/download/v<version>/<file>` for each release, and `<base>/latest/download/<file>` for the latest. The top of each script lists the rest of its settings.

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

- **Files it reads as root.** Outside a container, open-ferry run as root, or elevated on Windows, reads `.env` and `config.yaml` only when root or the administrators alone can change them and every directory above them, and on macOS not at all. In a container (Docker, Podman, or a Kubernetes pod with its service account token mounted) it reads them as they are, so a bind-mounted config owned by your user works.
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

To look at or change a setup, or to let a coding agent do it, use `open-ferry status`, `config`, `keys`, `credentials` and `clients`, or the MCP server, `open-ferry mcp`: they check each change, mask secrets and ask before anything risky ([docs/cli.md](docs/cli.md#looking-at-and-changing-a-setup), [docs/mcp.md](docs/mcp.md)).

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

It shows each credential's state, with the reason and what to do about it, and from there you can upload credential files, sign in to Claude or Codex, turn a credential off or on, reset its cooldown, check its quota, or delete it. On a Codex sign-in saved in a file, **Add OpenAI API access** binds Sign in with ChatGPT to it, as [`-siwc-bind`](docs/cli.md#binding-sign-in-with-chatgpt-to-a-codex-credential) does: you sign in on ChatGPT's site, and if the browser can't come back to open-ferry, you paste the address it ends on. Signing in to Claude goes against Anthropic's terms and puts the account at risk ([why](docs/claude-subscription.md#the-claude-sign-in)). Its Settings page has the common settings as a form, checked as you type and reviewed against the server before each save, along with the client API keys and `config.yaml` in an editor that shows a diff before saving. A save keeps the file's comments and the settings the page doesn't show, and takes effect at once. It writes the file as CLIProxyAPI's writer does, so the first save can move keys, change the indentation, remove double quotes a mapping key doesn't need and write non-empty flow collections in block style; values keep their quotes, and the file as it was before each save is kept as `config.yaml.bak`. On a first run, the Overview replaces CLIProxyAPI's example client keys, which keep the proxy in safe mode, with a new key in one step, and its client setups use that key. Besides the management API, the dashboard reads an API of its own ([docs/dashboard-api.md](docs/dashboard-api.md)): usage and estimated cost per model, credential and client key, from a local SQLite ledger; a search of the request logs; and ready-made setups for common clients. The ledger is `open-ferry-usage.sqlite3` in the log directory. It records calls while `usage-statistics-enabled` is on, for 90 days by default, with no prompts or answers and client keys only masked, and estimates costs from the prices you enter.

When a model fails on a credential with an overload or another error that should pass (HTTP 408, 500, 502 to 504, or 520 to 526), it rests on that credential for 60 seconds, or as long as the provider asks, as in CLIProxyAPI. With only one credential for that model, its requests get a 503 until then, and the Credentials page shows the credential as resting, with its cooldown to reset. A second credential avoids it, and so does a shorter `routing.cooldown.transient-error-cooldown-seconds` (-1 turns these rests off).

## Parity with CLIProxyAPI

`tools/parity` checks open-ferry against the CLIProxyAPI release it ports. It builds small Go programs from a CLIProxyAPI checkout, sends the same input through upstream's code and through ours, and compares what comes out. Its suites cover every translator ported so far, requests and responses, streamed and not, and other code whose output has to match upstream's, such as the reasoning signature checks, the model registry and the config writer. Each suite runs hand-written cases and random ones made from a seed. A case is *identical* (the same JSON, key order and number text included), *equivalent* (it differs only by a deviation we document), *known* (a hand-written case marked as a known difference) or *different*, and a clean run has no different cases. The deviations, and what we leave out on purpose, are in [UPSTREAM.md](UPSTREAM.md#deliberately-not-ported); [tools/parity/README.md](tools/parity/README.md) describes each suite.

<!-- parity-summary:start -->
<!-- Written by tools/parity's --summary option. Don't edit it by hand: rerun the tool. -->
Against CLIProxyAPI v8.0.32 (commit `911f49cbd49f`), built with go1.26.4, with `--random 5000 --seed 1`: 6,429 hand-written and 553,320 random cases in all.

| Suites | Cases | Identical | Equivalent | Known | Different |
|---:|---:|---:|---:|---:|---:|
| 108 | 559,749 | 508,225 | 51,422 | 102 | 0 |

<details>
<summary>Each suite</summary>

| Suite | Cases | Identical | Equivalent | Known | Different |
|---|---:|---:|---:|---:|---:|
| Claude → Codex request | 5,016 | 2,329 | 2,685 | 2 | 0 |
| Codex → Claude response, streaming | 5,012 | 4,815 | 197 | 0 | 0 |
| Codex → Claude response, non-streaming | 5,009 | 5,009 | 0 | 0 | 0 |
| Responses → Codex request | 5,034 | 5,032 | 0 | 2 | 0 |
| Codex → Responses response, streaming | 5,013 | 5,012 | 0 | 1 | 0 |
| Codex → Responses response, non-streaming | 5,013 | 5,013 | 0 | 0 | 0 |
| Chat Completions → Codex request | 5,038 | 4,861 | 174 | 3 | 0 |
| Codex → Chat Completions response, streaming | 5,024 | 4,888 | 135 | 1 | 0 |
| Codex → Chat Completions response, non-streaming | 5,012 | 4,875 | 137 | 0 | 0 |
| Claude → Codex request, compatibility mode | 5,016 | 2,329 | 2,685 | 2 | 0 |
| Chat Completions → Claude request | 5,062 | 941 | 4,119 | 2 | 0 |
| Chat Completions → Claude request, compatibility mode | 5,062 | 941 | 4,119 | 2 | 0 |
| Claude → Chat Completions response, streaming | 5,009 | 5,009 | 0 | 0 | 0 |
| Claude → Chat Completions response, non-streaming | 7,519 | 7,519 | 0 | 0 | 0 |
| Responses → Claude request | 5,098 | 656 | 4,440 | 2 | 0 |
| Responses → Claude request, compatibility mode | 5,098 | 651 | 4,445 | 2 | 0 |
| Claude → Responses response, streaming | 5,018 | 4,981 | 37 | 0 | 0 |
| Claude → Responses response, non-streaming | 7,527 | 7,454 | 73 | 0 | 0 |
| Responses → Chat Completions request | 5,165 | 3,193 | 1,964 | 8 | 0 |
| Chat Completions → Responses response, streaming | 5,078 | 5,069 | 9 | 0 | 0 |
| Chat Completions → Responses response, non-streaming | 5,040 | 4,953 | 87 | 0 | 0 |
| Claude → Chat Completions request | 5,058 | 2,618 | 2,436 | 4 | 0 |
| Claude → Chat Completions request, compatibility mode | 5,058 | 2,618 | 2,436 | 4 | 0 |
| Chat Completions → Claude response, streaming | 5,072 | 5,072 | 0 | 0 | 0 |
| Chat Completions → Claude response, non-streaming | 5,033 | 5,033 | 0 | 0 | 0 |
| Chat Completions passthrough request | 5,056 | 5,056 | 0 | 0 | 0 |
| Chat Completions passthrough response, streaming | 5,051 | 5,051 | 0 | 0 | 0 |
| Chat Completions passthrough response, non-streaming | 5,031 | 5,031 | 0 | 0 | 0 |
| Gemini → Codex request | 5,018 | 3,599 | 1,419 | 0 | 0 |
| Codex → Gemini response, streaming | 5,013 | 3,035 | 1,978 | 0 | 0 |
| Codex → Gemini response, non-streaming | 5,012 | 3,254 | 1,758 | 0 | 0 |
| Gemini → Claude request | 5,018 | 283 | 4,735 | 0 | 0 |
| Claude → Gemini response, streaming | 5,009 | 5,009 | 0 | 0 | 0 |
| Claude → Gemini response, non-streaming | 5,019 | 5,019 | 0 | 0 | 0 |
| Gemini → Chat Completions request | 5,018 | 3,933 | 1,085 | 0 | 0 |
| Chat Completions → Gemini response, streaming | 5,051 | 5,051 | 0 | 0 | 0 |
| Chat Completions → Gemini response, non-streaming | 5,031 | 4,959 | 72 | 0 | 0 |
| Signature checks and replay decisions | 5,057 | 4,654 | 402 | 1 | 0 |
| Claude Messages signature sanitizers | 5,012 | 4,985 | 27 | 0 | 0 |
| Gemini thought signature sanitizer and validators | 5,006 | 5,006 | 0 | 0 | 0 |
| Translator registry, requests | 7,914 | 6,963 | 925 | 26 | 0 |
| Translator registry, streaming responses | 9,140 | 8,950 | 187 | 3 | 0 |
| Translator registry, non-streaming responses | 8,896 | 8,670 | 225 | 1 | 0 |
| Translator registry, lookups and token counts | 5,108 | 5,108 | 0 | 0 | 0 |
| Completions → Chat Completions request | 5,040 | 4,435 | 601 | 4 | 0 |
| Chat Completions → Completions response | 5,035 | 4,646 | 385 | 4 | 0 |
| Chat Completions → Completions stream chunks | 5,019 | 3,995 | 1,022 | 2 | 0 |
| Gemini → Gemini request | 5,026 | 5,025 | 1 | 0 | 0 |
| Gemini passthrough response, streaming | 5,023 | 5,023 | 0 | 0 | 0 |
| Gemini passthrough response, non-streaming | 5,028 | 5,028 | 0 | 0 | 0 |
| Claude → Gemini request | 5,039 | 4,817 | 221 | 1 | 0 |
| Claude → Gemini request, compatibility mode | 5,039 | 4,817 | 221 | 1 | 0 |
| Gemini → Claude response, streaming | 5,034 | 4,950 | 84 | 0 | 0 |
| Gemini → Claude response, non-streaming | 5,028 | 5,028 | 0 | 0 | 0 |
| Chat Completions → Gemini request | 5,059 | 4,839 | 219 | 1 | 0 |
| Gemini → Chat Completions response, streaming | 5,030 | 4,895 | 135 | 0 | 0 |
| Gemini → Chat Completions response, non-streaming | 5,027 | 4,669 | 358 | 0 | 0 |
| Thinking settings for Codex and Responses | 5,031 | 5,030 | 0 | 1 | 0 |
| Thinking settings for Chat Completions | 5,017 | 5,017 | 0 | 0 | 0 |
| Responses → Gemini request | 5,037 | 5,036 | 1 | 0 | 0 |
| Gemini → Responses response, streaming | 5,049 | 5,048 | 0 | 1 | 0 |
| Gemini → Responses response, non-streaming | 5,025 | 5,024 | 0 | 1 | 0 |
| Codex client model list | 5,021 | 5,021 | 0 | 0 | 0 |
| Codex multi-agent v2 tools readied | 5,027 | 5,027 | 0 | 0 | 0 |
| Codex multi-agent v2 request optimized | 5,015 | 5,015 | 0 | 0 | 0 |
| Codex agent messages for other formats | 5,011 | 5,011 | 0 | 0 | 0 |
| Codex orphan delegation outputs | 5,020 | 5,019 | 0 | 1 | 0 |
| Codex multi-agent v2 namespace restored | 5,016 | 5,015 | 0 | 1 | 0 |
| Payload rules applied | 5,197 | 5,196 | 0 | 1 | 0 |
| Usage parsed from upstream responses | 5,112 | 5,112 | 0 | 0 | 0 |
| First-token events | 5,177 | 5,177 | 0 | 0 | 0 |
| Sessions requests name | 5,093 | 5,091 | 0 | 2 | 0 |
| Session identities derived | 5,046 | 5,045 | 0 | 1 | 0 |
| Config change details | 5,064 | 5,062 | 2 | 0 | 0 |
| Config file writes | 5,088 | 5,057 | 29 | 2 | 0 |
| Quota snapshots of response headers | 5,028 | 5,027 | 0 | 1 | 0 |
| Claude → Interactions request | 5,035 | 4,852 | 183 | 0 | 0 |
| Claude → Interactions request, compatibility mode | 5,035 | 4,872 | 163 | 0 | 0 |
| Interactions → Claude response, streaming | 5,015 | 5,014 | 1 | 0 | 0 |
| Interactions → Claude response, non-streaming | 5,030 | 5,030 | 0 | 0 | 0 |
| Interactions → Claude request | 5,045 | 4,842 | 203 | 0 | 0 |
| Claude → Interactions response, streaming | 5,010 | 5,010 | 0 | 0 | 0 |
| Claude → Interactions response, non-streaming | 5,014 | 5,014 | 0 | 0 | 0 |
| Chat Completions → Interactions request | 5,012 | 4,865 | 147 | 0 | 0 |
| Interactions → Chat Completions response, streaming | 5,012 | 4,903 | 109 | 0 | 0 |
| Interactions → Chat Completions response, non-streaming | 5,009 | 4,205 | 804 | 0 | 0 |
| Interactions → Chat Completions request | 5,008 | 4,475 | 533 | 0 | 0 |
| Chat Completions → Interactions response, streaming | 5,007 | 5,007 | 0 | 0 | 0 |
| Chat Completions → Interactions response, non-streaming | 5,006 | 4,925 | 81 | 0 | 0 |
| Responses → Interactions request | 5,026 | 4,978 | 43 | 5 | 0 |
| Interactions → Responses request | 5,011 | 4,403 | 607 | 1 | 0 |
| Interactions → Responses response, streaming | 5,013 | 5,013 | 0 | 0 | 0 |
| Interactions → Responses response, non-streaming | 5,010 | 4,613 | 397 | 0 | 0 |
| Interactions → Responses tool input error (FinalizeToolInput) | 5,013 | 5,013 | 0 | 0 | 0 |
| Responses → Interactions response, streaming | 5,005 | 5,005 | 0 | 0 | 0 |
| Responses → Interactions response, non-streaming | 5,004 | 5,004 | 0 | 0 | 0 |
| Interactions → Codex request | 5,042 | 3,967 | 1,072 | 3 | 0 |
| Codex → Interactions stream | 5,021 | 5,020 | 0 | 1 | 0 |
| Codex → Interactions non-stream | 5,014 | 5,013 | 0 | 1 | 0 |
| Interactions → Gemini request | 5,144 | 5,025 | 119 | 0 | 0 |
| Gemini → Interactions response, streaming | 5,020 | 4,700 | 320 | 0 | 0 |
| Gemini → Interactions response, non-streaming | 5,019 | 4,945 | 74 | 0 | 0 |
| Gemini → Interactions request | 5,084 | 5,011 | 73 | 0 | 0 |
| Interactions → Gemini response, streaming | 5,011 | 4,927 | 84 | 0 | 0 |
| Interactions → Gemini response, non-streaming | 5,016 | 4,877 | 139 | 0 | 0 |
| Interactions passthrough request | 5,006 | 5,006 | 0 | 0 | 0 |
| Interactions passthrough response, streaming | 5,002 | 5,002 | 0 | 0 | 0 |
| Interactions passthrough response, non-streaming | 5,005 | 5,005 | 0 | 0 | 0 |

</details>
<!-- parity-summary:end -->

To rerun it and update the summary above, use Go 1.26 ([UPSTREAM.md](UPSTREAM.md#checking-parity) says how to get it) and a CLIProxyAPI checkout at the pinned tag beside this one:

```sh
git clone --branch v8.0.32 https://github.com/router-for-me/CLIProxyAPI ../CLIProxyAPI
cargo run --release -p open-ferry-parity -- --upstream ../CLIProxyAPI --go go1.26.4 --random 5000 --seed 1 --summary README.md
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

- **Client impersonation.** Requests use each provider's official OAuth flow and its documented protocol headers. We don't spoof TLS fingerprints, generate synthetic user IDs, or otherwise disguise proxied traffic. Sign in with ChatGPT has one narrow device-ID exception: a random v4 UUID open-ferry generates, sends only in the authorize URL and stores per credential, never as a session or request identity. It names the agent `open-ferry`; ID-token signatures and nonces aren't verified, and the SIWC account isn't matched to the outer account. So the CLIProxyAPI providers and sign-ins that only work by posing as another company's app aren't ported ([ROADMAP.md](ROADMAP.md#not-planned)).
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
