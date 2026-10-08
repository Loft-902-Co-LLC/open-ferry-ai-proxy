# Roadmap

What open-ferry is building next, in order. [UPSTREAM.md](UPSTREAM.md) says what is ported today. No dates are promised.

## v1: a complete proxy for one person or a small team

Already in place:
- **Parity with CLIProxyAPI v8.0.20, its latest release, for every provider we support:** Codex, Claude, Gemini, Gemini Interactions, Vertex AI, Meta (API keys and access tokens), xAI (API keys) and any OpenAI-compatible upstream. We follow upstream's releases; what we do differently is listed in UPSTREAM.md.
- CLIProxyAPI's config and auth directory, read as they are.
- The OpenAI, Claude and Gemini client APIs, over HTTP and WebSocket.
- The credential manager.
- The management API subset T3 Code uses.
- The images endpoints (`/v1/images/generations` and `/v1/images/edits`), served by Codex, xAI and OpenAI-compatible upstreams.
- The video endpoints (the `/v1/videos` and `/openai/v1/videos` routes), served by xAI.
- The speech endpoints (`/v1/audio/speech` and `/v1/tts`), served by xAI.
- CLIProxyAPI's terminal UI (`open-ferry -tui`), as a client of the management API.
- Management writes: the routes that change the config and the credentials. A config write is checked before it lands, replaces the file in one step, keeps the previous one as `config.yaml.bak` and the file's comments, and takes effect before it is answered.
- A web dashboard at `/dashboard/`, built into the binary in place of CLIProxyAPI's downloaded panel, and much easier to use than CLIProxyAPI's management center:
  - each credential's state at a glance (health, cooldowns and quota, with the reason and what to do about it), the failing and resting ones first and the healthy ones folded away, with sign-ins and uploads;
  - once a client has connected, an Overview that leads with today's calls and each account's health;
  - usage, latency and estimated cost per request, model, credential and client key from a local SQLite ledger;
  - a search of the request logs;
  - **no YAML for the basics:** a first run gets you from nothing to a working client in a few steps: replace the example client keys with a new one, add a provider key or sign in, then copy a ready-made client setup;
  - **settings in forms:** every common setting is checked as you type and reviewed against the server before saving, client key changes included, and the page asks before you leave with changes unsaved. The raw YAML is still there, with a diff before saving;
  - a layout that works on a phone as well as on a desktop.
- Each Claude and Codex credential's quota as the provider's last response gave it: the management API lists it, and the dashboard shows how much of each window is used, when it starts over, and which one stopped the account.
- Codex's response steering on the Responses WebSocket (`upstream.codex.response-steering`, experimental and off by default, as upstream).
- Session affinity (`routing.session-affinity`, off by default as upstream): a conversation stays on the credential that served it, so its prompt cache stays warm, and moves only when that credential can't serve. Session IDs are only routing keys: none is sent upstream, logged or saved.
- `claude-cli`, not in CLIProxyAPI: every Claude model on your own Claude subscription, through your own installed Claude Code, which open-ferry runs for each request. Claude Code signs itself in; open-ferry never reads or stores its credentials. The dashboard shows each entry's state. See [docs/claude-subscription.md](docs/claude-subscription.md).
- Model catalogs from files: a catalog file the `models` section names is read at start and again when it changes, so a new model can be added without waiting for a release. Unlike upstream, open-ferry downloads no catalog. Without a file it uses the catalogs built into the binary, which we update by hand when providers release models.
- Release basics: CI on Linux and Windows, with the install scripts tested on macOS too; a release workflow that builds binaries for Linux (glibc, and static musl builds for Alpine and older systems), macOS and Windows, with `SHA256SUMS` and build provenance attestations; a changelog; and a migration guide for CLIProxyAPI users.
- Ways to install besides building from source, which arrive with the first release:
  - install scripts for Linux, macOS and Windows that check the download against the release's `SHA256SUMS` (and its build attestation, where `gh` is installed) and write a starting config with `open-ferry init`;
  - a container image on GitHub's registry, for amd64 and arm64, that uses the same paths as CLIProxyAPI's image (`/CLIProxyAPI/config.yaml`, `/root/.cli-proxy-api` and `/CLIProxyAPI/logs`), so an existing Docker Compose file switches over by changing the image name.
- Setting up from the command line ([docs/cli.md](docs/cli.md)):
  - `open-ferry init` writes a starting config with new keys;
  - `open-ferry check` looks over the config, the auth directory, the port, the dashboard and the clock before a start, says how to fix what it finds, and exits with a code a CI job or a service manager can act on;
  - `open-ferry service install` and `uninstall` run the proxy at login or at boot, under systemd, launchd, or Windows' Task Scheduler or service manager.
- Numbers to back the claims: the parity tool's results at the pin, in the README, and a benchmark against the pinned CLIProxyAPI build that anyone can rerun ([docs/benchmarks.md](docs/benchmarks.md)). Its first results are preliminary.
- A setup page for coding agents ([docs/agents.md](docs/agents.md)), and an `llms.txt`.

Still to come:

- **Routing by quota:** a strategy that picks by the quota the providers report (now recorded for Claude and Codex): the credential whose limit resets soonest, or the one with the most left, keeping a reserve on each. Upstream has no such strategy, so this would be open-ferry's own.
- **Switching from CLIProxyAPI in one step:** `open-ferry migrate` finds an existing CLIProxyAPI, its config and auth directory and what starts it (a service, a scheduled task, a launcher or a container), says what carries over and what doesn't, backs up the config and credentials, and moves the proxy to open-ferry on the same port with the same files, so clients change nothing. The install scripts offer it when they find CLIProxyAPI, and `open-ferry migrate --undo` switches back.
- **The first release, 0.1.0,** after it has been tested in real use. The install scripts and the container image are first published with it.
- **A Homebrew tap.**
- **Benchmark results to quote,** in place of the preliminary ones: a run against the pinned CLIProxyAPI release on a cloud machine anyone can rent, which also measures requests translated between the API formats.
- **Being findable:** once the first release is out, ask to be listed with the related projects in CLIProxyAPI's README.

## After v1: smaller additions

Worth having, but not needed for v1:

- **A cap on long quota cooldowns:** when a provider says its limit resets hours from now, wait at most an hour, then let one request through to check, doubling the wait each time it fails. The dashboard says when the next check is.
- **In the dashboard:**
  - credential states and new requests shown as they happen, without reloading;
  - a switch that hides emails and keys, for sharing the screen;
  - each request saying why its credential was picked.
- **A management port of its own:** an option to serve the management API and the dashboard only on a second address, such as loopback, so the public port doesn't have those routes at all.
- **`open-ferry-translate` on crates.io,** once its API is stable, for anyone who only wants the translation between the API formats.
- **For teams:** groups of credentials, client keys limited to a group, and limits on each key's requests and spending. The usage ledger already counts each key's use.

## v2: Realtime

- **The Realtime API,** over WebSocket and WebRTC, for the supported providers that offer it.

## v3: plugins, if they are worth it

First, survey the plugins people actually use with CLIProxyAPI: its plugin store, and the `cpa-plugin-*` projects. Then decide:
- whether a plugin host is worth having;
- what plugins may do;
- how plugins are isolated and verified.

Plugins run third-party code, so a store needs its own security design before it ships.

## v4: Home and cluster mode

[CLIProxyAPIHome](https://github.com/router-for-me/CLIProxyAPIHome) is a separate server for large deployments.
- **How it works:** many CLIProxyAPI nodes connect to it over mutually authenticated TLS, speaking the Redis protocol. Home keeps everything in SQLite or PostgreSQL.
- **What it does:**
  - holds the credentials;
  - picks the account that serves each request across all nodes;
  - refreshes tokens;
  - publishes the config;
  - counts usage;
  - hands out plugins.

It only matters to someone running many proxy instances against one pool of accounts, so it comes last. How open-ferry gets there is decided then: by letting its instances share a credential store, or by speaking Home's protocol, which another project owns and changes often.

CLIProxyAPI's remote stores are the likely first step. They keep the config and the credentials in PostgreSQL, a git repository or an object store instead of local files (`PGSTORE_*`, `GITSTORE_*` and `OBJECTSTORE_*`, which open-ferry ignores today).

## Not planned

These work by presenting the proxy as another company's app, which this project doesn't do (see [README.md](README.md#non-goals)):

- The **Antigravity, Gemini CLI, Kimi and Devin** providers. Kimi's API keys work today through `openai-compatibility` or `claude-api-key`.
- **Meta's and xAI's sign-in flows.** Their API keys are supported.
- The **AI Studio relay**, which runs requests through a logged-in browser session.
- **TLS fingerprinting, made-up client or device IDs,** and the rest of CLIProxyAPI's "cloaking".
