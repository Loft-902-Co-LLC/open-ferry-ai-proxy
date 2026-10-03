# Upstream: CLIProxyAPI

open-ferry-ai-proxy is a port of [router-for-me/CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) (MIT).

## Pinned reference

| | |
|---|---|
| Version | `v8.0.10` |
| Commit | `6fecc6e5567912661654a4eaf9b8f5436facd1c2` (2026-10-02) |
| Location | `reference/cliproxyapi` (git submodule) |

When we move the pin, we update this table and note behaviour changes in the PR.

## Porting conventions

- **Port behaviour, not lines.** Upstream manipulates raw JSON with gjson/sjson. We use `serde_json::Value` so unknown fields pass through untouched, and typed structs only where they help.
- **Every ported file gets a header** naming its source, for example:

  ```rust
  // Ported from CLIProxyAPI internal/translator/codex/claude (v8.0.10, MIT).
  // https://github.com/router-for-me/CLIProxyAPI
  ```

- **Deviations are documented** in the file, with the reason (usually an upstream issue number).
- **Go means Go 1.26.** Upstream's releases are built with Go 1.26.4, so where its behaviour comes from Go's standard library we match Go 1.26, whatever version a ported file's header names. The difference shows in `encoding/json`, which from Go 1.27 runs on its v2 implementation, and in the Unicode tables: Go 1.26 has Unicode 15.0. Case mapping uses Rust's newer tables, so a letter added to Unicode since 15.0 may change case where upstream leaves it. Where upstream calls `strings.ToLower` or `ToUpper`, the port maps each character by its simple mapping as Go does (`open_ferry_translate::go::to_lower` and `to_upper`), not with `str::to_lowercase`, which differs for `İ`, a word-final `Σ` and `ß`. Where it calls `strings.EqualFold`, the port walks Go's case-folding orbits (`go::equal_fold`), so `ﬅ` and `ﬆ` differ though both upper-case to `ST`. Go's `json.Valid` allows 10,000 levels of nesting and gjson's `Valid` any; `go::json_valid` and `go::gjson_valid` follow whichever upstream calls.
- **Bugs we find upstream get reported upstream**, with a failing test case where we can.

## Module map

Upstream translators live at `internal/translator/<upstream>/<client-format>/`. For example, `codex/claude` turns a Claude Messages request into a Codex request and translates the response back.

| open-ferry module | Upstream source | Status |
|---|---|---|
| `open-ferry-translate`: Claude client → Codex | `internal/translator/codex/claude` | Request (including compatibility mode) and response ported (`codex::claude`) |
| `open-ferry-translate`: reasoning signatures | `internal/signature` | Ported (`signature`): checks for Claude, Gemini, GPT, Grok, Kimi and SWE signatures, replay decisions, and the Claude Messages and Gemini sanitizers |
| `open-ferry-translate`: Chat Completions → Codex | `internal/translator/codex/openai/chat-completions` | Request and response ported (`codex::openai::chat_completions`), including the `apply_patch` bridge |
| `open-ferry-translate`: Responses → Codex | `internal/translator/codex/openai/responses` | Request and response ported (`codex::openai::responses`), except the `apply_patch` bridge |
| `open-ferry-translate`: Chat Completions → Claude | `internal/translator/claude/openai/chat-completions` | Request (including compatibility mode) and response ported (`claude::openai::chat_completions`) |
| `open-ferry-translate`: Responses → Claude | `internal/translator/claude/openai/responses` | Request (including compatibility mode) and response ported (`claude::openai::responses`) |
| `open-ferry-translate`: Responses → Chat Completions | `internal/translator/openai/openai/responses` | Request and response ported (`openai::responses`), including the `apply_patch` bridge. Upstream's Chat Completions handler also uses the request half, for a client that sends it a Responses body |
| `open-ferry-translate`: Claude → Chat Completions | `internal/translator/openai/claude` | Request (including compatibility mode), response and token counts ported (`openai::claude`) |
| `open-ferry-translate`: Chat Completions → Chat Completions | `internal/translator/openai/openai/chat-completions` | Ported (`openai::chat_completions`): requests and responses pass through, with the request's model replaced |
| `open-ferry-translate`: registry | `sdk/translator` | Ported (`registry`): the built-in translators above, the fallback for pairs with none, and reasoning summary settings carried between formats. Plugin hooks and middleware are not ported |
| `open-ferry-translate`: legacy Completions | `sdk/api/handlers/openai/openai_handlers.go` | Ported (`completions`): the request, response and stream chunk conversions the `/v1/completions` handler uses. The handler itself comes with the server |
| `open-ferry-server`: routes and middleware | `internal/api/server_routes.go`, `server_middleware.go`, `internal/access`, `sdk/access` | Ported (`app`, `auth`): `/healthz`, `/v1/models`, Chat Completions, legacy Completions, Claude Messages and token counts, and Responses with compact, also under `/backend-api/codex`; the Gemini API's `/v1beta/models` routes; client keys, safe mode and CORS. Other routes, such as images, videos, realtime and `/v1beta/interactions`, are not ported yet. `router_with` serves another router beside these routes, inside the same CORS, logging and panic handling, which is how the binary mounts the management API |
| `open-ferry-server`: handler plumbing | `sdk/api/handlers/handlers*.go`, `request_body.go`, `model_execution.go`, `internal/util/provider.go`, `internal/thinking/suffix.go` | Ported: reading and decoding bodies, model routing (`auto` and thinking suffixes), error bodies, bootstrap retries, stream forwarding and keep-alives. Calls go to a `Dispatcher` (`open-ferry-core::exec`) |
| `open-ferry-server`: OpenAI and Claude handlers | `sdk/api/handlers/openai/openai_handlers.go`, `sdk/api/handlers/claude/code_handlers.go` | Ported (`handlers::openai`, `handlers::claude`). Chat Completions takes Responses bodies too, through the Responses to Chat Completions translator |
| `open-ferry-server`: Responses over HTTP | `sdk/api/handlers/openai/openai_responses_handlers.go` | Ported (`handlers::responses`), except Codex multi-agent v2 tools and orphan delegation |
| `open-ferry-server`: Responses WebSocket | `sdk/api/handlers/openai/openai_responses_websocket*.go` | Ported (`handlers::responses_ws`): incremental input, transcript and tool-call repair, warm-ups answered locally, and hand-off to a credential's upstream WebSocket. Response steering and the request log's timeline are not ported |
| `open-ferry-server`: Gemini handlers | `sdk/api/handlers/gemini/gemini_handlers.go`, the `gemini` case of `convertModelToMap` in `internal/registry/model_registry.go` | Ported (`handlers::gemini`): the model list and single model, `generateContent`, `countTokens` and `streamGenerateContent`, which is SSE unless the client asks for another `alt`, then raw bytes typed as Go's server sniffs them. The list is sorted by ID, where upstream's order varies; a bad `%` escape in the path is read as it is, where Go's server answers 400; and `POST /v1beta/models` gets 404, where gin redirects it. The registry doesn't fill `ModelInfo`'s token limits and generation methods yet. Interactions and Home mode are not ported |
| `open-ferry-core`: call seam | `sdk/cliproxy/executor/types.go`, `sdk/cliproxy/auth` errors | Ported (`exec`, `models`): the call the HTTP layer hands the credential manager, and the manager's errors as the handlers report them |
| `open-ferry-core`: config | `internal/config`, `internal/watcher`, `internal/safemode`, `ResolveAuthDir` in `internal/util/util.go` | Ported (`config`): `config.yaml` in the legacy and v8 layouts, decoded with yaml.v3's rules, then normalized as upstream does; safe mode for the example API keys; and the watcher on the config file and the auth directory. `config_v8.go` and `oauth_scope.go` follow v8.0.11, whose v8 layout adds the `upstream` section. The config is never written back |
| `open-ferry-core`: credentials | `sdk/cliproxy/auth/types.go`, `status.go`, `classification.go`, `metadata_*.go`, `weight.go`, `sdk/auth/filestore.go`, `internal/watcher/synthesizer` | Ported (`auth`): the credential type and its readers, the auth file store, and making credentials from auth files and from the config's API keys. Fields the project has no use for, such as `claude_device_ids`, are kept in the files untouched |
| `open-ferry-core`: model registry | `internal/registry`, `sdk/cliproxy/service_models.go` | Ported (`registry`): the built-in catalog, each credential's models after aliases and exclusions, and the suspension and quota state the credential manager publishes. Remote catalog updates aren't ported, so the built-in catalog is always the one used |
| `open-ferry-core`: credential manager | `sdk/cliproxy/auth` (`conductor*.go`, `scheduler.go`, `selector.go`, `oauth_model_alias.go`, `auto_refresh_loop.go` and the rest) | Ported (`manager`): picking a credential (round robin, fill first, priorities, weights), retries across credentials, cooldowns and quota, model aliases and force mapping, request-scoped error rules, token refresh and auto refresh; and what the management API reads: each credential's index, call counts and recent calls, and its cooldowns. Session affinity, Home, plugin schedulers, the cooldown state store and request preparation aren't ported |
| `open-ferry-providers`: Codex | `internal/auth/codex`, `sdk/auth/codex*.go`, `internal/runtime/executor/codex_executor*.go` | Ported (`codex`): the OAuth and device logins, token refresh, and the HTTP executor for Responses, compact and local token counts. Deferred: the Responses WebSocket upstream, the reasoning replay cache, image generation and the `apply_patch` bridge |
| `open-ferry-providers`: Claude | `internal/auth/claude`, `internal/runtime/executor/claude_executor*.go` | Ported (`claude`): the OAuth login, token refresh, and the executor for Messages and token counts, with rate-limit cooldowns. The Claude Code profile is not ported (see [below](#deliberately-not-ported)) |
| `open-ferry-providers`: OpenAI-compatible | `internal/runtime/executor/openai_compat_executor.go`, `helps/openai_compat_max_tokens.go`, `helps/openai_compat_tool_results.go`, `helps/token_helpers.go` | Ported (`openai_compat`): the executor for the config's `openai-compatibility` upstreams, for Chat Completions (and OpenAI Responses for compact), streaming, local token counts, `max_tokens` and tool result adjustments by model, prompt cache keys a client sends, and upstream's stream and error checks. Not ported: the Images endpoints, applying a model's thinking suffix, payload rules and the `is-compat` flag |
| `open-ferry`: the binary | `cmd/server/main.go`, `sdk/cliproxy/service*.go`, `internal/cmd` (logins), the TLS half of `internal/api/server.go` | Ported: the flags, the Codex and Claude logins, and serving with background refresh, config and auth file reloads, graceful shutdown and TLS, with the management API beside the proxy's routes. Each OpenAI-compatible provider of the config gets credentials and an executor, which follow config reloads. Logs go to standard output only, and nothing looks up the public IP |
| `open-ferry-management`: access | `internal/api/server_management.go`, `server_management_v8.go`, `internal/api/handlers/management/handler.go`, and gin's `ClientIP` as `internal/api/server.go` sets it up | Ported (`access`, `client_ip`, `state`): the API answers only while `remote-management.secret-key` or `MANAGEMENT_PASSWORD` is set, following config reloads; the key from `Authorization` (bearer or bare) or `X-Management-Key`, plain or bcrypt, compared in constant time; only 127.0.0.1 and ::1 count as local unless `allow-remote` is on, with `trusted-proxies` deciding whose forwarding headers count; five failures ban an address for thirty minutes; and the `X-CPA-*` headers. The local management password, Home mode and the plugin header are not ported |
| `open-ferry-management`: `auth-files` | `internal/api/handlers/management/auth_files*.go` | Ported (`auth_files`): `GET auth-files` and `GET auth-files/models` (v8: `credentials`, `credentials/models`), with the `name` and `auth_index` filters, paging, states, cooldowns, call counts and recent requests. The other `auth-files` routes (upload, download, delete, status, fields, refresh) are not ported, nor is the listing from disk when there is no credential manager |
| `open-ferry-management`: `api-call` | `internal/api/handlers/management/api_tools.go`, `sdk/proxyutil/proxy.go` | Ported (`api_call`, `proxy`, and the parts of Go's HTTP client the result depends on): `POST api-call` (v8: `requests/api-call`), with `$TOKEN$` substitution, the proxy choice and Go's redirect rules. Token refresh for Antigravity, Meta and xAI credentials is not ported |
| `open-ferry-management`: `reset-quota` | `internal/api/handlers/management/quota.go` | Ported (`quota`): `POST reset-quota` (v8: `routing/cooldown/reset`) |

### Deviations so far

Each ported file lists its deviations in its module docs. Most are byproducts of using `serde_json` rather than raw bytes:

- **Key order is kept.** Upstream round-trips tool schemas through Go maps, which sorts their keys. We keep the client's order (`serde_json` with `preserve_order`). In the Responses and Claude to Chat Completions requests we keep a schema's number text too, where upstream writes each number as a `float64` (`1.50` becomes `1.5`, and integers beyond 2^53 lose precision). In the Claude to Chat Completions request, a `properties` the schema is given is added last.
- **Embedded JSON is re-serialized compactly.** Where upstream copies a client's raw JSON bytes into a string field (for example `function_call.arguments`), we write the same value as compact JSON. Number text is kept exactly (`arbitrary_precision`). In responses this affects a web search's `partial_json`. Function call arguments are passed through byte for byte. In the Responses to Chat Completions request it also affects a custom tool call's `input` that isn't a string, which upstream copies as text into the call's `{"input": ...}` arguments. In the Claude to Chat Completions request it affects a tool call's `arguments`, a tool result's content that isn't text, and a value read as text that isn't a string, such as a `stop_sequences` item or a `user` that is an object. In Chat Completions responses translated for Claude and Responses clients, it affects values read as text that are objects or arrays, such as content, reasoning and tool call arguments.
- **An empty `reasoning` object is no reasoning however it's written.** The Responses to Chat Completions translator compares the object's text with `{}`, so upstream counts `{ }` as reasoning turned on and gives tool call turns the `[reasoning unavailable]` placeholder. We don't.
- **Strings use `serde_json`'s escaping.** Where upstream writes a string with Go's JSON encoder, it escapes `<`, `>`, `&`, U+2028 and U+2029 as `\u003c` and so on. We write them as they are; the JSON values are the same.
- **Malformed JSON is not read.** A Codex `data:` line that isn't valid JSON is treated as an event with no fields, or gives no chunk in Chat Completions; gjson reads what it can from it. The same goes for a Claude `data:` line, and for a Chat Completions `data:` line or response translated for Claude and Responses clients. `serde_json` also rejects an unpaired surrogate escape such as `\ud800`, which gjson reads as U+FFFD. Nor can `serde_json` read JSON nested more than 128 levels deep. A Claude `data:` line that is valid JSON but can't be read for one of these reasons, or because it isn't UTF-8, ends a Responses stream with `response.failed` when the request declares `apply_patch`, since it could carry part of a patch; otherwise it gives nothing. A Chat Completions line or response does the same when translated for a Responses client, a whole response becoming the failed response. Translated for a Claude client, a whole response whose tool call arguments can't be read gives the call an empty `input`, where upstream copies them. Providers don't send such lines, so this only matters for corrupted streams. Likewise, a Chat Completions tool message's string content is read as JSON parts only if all of it is valid JSON, and so is a Responses tool output's string when it is checked for images to send to Chat Completions.
- **Block order instead of Go map order.** Where upstream walks a Go map, whose order is random, and the output depends on it, we go in ascending order. When a Claude stream ends with tool calls still open, the Responses translator closes them in block order.
- **Byte-length truncation keeps whole characters.** Upstream cuts names and IDs at 64 bytes and can split a UTF-8 character; we stop at the character boundary before it.
- **Duplicate object keys: the last one wins.** gjson reads the first occurrence of a key and `serde_json` keeps the last. RFC 8259 leaves this to the parser. For the same reason, a Gemini part with two `thoughtSignature` keys counts as normalized; upstream re-sanitizes it.
- **Out-of-range numbers saturate.** Where upstream converts a float such as `1e30` to an integer, Go's result depends on the CPU: amd64 gives the minimum int64, arm64 saturates. We saturate, so a huge thinking budget maps to the highest effort, and a token count of `1e400` becomes the largest int64.
- **Numbers beyond `f64` are kept as written.** Where upstream copies a value as a number, such as Chat Completions' `reasoning_effort`, Go writes `1e400` as `+Inf`, which isn't JSON. We keep the number's text. Negative zero becomes `0` there, where Go writes `-0`. Where upstream converts the value to a float first, as for a Claude request's `top_p`, a Claude request's `temperature` and `top_p` sent to Chat Completions, or a legacy Completions request's `temperature`, we leave out a value that isn't finite. Where Go's `json.Marshal` meets such a number, as in a Completions choice's `logprobs`, it fails and upstream writes the field with no value; we keep the number. A Responses result that repeats such a `temperature` or `top_p` from the request gets `null`. In the Responses to Chat Completions request, such a number in `text.format` or in a tool's parameter schema is kept as written; upstream writes `+Inf`, or fails to write the request when the number is inside an object or array. In the Claude to Chat Completions request, such a number in a tool's `input_schema` is kept as written; upstream fails to write the tool.
- **No made-up user IDs.** When a Chat Completions or Responses client sends no user ID, upstream fills the Claude request's `metadata.user_id` with a hash of the conversation. We pass on only an ID the client sent, in `metadata.user_id` or `user`, and otherwise send empty `metadata` (see [Deliberately not ported](#deliberately-not-ported)).
- **Generated tool call IDs come from a keyed hasher.** A Chat Completions or Responses tool call without an ID gets one in upstream's form, `toolu_` and 24 letters and digits. Upstream draws them from the operating system's random source; we draw them from the standard library's randomly keyed hasher, so the crate needs no random-number dependency. The IDs only need to be unique within a conversation.
- **Registry response streams are objects.** Upstream's registry passes each response chunk with a `*any` that the translator fills on the first call. Ours makes a `ResponseStream` per response, which looks up the translator once and holds its state. It leaves out empty chunks, which upstream's stream manager drops before its handlers see them: its Codex to Claude translator returns one, possibly empty, for each `data:` line, and its Chat Completions passthrough one for each line but `[DONE]`.
- **The translators read the static model catalog.** Whether a Claude model takes adaptive thinking or a token budget comes from upstream's model registry, which holds the models of configured accounts and falls back to a static catalog. The translators read the static catalog; the Claude executor reads the registry.
- **A `null` client request counts as missing.** The Responses response translators for Claude and Chat Completions upstreams read the tools and the repeated fields from the client's request, else the translated one. A client's request that is JSON `null` counts as missing here; upstream takes it. The server routes no such request, since it names no model.
- **Escaped cache breakpoint keys are removed too.** Upstream strips `prompt_cache_breakpoint` from Responses `input` only when the body holds the key unescaped. We strip it however it's written.
- **The signature sanitizer's default target falls back to the model.** `ClaudeMessagesSanitizeOptions::default()` targets `Provider::Unknown`, which falls back to the target model's provider. Upstream's zero value is an empty provider, which skips that fallback.
- **Warnings are not logged yet.** Upstream's Responses to Claude translator logs a warning when it drops input items of a type it can't convert, and when a history still breaks Claude's tool pairing rules after its repair. We drop the same items and make the same repairs, without the warnings, until there is a server to log them.
- **Protobuf errors always use a regular space.** Signature errors include protobuf-go's parse errors, which start with `proto:` and a space. protobuf-go makes that space a non-breaking one in some builds, chosen per binary so that callers don't compare error strings. We always write a regular space.
- **Not ported in `signature`:** upstream's debug logging when it sanitizes Gemini signatures. Its tests of that logging are not ported, nor are tests that read captured signature corpora, which aren't in its repository.
- **Not yet ported in `codex::openai::responses`:** the `apply_patch` bridge. Only upstream's Codex executor turns it on, and ours doesn't yet.
- **OpenAI-compatible upstreams get open-ferry's user agent.** Requests to them say `User-Agent: open-ferry/<version>`, unless the client sent its own, where upstream always sends `cli-proxy-openai-compat`; an entry's `headers` can't set `User-Agent` or another header that says which client is calling. No `prompt_cache_key` is made up: one the client sends still reaches a provider with `support-prompt-cache-key`, but upstream's keys derived from a Claude Code prompt or a session aren't sent.
- **OpenAI-compatible executors follow the config, not every reload.** They are made again only when `proxy-url` or `openai-compatibility` changes, and one that no credential uses any more is unregistered; upstream makes every executor again on each reload and keeps them. A credential of a provider this port has no executor for isn't served, where upstream hands it to an OpenAI-compatible executor.
- **The non-streaming response expects a complete final event.** Codex's `response.completed` often has an empty `output`. The Codex executor fills it with the streamed items before calling the translator, as upstream's does.
- **Provider errors don't repeat the credential's secret.** An upstream's error body, or an error in its stream, reaches the client as the error's message. Where it quotes the API key or token the request was sent with, as it is or escaped as JSON, the Codex, Claude and OpenAI-compatible executors replace it with `[redacted]`; upstream passes it on. A key shorter than 8 bytes is left alone, since it could be an ordinary word.
- **OpenAI-compatible streams are bounded.** A frame's `data:` lines, joined, may hold at most 50 MiB, as one line may; a bigger frame ends the stream with a 502, `upstream SSE data frame is too large`. Upstream holds any amount.
- **OpenAI-compatible base URLs are read as WHATWG URLs.** Their `.` and `..` segments are resolved, where Go sends them as written. A base URL with an ASCII control character fails before anything is sent, as in Go, with Go's message but without the URL, which may hold a secret. As with Go's HTTP client, a custom `Content-Length`, `Transfer-Encoding` or `Trailer` header is ignored.

### The management API

`open-ferry-management` serves four of upstream's management routes, under both of their names:

| Route | v8 route |
|---|---|
| `GET /v0/management/auth-files` | `GET /v8/management/credentials` |
| `GET /v0/management/auth-files/models` | `GET /v8/management/credentials/models` |
| `POST /v0/management/api-call` | `POST /v8/management/requests/api-call` |
| `POST /v0/management/reset-quota` | `POST /v8/management/routing/cooldown/reset` |

Bodies, statuses and headers are written as gin and Go's `encoding/json` write them, and request bodies, query strings, URLs and client addresses are read as gin and Go read them. The tests check them against answers recorded from Go programs built with upstream's `go.mod` (gin v1.10.1, Go 1.26 language settings). The config is never written.

**Not ported: every other management route.** It answers an empty 404, as upstream answers a path it has no route for, whatever the method; so does a ported path with another method. This covers, under `/v0/management` and their v8 names under `/v8/management`:

- the config: `config`, `config.yaml`, v8's `config/*path`, and each setting upstream exposes on its own (`debug`, `logging-to-file`, `logs-max-total-size-mb`, `error-logs-max-files`, `usage-statistics-enabled`, `proxy-url`, `quota-exceeded/*`, `request-log`, `ws-auth`, `request-retry`, `max-retry-credentials`, `max-retry-interval`, `force-model-prefix`, `routing/strategy`);
- the key lists (`api-keys`, and `gemini-`, `interactions-`, `claude-`, `codex-`, `xai-`, `meta-` and `vertex-api-key`, `openai-compatibility`) and the OAuth lists (`oauth-excluded-models`, `oauth-model-alias`, `oauth-request-scoped-errors`);
- the rest of `auth-files` (upload, download, delete, `status`, `fields`, `refresh`), `vertex/import` and v8's `oauth/import`;
- the logins: each provider's `*-auth-url`, v8's `oauth/auth-url`, `get-auth-status`, `oauth-session` and the OAuth callbacks;
- logs and usage: `logs`, `request-error-logs`, `request-log-by-id`, `api-key-usage`, `usage-queue`;
- quota: `quota/providers`, `quota/fetch`, `quota/reset`;
- `latest-version`, `model-definitions/:channel`, and the plugins and plugin store.

Nor are the management control panel (`/management.html`), the local management password, Home mode or the plugin host's management routes.

Deviations, each also noted in its module:

- **Paths match exactly.** While a key is set, gin redirects a ported path with a trailing slash (301 for `GET`, else 307) and matches a percent-encoded path decoded. Both get the empty 404 here, as they do on the server's other routes.
- **The config is never written.** Upstream hashes a plain `secret-key` with bcrypt when it loads the config and writes the hash back. We compare a plain key as written, in constant time and in full; upstream's bcrypt reads only the first 72 bytes, and a longer plain key fails to load.
- **The failed-attempt record is bounded.** It holds at most 4096 addresses; when it's full, the address least recently active is forgotten, which ends any ban on it early. Idle entries are purged when the record is next written, at most hourly, rather than by an hourly timer. Upstream's record has no bound.
- **`X-CPA-VERSION` is this crate's version.** `X-CPA-COMMIT` and `X-CPA-BUILD-DATE` come from `OPEN_FERRY_COMMIT` and `OPEN_FERRY_BUILD_DATE` at build time, else `none` and `unknown`. `X-CPA-SUPPORT-PLUGIN` isn't sent.
- **`auth-files` shows no quota observations.** The credential manager doesn't record them yet, so `quota` is always `{"signals":{}}` and `model_quotas` never appears. `supports_quota` and `quota_provider` come only from a `quota_probe` in a credential's metadata, since there is no plugin host.
- **`auth-files` times are in UTC.** Upstream writes some, such as file times and times read from files, in the server's time zone. One clock reading serves a whole listing, where upstream reads the clock for each credential. Unpaged, credentials whose names differ only in case keep the manager's order (by ID); upstream's sort isn't stable.
- **`api-call` never refreshes a credential.** An Antigravity, Meta or xAI credential's token is looked up as any other's. Upstream refreshes an Antigravity or xAI token about to expire, mints a Meta key from its `dca_token`, and answers `auth token refresh failed` when that fails; it also never takes an xAI credential's `id_token`.
- **`api-call` sends `User-Agent: open-ferry/<version>`** when the caller sends none, where upstream sends Go's `Go-http-client/1.1` (or `/2.0`). An empty `User-Agent` from the caller sends none, as upstream. Without an `Accept` from the caller, the HTTP client sends `Accept: */*`; upstream sends none. Otherwise the request carries exactly the caller's headers and what HTTP needs.
- **`api-call` reads at most 16 MiB of a response**, compressed or not; a larger one gives a 502 `failed to read response`. Upstream reads any size. Request bodies over 16 MiB get a 413 before they are read; upstream reads any size.
- **`api-call` uses Rust's HTTP stack.** The header map is applied in the order of its names (upstream's order is random, which matters only for names that differ in case alone). A request with a `Host` header goes over HTTP/1.1 so the header is sent as given; upstream may send it over HTTP/2 as `:authority`. A `Host` outside ASCII gives a 502, where upstream converts it to Punycode. The URL sent, a redirect's target and its `Referer` are the `url` crate's reading of the URL, which may normalize differently from Go's; a URL that the `url` crate refuses, such as one with an IPv6 zone, gives a 502. Through a forwarding proxy the request line names the `Host` header's host, as upstream's does, but as the `url` crate reads it (lowercased, without a default port), and one it refuses gives a 502. A `CONNECT` names a host and port alone, where upstream names the URL's path when it has one, and the whole URL through a forwarding proxy. A 2xx answer to `CONNECT` with a chunked body, and an HTTP/1.0 response with a `Transfer-Encoding`, give a 502; upstream reads them. `Expect: 100-continue` is sent but not waited on. SOCKS5 proxies are accepted but a call through one fails with a 502. A client is kept per proxy, at most 16, where upstream builds a connection pool per call.
- **Only Claude, Codex and OpenAI-compatible API keys have proxies in the config**, so a credential from another provider's key never finds its proxy there.
- **`reset-quota` picks the first credential by ID** when two share an index; upstream takes whichever its map yields first.
- **JSON corner cases.** A request body string holding an unpaired surrogate escape such as `\ud800` fails to decode; Go reads U+FFFD. A value from a credential's metadata is held by `serde_json`, so an integer `-0` is written back as `0`, and a number beyond `f64`'s range, which Go fails to decode, is written as it was read. `ToUpper` on a method leaves a Greek letter with a subscript iota unchanged; the method is refused either way.

## Other ported code

Some of upstream's behaviour comes from the details of Go libraries, so the parts it relies on are ported too:

- `go::base64` from Go's `encoding/base64`, and a table generated from `strconv.IsPrint` (BSD-3-Clause, [licenses/Go-LICENSE](licenses/Go-LICENSE)).
- `go::parse_float` from Go's `strconv/atof.go`, for numbers gjson reads from text, which may be hexadecimal floats or have underscores (BSD-3-Clause, [licenses/Go-LICENSE](licenses/Go-LICENSE)).
- `json::lenient` from gjson v1.18.0's `Get` and `String()`, for a custom tool call's `input` and a Claude web search's query, which upstream reads from text that may be malformed (MIT, [licenses/gjson-LICENSE](licenses/gjson-LICENSE)).
- `protowire` from protobuf-go's `encoding/protowire` v1.34.1 (BSD-3-Clause, [licenses/protobuf-go-LICENSE](licenses/protobuf-go-LICENSE)).
- `go_json` in `open-ferry-providers` from Go's `encoding/json` decoder, for a Codex ID token's claims: a repeated object merges into its struct, a slice reuses its spare elements, and invalid UTF-8 and lone surrogates read as U+FFFD (BSD-3-Clause, [licenses/Go-LICENSE](licenses/Go-LICENSE)).
- `handlers::gemini::sniff` in `open-ferry-server` from Go's `net/http/sniff.go`, for the `Content-Type` Go's server gives a raw Gemini stream that upstream leaves untyped (BSD-3-Clause, [licenses/Go-LICENSE](licenses/Go-LICENSE)).

The config loader in `open-ferry-core` reads YAML as upstream's yaml.v3 does, since that decides which settings a file holds:

- `config::yaml` and `config::decode` from yaml.v3 v3.0.1's `resolve.go`, `decode.go` and `yaml.go`: which plain scalars are numbers, booleans or timestamps, duplicate and merge keys, aliases, and its type errors (Apache-2.0, [licenses/go-yaml-LICENSE](licenses/go-yaml-LICENSE) and [licenses/go-yaml-NOTICE](licenses/go-yaml-NOTICE)).
- `config::duration` from Go's `time.ParseDuration` (BSD-3-Clause, [licenses/Go-LICENSE](licenses/Go-LICENSE)).

The OpenAI translators share helpers with upstream's other translators, ported as far as they need them:

- `apply_patch` from `internal/client/codex/apply-patch/tool.go`: Codex's custom `apply_patch` tool, carried as a function call with the arguments `{"input": "<patch>"}`.
- `responses_tools` from `internal/util/responses_tools.go`: which declaration a tool name refers to, across top-level tools, `additional_tools` and namespaces.
- `apply_patch::input` from `internal/translator/common/apply_patch_input.go` and `apply_patch_events.go`: reading the patch out of `apply_patch` function arguments as they stream in, and the custom tool events a Responses client expects for it.
- `common::openai_tools` from `internal/translator/common/openai_tools.go`: moving Chat Completions tool results to right after the assistant message that called them.
- `common::responses` from `internal/translator/common/responses.go`: pairing a Responses request's tool outputs with their calls (also used by the Claude translators); and, from the Responses response translators for Claude and Chat Completions upstreams, which request a response reads and the fields it repeats from it.
- `common::tool_names` from `internal/util/translator.go`: repairing tool call arguments written with single quotes, and giving a Claude client back a tool's name as it declared it when Chat Completions returns it in another case or spacing.

The Claude translators use more of upstream's shared code:

- `models` from `internal/registry`: the static model catalog, with each model's thinking settings and output token limit. `models/models.json` is upstream's `internal/registry/models/models.json`, copied unchanged.
- `thinking` from `internal/thinking`: thinking budgets and levels, model-name suffixes, and whether a client asked to see reasoning summaries.
- `common::cache_control` and `common::claude` from `internal/translator/common` and `internal/util`: `cache_control` markers, grouping messages into turns, structured output instructions, and tool name and ID sanitizing.
- `schema` from `internal/util/claude_schema.go`: making a tool's JSON Schema fit for Claude.

The server edits some client JSON in place, as upstream does with gjson and sjson, so that the bytes a client sent go on as they came. Its `json` module (`crates/open-ferry-server/src/json.rs`) ports the parts of gjson v1.18.0 and sjson v1.2.5 that its Responses handlers need (MIT, [licenses/gjson-LICENSE](licenses/gjson-LICENSE) and [licenses/sjson-LICENSE](licenses/sjson-LICENSE)). The WebSocket handshake is checked as gorilla/websocket v1.5.3's `Upgrader` checks it (BSD-2-Clause, [licenses/gorilla-websocket-LICENSE](licenses/gorilla-websocket-LICENSE)).

The management API reads requests and writes answers as gin v1.10.1 and Go's standard library do in upstream, so `open-ferry-management` ports the parts of them it relies on (gin: MIT, [licenses/gin-LICENSE](licenses/gin-LICENSE); Go: BSD-3-Clause, [licenses/Go-LICENSE](licenses/Go-LICENSE)):

- `client_ip` from gin's `ClientIP` and trusted proxy checks, with Go's `net.ParseIP`, `ParseCIDR` and `IP.String`: the address a request comes from, which decides whether it is local and which address a ban falls on.
- `bind` from gin's `ShouldBindJSON` over Go's `encoding/json` decoder, `json` from gin's `c.JSON` over its encoder, and `query` from gin's `c.Query` over Go's `url.ParseQuery`.
- `go_url` from Go's `url.Parse` and `netip.ParseAddr`, and `go` from `strings.EqualFold` and `ToUpper`, `strconv.ParseBool` and `Atoi`, `utf8.DecodeRune`, `textproto.CanonicalMIMEHeaderKey` and `time.Duration.String`.
- In `api_call`, what of Go's `net/http` client decides what is sent and answered: valid methods, the `Host` header and the request line, `Content-Length` and `Transfer-Encoding`, the body after a switch of protocols or a `CONNECT`, gzip, basic auth from the URL, and the redirect rules, including which headers a redirect keeps.
- In `access`, how golang.org/x/crypto v0.54.0's `bcrypt` reads a hash, which ignores whatever follows its 60 characters (BSD-3-Clause, under the same license as Go, [licenses/Go-LICENSE](licenses/Go-LICENSE)).

## Checking parity

`tools/parity` runs the same requests and event streams through upstream's Go translators and through ours, then compares the output. It covers every translator ported so far. It needs Go and a CLIProxyAPI checkout:

```sh
cargo run --release -p open-ferry-parity -- --upstream ../CLIProxyAPI
```

`--go` picks the Go toolchain. To build upstream as its releases are built, use Go 1.26 (`go install golang.org/dl/go1.26.4@latest`, then `go1.26.4 download`) and pass `--go go1.26.4`.

See [tools/parity/README.md](tools/parity/README.md).

## Deliberately not ported

- **Client impersonation:** TLS fingerprinting (uTLS), synthetic user IDs, forged client build fingerprints, and related "cloaking" code. We send each provider's documented OAuth headers and nothing that disguises the client. OpenAI-compatible upstreams get `User-Agent: open-ferry/<version>` rather than upstream's `cli-proxy-openai-compat`.
- **Providers beyond Codex, Claude and OpenAI-compatible upstreams** for now (Antigravity, Gemini, Vertex, xAI, Kimi, Meta, Devin, AI Studio relay). Their reasoning signatures are recognized, because a conversation can move between providers and each signature must be kept, dropped or replaced before it's replayed.
- **Plugin host and store**, **cluster mode** (CLIProxyAPIHome), **TUI**, **Realtime/WebRTC**, **images and video** endpoints.

## Upstream issues we intend to address

These come from CLIProxyAPI's tracker. Each is a test case for us, and a fix we'll offer back where it applies to the Go code.

| Issue | Problem |
|---|---|
| [#2596](https://github.com/router-for-me/CLIProxyAPI/issues/2596) | `previous_response_id` not chained across WebSocket turns |
| [#5413](https://github.com/router-for-me/CLIProxyAPI/issues/5413) | No WebSocket pings during long reasoning; Cloudflare drops the connection |
| [#6006](https://github.com/router-for-me/CLIProxyAPI/issues/6006) | Warmup frames forwarded upstream as real generations |
| [#5545](https://github.com/router-for-me/CLIProxyAPI/issues/5545) | Codex SSE streams die after ~30 s of upstream silence |
| [#5360](https://github.com/router-for-me/CLIProxyAPI/issues/5360) | A stalled upstream never triggers credential rotation |
| [#3783](https://github.com/router-for-me/CLIProxyAPI/issues/3783) | Concurrent token refreshes reuse the same refresh token |
| [#3200](https://github.com/router-for-me/CLIProxyAPI/issues/3200) | Built-in usage statistics were removed |
