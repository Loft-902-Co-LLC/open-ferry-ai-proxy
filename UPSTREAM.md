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
| `open-ferry-translate`: Chat Completions → Codex | `internal/translator/codex/openai/chat-completions` | Planned |
| `open-ferry-translate`: Responses → Codex | `internal/translator/codex/openai/responses` | Request and response ported (`codex::openai::responses`), except the `apply_patch` bridge |
| `open-ferry-translate`: Chat Completions → Claude | `internal/translator/claude/openai/chat-completions` | Planned |
| `open-ferry-translate`: Responses → Claude | `internal/translator/claude/openai/responses` | Planned |
| `open-ferry-translate`: registry | `sdk/translator` | Planned |
| `open-ferry-server`: Responses WebSocket | `sdk/api/handlers/openai/openai_responses_websocket*.go` | Planned |
| `open-ferry-core`: Codex and Claude OAuth | `internal/auth/codex`, `internal/auth/claude` | Planned |
| `open-ferry-management`: `auth-files` | `internal/api/handlers/management/auth_files*.go` | Planned |
| `open-ferry-management`: `api-call` | `internal/api/handlers/management/api_tools.go` | Planned |
| `open-ferry-management`: `reset-quota` | `internal/api/server_management.go` | Planned |

### Deviations so far

Each ported file lists its deviations in its module docs. Most are byproducts of using `serde_json` rather than raw bytes:

- **Key order is kept.** Upstream round-trips tool schemas through Go maps, which sorts their keys. We keep the client's order (`serde_json` with `preserve_order`).
- **Embedded JSON is re-serialized compactly.** Where upstream copies a client's raw JSON bytes into a string field (for example `function_call.arguments`), we write the same value as compact JSON. Number text is kept exactly (`arbitrary_precision`). In responses this affects a web search's `partial_json`, where Go's encoder also escapes `<`, `>` and `&`. Function call arguments are passed through byte for byte.
- **Malformed events carry no fields.** A Codex `data:` line that isn't valid JSON is treated as an event with no fields; gjson reads what it can from it. Codex sends valid JSON, so this only matters for corrupted streams.
- **Byte-length truncation keeps whole characters.** Upstream cuts names and IDs at 64 bytes and can split a UTF-8 character; we stop at the character boundary before it.
- **Duplicate object keys: the last one wins.** gjson reads the first occurrence of a key and `serde_json` keeps the last. RFC 8259 leaves this to the parser. For the same reason, a Gemini part with two `thoughtSignature` keys counts as normalized; upstream re-sanitizes it.
- **Out-of-range numbers saturate.** Where upstream converts a float such as `1e30` to an integer, Go's result depends on the CPU: amd64 gives the minimum int64, arm64 saturates. We saturate, so a huge thinking budget maps to the highest effort.
- **Escaped cache breakpoint keys are removed too.** Upstream strips `prompt_cache_breakpoint` from Responses `input` only when the body holds the key unescaped. We strip it however it's written.
- **The signature sanitizer's default target falls back to the model.** `ClaudeMessagesSanitizeOptions::default()` targets `Provider::Unknown`, which falls back to the target model's provider. Upstream's zero value is an empty provider, which skips that fallback.
- **Protobuf errors always use a regular space.** Signature errors include protobuf-go's parse errors, which start with `proto:` and a space. protobuf-go makes that space a non-breaking one in some builds, chosen per binary so that callers don't compare error strings. We always write a regular space.
- **Not ported in `signature`:** upstream's debug logging when it sanitizes Gemini signatures. Its tests of that logging are not ported, nor are tests that read captured signature corpora, which aren't in its repository.
- **Not yet ported in `codex::openai::responses`:** the `apply_patch` bridge. Only upstream's Codex executor turns it on, so it will come with the executor.
- **The non-streaming response expects a complete final event.** Codex's `response.completed` often has an empty `output`. Upstream's executor fills it with the streamed items before calling the translator, and ours will do the same when the executor is ported.

## Other ported code

Signature checks depend on details of two Go libraries, so the parts used are ported too:

- `go::base64` from Go's `encoding/base64`, and a table generated from `strconv.IsPrint` (BSD-3-Clause, [licenses/Go-LICENSE](licenses/Go-LICENSE)).
- `protowire` from protobuf-go's `encoding/protowire` v1.34.1 (BSD-3-Clause, [licenses/protobuf-go-LICENSE](licenses/protobuf-go-LICENSE)).

## Checking parity

`tools/parity` runs the same requests and Codex event streams through upstream's Go translators and through ours, then compares the output. It covers every translator ported so far. It needs Go and a CLIProxyAPI checkout:

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
