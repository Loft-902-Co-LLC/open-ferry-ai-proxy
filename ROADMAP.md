# Roadmap

What open-ferry is building next, in order. [UPSTREAM.md](UPSTREAM.md) says what is ported today. No dates are promised.

## v1: a complete proxy for one person or a small team

Already in place:
- CLIProxyAPI's config and auth directory, read as they are.
- The OpenAI, Claude and Gemini client APIs, over HTTP and WebSocket.
- The credential manager.
- The management API subset T3 Code uses.
- The images endpoints (`/v1/images/generations` and `/v1/images/edits`), served by Codex, xAI and OpenAI-compatible upstreams.
- The video endpoints (the `/v1/videos` and `/openai/v1/videos` routes), served by xAI.
- CLIProxyAPI's terminal UI (`open-ferry -tui`), as a client of the management API.
- Management writes: the routes that change the config and the credentials. A config write is checked before it lands, replaces the file in one step, keeps the previous one as `config.yaml.bak` and the file's comments, and takes effect before it is answered.
- A web dashboard at `/dashboard/`, built into the binary in place of CLIProxyAPI's downloaded panel, and much easier to use than CLIProxyAPI's management center:
  - each credential's state at a glance (health, cooldowns and quota, with the reason and what to do about it), with sign-ins and uploads;
  - usage, latency and estimated cost per request, model, credential and client key from a local SQLite ledger;
  - a search of the request logs;
  - **no YAML for the basics:** a first run gets you from nothing to a working client in a few steps: replace the example client keys with a new one, add a provider key or sign in, then copy a ready-made client setup;
  - **settings in forms:** every common setting is checked as you type and reviewed against the server before saving. The raw YAML is still there, with a diff before saving.
- Release basics: CI on Linux and Windows, a release workflow that builds binaries for Linux, macOS and Windows, a changelog, and a migration guide for CLIProxyAPI users.

Still to come:

- **Parity with the latest CLIProxyAPI release for every provider we support:** Codex, Claude, Gemini, Gemini Interactions, Vertex AI, Meta (API keys and access tokens), xAI (API keys) and any OpenAI-compatible upstream. We follow upstream's releases; the pin is at v8.0.15. What we do differently is listed in UPSTREAM.md.
- **The upstream features whose settings are read but not yet acted on:**
  - session affinity (`routing.session-affinity` and its options), which keeps a conversation on one credential so its prompt cache stays warm, and moves it only when that credential can't serve;
  - Codex's response steering on the Responses WebSocket (`upstream.codex.response-steering`, experimental and off by default upstream);
  - the model catalog sources in the `models` section, as files: a catalog file the config names is read at start and again when it changes, so a new model can be added without waiting for a release. Unlike upstream, open-ferry downloads no catalog. Without a file it uses the catalogs built into the binary, and we update those by hand when providers release models.
- **Routing by quota:** a strategy that picks by the quota the providers report: the credential whose limit resets soonest, or the one with the most left, keeping a reserve on each. Upstream has no such strategy, so this would be open-ferry's own.
- **Ways to install besides building from source,** starting with the first release:
  - a container image on GitHub's registry, for amd64 and arm64, that uses the same paths as CLIProxyAPI's image (`/CLIProxyAPI/config.yaml`, `/root/.cli-proxy-api` and `/CLIProxyAPI/logs`), so an existing Docker Compose file switches over by changing the image name;
  - install scripts for Linux, macOS and Windows that check the download against the release's `SHA256SUMS` (and its build attestation, where `gh` is installed), write a starting config with a new client key, and print the dashboard's address;
  - a Homebrew tap;
  - a static Linux build (musl) next to the glibc ones, for Alpine and older systems;
  - `open-ferry service install` and `uninstall`, for systemd, launchd and Windows services.
- **`open-ferry check`:** looks over the config, the auth directory, the port, the dashboard and the clock before a start, says how to fix what it finds, and exits with a code a CI job or a service manager can act on.
- **Numbers to back the claims:**
  - the parity tool's results at each pin, in the README;
  - a benchmark against the pinned CLIProxyAPI build that anyone can rerun: requests per second, CPU per request, memory, start time and latency on a long conversation.
- **A setup page for coding agents** (and an `llms.txt`): what an agent needs to install the proxy and point a client at it.
- **Being findable:** once the first release is out, ask to be listed with the related projects in CLIProxyAPI's README.

## After v1: smaller additions

Worth having, but not needed for v1:

- **A cap on long quota cooldowns:** when a provider says its limit resets hours from now, wait at most an hour, then let one request through to check, doubling the wait each time it fails. The dashboard says when the next check is.
- **In the dashboard:**
  - credential states and new requests shown as they happen, without reloading;
  - a switch that hides emails and keys, for sharing the screen;
  - each request saying why its credential was picked;
  - a layout that works on a phone.
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
