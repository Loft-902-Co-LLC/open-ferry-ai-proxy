# Configuration switches

Both legacy and v8 YAML layouts are accepted. The v8 management API edits the v8 paths below; saves preserve values and comments, render nonempty collections in block style, and remove unnecessary double quotes from string mapping keys. Empty lists stay `[]`, and scalar values keep their quoting.

| v8 path | Legacy path | Default and meaning |
|---|---|---|
| `server.max-request-body-bytes` | `max-request-body-bytes` | Integer, `0`. A positive value is the most bytes a request body may have, before and after decoding, and a larger one receives HTTP 413; it also bounds each Responses WebSocket message. A value <= 0 keeps open-ferry's default of 64 MiB, where upstream has no limit. |
| `routing.session-affinity-rebind-on` | Same | String, effective default `any`. `quota` preserves the binding across transient failovers. See [routing](routing.md). |
| `requests.streaming.send-keepalive-before-ft` | `streaming.send-keepalive-before-ft` | Boolean, `false`. Sends SSE heartbeats before the first token only when `keepalive-seconds` is positive. |
| `upstream.claude.tool-search` | `claude.tool-search` | Boolean, `false`. Enables Codex client `tool_search` on Claude Responses routes. Official Anthropic credentials and trusted custom endpoints use native `defer_loading` and `tool_reference`; other credentials use the plain translation. The catalog advertises search only when every credential serving a Claude model supports native search. |
| `api-keys.claude[].keys[].native-tool-search` | `claude-api-key[].native-tool-search` | Boolean, `false`. Trusts a key's custom base URL with native Anthropic search; requires the global tool-search switch. Official `api.anthropic.com` credentials do not need it. |
| `api-keys.vertex[].keys[].interactions` | `vertex-api-key[].interactions` | Optional boolean, unset/false keeps the Gemini bridge. `true` sends Interactions requests to `/v1beta1/interactions` (streams add `?alt=sse`). Service-account credential JSON uses `interactions` at the root; global accounts call `/v1beta1/projects/{project}/locations/global/interactions`, while regional accounts retain the bridge. Explicit false remains explicit through a save. |

These switches' configuration contract is available for the v8.0.27 sync. Runtime behavior is wired by the corresponding provider and server changes.
