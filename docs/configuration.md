# Configuration switches

Both legacy and v8 YAML layouts are accepted. The v8 management API edits the v8 paths below; saves preserve values and comments, render nonempty collections in block style, and remove unnecessary double quotes from string mapping keys. Empty lists stay `[]`, and scalar values keep their quoting.

| v8 path | Legacy path | Default and meaning |
|---|---|---|
| `server.max-request-body-bytes` | `max-request-body-bytes` | Integer, `0`. A positive value is the most bytes a request body may have, before and after decoding, and a larger one receives HTTP 413; it also bounds each Responses WebSocket message. A value <= 0 keeps open-ferry's default of 64 MiB, where upstream has no limit. |
| `routing.session-affinity-rebind-on` | Same | String, effective default `any`. `quota` preserves the binding across transient failovers. See [routing](routing.md). |
| `requests.streaming.send-keepalive-before-ft` | `streaming.send-keepalive-before-ft` | Boolean, `false`. Sends SSE heartbeats before the first token only when `keepalive-seconds` is positive. |
| `upstream.claude.tool-search` | `claude.tool-search` | Boolean, `false`. Enables Codex client `tool_search` on Claude Responses routes. Official Anthropic credentials and trusted custom endpoints use native `defer_loading` and `tool_reference`; other credentials use the plain translation. The catalog advertises search only when every credential serving a Claude model supports native search. |
| `api-keys.claude[].keys[].native-tool-search` | `claude-api-key[].native-tool-search` | Boolean, `false`. Trusts a key's custom base URL with native Anthropic search; requires the global tool-search switch. Official `api.anthropic.com` credentials do not need it. |
| `api-keys.vertex[].keys[].interactions` | `vertex-api-key[].interactions` | Optional boolean, unset/false keeps the Gemini bridge. `true` sends Interactions requests to `/v1beta1/interactions` (streams add `?alt=sse`). Service-account credential JSON uses `interactions` at the root; opted-in accounts call `/v1beta1/projects/{project}/locations/global/interactions`, including accounts configured for a regional location, as upstream does. Explicit false remains explicit through a save. |
| `oauth.providers.codex.signin-with-chatgpt-identifier` | `codex.signin-with-chatgpt-identifier` | String, empty. Accepted for configuration compatibility only: trimmed and saved; changes are reported without the value. It has no runtime effect. open-ferry always identifies itself as `open-ferry`. |

The compatibility-only `signin-with-chatgpt-identifier` has no runtime effect and warns once per load when set, without logging its value.

## Ingress limits (open-ferry additions)

`server.ingress-limits` in v8, or top-level `ingress-limits` in legacy YAML,
protects both the proxy and `management.separate-address`. Their defaults are the
same; the buffered-byte budget is shared. Each field is an integer. Unset or `0`
keeps its default, and `-1` (any negative value) disables that guard. Positive
durations are seconds, capped at one year. These are additions to open-ferry;
CLIProxyAPI's main Go HTTP server sets no such timeouts.

| Field | Default | Behavior |
|---|---|---|
| `max-connections` | OS-derived; 1024 on Windows | Concurrent accepted sockets per listener, including TLS handshakes and upgrades. At capacity, accepts wait and the kernel backlog queues clients. No excess socket is registered or duplicated. Capacity recovers when an IO owner closes. |
| `header-timeout-seconds` | 30 | A complete first head is due after accept, or after TLS succeeds. HTTP/1 keep-alive counts from the preceding response's last bytes; therefore its effective idle wait is the smaller of this and `idle-timeout-seconds`. HTTP/2 also bounds each HEADERS/CONTINUATION block without resetting on fragments. |
| `tls-handshake-timeout-seconds` | 10 | Inbound TLS only, independent of the compatibility-only outbound `requests.tls-handshake-timeout-seconds`. |
| `idle-timeout-seconds` | 120 | No request/response in flight. HTTP/2 with no open streams sends graceful GOAWAY; CONNECT remains open until both stream halves finish. |
| `body-idle-timeout-seconds` | 60 | A body with no new bytes gets 408 and closes. Empty data frames do not count as progress. |
| `body-total-timeout-seconds` | 900 | Maximum whole upload time. This bounds clients trickling just faster than the progress timeout; it never limits the subsequent response or an upgraded stream. |
| `buffered-body-bytes` | 268435456 (256 MiB) | Raw and decoded request-body storage across both listeners. A positive budget is at least the effective per-request body limit (64 MiB by default). |
| `write-stall-timeout-seconds` | 120 | Output bytes waiting without progress, including HTTP/2 flow-control stalls. A producer waiting on its upstream has no pending output and no write-stall timer. |

On Unix, startup tries to raise `RLIMIT_NOFILE`'s soft limit to its hard limit.
From the resulting soft limit, half (at least 256 descriptors) is reserved for
files, credentials and upstream sockets. The remainder is divided by four: two
listeners and two handles per accepted connection. The result is at least one
and at most 8192 per listener. Windows has no corresponding Winsock process
descriptor limit; the fallback handle envelope is 8192, giving 1024 per listener.
An explicit positive `max-connections` overrides this calculation.

Bodies reserve declared `Content-Length` before reading, including before an
`Expect: 100-continue` body. Chunked bodies reserve as bytes arrive. Exhaustion
returns 503 with `Retry-After: 1`. Upload timeouts return 408. The JSON shape is
the server's pre-handler error shape, `{"error":"message"}`. HTTP/1 responses
close the connection; HTTP/2 starts GOAWAY, allowing other already-open streams
to finish. Reservations follow byte owners through handlers, decoding, logging
and logging replay, and are released on errors, disconnect, cancellation and
unwind. Logging shares admitted bytes rather than copying them. Parsed JSON,
translations, response bodies and protocol buffers are not part of this raw and
decoded request-body budget. Raw oversized bodies are rejected before routing
or logging, using the same pre-handler 413 shape as declared oversized bodies.
Multipart uploads also reserve one additional raw body's worth for parser
copies, before any declared body is read. Admission therefore requires room for
both raw input and multipart copies. A reservation is retained through the
response so extractors that copy input cannot release accounting prematurely;
log-owned byte clones may keep it longer while queued for writing.

Reload changes admission capacity immediately, without evicting existing
connections; new connections snapshot the new transport deadlines. Each upload
snapshots its body deadlines and per-request bound. Budget changes affect all
subsequent reservations, including growth of an existing chunked/decoded body.
Existing bytes remain charged; reducing the cap below current usage blocks
growth/admission until enough owners release their bytes. A reload never creates
a fresh accounting pool. Both listeners share rate-limited warnings, with guard
names and counts, never request data, headers or keys.

SSE responses may run indefinitely, including silence while awaiting upstream
data, while they remain in flight. WebSockets over HTTP/1 upgrade and HTTP/2
extended CONNECT have no header, upload or idle cutoff once upgraded. Their
sockets still occupy admission capacity, their existing per-message body bound
still applies, stalled writes are bounded, and blue-green `cut` still closes
them. The existing Responses keep-alive/ping behavior is unchanged; no new ping
or mandatory maximum WebSocket lifetime is introduced. A quiet upgraded socket
may therefore occupy one admission slot until the peer leaves or the service
cuts it.

These other switches' configuration contract is available for the v8.0.32 sync. Runtime behavior is wired by the corresponding provider and server changes.

The v8.0.32 contract also carries these settings for compatibility. open-ferry ignores them because it does not impersonate clients or cloak Claude requests.

| v8 path | Legacy path | Default and meaning |
|---|---|---|
| `requests.tls-handshake-timeout-seconds` | `tls-handshake-timeout-seconds` | Integer, `0`. Upstream bounds only uTLS handshakes; its standard fallback transport is unaffected. Retained but ignored here. |
| `upstream.claude.session-date-pin-days` | `claude.session-date-pin-days` | Optional integer, defaults to `7`; explicit `0` is retained. Upstream's cloaked session date retention. Ignored here. |
| `upstream.claude.header-defaults.anthropic-beta-strip` | `claude-header-defaults.anthropic-beta-strip` | String list, empty. Trimmed and deduplicated in order, with empty names removed. Ignored here. |
| `upstream.claude.header-defaults.anthropic-beta-append` | `claude-header-defaults.anthropic-beta-append` | String list, empty. Same normalization as strip. Ignored here. |
| `upstream.claude.header-defaults.inject-diagnostics` | `claude-header-defaults.inject-diagnostics` | Optional boolean, unset means `true` upstream. Explicit false is retained. Ignored here. |
| `upstream.claude.header-defaults.client-request-id` | `claude-header-defaults.client-request-id` | String, empty means `on` upstream. Trimmed and lowercased; `on`, `off`, `passthrough` accepted, unknown modes reset to empty. Ignored here. |
