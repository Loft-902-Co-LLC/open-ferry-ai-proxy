# Configuration switches

Both legacy and v8 YAML layouts are accepted. The v8 management API edits the v8 paths below; saves preserve values and comments, render nonempty collections in block style, and remove unnecessary double quotes from string mapping keys. Empty lists stay `[]`, and scalar values keep their quoting.

| v8 path | Legacy path | Default and meaning |
|---|---|---|
| `self-update.channel` | Same | String, `stable` (also when empty) or opt-in `preview`. Other values fail validation. Updates never install older versions when returning to stable. |
| `server.max-request-body-bytes` | `max-request-body-bytes` | Integer, `0`. A positive value is the most bytes a proxy request body may have, before and after decoding, and a larger one receives HTTP 413; it also bounds each Responses WebSocket message. A value <= 0 keeps open-ferry's default of 64 MiB, where upstream has no limit. |
| `routing.session-affinity-rebind-on` | Same | String, effective default `any`. `quota` preserves the binding across transient failovers. See [routing](routing.md). |
| `requests.streaming.send-keepalive-before-ft` | `streaming.send-keepalive-before-ft` | Boolean, `false`. Sends SSE heartbeats before the first token only when `keepalive-seconds` is positive. |
| `upstream.claude.tool-search` | `claude.tool-search` | Boolean, `false`. Enables Codex client `tool_search` on Claude Responses routes. Official Anthropic credentials and trusted custom endpoints use native `defer_loading` and `tool_reference`; other credentials use the plain translation. The catalog advertises search only when every credential serving a Claude model supports native search. |
| `api-keys.claude[].keys[].native-tool-search` | `claude-api-key[].native-tool-search` | Boolean, `false`. Trusts a key's custom base URL with native Anthropic search; requires the global tool-search switch. Official `api.anthropic.com` credentials do not need it. |
| `api-keys.vertex[].keys[].openapi` | `vertex-api-key[].openapi` | Optional boolean: unset selects OpenAPI automatically for non-Google publisher models; `true` forces OpenAPI, `false` keeps the native route. A model override wins; Imagen always stays native. Groups may supply inherited `models`; `openapi` and `openapi-location` belong on individual keys. |
| `api-keys.vertex[].keys[].openapi-location` | `vertex-api-key[].openapi-location` | String, default empty: override the OpenAPI resource location and Google hostname together. Without an override, a full resource URL keeps its own location; otherwise the service account location or `us-central1` applies. |
| `api-keys.vertex[].keys[].models[].openapi` | `vertex-api-key[].models[].openapi` | Optional boolean, unset inherits the credential or automatic selection. Matches upstream names and aliases, including credential prefixes and exact thinking suffixes. No per-model location setting exists. |
| `oauth.model-alias.vertex[].openapi`, `oauth.settings.vertex[].openapi` | `oauth-model-alias.vertex[].openapi`, `oauth-settings.vertex[].openapi` | Optional booleans for service-account/OAuth models; API-key credentials do not inherit them. Per-account rules take priority over global aliases, then global settings. Auth-file roots accept `openapi`, `openapi_location` or `openapi-location`; `model_aliases`, `model-aliases` and `models` accept model rule objects mixed with strings. |
| `api-keys.vertex[].keys[].interactions` | `vertex-api-key[].interactions` | Optional boolean, unset/false keeps the Gemini bridge. `true` sends Interactions requests to `/v1beta1/interactions` (streams add `?alt=sse`). Service-account credential JSON uses `interactions` at the root; opted-in accounts call `/v1beta1/projects/{project}/locations/global/interactions`, including accounts configured for a regional location, as upstream does. Explicit false remains explicit through a save. |
| `client.codex.web-search-result-cache.enabled` | Same | Boolean, `false`. Caches Claude web-search results for Responses sessions, isolated by session, caller and selected credential. Disabling the cache or changing its policy clears stored results. |
| `client.codex.web-search-result-cache.ttl` | Same | Go duration string or integer seconds; empty or zero uses `24h`. Sliding expiry renews on use. Negative or invalid durations are rejected. |
| `client.codex.web-search-result-cache.limit` | Same | Integer, `0` uses `1024` entries. Negative values are rejected. Results have no per-entry byte cap; this is an entry-count bound. |
| `api-keys.claude[].models[].thinking.disable-type` | `claude-api-key[].models[].thinking.disable-type` | String, empty uses `disabled`. Overrides the API thinking type used when thinking is off; Sonnet 5.5 uses `between_tools`. Omitted model capabilities inherit catalog settings. The field is also accepted wherever a configured model has a `thinking` mapping. |
| `oauth.providers.codex.signin-with-chatgpt-identifier` | `codex.signin-with-chatgpt-identifier` | String, empty. Accepted for configuration compatibility only: trimmed and saved; changes are reported without the value. It has no runtime effect. open-ferry always identifies itself as `open-ferry`. |

These other switches' configuration contract is available for the v8.0.38 sync. Runtime behavior is wired by the corresponding provider and server changes.

The compatibility-only `signin-with-chatgpt-identifier` has no runtime effect and warns once per load when set, without logging its value.

## Ingress limits (open-ferry additions)

`server.ingress-limits` in v8, or top-level `ingress-limits` in legacy YAML,
protects both listeners. Each field is an integer: unset or `0` keeps its
configured default; any negative value disables that guard. Positive timeout
values are seconds (at most one year). Each worker has its own accounting.

| Key | Configured default | Meaning |
|---|---:|---|
| `max-connections` | OS-derived | Per listener, including TLS and upgrades. At capacity, accepts wait in the kernel backlog; no extra socket is registered or duplicated. |
| `header-timeout-seconds` | 30 | Complete first head after accept or TLS completion. H1 keepalive starts at the last response bytes, so its idle gap is the smaller of this and the idle timeout. H2 also limits each HEADERS/CONTINUATION block. |
| `tls-handshake-timeout-seconds` | 10 | Inbound TLS, separate from the outbound compatibility setting. |
| `idle-timeout-seconds` | 120 | No accepted request or upgraded H2 task remains. An idle H2 peer gets graceful GOAWAY. |
| `body-idle-timeout-seconds` | 60 | No new upload bytes: 408 and close/drain. Empty DATA does not count as progress. |
| `body-total-timeout-seconds` | 900 | Whole upload, preventing an indefinitely trickling body. It does not limit normal response streaming or upgrades. |
| `buffered-body-bytes` | 268435456 (256 MiB) | Aggregate authenticated request storage, subject to the floor and management reserve below. |
| `write-stall-timeout-seconds` | 120 | Pending output without progress. H2 resets only a stalled response stream with CANCEL while connection-level flow-control or TCP stalls close the connection. |

On Unix, startup tries to raise the soft descriptor limit within the hard limit.
macOS clamps to `kern.maxfilesperproc`, falling back to OPEN_MAX (10240), and
retries a rejected target once at that lower ceiling. The cap reserves half the
limit (at least 256 descriptors), divides the remainder by four for two listeners
and two handles, and has a floor of max(limit / 8, 16) and a ceiling of 8192.
Windows uses an 8192-handle envelope, yielding the unchanged 1024 per listener.
A failed raise warns once with the fallback cap. Helper processes restore the
original soft limit before exec only after a successful raise. They preserve
inherited hard limits, never raise a subsequently lowered soft limit, and still
spawn if observation/restoration fails. Accepted sockets are not inherited.

Authentication runs before any body reservation or read, using the route's
existing client-key or management-key check. Missing/wrong credentials keep their
existing 401/403; keyless proxy setups still use the budget. Full request logging
also waits for a body consumer. The POST callbacks `/v0/management/oauth-callback`
and `/v8/management/oauth/callback` retain their 64 KiB bound and share a separate
process-wide 2 MiB pool (32 maximum JSON callbacks). Pool exhaustion returns 503
with Retry-After: 1. Declared lengths are refused before polling; chunked bodies
first yield a frame, dropped if its capacity cannot be reserved. GET callbacks,
including `/anthropic/callback` and `/codex/callback`, read queries only and never
poll bodies. Health, unknown
routes and OPTIONS also do not consume bodies.

The effective aggregate budget is max(configured budget, 3 * proxy body limit +
96 MiB). The 96 MiB reserve fits one maximum 32 MiB management multipart upload
(raw input plus two parser copies), even while proxy storage fills its share.
Management may use otherwise free aggregate capacity; proxy storage cannot use
that reserve. With the default 64 MiB proxy limit, the configured 256 MiB default
therefore has an effective 288 MiB floor, including 96 MiB for management. Raising
the proxy limit to 256 MiB gives an 864 MiB floor. Management retains its handler
bounds (16 MiB normally, 32 MiB for uploads) on either listener, independently of
`max-request-body-bytes`; dashboard API handlers retain their 64 KiB bound. This
keeps new proxy charges out of management headroom. After a lowering reload,
retained old proxy charges may temporarily use the new headroom until released.

Declared lengths reserve before the first body poll, including 100-continue.
Chunked input grows geometrically and charges its full Vec capacity before
allocation, with capacity capped at the upload limit. The bound excludes allocator
granularity and transient old/new Vec overlap during realloc. Exhaustion answers 503 with
Retry-After: 1. Declared oversize answers the flat pre-handler 413 after auth;
chunked oversize is replayed as a body-read error so each handler keeps its
upstream 413 shape. Recoverable H2 size/budget errors do not drain the connection.
Timeouts use the existing flat error shape and request connection closure.

Multipart reserves two extra body lengths. Raw owners and decoding allocations
stay charged while the handler uses them; temporary holds are released when it
returns, including for SSE. Bytes retained by the response itself stay charged
until their last owner disappears. Existing owned management writes retain their
allowance while finishing after a disconnect. Zstd windows over min(body limit,
8 MiB) are refused before init. Each frame uses its own window to precharge ring
growth/overlap plus 6 MiB of fixed scratch, including malformed-input allowances.
An entropy-header preflight bounds FSE probability vectors before decoding and
accepts all ruzstd sequence-count encodings, including two-byte zero. Later frames
grow that reservation before init; the decoder is destroyed before its working
charge is released, including on unwind. Output drains in fixed chunks. Parsed
objects, translations, responses, protocol buffers and log rendering are outside
this payload budget.
Request logging instead captures and renders at most a marked 64 KiB prefix;
compressed expansion over the rendering bound is omitted with a size note.
Unread bodies (including Full-mode 401/404/405 and early 400 responses) remain
empty with an incomplete-capture marker; upstream eagerly logs those bodies.

Reload updates admission immediately without eviction, and changes the shared
budget without discarding existing charges. Each upload carries its original
size/deadline snapshot through downstream readers and decoding. New connections
snapshot transport deadlines. Lowering a budget blocks growth until owners free
space. Warnings contain only static guard names and counts, rate-limited per guard.

Only pending output starts the write timer: upstream silence while an SSE
producer waits does not. Progress on one H2 stream never masks another's stall,
and queued chunks of one progressing stream share its clock. Connection-window
stalls have an independent clock. With zero connection credit, a stream's own
deadline is deferred by at most one second; other streams cannot renew it.
This can also cancel a healthy stream starved by congestion for that long.
Live producers can receive RST_STREAM/CANCEL; reset attempts keep a fixed
one-second fallback deadline that subsequent DATA progress cannot extend. If a
finished producer left final DATA queued, or reset ownership survives that grace,
the connection is cut. TCP progress is observed below TLS, including flushes that
write bytes while returning Pending.

During GOAWAY, incomplete-header deadlines remain armed. An idle peer gets five
seconds to complete the exchange. The drain also has an absolute bound of five
seconds plus the upload total deadline (or upload idle deadline, or 900 seconds
when both are disabled), even if streams continue arriving. With defaults that
bound is 905 seconds. This bound applies to idle/408 drains, not blue-green
release or stop. After a 408 on one H2 stream it cuts other streams too, including
progressing SSE. Accepted protocol task lifetimes, including H2 CONNECT,
determine activity; late DATA cannot invent an open stream.

SSE responses otherwise have no maximum duration. H1 WebSockets and H2 extended
CONNECT leave header/upload/idle timers once upgraded. They retain admission,
the TCP write guard, existing frame/message bounds (64 MiB default), a bounded
16-item input queue, existing optional ping handling, and blue-green cut. A quiet
upgrade or an H2 WebSocket blocked by its stream window can retain a slot; the
HTTP response-stream watchdog does not impose a new WebSocket ping or lifetime.

The v8.0.38 contract also carries these settings for compatibility. open-ferry ignores them because it does not impersonate clients or cloak Claude requests.

| v8 path | Legacy path | Default and meaning |
|---|---|---|
| `requests.tls-handshake-timeout-seconds` | `tls-handshake-timeout-seconds` | Integer, `0`. Upstream bounds only uTLS handshakes; its standard fallback transport is unaffected. Retained but ignored here. |
| `upstream.claude.session-date-pin-days` | `claude.session-date-pin-days` | Optional integer, defaults to `7`; explicit `0` is retained. Upstream's cloaked session date retention. Ignored here. |
| `upstream.claude.header-defaults.anthropic-beta-strip` | `claude-header-defaults.anthropic-beta-strip` | String list, empty. Trimmed and deduplicated in order, with empty names removed. Ignored here. |
| `upstream.claude.header-defaults.anthropic-beta-append` | `claude-header-defaults.anthropic-beta-append` | String list, empty. Same normalization as strip. Ignored here. |
| `upstream.claude.header-defaults.inject-diagnostics` | `claude-header-defaults.inject-diagnostics` | Optional boolean, unset means `true` upstream. Explicit false is retained. Ignored here. |
| `upstream.claude.header-defaults.client-request-id` | `claude-header-defaults.client-request-id` | String, empty means `on` upstream. Trimmed and lowercased; `on`, `off`, `passthrough` accepted, unknown modes reset to empty. Ignored here. |

`self-update.channel` selects `stable` (the default, also for empty) or `preview`; any other value fails validation. Preview considers both latest and the signed preview release and chooses the newer. The updater has no channel environment override. `open-ferry update -channel stable|preview` and the dashboard Settings tab write this setting. Updates never downgrade when leaving preview. See [updates](updates.md).
