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
| `open-ferry-translate`: Responses → Chat Completions | `internal/translator/openai/openai/responses` | Request ported (`openai::responses`), for a Chat Completions upstream that is sent a Responses body. Not in `registry` yet: upstream registers it together with the response half, which is not ported |
| `open-ferry-translate`: registry | `sdk/translator` | Ported (`registry`): the built-in translators above, the fallback for pairs with none, and reasoning summary settings carried between formats. Plugin hooks and middleware are not ported |
| `open-ferry-translate`: legacy Completions | `sdk/api/handlers/openai/openai_handlers.go` | Ported (`completions`): the request, response and stream chunk conversions the `/v1/completions` handler uses. The handler itself comes with the server |
| `open-ferry-server`: routes and middleware | `internal/api/server_routes.go`, `server_middleware.go`, `internal/access`, `sdk/access` | Ported (`app`, `auth`): `/healthz`, `/v1/models`, Chat Completions, legacy Completions, Claude Messages and token counts, and Responses with compact, also under `/backend-api/codex`; client keys, safe mode and CORS. The management API and other providers' routes are not ported yet |
| `open-ferry-server`: handler plumbing | `sdk/api/handlers/handlers*.go`, `request_body.go`, `model_execution.go`, `internal/util/provider.go`, `internal/thinking/suffix.go` | Ported: reading and decoding bodies, model routing (`auto` and thinking suffixes), error bodies, bootstrap retries, stream forwarding and keep-alives. Calls go to a `Dispatcher` (`open-ferry-core::exec`) |
| `open-ferry-server`: OpenAI and Claude handlers | `sdk/api/handlers/openai/openai_handlers.go`, `sdk/api/handlers/claude/code_handlers.go` | Ported (`handlers::openai`, `handlers::claude`). Chat Completions takes Responses bodies too, through the Responses to Chat Completions translator |
| `open-ferry-server`: Responses over HTTP | `sdk/api/handlers/openai/openai_responses_handlers.go` | Ported (`handlers::responses`), except Codex multi-agent v2 tools and orphan delegation |
| `open-ferry-server`: Responses WebSocket | `sdk/api/handlers/openai/openai_responses_websocket*.go` | Ported (`handlers::responses_ws`): incremental input, transcript and tool-call repair, warm-ups answered locally, and hand-off to a credential's upstream WebSocket. Response steering and the request log's timeline are not ported |
| `open-ferry-core`: call seam | `sdk/cliproxy/executor/types.go`, `sdk/cliproxy/auth` errors | Ported (`exec`, `models`): the call the HTTP layer hands the credential manager, and the manager's errors as the handlers report them |
| `open-ferry-core`: Codex and Claude OAuth | `internal/auth/codex`, `internal/auth/claude` | Planned |
| `open-ferry-management`: `auth-files` | `internal/api/handlers/management/auth_files*.go` | Planned |
| `open-ferry-management`: `api-call` | `internal/api/handlers/management/api_tools.go` | Planned |
| `open-ferry-management`: `reset-quota` | `internal/api/server_management.go` | Planned |

### Deviations so far

Each ported file lists its deviations in its module docs. Most are byproducts of using `serde_json` rather than raw bytes:

- **Key order is kept.** Upstream round-trips tool schemas through Go maps, which sorts their keys. We keep the client's order (`serde_json` with `preserve_order`). In the Responses to Chat Completions request we keep a schema's number text too, where upstream writes each number as a `float64` (`1.50` becomes `1.5`, and integers beyond 2^53 lose precision).
- **Embedded JSON is re-serialized compactly.** Where upstream copies a client's raw JSON bytes into a string field (for example `function_call.arguments`), we write the same value as compact JSON. Number text is kept exactly (`arbitrary_precision`). In responses this affects a web search's `partial_json`. Function call arguments are passed through byte for byte. In the Responses to Chat Completions request it also affects a custom tool call's `input` that isn't a string, which upstream copies as text into the call's `{"input": ...}` arguments.
- **An empty `reasoning` object is no reasoning however it's written.** The Responses to Chat Completions translator compares the object's text with `{}`, so upstream counts `{ }` as reasoning turned on and gives tool call turns the `[reasoning unavailable]` placeholder. We don't.
- **Strings use `serde_json`'s escaping.** Where upstream writes a string with Go's JSON encoder, it escapes `<`, `>`, `&`, U+2028 and U+2029 as `\u003c` and so on. We write them as they are; the JSON values are the same.
- **Malformed JSON is not read.** A Codex `data:` line that isn't valid JSON is treated as an event with no fields, or gives no chunk in Chat Completions; gjson reads what it can from it. The same goes for a Claude `data:` line, and `serde_json` also rejects an unpaired surrogate escape such as `\ud800`, which gjson reads as U+FFFD. Nor can `serde_json` read JSON nested more than 128 levels deep. A Claude `data:` line that is valid JSON but can't be read for one of these reasons, or because it isn't UTF-8, ends a Responses stream with `response.failed` when the request declares `apply_patch`, since it could carry part of a patch; otherwise it gives nothing. Providers don't send such lines, so this only matters for corrupted streams. Likewise, a Chat Completions tool message's string content is read as JSON parts only if all of it is valid JSON, and so is a Responses tool output's string when it is checked for images to send to Chat Completions.
- **Block order instead of Go map order.** Where upstream walks a Go map, whose order is random, and the output depends on it, we go in ascending order. When a Claude stream ends with tool calls still open, the Responses translator closes them in block order.
- **Byte-length truncation keeps whole characters.** Upstream cuts names and IDs at 64 bytes and can split a UTF-8 character; we stop at the character boundary before it.
- **Duplicate object keys: the last one wins.** gjson reads the first occurrence of a key and `serde_json` keeps the last. RFC 8259 leaves this to the parser. For the same reason, a Gemini part with two `thoughtSignature` keys counts as normalized; upstream re-sanitizes it.
- **Out-of-range numbers saturate.** Where upstream converts a float such as `1e30` to an integer, Go's result depends on the CPU: amd64 gives the minimum int64, arm64 saturates. We saturate, so a huge thinking budget maps to the highest effort, and a token count of `1e400` becomes the largest int64.
- **Numbers beyond `f64` are kept as written.** Where upstream copies a value as a number, such as Chat Completions' `reasoning_effort`, Go writes `1e400` as `+Inf`, which isn't JSON. We keep the number's text. Negative zero becomes `0` there, where Go writes `-0`. Where upstream converts the value to a float first, as for a Claude request's `top_p` or a legacy Completions request's `temperature`, we leave out a value that isn't finite. Where Go's `json.Marshal` meets such a number, as in a Completions choice's `logprobs`, it fails and upstream writes the field with no value; we keep the number. A Responses result that repeats such a `temperature` or `top_p` from the request gets `null`. In the Responses to Chat Completions request, such a number in `text.format` or in a tool's parameter schema is kept as written; upstream writes `+Inf`, or fails to write the request when the number is inside an object or array.
- **No made-up user IDs.** When a Chat Completions or Responses client sends no user ID, upstream fills the Claude request's `metadata.user_id` with a hash of the conversation. We pass on only an ID the client sent, in `metadata.user_id` or `user`, and otherwise send empty `metadata` (see [Deliberately not ported](#deliberately-not-ported)).
- **Generated tool call IDs come from a keyed hasher.** A Chat Completions or Responses tool call without an ID gets one in upstream's form, `toolu_` and 24 letters and digits. Upstream draws them from the operating system's random source; we draw them from the standard library's randomly keyed hasher, so the crate needs no random-number dependency. The IDs only need to be unique within a conversation.
- **Registry response streams are objects.** Upstream's registry passes each response chunk with a `*any` that the translator fills on the first call. Ours makes a `ResponseStream` per response, which looks up the translator once and holds its state. It leaves out empty chunks, which upstream's handlers skip.
- **Only the static model catalog.** Whether a Claude model takes adaptive thinking or a token budget comes from upstream's model registry, which holds the models of configured accounts and falls back to a static catalog. Only the static catalog is ported so far; the registry comes with accounts.
- **Escaped cache breakpoint keys are removed too.** Upstream strips `prompt_cache_breakpoint` from Responses `input` only when the body holds the key unescaped. We strip it however it's written.
- **The signature sanitizer's default target falls back to the model.** `ClaudeMessagesSanitizeOptions::default()` targets `Provider::Unknown`, which falls back to the target model's provider. Upstream's zero value is an empty provider, which skips that fallback.
- **Warnings are not logged yet.** Upstream's Responses to Claude translator logs a warning when it drops input items of a type it can't convert, and when a history still breaks Claude's tool pairing rules after its repair. We drop the same items and make the same repairs, without the warnings, until there is a server to log them.
- **Protobuf errors always use a regular space.** Signature errors include protobuf-go's parse errors, which start with `proto:` and a space. protobuf-go makes that space a non-breaking one in some builds, chosen per binary so that callers don't compare error strings. We always write a regular space.
- **Not ported in `signature`:** upstream's debug logging when it sanitizes Gemini signatures. Its tests of that logging are not ported, nor are tests that read captured signature corpora, which aren't in its repository.
- **Not yet ported in `codex::openai::responses`:** the `apply_patch` bridge. Only upstream's Codex executor turns it on, so it will come with the executor.
- **The non-streaming response expects a complete final event.** Codex's `response.completed` often has an empty `output`. Upstream's executor fills it with the streamed items before calling the translator, and ours will do the same when the executor is ported.

## Other ported code

Some of upstream's behaviour comes from the details of Go libraries, so the parts it relies on are ported too:

- `go::base64` from Go's `encoding/base64`, and a table generated from `strconv.IsPrint` (BSD-3-Clause, [licenses/Go-LICENSE](licenses/Go-LICENSE)).
- `go::parse_float` from Go's `strconv/atof.go`, for numbers gjson reads from text, which may be hexadecimal floats or have underscores (BSD-3-Clause, [licenses/Go-LICENSE](licenses/Go-LICENSE)).
- `json::lenient` from gjson v1.18.0's `Get` and `String()`, for a custom tool call's `input` and a Claude web search's query, which upstream reads from text that may be malformed (MIT, [licenses/gjson-LICENSE](licenses/gjson-LICENSE)).
- `protowire` from protobuf-go's `encoding/protowire` v1.34.1 (BSD-3-Clause, [licenses/protobuf-go-LICENSE](licenses/protobuf-go-LICENSE)).

The OpenAI translators share helpers with upstream's other translators, ported as far as they need them:

- `apply_patch` from `internal/client/codex/apply-patch/tool.go`: Codex's custom `apply_patch` tool, carried as a function call with the arguments `{"input": "<patch>"}`.
- `responses_tools` from `internal/util/responses_tools.go`: which declaration a tool name refers to, across top-level tools, `additional_tools` and namespaces.
- `apply_patch::input` from `internal/translator/common/apply_patch_input.go` and `apply_patch_events.go`: reading the patch out of `apply_patch` function arguments as they stream in, and the custom tool events a Responses client expects for it.
- `common::openai_tools` from `internal/translator/common/openai_tools.go`: moving Chat Completions tool results to right after the assistant message that called them.
- `common::responses` from `internal/translator/common/responses.go`: pairing a Responses request's tool outputs with their calls (also used by the Claude translators).

The Claude translators use more of upstream's shared code:

- `models` from `internal/registry`: the static model catalog, with each model's thinking settings and output token limit. `models/models.json` is upstream's `internal/registry/models/models.json`, copied unchanged.
- `thinking` from `internal/thinking`: thinking budgets and levels, model-name suffixes, and whether a client asked to see reasoning summaries.
- `common::cache_control` and `common::claude` from `internal/translator/common` and `internal/util`: `cache_control` markers, grouping messages into turns, structured output instructions, and tool name and ID sanitizing.
- `schema` from `internal/util/claude_schema.go`: making a tool's JSON Schema fit for Claude.

The server edits some client JSON in place, as upstream does with gjson and sjson, so that the bytes a client sent go on as they came. `handlers::responses::json` and `handlers::responses_ws::json` port the parts of gjson v1.18.0 and sjson v1.2.5 they need (MIT, [licenses/gjson-LICENSE](licenses/gjson-LICENSE) and [licenses/sjson-LICENSE](licenses/sjson-LICENSE)). The WebSocket handshake is checked as gorilla/websocket v1.5.3's `Upgrader` checks it (BSD-2-Clause, [licenses/gorilla-websocket-LICENSE](licenses/gorilla-websocket-LICENSE)).

## Checking parity

`tools/parity` runs the same requests and event streams through upstream's Go translators and through ours, then compares the output. It covers every translator ported so far. It needs Go and a CLIProxyAPI checkout:

```sh
cargo run --release -p open-ferry-parity -- --upstream ../CLIProxyAPI
```

See [tools/parity/README.md](tools/parity/README.md).

## Deliberately not ported

- **Client impersonation:** TLS fingerprinting (uTLS), synthetic user IDs, forged client build fingerprints, and related "cloaking" code. We send each provider's documented OAuth headers and nothing that disguises the client.
- **Providers beyond Codex and Claude** for now (Antigravity, Gemini, Vertex, xAI, Kimi, Meta, Devin, AI Studio relay). Their reasoning signatures are recognized, because a conversation can move between providers and each signature must be kept, dropped or replaced before it's replayed.
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
