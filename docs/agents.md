# open-ferry for coding agents

What an agent needs to install open-ferry, set it up, change its setup, and point a client at it. Every command here is one open-ferry or its install scripts define. Where something arrives with the first release, it says so.

**To look at a setup or change it, use open-ferry's own commands (`open-ferry status`, `config`, `keys`, `credentials` and `clients`) or its MCP server (`open-ferry mcp`)**, not hand edits of the YAML or curl with the management key: they check each change, keep secrets out of what they print and out of the command line, and ask for a yes before anything risky. See [Look at it and change it](#look-at-it-and-change-it-the-commands-and-the-mcp-server).

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

## Look at it and change it: the commands and the MCP server

To look at a setup and change it, use these commands, or the MCP server that serves the same actions as tools, rather than editing the YAML or calling the management API with curl. They check each change as the management API does, mask every secret in what they print, and need an explicit yes before anything risky.

| Command | MCP tool | What it does |
|---|---|---|
| `open-ferry status` | `status` | Whether a server runs for the config: its version, addresses, credentials by state and today's calls |
| `open-ferry config get <path>` | `config_get` | One setting, masked; or that it isn't set, and its default |
| `open-ferry config set <path> <value>` | `config_set` | Sets a setting to a YAML or JSON value |
| `open-ferry config unset <path>` | `config_unset` | Removes a setting, so its default applies |
| `open-ferry config show` | `config_show` | The whole config, masked |
| `open-ferry config diff` | `config_diff` | What the last change made |
| `open-ferry config undo` | `config_undo` | Reverses the last change; run it again to redo it |
| `open-ferry config replace --from-file <file>` | `config_replace` | Replaces the whole config |
| `open-ferry keys list` | `keys_list` | The client keys, masked |
| `open-ferry keys add --generate` | `keys_add` | Adds a new client key, and shows it once |
| `open-ferry keys remove <index>` | `keys_remove` | Removes a client key |
| `open-ferry credentials list` | `credentials_list` | The running server's credentials and their state |
| `open-ferry credentials enable <credential>`, `disable` | `credentials_enable`, `credentials_disable` | Turns a credential on or off |
| `open-ferry credentials reset-quota <credential>` | `credentials_reset_quota` | Clears its cooldowns and quota state |
| `open-ferry credentials remove <credential>` | `credentials_remove` | Deletes its file |
| `open-ferry credentials login codex` | `credentials_login` | Starts a sign-in, and waits for it |
| `open-ferry clients setup <client>` | `clients_setup` | Prints a client's setup; writes nothing |

Each command takes `--config PATH` (else `config.yaml` in the working directory, else the installed config above) and `--json`, which prints one JSON object: the same one its tool returns. [docs/cli.md](cli.md#looking-at-and-changing-a-setup) has every flag, with examples; [docs/mcp.md](mcp.md) has the tools, and how to add the server to Claude Code, Codex, T3 Code or Cursor.

- **Run them with `--json`, and read the exit code:** `0` done; `1` failed; `2` bad usage (an unknown command, flag or setting, or a secret given as an argument); `3` needs `--yes`, or was declined; `4` the server isn't running. A failure's JSON has `error` (a code such as `needs_confirmation` or `unknown_path`), `message` and, when there is one, `hint`.
- **Paths** are the v8 config's, dotted: `routing.strategy`, `server.port`, `routing.quota.prefer`, `access.api-keys`. An unknown one is refused with the nearest known name. A list is set whole, not one item at a time.

### Where a change goes

- **While a server runs for the config**, and the command has its management key, a change goes through the server's management API. The server checks it, writes it with its config writer (the old file kept, comments kept) and applies it at once, as it does a change made in the dashboard. The server is then the only writer, so the commands and the dashboard don't lose each other's changes, but for one race: the management API's routes take no precondition, so a dashboard save to the same setting that lands between the command's check of the server's config and its request is overwritten ([details](cli.md#how-they-reach-the-server)).
- **Otherwise** the change is written to the file, with the same checks and the same writer. A running server loads it when it sees the file change, if it runs this config; one started later reads it. Until a running server has loaded it, a save of the server's own settings (from the dashboard or the management API) can write over it, so give the commands the management key when a server runs.
- **A server that runs another config**, one on the same port started from another file, is never changed: a server counts only when the file it runs holds the same bytes as this config. Commands stop with `other_config` and say to pass `--config` with the path of the config it runs. A server that runs a copy of this config, another file with the same bytes, counts as running it; the change then goes to its file, and the report's `note` says this file didn't change.
- A change is made only to the file it was worked out from: when the file changes meanwhile, it is worked out again from the file as it is, and made only if it then needs no confirmation. One that does is refused with `config_changed` (run it again), whatever `--yes` said, as that was given for the change as first worked out.
- The report says which way it went (`"via": "server"` or `"file"`), lists each setting it changed with its old and new value, masked, and says that `open-ferry config undo` reverses it.
- Every write of the config, by these commands, the dashboard or the management API, keeps the file it replaces as `<config>.bak`. `config undo` puts that back, and keeps what it replaces as the new `.bak`, so running it again redoes the change. When the config was changed since that write, as by a hand edit, an undo loses that change too, so it is refused with `changed_since` (exit code `1`, with what it would change) unless confirmed with `--yes` or `confirm: true`.
- `credentials` commands need the running server; without one they exit with `4` and say how to start it. `status` exits with `4` too when nothing runs, after saying what the config holds.

### The management key

A command that calls the server looks for the management key in this order:

1. the config's `management.secret-key`, when it is stored plain (not as the bcrypt hash a server makes of it);
2. `MANAGEMENT_PASSWORD`, from the environment or a `.env` file in the working directory;
3. the file `--management-key-file` or `OPEN_FERRY_MANAGEMENT_KEY_FILE` names.

It is never taken from the command line, where process lists, shell history and agent transcripts would keep it: a `--management-key` flag is refused. With no key, settings commands change the file, and the commands that need the server say there is no key.

### What needs a yes

These change nothing without `--yes` (`confirm: true` for a tool):

- deleting a credential (`credentials remove`) or a client key (`keys remove`);
- showing a secret in full (`keys list --reveal`, `clients setup --reveal`); a tool never shows an existing secret;
- replacing the whole config (`config replace`);
- a Claude sign-in (`credentials login claude`), which goes against Anthropic's terms ([why](claude-subscription.md#the-claude-sign-in));
- a change to a **sensitive setting**, by `config set`, `unset`, `replace` or `undo`:
  - `management.allow-remote`, `management.secret-key` and `management.separate-address`, which decide who can reach the management API;
  - `server.host` set to anything but `localhost` or a loopback address, or unset, which opens the proxy to other machines;
  - anything under `server.tls`, which decides how clients connect;
  - anything under `server.trusted-proxies`, which decides whose forwarded addresses are believed, and so which clients count as local;
  - removing the last client key in `access.api-keys`. Blank and repeated keys don't count, as the server ignores them, so `[""]` counts as no key.

At a terminal such a command asks first. Without a terminal, as in an agent's shell, it changes nothing, exits with `3`, and its answer says what it would change (`would.changes`, masked), why (`would.reasons`) and the SHA-256 of the config file it was worked out from (`would.config_sha256`). Show that to the user, and run it again with `--yes` only when they agree, with `--expect-sha256` and that hash (`expect_sha256` for a tool): the change is then made only if the config is still the one shown, and is otherwise refused with `config_changed`, nothing changed.

### Secrets

- What the commands print never holds a secret they weren't asked to show: client keys, the management key, provider API keys, tokens, passwords in URLs, and email addresses are masked, as the dashboard masks them.
- A secret is never taken as an argument: give it on standard input with `--from-stdin`, or in a file with `--from-file` (`from_file` for a tool). `config set management.secret-key <value>` is refused with exit code `2`.
- A file is read only for a secret, never from the auth directory and never when it is a credential file, so a sign-in's tokens or a key can't be copied into a setting (`unsafe_file`). A credential file is one with a PEM block, or with a sign-in's or a key's field at any depth, such as `access_token`, `accessToken`, `refresh-token`, `tokens`, `private_key`, `client_secret` or `sessionKey` ([the full rule](cli.md#secrets-in-what-they-print)). A tool call with `from_file` needs `confirm: true`. A credential file's tokens are scrubbed from everything the commands print.
- `keys add --generate` prints the new key once, since the user needs it to set up a client. A tool returns it only with `confirm: true`, as it then sits in the transcript; else it writes it to the new file `to_file` names, readable by the user alone on Linux and macOS, and gives the path.

### Examples

```sh
open-ferry status --json
open-ferry config get routing.strategy
open-ferry config set routing.strategy fill-first
open-ferry config undo
open-ferry config set server.host 0.0.0.0     # exit code 3: needs --yes
open-ferry config set management.secret-key --from-file new-key.txt --yes
open-ferry keys add --generate
open-ferry credentials list --json
open-ferry credentials disable 870dd779bc223fa9
open-ferry clients setup codex
```

`config set` prints:

```
Setting routing.strategy: done, through the running server.
  routing.strategy: (not set) -> "fill-first"
Undo it with `open-ferry config undo`.
```

and with `--json`:

```json
{
  "action": "set",
  "path": "routing.strategy",
  "changed": true,
  "via": "server",
  "changes": [{"path": "routing.strategy", "new": "fill-first"}],
  "undo": "Undo it with `open-ferry config undo`."
}
```

## Give it credentials

A fresh config has no upstream credentials, so every model request fails until it has one. Either:

- **Sign in to Codex** (a ChatGPT account): with the server running, `open-ferry credentials login codex` prints an address for the user to open in a browser and waits for the sign-in to finish (`--no-wait` returns at once with a `state`; `--state <state>` waits for it later). The `credentials_login` tool does the same in two calls. Without a running server, `open-ferry -config PATH -codex-login`, or `-codex-device-login` on a machine without a browser.
- **Use Claude through your own Claude Code**, with a `claude-cli` entry in the config ([docs/claude-subscription.md](claude-subscription.md)). There is a `-claude-login` too, but signing in that way goes against Anthropic's terms ([why](claude-subscription.md#the-claude-sign-in)).
- **Add API keys** to the config's `api-keys` section: `gemini`, `interactions`, `vertex`, `codex`, `claude`, `xai`, `meta`, or an `openai-compatibility` provider (any upstream that speaks Chat Completions). The template, [`config.example.yaml`](../config.example.yaml), has a commented example of each. An API key is a secret, so it goes in a file, never on the command line: write the provider's list, as the config holds it, to a file only the user can read, then `open-ferry config set api-keys.gemini --from-file gemini.yaml`. The list replaces the one in the config, so it must hold any entries the config has already. For example:

  ```yaml
  - name: gemini
    keys:
      - api-key: "<the key>"
  ```

  A config in CLIProxyAPI's legacy layout names them `gemini-api-key`, `claude-api-key` and so on, which is still read.

The dashboard (below) can sign in and upload credential files too. The server picks up changes to the config and the auth directory without a restart. `open-ferry credentials list` then shows each credential and its state.

## Point a client at it

Each client needs the proxy's address, `http://127.0.0.1:8317` with the config's host and port, and a client key from the config's `access.api-keys` (the one `init` printed). `open-ferry clients setup <client>` prints the setup for `openai-python`, `openai-node`, `codex`, `claude-code` or `curl`, with the address and a model the server serves filled in, and the key masked; it writes no other program's config. The dashboard's Overview page, at `/dashboard/`, writes the same setups. To see which models the proxy can serve now:

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

The commands above call the management API for you. To call it yourself, as a fallback:

- **The management API** is CLIProxyAPI's, at `/v0/management/` (and `/v8/management/`) on the proxy's port: credentials and their state, sign-ins, the config and the client keys, logs, usage and quota. Send the management key, the config's `management.secret-key`, as `Authorization: Bearer <key>`. It answers only on the same machine unless `management.allow-remote` is on, and is off while no management key is set. Five failed attempts from one address ban it for thirty minutes. For example:

  ```sh
  curl -H "Authorization: Bearer $MANAGEMENT_KEY" http://127.0.0.1:8317/v0/management/auth-files
  ```

  Which routes are ported is in [UPSTREAM.md](../UPSTREAM.md).
- **The dashboard** is at `http://127.0.0.1:8317/dashboard/`; sign in with the management key. Its own API, for usage, request-log search and client setups, is at `/open-ferry/api/v1/` and described in [docs/dashboard-api.md](dashboard-api.md).
- **A management address of its own.** When the config sets `management.separate-address`, such as `127.0.0.1:8318`, the management API, the dashboard and its API are served at that address alone, not on the proxy's port, which answers their paths with 404; use that address in the URLs above. `open-ferry check` shows it on its `management address` line.

## More

- [README](../README.md): what open-ferry does, and building it.
- [docs/cli.md](cli.md): every command and flag.
- [docs/mcp.md](mcp.md): the MCP server's tools, and adding it to an agent's app.
- [Migrating from CLIProxyAPI](migrating-from-cliproxyapi.md): what carries over and what doesn't.
- [UPSTREAM.md](../UPSTREAM.md): what was ported from CLIProxyAPI, and every deliberate difference.
