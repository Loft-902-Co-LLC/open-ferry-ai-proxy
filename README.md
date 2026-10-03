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

**Pre-alpha.** This README describes what we're building, not all of what exists. `open-ferry` reads CLIProxyAPI's `config.yaml` and auth directory, signs in to Codex and Claude with their OAuth logins (`-codex-login`, `-codex-device-login`, `-claude-login`), and serves Chat Completions, legacy Completions, Claude Messages, and OpenAI Responses over HTTP and WebSocket, with upstream's routing, errors, streaming and keep-alives. It serves the Gemini API's `/v1beta/models` routes too (model lists, `generateContent`, `streamGenerateContent` and `countTokens`), and the model list Codex clients fetch, from upstream's Codex client catalog. It also calls the OpenAI-compatible upstreams of the config (`openai-compatibility`), any provider that speaks Chat Completions, with their API keys, headers, proxies and model aliases, and Gemini and Vertex AI, with the config's `gemini-api-key` and `vertex-api-key` entries and Vertex AI service accounts from the auth directory. Requests to Codex and to the OpenAI-compatible upstreams carry the thinking setting a model suffix such as `gpt-5.5(high)` or the client's request asks for, checked against the model's levels. Behind the server, upstream's credential manager picks an account per request, with its retries, cooldowns and model aliases, refreshes tokens in the background, and follows changes to the config and auth files. With the config's `codex.model-level-cooling`, a Codex usage limit cools only the model, and with `codex.stream-bootstrap-buffering`, a Codex stream that reports an overload before it starts fails over to another account. It also serves the part of CLIProxyAPI's `/v0/management` API that T3 Code's hub uses, with upstream's key checks and bans: `auth-files` (and `auth-files/models`), `api-call` and `reset-quota`, under their `/v8/management` names too. The other management routes aren't ported yet. The translators between Codex and three client formats, Claude Messages, OpenAI Responses and OpenAI Chat Completions, are ported in both directions, as are those between Claude and the two OpenAI formats, along with upstream's checks of every provider's reasoning signatures. So are the translators for upstreams that only speak OpenAI Chat Completions, for Claude Messages and OpenAI Responses clients, the passthrough for Chat Completions clients, the translators from Gemini `generateContent` clients to Codex, Claude and Chat Completions upstreams, and the translators for Gemini, Claude Messages, Chat Completions and OpenAI Responses clients to Gemini upstreams. For Claude clients of Codex, which drop Codex's encrypted reasoning, it keeps each turn's reasoning and tool calls in memory by the session the client names and puts them back in that session's next request, as upstream does. The translators are checked against upstream's ([UPSTREAM.md](UPSTREAM.md#checking-parity)).

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
| `crates/open-ferry-core` | Config, credentials and their store, the model registry, and the credential manager that routes calls |
| `crates/open-ferry-providers` | The Codex and Claude OAuth logins and executors, the OpenAI-compatible executor, and the Gemini and Vertex AI executors |
| `crates/open-ferry-translate` | Format translators between OpenAI, Anthropic and Gemini. Pure functions with no I/O, publishable on its own |
| `crates/open-ferry-server` | HTTP and WebSocket handlers for `/v1/*` and the Gemini API's `/v1beta/*` |
| `crates/open-ferry-management` | CLIProxyAPI-compatible `/v0/management` API |
| `reference/cliproxyapi` | Upstream source, pinned as a git submodule. Used as the spec and for comparison tests; never compiled in |

## A word on terms of service

This tool lets you use your own subscription credentials through a local proxy. Whether that's allowed depends on each provider's terms, and some users of similar tools have had accounts restricted. It's your account and your call; read the terms first.

## License

MIT. See [LICENSE](LICENSE). The Luis Pater and Router-For.ME copyright lines cover the portions ported from CLIProxyAPI (MIT); its original license is reproduced verbatim in [licenses/CLIProxyAPI-LICENSE](licenses/CLIProxyAPI-LICENSE). Small parts of Go's standard library, gin, protobuf-go, gjson, sjson and gorilla/websocket are ported too, under their BSD and MIT licenses, as are yaml.v3's decoding rules, under the Apache License 2.0 with its NOTICE; all are in [licenses/](licenses).
