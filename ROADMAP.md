# Roadmap

What open-ferry is building next, in order. [UPSTREAM.md](UPSTREAM.md) says what is ported today. No dates are promised.

## v1: a complete proxy for one person or a small team

Already in place:
- CLIProxyAPI's config and auth directory, read as they are.
- The OpenAI, Claude and Gemini client APIs, over HTTP and WebSocket.
- The credential manager.
- The management API subset T3 Code uses.
- CLIProxyAPI's terminal UI (`open-ferry -tui`), as a client of the management API.

Still to come:

- **Parity with the latest CLIProxyAPI release for every provider we support:** Codex, Claude, Gemini, Gemini Interactions, Vertex AI, Meta (API keys and access tokens), xAI (API keys) and any OpenAI-compatible upstream. We follow upstream's releases; the pin is at v8.0.15. What we do differently is listed in UPSTREAM.md.
- **Image and video endpoints:** `/v1/images/generations`, `/v1/images/edits` and the `/v1/videos` routes, for the supported providers that offer them.
- **Management writes:** the routes that change the config and credentials, which the dashboard and the TUI need. Each write is checked before it lands, is atomic, and keeps the previous file.
- **A web dashboard** that is much easier to use than CLIProxyAPI's management center:
  - **No YAML for the basics.** A first run gets you from nothing to a working client in a few steps: add a key or sign in, then copy a ready-made client setup.
  - **Each credential's state at a glance.** Health, cooldowns and quota, each with the reason and what to do about it.
  - **Usage.** Tokens, latency and cost per request, model and credential, kept in a local SQLite ledger.
  - **Settings in forms.** Every setting is checked as you type, and a diff is shown before saving. The raw YAML is still there for those who want it.
  - **Request logs.** Searchable, with secrets masked.
  - **Ships inside the binary.** Nothing is downloaded at runtime; CLIProxyAPI downloads its panel from a GitHub release and keeps updating it in the background.
- **Release basics:**
  - CI on Linux and Windows;
  - release binaries for Linux, macOS and Windows;
  - a changelog;
  - a migration guide for CLIProxyAPI users.

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

## Not planned

These work by presenting the proxy as another company's app, which this project doesn't do (see [README.md](README.md#non-goals)):

- The **Antigravity, Gemini CLI, Kimi and Devin** providers. Kimi's API keys work today through `openai-compatibility` or `claude-api-key`.
- **Meta's and xAI's sign-in flows.** Their API keys are supported.
- The **AI Studio relay**, which runs requests through a logged-in browser session.
- **TLS fingerprinting, made-up client or device IDs,** and the rest of CLIProxyAPI's "cloaking".
