# Open Ferry

A Rust port of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) with a built-in UI and first-class WebSocket support.

> [!NOTE]
> **Credit where it's due.** Open Ferry is a port of CLIProxyAPI by
> [Luis Pater](https://github.com/luispater) and [Router-For.ME](https://github.com/router-for-me),
> and of the work of its 260+ contributors. Its request translation, routing, and
> management API are modelled on CLIProxyAPI's, and it reads CLIProxyAPI's config and
> auth files. This project is **not affiliated with or endorsed by** CLIProxyAPI,
> Router-For.ME, OpenAI, Anthropic, or Google. See [UPSTREAM.md](UPSTREAM.md) for exactly what
> was ported from where.

## Status

**Pre-alpha.** This README describes what we're building, not all of what exists. The first release, 0.1.0, isn't out yet. Candidates 0.0.3 and 0.0.4 are published to test updating from one signed release to the next. They install and update like any release.

- Connect coding agents through Chat Completions, Responses, Claude Messages or the Gemini API.
- Use Codex, API-key providers or your own installed Claude Code through one local proxy.
- Route across accounts with retries, cooldowns and model aliases.
- Manage credentials, settings, usage and logs from the built-in [dashboard](#dashboard).

The full [feature inventory](docs/features.md) lists the ported endpoints, providers and translation behaviour, including the limits of the Claude sign-in.
The [compatibility matrix](docs/compatibility.md) shows which protocol, provider, credential and capability combinations the tests cover, and which are untested.

## Quick start

This example uses an API key for an OpenAI-compatible provider. It needs no browser sign-in. You need that provider's key, Chat Completions base URL and a model it offers.

1. **Install.** On Linux or macOS:
   ```sh
   curl -fsSL https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.sh | sh
   ```
   On Windows, use PowerShell: `irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1 | iex`. Follow the script's PATH instructions if needed. [Other ways to install](#install). If it found CLIProxyAPI, follow the [migration guide](docs/migrating-from-cliproxyapi.md#switching-over) instead; a switch uses CLIProxyAPI's config.

2. **Add a provider and start.** Use the config the installer wrote or kept. Set its printed path: in sh, `CONFIG='<config path printed by the installer>'`; in PowerShell, `$CONFIG = '<config path printed by the installer>'`. For a new config, the installer printed the client key once. Retrieve it with `open-ferry keys list --reveal --yes --config "$CONFIG"` if needed. Save this as `provider.yaml` in your working directory, replacing all three `YOUR-...` values with your provider's values. Keep the files and keys private.
   Open Ferry and CLIProxyAPI both default to port 8317: stop CLIProxyAPI first, switch with `open-ferry migrate`, or choose another free port (for example, `open-ferry config set server.port 18317 --config "$CONFIG"`) and change the client URLs below to that port.
   ```yaml
   - name: example
     base-url: "https://YOUR-PROVIDER/v1"
     keys:
       - api-key: "YOUR-PROVIDER-KEY"
     models:
       - name: "YOUR-PROVIDER-MODEL"
         alias: "quick-start"
   ```
   ```sh
   open-ferry config set api-keys.openai-compatibility --from-file ./provider.yaml --config "$CONFIG"
   open-ferry -config "$CONFIG"
   ```
   Leave the server running. [Other provider options](docs/agents.md#give-it-credentials) include Codex sign-in and your own Claude Code; the [dashboard](#dashboard) can sign in and upload credentials too.

3. **Connect a tool.** In a second terminal in the same directory, set `CONFIG` again as above, then run `open-ferry clients setup claude-code --config "$CONFIG" --model quick-start` for an installed Claude Code. Copy its commands, replace the masked key with the client key from the installer (`access.api-keys`) and run them. This uses base URL `http://127.0.0.1:8317` and model `quick-start`. OpenAI-compatible tools use `http://127.0.0.1:8317/v1`. Both need the client key, not the provider or management key. [Codex and SDK examples](docs/agents.md#point-a-client-at-it) are alternatives.

4. **Verify a reply.** Save this as `request.json` in the same directory:
   ```json
   {"model":"quick-start","messages":[{"role":"user","content":"Say hello."}]}
   ```
   Replace `YOUR-CLIENT-KEY` below with the client key and run this request (on Windows use `curl.exe` and put it on one line):
   ```sh
   curl -sS http://127.0.0.1:8317/v1/chat/completions \
     -H 'Authorization: Bearer YOUR-CLIENT-KEY' -H 'Content-Type: application/json' \
     --data-binary '@request.json'
   ```
   Success is JSON with the model's reply in `choices[0].message.content`. An HTTP 401 with `{"error":"Invalid API key"}` means the client key is wrong. For other errors, read the server terminal and run `open-ferry check -config "$CONFIG"`; check the provider's URL, model and key, or the dashboard's Credentials and Logs pages.

## Install

Stable is the default release channel. To opt into preview, pass `--channel preview` to `install.sh` or `-Channel preview` to `install.ps1`; see [release channels](docs/updates.md).


Coming from CLIProxyAPI? Install Open Ferry, then run `open-ferry migrate`, which switches your setup over on its config and credentials, and `open-ferry migrate -undo` switches back. The install scripts offer to run it when they find CLIProxyAPI. The [migration guide](docs/migrating-from-cliproxyapi.md) covers what carries over and what doesn't.

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
- looks for CLIProxyAPI with `open-ferry migrate -json`, which changes nothing. When it finds it, it says so and asks `Switch to open-ferry now? [y/N]`; on yes, it runs `open-ferry migrate -yes`, which switches CLIProxyAPI's setup to Open Ferry on its own config and credentials ([migration guide](docs/migrating-from-cliproxyapi.md#switching-over)). With no one at a terminal to ask, it prints the command to run instead. `install.sh` asks when its output is a terminal and it can open `/dev/tty`, so piped from `curl` too; `install.ps1` asks unless PowerShell runs `-NonInteractive`;
- if there's no config where `open-ferry init` writes one by default ([below](#download-a-release)), and it didn't find CLIProxyAPI, runs `open-ferry init`, which prints the new keys once. A config that's already there is kept as it is. When it found CLIProxyAPI, it writes no config, as one would compete with CLIProxyAPI's for its port;
- prints how to start Open Ferry with that config, run it at login with `open-ferry service install`, open the dashboard, and check the setup with `open-ferry check`. After a switch, it prints how to check the setup and open the dashboard on CLIProxyAPI's config, and how to switch back.

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
curl -fsSL https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.sh | sh -s -- --version 0.0.4
```

```powershell
& ([scriptblock]::Create((irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1))) -Version 0.0.4
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
| `--migrate` | `-Migrate` | When CLIProxyAPI is found, switch it to Open Ferry without asking, as for an unattended install. The script fails (exit code 1) when it can't be switched, or the switch fails; with no CLIProxyAPI, it only says so |
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

Each release has a container image for linux/amd64 and linux/arm64, `ghcr.io/loft-902-co-llc/open-ferry`, tagged with the version, such as `0.1.0`. The `preview` tag follows the newest release on the preview channel, and `latest` the newest stable release; promoting a release moves `latest` to the same image, without a rebuild. It holds the release's static binary on Alpine, at the paths of CLIProxyAPI's image: the config is `/CLIProxyAPI/config.yaml`, the auth directory `/root/.cli-proxy-api`, the logs `/CLIProxyAPI/logs`, and the port 8317. The image has a build provenance attestation too:

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

- **Files it reads as root.** Outside a container, Open Ferry run as root, or elevated on Windows, reads `.env` and `config.yaml` only when root or the administrators alone can change them and every directory above them, and on macOS not at all. In a container (Docker, Podman, or a Kubernetes pod with its service account token mounted) it reads them as they are, so a bind-mounted config owned by your user works.
- **The host.** `-host ""` leaves the config's `server.host` empty, as the template has it, so Open Ferry listens on every interface of the container. That's what a container needs: Docker forwards the published port to the container's own address, not to its loopback. The Compose file publishes the port on the host's 127.0.0.1 only; to serve other machines, publish it on another address.
- **The dashboard.** Requests from the host reach the container through Docker's gateway, not its loopback, so the dashboard and the management API turn them away (403) unless `management.allow-remote` is `true`. Every management request still needs the management key.
- **A management port of its own.** To keep the management API and the dashboard off the proxy's port, set `separate-address: ":8318"` under `management:` (every interface of the container, as for `server.host`), keep `allow-remote: true`, and publish 8318 on the host's 127.0.0.1 only: `-p 127.0.0.1:8318:8318`, or the commented line in `docker-compose.yml`. The dashboard is then at `http://127.0.0.1:8318/dashboard/`, and only the proxy's port, 8317, need be published on another address.
- **Signing in.** The Codex and Claude sign-ins wait for the browser on the container's own loopback, which no published port reaches. Sign in from the dashboard, and paste the address the browser lands on into its sign-in dialog; use the device login, `docker compose exec open-ferry open-ferry -codex-device-login`; or copy existing auth files into `./auths`.
- **Saving the config.** With `config.yaml` mounted on its own, as above, Open Ferry can't replace the file in one step, so a save from the dashboard or the management API writes over it in place, as CLIProxyAPI does, and leaves the backup, `config.yaml.bak`, inside the container, where it goes when the container does. To have each save replace the file in one step and keep the backup beside it, mount a directory that holds `config.yaml` instead; `docker-compose.yml` shows how.
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

Each release also has CycloneDX software bills of materials (SBOMs): one per target for the Rust dependencies, and one for the dashboard's npm dependencies built into every binary. `SHA256SUMS` lists them too, so you check them the same way. See [SECURITY.md](SECURITY.md#signed-releases).

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

To build the [dashboard](#dashboard) in, build its app first, with [Node.js](https://nodejs.org/) 22.22.2 or a later 22.x, 24.15.0 or a later 24.x, or 26 and newer. Then build Open Ferry:

```sh
cd dashboard
npm ci
npm run build
cd ..
cargo build --release --locked -p open-ferry
```

Without the app the build works all the same, and so do the proxy, the management API and the dashboard's own API, but `/dashboard/` shows only a page saying the app wasn't built in, and how to build it. The release binaries have it built in.

## Dashboard

Open Ferry serves a web dashboard of its own at `/dashboard/` on the proxy's port, such as `http://localhost:<port>/dashboard/`, and CLIProxyAPI's `/management.html` sends the browser there. It is built into the binary and fetches nothing from elsewhere. Sign in with the management key, the config's `management.secret-key` or `MANAGEMENT_PASSWORD`, or on the same machine with the `-password` Open Ferry was started with; the dashboard keeps it for that browser tab only. As with the management API, it answers only on the same machine unless `management.allow-remote` is on, which `MANAGEMENT_PASSWORD` implies, and `management.disable-control-panel` turns it off. A config in the legacy layout has these keys under `remote-management:`, which is still read.

To keep the management API, the dashboard and its API off the proxy's port altogether, set `management.separate-address` to an address of their own, such as `127.0.0.1:8318`: they are then served there alone, and the proxy's port answers every one of their paths with a 404, so a client that reaches the proxy finds no management route at all. That address doesn't serve the proxy. Both use `server.tls`, and a change takes a restart. This setting is Open Ferry's own: CLIProxyAPI ignores it and serves its management API on its port.

It shows each credential's state, with the reason and what to do about it, and from there you can upload credential files, sign in to Claude or Codex, turn a credential off or on, reset its cooldown, check its quota, or delete it. On a Codex sign-in saved in a file, **Add OpenAI API access** binds Sign in with ChatGPT to it, as [`-siwc-bind`](docs/cli.md#binding-sign-in-with-chatgpt-to-a-codex-credential) does: you sign in on ChatGPT's site, and if the browser can't come back to Open Ferry, you paste the address it ends on. Signing in to Claude goes against Anthropic's terms and puts the account at risk ([why](docs/claude-subscription.md#the-claude-sign-in)). Its Settings page has the common settings as a form, checked as you type and reviewed against the server before each save, along with the client API keys and `config.yaml` in an editor that shows a diff before saving. A save keeps the file's comments and the settings the page doesn't show, and takes effect at once. It writes the file as CLIProxyAPI's writer does, so the first save can move keys, change the indentation, remove double quotes a mapping key doesn't need and write non-empty flow collections in block style; values keep their quotes, and the file as it was before each save is kept as `config.yaml.bak`. On a first run, the Overview replaces CLIProxyAPI's example client keys, which keep the proxy in safe mode, with a new key in one step, and its client setups use that key. Besides the management API, the dashboard reads an API of its own ([docs/dashboard-api.md](docs/dashboard-api.md)): usage and estimated cost per model, credential and client key, from a local SQLite ledger; a search of the request logs; and ready-made setups for common clients. The ledger is `open-ferry-usage.sqlite3` in the log directory. It records calls while `usage-statistics-enabled` is on, for 90 days by default, with no prompts or answers and client keys only masked, and estimates costs from the prices you enter.

When a model fails on a credential with an overload or another error that should pass (HTTP 408, 500, 502 to 504, or 520 to 526), it rests on that credential for 60 seconds, or as long as the provider asks, as in CLIProxyAPI. With only one credential for that model, its requests get a 503 until then, and the Credentials page shows the credential as resting, with its cooldown to reset. A second credential avoids it, and so does a shorter `routing.cooldown.transient-error-cooldown-seconds` (-1 turns these rests off).

## Parity with CLIProxyAPI

`tools/parity` checks Open Ferry against the CLIProxyAPI release it ports. It builds small Go programs from a CLIProxyAPI checkout, sends the same input through upstream's code and through ours, and compares what comes out. Its suites cover every translator ported so far, requests and responses, streamed and not, and other code whose output has to match upstream's, such as the reasoning signature checks, the model registry and the config writer. Each suite runs hand-written cases and random ones made from a seed. A case is *identical* (the same JSON, key order and number text included), *equivalent* (it differs only by a deviation we document), *known* (a named, documented upstream difference) or *different*. A run fails on an unaccepted Different result. Expected config-save certification refusals are checked against a [recorded baseline](tools/parity/baseline/config-save.json) and remain Different in these totals; non-certification failures still fail the run. The deviations, and what we leave out on purpose, are in [UPSTREAM.md](UPSTREAM.md#deliberately-not-ported); [tools/parity/README.md](tools/parity/README.md) describes each suite.

<!-- parity-summary:start -->
<!-- Written by tools/parity's --summary option. Don't edit it by hand: rerun the tool. -->
Against CLIProxyAPI v8.0.42 (commit `bc0643405a1d`), built with go1.26.10, with `--random 5000 --seed 1`: 10,929 hand-written and 553,320 random cases in all.

| Suites | Cases | Identical | Equivalent | Known | Different |
|---:|---:|---:|---:|---:|---:|
| 108 | 564,249 | 509,680 | 51,563 | 2,767 | 239 |

Upstream lossy plugin re-encode: 2 cases (included in Equivalent).
Upstream block scalar counterfactual: 19 cases (included in Equivalent).
Upstream archived scalar counterfactual: 1 cases (included in Equivalent).
Upstream folded scalar re-encode: 3 cases (included in Equivalent).

Upstream config version comment loss: 555 cases (included in Equivalent).

| Deviation | Cases |
|---|---:|
| call ID derived from compact JSON | 765 |
| change line upstream doesn't log | 2 |
| createTime written in UTC | 3,974 |
| cut at a character boundary | 2,998 |
| embedded JSON re-serialized | 16,388 |
| made-up user ID left out | 22,237 |
| plugin comment written once, not twice | 4 |
| protobuf error prefix space | 431 |
| retained canonical version comment | 6 |
| retained comment source owners | 763 |
| retained dropped key comment | 5 |
| tool parameter key order | 9,455 |
| tool parameter number text | 3,103 |
| unloadable file refused, not written | 3 |
| upstream archived scalar counterfactual | 1 |
| upstream block scalar counterfactual | 19 |
| upstream config version comment loss | 555 |
| upstream folded scalar re-encode | 3 |
| upstream lossy plugin re-encode | 2 |

<details>
<summary>Each suite</summary>

| Suite | Cases | Identical | Equivalent | Known | Different |
|---|---:|---:|---:|---:|---:|
| Claude → Codex request | 5,025 | 2,339 | 2,685 | 1 | 0 |
| Codex → Claude response, streaming | 5,039 | 4,842 | 197 | 0 | 0 |
| Codex → Claude response, non-streaming | 5,016 | 5,016 | 0 | 0 | 0 |
| Responses → Codex request | 5,042 | 5,040 | 0 | 2 | 0 |
| Codex → Responses response, streaming | 5,013 | 5,012 | 0 | 1 | 0 |
| Codex → Responses response, non-streaming | 5,013 | 5,013 | 0 | 0 | 0 |
| Chat Completions → Codex request | 5,046 | 4,869 | 174 | 3 | 0 |
| Codex → Chat Completions response, streaming | 5,024 | 5,023 | 0 | 1 | 0 |
| Codex → Chat Completions response, non-streaming | 5,012 | 5,012 | 0 | 0 | 0 |
| Claude → Codex request, compatibility mode | 5,025 | 2,339 | 2,685 | 1 | 0 |
| Chat Completions → Claude request | 5,077 | 942 | 4,134 | 1 | 0 |
| Chat Completions → Claude request, compatibility mode | 5,077 | 942 | 4,134 | 1 | 0 |
| Claude → Chat Completions response, streaming | 5,025 | 5,025 | 0 | 0 | 0 |
| Claude → Chat Completions response, non-streaming | 7,543 | 7,543 | 0 | 0 | 0 |
| Responses → Claude request | 5,149 | 655 | 4,492 | 2 | 0 |
| Responses → Claude request, compatibility mode | 5,149 | 651 | 4,496 | 2 | 0 |
| Claude → Responses response, streaming | 5,034 | 4,996 | 38 | 0 | 0 |
| Claude → Responses response, non-streaming | 7,551 | 7,479 | 72 | 0 | 0 |
| Responses → Chat Completions request | 5,183 | 3,211 | 1,964 | 8 | 0 |
| Chat Completions → Responses response, streaming | 5,078 | 5,070 | 8 | 0 | 0 |
| Chat Completions → Responses response, non-streaming | 5,040 | 4,954 | 86 | 0 | 0 |
| Claude → Chat Completions request | 5,064 | 2,625 | 2,436 | 3 | 0 |
| Claude → Chat Completions request, compatibility mode | 5,064 | 2,625 | 2,436 | 3 | 0 |
| Chat Completions → Claude response, streaming | 5,072 | 5,072 | 0 | 0 | 0 |
| Chat Completions → Claude response, non-streaming | 5,033 | 5,033 | 0 | 0 | 0 |
| Chat Completions passthrough request | 5,064 | 5,064 | 0 | 0 | 0 |
| Chat Completions passthrough response, streaming | 5,051 | 5,051 | 0 | 0 | 0 |
| Chat Completions passthrough response, non-streaming | 5,031 | 5,031 | 0 | 0 | 0 |
| Gemini → Codex request | 5,045 | 3,620 | 1,425 | 0 | 0 |
| Codex → Gemini response, streaming | 5,014 | 3,035 | 1,979 | 0 | 0 |
| Codex → Gemini response, non-streaming | 5,013 | 3,254 | 1,759 | 0 | 0 |
| Gemini → Claude request | 5,042 | 290 | 4,752 | 0 | 0 |
| Claude → Gemini response, streaming | 5,009 | 5,009 | 0 | 0 | 0 |
| Claude → Gemini response, non-streaming | 5,019 | 5,019 | 0 | 0 | 0 |
| Gemini → Chat Completions request | 5,045 | 3,954 | 1,091 | 0 | 0 |
| Chat Completions → Gemini response, streaming | 5,052 | 5,052 | 0 | 0 | 0 |
| Chat Completions → Gemini response, non-streaming | 5,032 | 4,959 | 73 | 0 | 0 |
| Signature checks and replay decisions | 5,115 | 4,710 | 404 | 1 | 0 |
| Claude Messages signature sanitizers | 5,012 | 4,985 | 27 | 0 | 0 |
| Gemini thought signature sanitizer and validators | 5,028 | 5,028 | 0 | 0 | 0 |
| Translator registry, requests | 8,206 | 7,145 | 1,005 | 56 | 0 |
| Translator registry, streaming responses | 10,305 | 9,946 | 159 | 200 | 0 |
| Translator registry, non-streaming responses | 9,226 | 9,024 | 201 | 1 | 0 |
| Translator registry, lookups and token counts | 5,108 | 5,108 | 0 | 0 | 0 |
| Completions → Chat Completions request | 5,040 | 4,580 | 456 | 4 | 0 |
| Chat Completions → Completions response | 5,035 | 4,742 | 289 | 4 | 0 |
| Chat Completions → Completions stream chunks | 5,019 | 4,243 | 774 | 2 | 0 |
| Gemini → Gemini request | 5,050 | 5,049 | 1 | 0 | 0 |
| Gemini passthrough response, streaming | 5,023 | 5,023 | 0 | 0 | 0 |
| Gemini passthrough response, non-streaming | 5,028 | 5,028 | 0 | 0 | 0 |
| Claude → Gemini request | 5,073 | 4,830 | 220 | 23 | 0 |
| Claude → Gemini request, compatibility mode | 5,059 | 4,827 | 220 | 12 | 0 |
| Gemini → Claude response, streaming | 5,764 | 5,650 | 114 | 0 | 0 |
| Gemini → Claude response, non-streaming | 5,161 | 5,161 | 0 | 0 | 0 |
| Chat Completions → Gemini request | 5,102 | 4,871 | 219 | 12 | 0 |
| Gemini → Chat Completions response, streaming | 5,215 | 5,080 | 135 | 0 | 0 |
| Gemini → Chat Completions response, non-streaming | 5,095 | 4,737 | 358 | 0 | 0 |
| Thinking settings for Codex and Responses | 5,246 | 5,245 | 0 | 1 | 0 |
| Thinking settings for Chat Completions | 5,224 | 5,224 | 0 | 0 | 0 |
| Responses → Gemini request | 5,071 | 5,059 | 1 | 11 | 0 |
| Gemini → Responses response, streaming | 5,234 | 5,233 | 0 | 1 | 0 |
| Gemini → Responses response, non-streaming | 5,093 | 5,092 | 0 | 1 | 0 |
| Codex client model list | 5,021 | 5,021 | 0 | 0 | 0 |
| Codex multi-agent v2 tools readied | 5,027 | 5,027 | 0 | 0 | 0 |
| Codex multi-agent v2 request optimized | 5,015 | 5,015 | 0 | 0 | 0 |
| Codex agent messages for other formats | 5,011 | 5,011 | 0 | 0 | 0 |
| Codex orphan delegation outputs | 5,020 | 5,019 | 0 | 1 | 0 |
| Codex multi-agent v2 namespace restored | 5,016 | 5,015 | 0 | 1 | 0 |
| Payload rules applied | 5,197 | 5,196 | 0 | 1 | 0 |
| Usage parsed from upstream responses | 5,360 | 5,360 | 0 | 0 | 0 |
| First-token events | 5,177 | 5,177 | 0 | 0 | 0 |
| Sessions requests name | 5,093 | 5,091 | 0 | 2 | 0 |
| Session identities derived | 5,046 | 5,045 | 0 | 1 | 0 |
| Config change details | 5,071 | 5,069 | 2 | 0 | 0 |
| Config file writes | 5,110 | 3,620 | 1,240 | 11 | 239 |
| Quota snapshots of response headers | 5,028 | 5,027 | 0 | 1 | 0 |
| Claude → Interactions request | 5,035 | 4,852 | 183 | 0 | 0 |
| Claude → Interactions request, compatibility mode | 5,035 | 4,872 | 163 | 0 | 0 |
| Interactions → Claude response, streaming | 5,016 | 5,015 | 1 | 0 | 0 |
| Interactions → Claude response, non-streaming | 5,032 | 5,032 | 0 | 0 | 0 |
| Interactions → Claude request | 5,045 | 4,842 | 203 | 0 | 0 |
| Claude → Interactions response, streaming | 5,010 | 5,010 | 0 | 0 | 0 |
| Claude → Interactions response, non-streaming | 5,015 | 5,015 | 0 | 0 | 0 |
| Chat Completions → Interactions request | 5,012 | 4,865 | 147 | 0 | 0 |
| Interactions → Chat Completions response, streaming | 5,012 | 5,012 | 0 | 0 | 0 |
| Interactions → Chat Completions response, non-streaming | 5,009 | 4,303 | 706 | 0 | 0 |
| Interactions → Chat Completions request | 5,009 | 4,475 | 534 | 0 | 0 |
| Chat Completions → Interactions response, streaming | 5,007 | 5,007 | 0 | 0 | 0 |
| Chat Completions → Interactions response, non-streaming | 5,006 | 4,926 | 80 | 0 | 0 |
| Responses → Interactions request | 5,026 | 4,978 | 43 | 5 | 0 |
| Interactions → Responses request | 5,011 | 4,403 | 607 | 1 | 0 |
| Interactions → Responses response, streaming | 5,013 | 5,013 | 0 | 0 | 0 |
| Interactions → Responses response, non-streaming | 5,010 | 4,613 | 397 | 0 | 0 |
| Interactions → Responses tool input error (FinalizeToolInput) | 5,013 | 5,013 | 0 | 0 | 0 |
| Responses → Interactions response, streaming | 5,005 | 5,005 | 0 | 0 | 0 |
| Responses → Interactions response, non-streaming | 5,004 | 5,004 | 0 | 0 | 0 |
| Interactions → Codex request | 5,045 | 3,971 | 1,072 | 2 | 0 |
| Codex → Interactions stream | 5,022 | 5,021 | 0 | 1 | 0 |
| Codex → Interactions non-stream | 5,015 | 5,014 | 0 | 1 | 0 |
| Interactions → Gemini request | 5,144 | 5,025 | 119 | 0 | 0 |
| Gemini → Interactions response, streaming | 5,020 | 2,439 | 200 | 2,381 | 0 |
| Gemini → Interactions response, non-streaming | 5,019 | 5,018 | 1 | 0 | 0 |
| Gemini → Interactions request | 5,084 | 5,011 | 73 | 0 | 0 |
| Interactions → Gemini response, streaming | 5,011 | 5,010 | 1 | 0 | 0 |
| Interactions → Gemini response, non-streaming | 5,016 | 4,924 | 92 | 0 | 0 |
| Interactions passthrough request | 5,006 | 5,006 | 0 | 0 | 0 |
| Interactions passthrough response, streaming | 5,002 | 5,002 | 0 | 0 | 0 |
| Interactions passthrough response, non-streaming | 5,005 | 5,005 | 0 | 0 | 0 |

</details>

### Config-save baseline ratchet

Baseline: `C:\Users\MattLoft\AppData\Local\Temp\p42-auth-r7-integration\tools\parity\baseline\config-save.json`. Different remains Different; this budget grants no certification.

| Kind | Cases | Per 1,000 | Baseline |
|---|---:|---:|---|
| `check-1-no-subset/missing_snapshot/save` | 1 | 0.20 | known kind; total bound below |
| `check-1-no-subset/snapshot_bytes/nested` | 0 | 0.00 | known kind; total bound below |
| `check-1-no-subset/snapshot_bytes/save` | 0 | 0.00 | known kind; total bound below |
| `check-1-no-subset/snapshot_bytes/write` | 1 | 0.20 | known kind; total bound below |
| `check-2/comment_free_value_path/write` | 0 | 0.00 | known kind; total bound below |
| `check-2/missing_value_snapshot/save` | 2 | 0.39 | known kind; total bound below |
| `check-3/go_budget/save` | 8 | 1.57 | known kind; total bound below |
| `check-3/go_owner_witness/save` | 0 | 0.00 | known kind; total bound below |
| `check-3/owner_budget/nested` | 22 | 4.31 | known kind; total bound below |
| `check-3/owner_budget/save` | 144 | 28.18 | known kind; total bound below |
| `check-3/owner_budget/write` | 17 | 3.33 | known kind; total bound below |
| `check-3/physical_layout/nested` | 0 | 0.00 | known kind; total bound below |
| `check-3/source_budget/save` | 23 | 4.50 | known kind; total bound below |
| `check-3/source_budget/write` | 21 | 4.11 | known kind; total bound below |
| `protected-replay/missing_snapshot/save` | 0 | 0.00 | known kind; total bound below |

Total Different: **239/5110 (46.77 per 1,000)**; maximum **70 per 1,000**. Kind counts can overlap.
Baseline can be lowered; refresh: `cargo run --locked -p open-ferry-parity -- --write-baseline tools/parity/baseline/config-save.json --baseline-observation <run.json> ... (see tools/parity/README.md)`.

Ratchet verdict: **PASS**.
<!-- parity-summary:end -->

To rerun it and update the summary above, use Go 1.26.10 or later ([UPSTREAM.md](UPSTREAM.md#checking-parity) says how to get it) and a CLIProxyAPI checkout at the pinned tag beside this one:

```sh
git clone --branch v8.0.42 https://github.com/router-for-me/CLIProxyAPI ../CLIProxyAPI
cargo run --release -p open-ferry-parity -- --upstream ../CLIProxyAPI --go go1.26.10 --random 5000 --seed 1 --summary README.md
```

Parity says nothing about speed: [docs/benchmarks.md](docs/benchmarks.md) compares the two proxies in front of the same fake upstream, on a rented cloud machine with 8 vCPUs. In its latest run, against CLIProxyAPI v8.0.38, CLIProxyAPI added 2.8 to 6.1 times as much time as Open Ferry to a coding agent's long conversation. Open Ferry spent 7% to 41% less CPU time per request, reused its connections to the upstream, and started in 16.9 ms against 27.2 ms. On short requests the two were close, with CLIProxyAPI adding a little less time at 16 clients at once, and under load CLIProxyAPI used less memory. The page has every number, and how to rerun it.

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

- **Client impersonation.** Requests use each provider's official OAuth flow and its documented protocol headers. We don't spoof TLS fingerprints, generate synthetic user IDs, or otherwise disguise proxied traffic. Sign in with ChatGPT has one narrow device-ID exception: a random v4 UUID Open Ferry generates, sends only in the authorize URL and stores per credential, never as a session or request identity. It names the agent `open-ferry`; ID-token signatures and nonces aren't verified, and the SIWC account isn't matched to the outer account. So the CLIProxyAPI providers and sign-ins that only work by posing as another company's app aren't ported ([ROADMAP.md](ROADMAP.md#not-planned)).
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
