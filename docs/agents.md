# open-ferry for coding agents

What an agent needs to install open-ferry, set it up, and point a client at it. Every command here is one open-ferry or its install scripts define. Where something arrives with the first release, it says so.

open-ferry is a local proxy, a Rust port of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI). On one port (8317 by default) it serves OpenAI Chat Completions and Responses, Claude Messages and the Gemini API, and passes each request on to an account or API key from its config: Codex and Claude sign-ins, your own Claude Code (`claude-cli`), Gemini, Vertex AI, xAI, and any OpenAI-compatible upstream. It is pre-alpha; no release has been published yet.

## Install

**With the first release**, the install scripts and the container image:

- **Linux and macOS:** `install.sh`, a POSIX shell script attached to each release. It downloads the release's archive for the platform (on Linux, the static musl build on Alpine and on systems with no glibc or one older than 2.31), refuses it unless it matches the release's `SHA256SUMS` and, when the GitHub CLI `gh` is installed, its build provenance attestation, and installs `~/.local/bin/open-ferry`. To read it before running it:

  ```sh
  curl -fsSLO https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.sh
  sh install.sh
  ```

  Or in one line: `curl -fsSL https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.sh | sh`.

- **Windows:** `install.ps1`, for Windows PowerShell 5.1 and PowerShell 7, attached to each release too. It installs `%LOCALAPPDATA%\Programs\open-ferry\open-ferry.exe`. Windows PowerShell runs no script files by default, so run the downloaded one with `-ExecutionPolicy Bypass`:

  ```powershell
  irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1 -OutFile install.ps1
  powershell -ExecutionPolicy Bypass -File .\install.ps1
  ```

  Or in one line: `irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1 | iex`.

  Neither script changes your `PATH` or shell profile; each says how to add the directory when it isn't on the `PATH`. When there is no config at the default path below, each runs `open-ferry init`, which prints the new keys once; when there is one, it leaves it alone. Each ends by printing the commands to start open-ferry, run it at login, open the dashboard and check the setup. Their options (`--version`, `--bin-dir`, `--config`, `--no-attestation`; `-Version`, `-InstallDir`, `-ConfigPath`, `-NoAttestation`) are in the [README](../README.md#install-script).

- **Container:** the image `ghcr.io/loft-902-co-llc/open-ferry` (tags: the version, and `latest` unless it's a pre-release), for `linux/amd64` and `linux/arm64`. It keeps CLIProxyAPI's paths, so a Compose file for CLIProxyAPI switches by changing the image name: the config at `/CLIProxyAPI/config.yaml`, the auth directory at `/root/.cli-proxy-api`, logs in `/CLIProxyAPI/logs`, port 8317. In the container the server must listen on every interface, so write its config with `-host ""`, then start it:

  ```sh
  docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/out" \
    ghcr.io/loft-902-co-llc/open-ferry:latest open-ferry init -config /out/config.yaml -host ""
  docker run -d --name open-ferry -p 127.0.0.1:8317:8317 \
    -v "$PWD/config.yaml:/CLIProxyAPI/config.yaml" \
    -v "$PWD/auths:/root/.cli-proxy-api" \
    ghcr.io/loft-902-co-llc/open-ferry:latest
  ```

  Requests from the host reach the container through Docker's gateway, not its loopback, so the dashboard and the management API answer them only with `management.allow-remote: true` in the config. The repository's [`docker-compose.yml`](../docker-compose.yml) runs the same container with Compose, and the [README](../README.md#run-it-in-a-container) says how to sign in from a container.

**Until then**, build from source, with Rust and a C compiler ([README](../README.md#build-from-source)):

```sh
git clone https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy.git
cd open-ferry-ai-proxy
cargo build --release --locked -p open-ferry
```

The binary is `target/release/open-ferry` (`open-ferry.exe` on Windows).

## Where its files are

| What | Linux and macOS | Windows |
|---|---|---|
| The binary, as the install scripts put it | `~/.local/bin/open-ferry` | `%LOCALAPPDATA%\Programs\open-ferry\open-ferry.exe` |
| The config `open-ferry init` writes | `${XDG_CONFIG_HOME:-~/.config}/open-ferry/config.yaml` | `%APPDATA%\open-ferry\config.yaml` |
| The auth directory (sign-ins) | what the config's `oauth.auth-dir` says; the template's is `~/.cli-proxy-api`, shared with CLIProxyAPI | the same |

The server reads the config given with `-config`, else `config.yaml` in its working directory. It reads CLIProxyAPI's `config.yaml` and auth directory as they are.

## Set it up: `open-ferry init`

```sh
open-ferry init [-config PATH] [-host HOST] [-port N] [-force]
```

Writes a new config, without `-config` at the default path above, from the template built into the binary, with a new client key, a new management key, and `server.host` set to 127.0.0.1 (or what `-host` gives). It refuses to replace an existing file unless `-force` is given, and then keeps the old one as `config.yaml.bak`. It prints the config's path, both keys, the dashboard's address and how to start the proxy.

The keys are printed once. Keep them out of logs, commits and transcripts: the client key is what clients send, and the management key opens the management API and the dashboard. Both are in the config file, as written.

## Check it: `open-ferry check`

```sh
open-ferry check [-config PATH] [-json]
```

Looks over the setup without starting the server, writing anything or using the network: the config loads and isn't in safe mode (CLIProxyAPI's example client keys still listed), the management key, the auth directory and its credential files, whether something already listens on the configured address, the dashboard, the clock, and each `claude-cli` entry's Claude Code. Each finding is one line, `ok`, `warning` or `error`, with what to do when it isn't ok; `-json` prints one JSON object instead. Exit codes:

| Code | Meaning |
|---|---|
| `0` | No errors (warnings allowed) |
| `1` | At least one error |
| `2` | The check couldn't run, such as on bad usage |

`init`, `check` and `service` are described in full in [docs/cli.md](cli.md).

## Start it

```sh
open-ferry -config PATH
```

To run it in the background at login with the OS's service manager:

```sh
open-ferry service install [-config PATH]
open-ferry service status
open-ferry service uninstall
```

Add `-dry-run` to `install` or `uninstall` to print what they would do and change nothing.

## Give it credentials

A fresh config has no upstream credentials, so every model request fails until it has one. Either:

- **Sign in to Codex** (a ChatGPT account): `open-ferry -config PATH -codex-login`, or `-codex-device-login` on a machine without a browser.
- **Use Claude through your own Claude Code**, with a `claude-cli` entry in the config ([docs/claude-subscription.md](claude-subscription.md)). There is a `-claude-login` too, but signing in that way goes against Anthropic's terms ([why](claude-subscription.md#the-claude-sign-in)).
- **Add API keys** to the config's `api-keys` section: `gemini`, `interactions`, `vertex`, `codex`, `claude`, `xai`, `meta`, or an `openai-compatibility` provider (any upstream that speaks Chat Completions). The template, [`config.example.yaml`](../config.example.yaml), has a commented example of each: uncomment the `api-keys` line and the providers you want. A config in CLIProxyAPI's legacy layout names them `gemini-api-key`, `claude-api-key` and so on, which is still read.

The dashboard (below) can sign in and upload credential files too. The server picks up changes to the config and the auth directory without a restart.

## Point a client at it

Each client needs the proxy's address, `http://127.0.0.1:8317` with the config's host and port, and a client key from the config's `access.api-keys` (the one `init` printed). The dashboard's Overview page, at `/dashboard/`, writes these setups with your address, key and a model filled in. To see which models the proxy can serve now:

```sh
curl -H "Authorization: Bearer $OPEN_FERRY_API_KEY" http://127.0.0.1:8317/v1/models
```

**Codex CLI.** Add to `~/.codex/config.toml` (the first two lines go above any `[table]` in it), then start Codex with the client key in `OPEN_FERRY_API_KEY`:

```toml
model = "<model>"
model_provider = "open-ferry"

[model_providers.open-ferry]
name = "open-ferry"
base_url = "http://127.0.0.1:8317/v1"
env_key = "OPEN_FERRY_API_KEY"
wire_api = "responses"
```

```sh
export OPEN_FERRY_API_KEY='<client key>'
codex
```

**Claude Code**, with open-ferry as its LLM gateway:

```sh
export ANTHROPIC_BASE_URL='http://127.0.0.1:8317'
export ANTHROPIC_AUTH_TOKEN='<client key>'
export ANTHROPIC_MODEL='<model>'
claude
```

**An OpenAI-compatible client or SDK:** base URL `http://127.0.0.1:8317/v1`, API key the client key. In Python:

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8317/v1", api_key="<client key>")
completion = client.chat.completions.create(
    model="<model>",
    messages=[{"role": "user", "content": "Say hello."}],
)
print(completion.choices[0].message.content)
```

The Anthropic and Google Gen AI SDKs take the address without a path. The proxy translates between formats, so any model it serves is on every one of these routes: `POST /v1/chat/completions`, `POST /v1/responses`, `POST /v1/messages` and `POST /v1beta/models/{model}:generateContent`.

## The management API and the dashboard

- **The management API** is CLIProxyAPI's, at `/v0/management/` (and `/v8/management/`) on the proxy's port: credentials and their state, sign-ins, the config and the client keys, logs, usage and quota. Send the management key, the config's `management.secret-key`, as `Authorization: Bearer <key>`. It answers only on the same machine unless `management.allow-remote` is on, and is off while no management key is set. Five failed attempts from one address ban it for thirty minutes. For example:

  ```sh
  curl -H "Authorization: Bearer $MANAGEMENT_KEY" http://127.0.0.1:8317/v0/management/auth-files
  ```

  Which routes are ported is in [UPSTREAM.md](../UPSTREAM.md).
- **The dashboard** is at `http://127.0.0.1:8317/dashboard/`; sign in with the management key. Its own API, for usage, request-log search and client setups, is at `/open-ferry/api/v1/` and described in [docs/dashboard-api.md](dashboard-api.md).

## More

- [README](../README.md): what open-ferry does, and building it.
- [Migrating from CLIProxyAPI](migrating-from-cliproxyapi.md): what carries over and what doesn't.
- [UPSTREAM.md](../UPSTREAM.md): what was ported from CLIProxyAPI, and every deliberate difference.
