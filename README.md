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

**Pre-alpha. There is no server yet.** This README describes what we're building, not what exists. So far, the translators between Codex and three client formats, Claude Messages, OpenAI Responses and OpenAI Chat Completions, are ported in both directions, as are those between Claude and the two OpenAI formats, along with upstream's checks of every provider's reasoning signatures. All are checked against upstream's ([UPSTREAM.md](UPSTREAM.md#checking-parity)).

## Goals for v0.1

- **Providers:** Codex (ChatGPT subscription) and Claude, each through its official OAuth login.
- **Client APIs:** `/v1/responses` (HTTP + SSE and WebSocket), `/v1/messages`, `/v1/chat/completions`.
- **Drop-in config:** reads CLIProxyAPI's `config.yaml` and auth directory, so switching is cheap.
- **T3 Code compatible:** implements the `/v0/management` subset that T3 Code's CLIProxyAPI hub uses (`auth-files`, `api-call`, `reset-quota`).
- **WebSockets done carefully:** one account per connection, keepalive pings, clean fallback to HTTP, honest `previous_response_not_found` when state is lost.
- **Usage ledger:** per-request tokens, latency, and cost in SQLite, viewable in the UI.
- **One binary:** the web UI is embedded and served from the proxy; a desktop tray app may come later.

## Non-goals

- **Full parity with CLIProxyAPI.** We port the parts that matter for the goals above, and list everything else in [UPSTREAM.md](UPSTREAM.md).
- **Client impersonation.** Requests use each provider's official OAuth flow and its documented protocol headers. We don't spoof TLS fingerprints, generate synthetic user IDs, or otherwise disguise proxied traffic.
- **Plugins.** Possibly later.

## Layout

| Path | Purpose |
|---|---|
| `crates/open-ferry` | Binary: CLI and server entry point |
| `crates/open-ferry-core` | Config, accounts, credential store, routing |
| `crates/open-ferry-translate` | Format translators between OpenAI, Anthropic and Gemini. Pure functions with no I/O, publishable on its own |
| `crates/open-ferry-server` | HTTP and WebSocket handlers for `/v1/*` |
| `crates/open-ferry-management` | CLIProxyAPI-compatible `/v0/management` API |
| `reference/cliproxyapi` | Upstream source, pinned as a git submodule. Used as the spec and for comparison tests; never compiled in |

## A word on terms of service

This tool lets you use your own subscription credentials through a local proxy. Whether that's allowed depends on each provider's terms, and some users of similar tools have had accounts restricted. It's your account and your call; read the terms first.

## License

MIT. See [LICENSE](LICENSE). The Luis Pater and Router-For.ME copyright lines cover the portions ported from CLIProxyAPI (MIT); its original license is reproduced verbatim in [licenses/CLIProxyAPI-LICENSE](licenses/CLIProxyAPI-LICENSE). Small parts of Go's standard library, protobuf-go and gjson are ported too, under their BSD and MIT licenses in [licenses/](licenses).
