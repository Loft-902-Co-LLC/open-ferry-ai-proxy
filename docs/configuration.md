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

These other switches' configuration contract is available for the v8.0.27 sync. Runtime behavior is wired by the corresponding provider and server changes.

The v8.0.32 contract also carries these settings for compatibility. open-ferry ignores them because it does not impersonate clients or cloak Claude requests.

| v8 path | Legacy path | Default and meaning |
|---|---|---|
| `requests.tls-handshake-timeout-seconds` | `tls-handshake-timeout-seconds` | Integer, `0`. Upstream bounds only uTLS handshakes; its standard fallback transport is unaffected. Retained but ignored here. |
| `upstream.claude.session-date-pin-days` | `claude.session-date-pin-days` | Optional integer, defaults to `7`; explicit `0` is retained. Upstream's cloaked session date retention. Ignored here. |
| `upstream.claude.header-defaults.anthropic-beta-strip` | `claude-header-defaults.anthropic-beta-strip` | String list, empty. Trimmed and deduplicated in order, with empty names removed. Ignored here. |
| `upstream.claude.header-defaults.anthropic-beta-append` | `claude-header-defaults.anthropic-beta-append` | String list, empty. Same normalization as strip. Ignored here. |
| `upstream.claude.header-defaults.inject-diagnostics` | `claude-header-defaults.inject-diagnostics` | Optional boolean, unset means `true` upstream. Explicit false is retained. Ignored here. |
| `upstream.claude.header-defaults.client-request-id` | `claude-header-defaults.client-request-id` | String, empty means `on` upstream. Trimmed and lowercased; `on`, `off`, `passthrough` accepted, unknown modes reset to empty. Ignored here. |
