# Feature inventory

The [README](../README.md#status) has the current release status and a [quick start](../README.md#quick-start).

Open Ferry follows CLIProxyAPI **v8.0.42**. [UPSTREAM.md](../UPSTREAM.md#what-moving-from-v8038-to-v8042-left-out) lists the changes and retained differences. The README test totals and benchmark results keep the versions they measured.

## Client APIs and media

`open-ferry` reads CLIProxyAPI's `config.yaml` and auth directory, signs in to Codex and Claude with their OAuth logins (`-codex-login`, `-codex-device-login`, `-claude-login`), binds Sign in with ChatGPT to an existing Codex credential with [`-siwc-bind FILE`](cli.md#binding-sign-in-with-chatgpt-to-a-codex-credential) or the authenticated [management bind mode](dashboard-api.md#sign-in-with-chatgpt-binding), and serves Chat Completions, legacy Completions, Claude Messages, and OpenAI Responses over HTTP and WebSocket, with upstream's routing, errors, streaming and keep-alives.

Open Responses WebSockets recheck client keys and safe mode for each create, append and steer, updating usage attribution without changing earlier turns. Denials preserve the connection; deferred steering is checked again before sending. A socket can still interrupt its own work after access loss.

The Claude sign-in goes against Anthropic's terms and, in our testing, gets only the Haiku models ([why](claude-subscription.md#the-claude-sign-in)).

It serves the Gemini API's `/v1beta/models` routes too (model lists, `generateContent`, `streamGenerateContent` and `countTokens`), and the model list Codex clients fetch, from upstream's Codex client catalog.

With `xai-api-key` credentials it serves the video endpoints too, xAI's own `/v1/videos` routes and OpenAI's `/openai/v1/videos`, and streams the finished video without writing it to disk.

It serves OpenAI's speech endpoint, `/v1/audio/speech`, and xAI's `/v1/tts`, through xAI's Grok TTS models (`grok-tts`, `grok-voice-tts-1.0`).

It serves the images endpoints, `/v1/images/generations` and `/v1/images/edits`, through Codex's image generation tool, xAI's Grok Imagine image models and the image models of `openai-compatibility` providers, and reads an edit's uploaded images in memory, never writing them to disk.

Through Codex, the provider picks the image's size: it gets the request's `size` as sent, yet in our testing `1024x1024` came back as 1536x1024 and as 1254x1254.

The answer's `size` gives the real one.

## Providers and routing

It also calls the OpenAI-compatible upstreams of the config (`openai-compatibility`), any provider that speaks Chat Completions, with their API keys, headers, proxies and model aliases, and Gemini and Vertex AI, with the config's `gemini-api-key` and `vertex-api-key` entries and Vertex AI service accounts from the auth directory.

With the config's `claude-cli` entries, it serves Claude models through your own installed Claude Code, which signs itself in to your subscription and makes the requests to Anthropic ([docs/claude-subscription.md](claude-subscription.md)).

Requests to Codex and to the OpenAI-compatible upstreams carry the thinking setting a model suffix such as `gpt-5.5(high)` or the client's request asks for, checked against the model's levels.

Behind the server, upstream's credential manager picks an account per request, with its retries, cooldowns and model aliases, refreshes tokens in the background, and follows changes to the config and auth files.

With the config's `codex.model-level-cooling`, a Codex usage limit cools only the model. With `codex.stream-bootstrap-buffering` on (off by default), moderation metadata stays buffered before output so an early overload can fail over to another account.

Requests identified as Codex clients by User-Agent use their outer Codex login even when Sign in with ChatGPT is enabled; other clients keep the saved choice. Missing outer credentials return a request-only 401 without making a healthy Sign in with ChatGPT login unavailable. Refusals and refreshes stay independent for each login, and requests never change the saved choice.

When credential files change, Open Ferry keeps live cooldowns and refresh delays, rereads the file, and preserves concurrent user edits. Failed scans and file checks that cannot prove removal keep existing credentials. A matching `request-scoped-errors` 401 rule does not refuse or refresh a login; actions ending in `-and-cooldown` still start a cooldown.

Thinking suffixes such as `(fast)` now return 400 when unrecognized; Responses requests sent directly to Codex/xAI models that accept configuration updates keep their existing suffix handling. For OpenAI Chat Completions and Responses clients, unlisted thinking levels on OpenAI-compatible, Meta and other custom catalog models return 400 rather than being quietly adjusted. Compatibility models still map `xhigh`/`max` through their levels; Claude and Gemini clients keep clamping unsupported levels.

## Management and observability

It also serves the part of CLIProxyAPI's `/v0/management` API that T3 Code's hub uses, with upstream's key checks and bans: `auth-files` (and `auth-files/models`), `api-call` and `reset-quota`, under their `/v8/management` names too, and more: the credential files and state, the OAuth logins, the config reads and writes (a save keeps the file's comments and takes effect before the answer), `logs`, the usage queue and `quota/fetch`.

The plugin routes and other providers' ones aren't ported ([UPSTREAM.md](../UPSTREAM.md#deliberately-not-ported)).

Behind them, `request-log` and the error logs write each request's log to the log directory, `logging-to-file` writes `main.log`, `usage-statistics-enabled` queues a usage record for each call, the config's `payload` rules and `disable-image-generation` apply, and `save-cooldown-status` keeps cooldowns across restarts, all as upstream does (see [UPSTREAM.md](../UPSTREAM.md#observability)).

With `-tui` it runs upstream's terminal management UI, as a client of a running server's management API or, with `-standalone`, of a server it runs itself ([UPSTREAM.md](../UPSTREAM.md#the-tui)).

A web dashboard is built into the binary, at `/dashboard/` ([dashboard](../README.md#dashboard)).

Usage records distinguish the service tier the client requested, the tier sent after payload rules, and the tier in the reply. Claude replies report their tier from `usage.speed`.

## Translation and sessions

The translators between Codex and three client formats, Claude Messages, OpenAI Responses and OpenAI Chat Completions, are ported in both directions, as are those between Claude and the two OpenAI formats, along with upstream's checks of every provider's reasoning signatures.

So are the translators for upstreams that only speak OpenAI Chat Completions, for Claude Messages and OpenAI Responses clients, the passthrough for Chat Completions clients, the translators from Gemini `generateContent` clients to Codex, Claude and Chat Completions upstreams, and the translators for Gemini, Claude Messages, Chat Completions and OpenAI Responses clients to Gemini upstreams.

For Claude clients of Codex, which drop Codex's encrypted reasoning, it keeps each turn's reasoning and tool calls in memory by the session the client names and puts them back in that session's next request, as upstream does.

With `client.codex.optimize-multi-agent-v2`, a Codex client's multi-agent v2 requests can go to other upstreams, with the models it may delegate to listed in `spawn_agent`, and with `codex.orphan-delegation-compatibility`, a Codex sub-agent's delegation outputs without their call become user messages; a `codex-api-key` model marked `is-compat` gets requests in Codex's plain Responses dialect.

The translators are checked against upstream's ([UPSTREAM.md](../UPSTREAM.md#checking-parity)).

Gemini requests can mix built-in and function tools; model lists use a model's context and output limits when Gemini-specific limits are unavailable, leaving unknown limits out, and Vertex US/EU locations use regional endpoints. Tool schemas retain supported reference-sibling and conjunction rules within the existing resource bounds. Gemini-to-Claude tool calls retain arguments for implicitly open object schemas and wait for streamed arguments before emitting empty inputs. Candidate-less Gemini/Vertex prompt blocks return 502; Chat/Claude streams need a candidate finish reason.

Claude context-window exhaustion retains partial content, tool calls and usage, ending Chat Completions with `length` and Responses with `incomplete`/`max_output_tokens`. Deferred searches survive intervening tool outputs, missing prompt-overflow error codes are filled, and Codex commentary appears as unsigned Claude thinking with output order preserved.
