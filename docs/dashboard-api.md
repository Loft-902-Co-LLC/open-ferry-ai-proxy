# The dashboard API

The dashboard is a web app built into the binary and served at `/dashboard/`. It uses two APIs on its own origin:

- **The management API**, at `/v0/management/` (and `/v8/management/`), exactly as CLIProxyAPI has it: settings, credentials, client keys, sign-ins, logs.
- **The dashboard API**, described here, at `/open-ferry/api/v1/`, for what CLIProxyAPI has no route for: usage from the usage ledger, request-log search, and client setup. The prefix is open-ferry's own, so the management namespace stays exactly upstream's.

This document is the contract between the server and the app. A change to it is announced, and the version in the prefix changes only for a change an existing app can't take.

## Common rules

### Access

The dashboard API checks exactly what the management API checks, with the same code:

- **A management key must be set**, in the config's `management.secret-key` or in `MANAGEMENT_PASSWORD`. (A config in the legacy layout has the `management` keys under `remote-management:`, which is still read.)
- **The request carries the key**, as `Authorization: Bearer <key>` (what the app sends), a bare `Authorization: <key>`, or `X-Management-Key: <key>`.
- **Clients other than 127.0.0.1 and ::1** also need `management.allow-remote`, which a set `MANAGEMENT_PASSWORD` implies.
- **The local management password** (the command line's `-password`, or the one the TUI's standalone mode sets) is a key as well, from 127.0.0.1 and ::1 only; from anywhere else it is a wrong key. It doesn't stand in for a management key: while none is set, the answer is still `management_disabled`.
- **Five failed attempts from one address ban it for thirty minutes**, for the management API and the dashboard API alike: they share one record.

No cookies are used or set.

### Requests and responses

- Bodies are JSON (`Content-Type: application/json`), at most 64 KiB. Unknown fields in a body are refused, so a typo doesn't go unnoticed.
- Answers are JSON (`application/json; charset=utf-8`) with `Cache-Control: no-store`, except a log download, which is the log's bytes (also `no-store`).
- **Times** are RFC 3339. In answers they are UTC with milliseconds: `2026-10-05T12:34:56.789Z`. In queries any offset is taken: `2026-10-05T00:00:00Z`, `2026-10-05T02:00:00+02:00`.
- **A time range** is `from` (inclusive) and `to` (exclusive). Without `to` it ends now; without `from` it starts 24 hours before `to`.
- **Paging** uses an opaque `cursor`: pass the `next_cursor` of one page to get the next. `next_cursor` is `null` on the last page. A cursor is only good with the same filters.
- **Counts** of tokens and requests are integers. Costs are numbers (floating point), in the ledger's `currency`.

### Errors

Every error is a status and a body:

```json
{"error": "invalid_request", "message": "from must be before to"}
```

`error` is a code from the table; `message` is text for people, which may change.

| Status | `error` | When |
|---|---|---|
| 400 | `invalid_request` | A query parameter or body field is missing, malformed or out of range. The message names it. |
| 400 | `invalid_json` | The body isn't JSON, or isn't the expected object. |
| 400 | `invalid_cursor` | The `cursor` isn't one this server gave. |
| 400 | `invalid_log_file` | The log is a symbolic link or other reparse point, isn't a plain file, or has another hard link. |
| 401 | `missing_management_key` | No key was sent. |
| 401 | `invalid_management_key` | The key is wrong. Counts as a failed attempt. |
| 403 | `remote_management_disabled` | The client isn't local and remote management isn't allowed. |
| 403 | `ip_banned` | Too many failed attempts from this address. The message says how long the ban lasts. |
| 404 | `management_disabled` | No management key is set, so neither API serves anything. The management API answers an empty 404 then, or, while a local management password turns it on (until the first config reload), 403 `{"error": "remote management key not set"}`. |
| 404 | `not_found` | No such route, or no such log. |
| 405 | `method_not_allowed` | The route exists, the method doesn't. |
| 413 | `body_too_large` | The body is over 64 KiB. |
| 500 | `internal_error` | Something failed on the server; the message says what, without secrets. |
| 503 | `ledger_unavailable` | The usage ledger couldn't be opened. `GET /usage/ledger` says why. |

The management API's own errors keep upstream's shape, `{"error": "<text>"}`.

---

## Usage

Usage comes from the **usage ledger**, a SQLite file the server keeps, `open-ferry-usage.sqlite3` in the log directory (`logs` under `WRITABLE_PATH` if it's set, else the working directory's `logs` if it can be written to, else `logs` in the auth directory). It has one row for each upstream call the server makes for a client (each attempt, as the management usage queue has them), recorded while `usage-statistics-enabled` is on. It keeps no prompt or answer text, and no client key in clear.

### Filters

`GET /usage/summary`, `/usage/series` and `/usage/requests` take the same filters, all optional and combined with "and":

| Parameter | Matches |
|---|---|
| `from`, `to` | The time range, by when each call started. |
| `model` | The model sent upstream, exactly. |
| `provider` | The provider (`codex`, `claude`, `gemini`, `openai-compatible-<name>`, ...), exactly. |
| `credential` | A credential's `id`, as in `credential.id`. |
| `client_key` | A client key's `id`, as in `client_key.id`. |

and one grouping:

| Parameter | Groups by |
|---|---|
| `group_by=model` | model |
| `group_by=provider` | provider |
| `group_by=credential` | credential |
| `group_by=client_key` | client key |

### Metrics

A **metrics** object sums a set of calls:

```json
{
  "requests": 1520,
  "errors": 12,
  "input_tokens": 4810000,
  "cache_read_tokens": 3200000,
  "cache_write_tokens": 41000,
  "output_tokens": 220400,
  "reasoning_tokens": 90100,
  "total_tokens": 5030400,
  "latency_ms": {"p50": 2140, "p95": 9800, "p99": 15320},
  "ttft_ms": {"p50": 610, "p95": 2300, "p99": 4100},
  "cost": 4.1825,
  "unpriced_requests": 3
}
```

- `requests` counts calls; `errors` those that failed: the upstream answered with an error, or didn't answer.
- **Tokens** are in buckets that don't overlap, the way the usage queue's `token_breakdown` has them for every provider: `input_tokens` is all input, cache reads and writes included; `cache_read_tokens` and `cache_write_tokens` are the parts of it read from and written to the cache; `output_tokens` is all output, reasoning included; `reasoning_tokens` is the part of it that was reasoning. `total_tokens` is what the provider reported as the total, and may hold tokens in no bucket.
- **Latency** is from the call's start to its end; **TTFT** to the first token of a streamed answer. Percentiles are nearest-rank, in whole milliseconds. `ttft_ms` is `null` when no call in the set has a TTFT (no streamed answers); `latency_ms` is `null` for an empty set.
- **`cost`** is an estimate from the prices set with `PUT /usage/prices`, worked out when asked, so a price change applies to the past too (see Prices). `null` when no call in the set has a price. `unpriced_requests` counts the calls whose model has none.

### `GET /open-ferry/api/v1/usage/summary`

Totals over the range, and with `group_by` per group.

Query: the filters, `group_by`, and `limit` (groups to return, 1 to 500, default 50). Groups come by `requests`, most first.

```json
{
  "from": "2026-10-04T12:00:00.000Z",
  "to": "2026-10-05T12:00:00.000Z",
  "currency": "USD",
  "totals": { "requests": 1520, "errors": 12, "...": "a metrics object" },
  "group_by": "model",
  "groups": [
    {"key": "gpt-5.1-codex", "label": "gpt-5.1-codex", "metrics": { "...": "a metrics object" }},
    {"key": "claude-sonnet-4-5", "label": "claude-sonnet-4-5", "metrics": { "...": "..." }}
  ],
  "more_groups": false
}
```

Without `group_by`, `group_by` is `null` and `groups` is `[]`. `more_groups` says groups were left out by `limit`.

A group's `key` is what its filter takes; `label` is what to show:

| `group_by` | `key` | `label` |
|---|---|---|
| `model`, `provider` | the name | the name |
| `credential` | the credential's `id` | its label, else its `id` |
| `client_key` | the key's `id`, `""` for calls without a client key | the key masked, `""` for none |

A credential group also has `credential`, and a client-key group `client_key`, objects as in `GET /usage/requests`.

### `GET /open-ferry/api/v1/usage/series`

The metrics over time, in buckets.

Query: the filters, `group_by`, and:

| Parameter | |
|---|---|
| `bucket` | `minute`, `hour`, `day`, or `auto` (default): the smallest of those giving at most 200 buckets. |
| `utc_offset` | Minutes east of UTC, -840 to 840, default 0. Hour and day buckets start at whole hours and midnights at this offset; send the browser's offset so days are the user's. |
| `groups` | With `group_by`: the series to return, 1 to 20, default 5, the groups with the most requests. |

A range over 1,500 buckets is refused (`invalid_request`). Every bucket in the range is in the answer, in order, empty ones with zero counts and `null` percentiles and cost. The first and last buckets may be cut by the range.

```json
{
  "from": "2026-10-04T12:00:00.000Z",
  "to": "2026-10-05T12:00:00.000Z",
  "bucket": "hour",
  "bucket_seconds": 3600,
  "currency": "USD",
  "group_by": null,
  "series": [
    {
      "key": null,
      "label": null,
      "points": [
        {"start": "2026-10-04T12:00:00.000Z", "metrics": { "...": "a metrics object" }},
        {"start": "2026-10-04T13:00:00.000Z", "metrics": { "...": "a metrics object" }}
      ]
    }
  ],
  "more_groups": false
}
```

Without `group_by` there is one series, with `null` `key` and `label`. With it, one series per group, keyed and labelled as in the summary, and `more_groups` says some were left out.

### `GET /open-ferry/api/v1/usage/requests`

The calls themselves, newest first.

Query: the filters, and:

| Parameter | |
|---|---|
| `failed` | `true` for failed calls only, `false` for the rest. |
| `request_id` | The calls of one client request, by its full ID. |
| `limit` | 1 to 500, default 50. |
| `cursor` | From the previous page. |

```json
{
  "requests": [
    {
      "id": 48213,
      "time": "2026-10-05T11:58:02.114Z",
      "request_id": "0b7c3f4e-5d2a-4c1b-9e8f-1234abcd",
      "endpoint": "POST /v1/chat/completions",
      "provider": "codex",
      "model": "gpt-5.1-codex",
      "alias": "gpt-5.1-codex",
      "stream": true,
      "failed": false,
      "status": 200,
      "latency_ms": 4210,
      "ttft_ms": 640,
      "credential": {"id": "codex-user@example.com.json", "auth_index": "3", "label": "user@example.com", "auth_type": "oauth"},
      "client_key": {"id": "ck_5f0a3c19e2b7d468", "masked": "sk-...9f3k"},
      "tokens": {"input": 12400, "cache_read": 9000, "cache_write": 0, "output": 830, "reasoning": 512, "total": 13230},
      "cost": 0.0213
    }
  ],
  "next_cursor": "eyJ0IjoxNzU5NjY1NDgyMTE0LCJpIjo0ODIxM30"
}
```

- `id` is the ledger's row number. `request_id` is the client request's ID, which its request log is named by (see Request logs).
- `alias` is the model the client asked for, `model` the one sent upstream.
- `status` is the answer's status: 200 for a call that succeeded, the upstream's status for one that failed, 500 when there was none.
- `ttft_ms` is `null` when there was no streamed first token; `cost` `null` when the model has no price.
- `client_key` is `null` for a call without a client key. Its `id` is a keyed hash of the key, the same for every call with that key; `masked` shows a few characters of it. The key itself isn't kept.
- `credential` is `null` for a call that had none.

### `GET /open-ferry/api/v1/usage/ledger`

The ledger's state.

```json
{
  "available": true,
  "unavailable_reason": null,
  "recording": true,
  "usage_statistics_enabled": true,
  "file": "C:\\Users\\me\\logs\\open-ferry-usage.sqlite3",
  "size_bytes": 18350080,
  "rows": 48213,
  "oldest": "2026-07-08T09:12:44.001Z",
  "newest": "2026-10-05T11:58:02.114Z",
  "retention_days": 90,
  "max_rows": 1000000,
  "currency": "USD",
  "dropped_records": 0
}
```

- `available` is `false` when the file couldn't be opened or made; `unavailable_reason` says why, and every other usage route answers 503 `ledger_unavailable`. The other fields are then `null`, except `usage_statistics_enabled`.
- `recording` is `available` and `usage_statistics_enabled`. While `usage_statistics_enabled` is off nothing is recorded; the app may turn it on through the management API (`PUT /v0/management/usage-statistics-enabled` with `{"value": true}`).
- `oldest` and `newest` are `null` when there are no rows.
- **Rows older than `retention_days` are deleted**, and so are the oldest rows beyond `max_rows`, in the background: at start, hourly, and as rows come in. `rows` may be over `max_rows` for a moment.
- `dropped_records` counts records lost since start because the ledger fell behind; it should stay 0.

### `PATCH /open-ferry/api/v1/usage/ledger`

Changes the ledger's settings. They are kept in the ledger file, not in `config.yaml`. Each field is optional:

```json
{"retention_days": 30, "max_rows": 500000, "currency": "EUR"}
```

| Field | |
|---|---|
| `retention_days` | 1 to 3650. |
| `max_rows` | 10,000 to 10,000,000. |
| `currency` | 1 to 8 letters, shown beside costs. Nothing is converted. |

Answers the new state, as `GET /usage/ledger` does. Lowering a limit deletes rows soon after, not before the answer.

### `DELETE /open-ferry/api/v1/usage/records`

Deletes every usage row. Settings and prices stay.

```json
{"deleted": 48213}
```

### Prices

Prices are per million tokens, in the ledger's `currency`, as the user enters them. None are shipped. A call's cost is:

```
(input_tokens - cache_read_tokens - cache_write_tokens) × input
  + cache_read_tokens × (cache_read, else input)
  + cache_write_tokens × (cache_write, else input)
  + output_tokens × output
```

all over 1,000,000. A price is matched to a call by its `model` exactly.

#### `GET /open-ferry/api/v1/usage/prices`

```json
{
  "currency": "USD",
  "prices": [
    {"model": "gpt-5.1-codex", "input": 1.25, "cache_read": 0.125, "cache_write": null, "output": 10, "updated": "2026-10-01T08:00:00.000Z"}
  ],
  "unpriced_models": ["claude-sonnet-4-5"]
}
```

`prices` come by `model`. `unpriced_models` lists the models with rows in the ledger and no price, so the app can offer them.

#### `PUT /open-ferry/api/v1/usage/prices`

Sets one model's prices, replacing any it had.

```json
{"model": "gpt-5.1-codex", "input": 1.25, "cache_read": 0.125, "cache_write": null, "output": 10}
```

`model` is 1 to 256 characters. `input` and `output` are required; `cache_read` and `cache_write` may be `null` or left out, for "same as `input`". Each price is a number from 0 to 1,000,000. Answers the entry as `GET` lists it.

#### `DELETE /open-ferry/api/v1/usage/prices?model=<model>`

Removes a model's prices.

```json
{"deleted": true}
```

`deleted` is `false` when the model had none.

---

## Request logs

Request logs are the files the request log writes to the log directory: every request's while `request-log` is on, and while it is off only failed requests', as `error-*.log`. They hold what UPSTREAM.md's "Request logs" says, masked as it says. These routes serve them as they are on disk and mask nothing further.

A log's **name** is `<path>-<local time>-<short request ID>.log`, such as `v1-chat-completions-2026-10-05T115802-1234abcd.log`, with `error-` in front for an error log and `_1`, `_2`, ... before the ID's dash when a name was taken. The short request ID is the last eight characters of the request's ID.

A **log entry** describes one log:

```json
{
  "name": "v1-chat-completions-2026-10-05T115802-1234abcd.log",
  "kind": "request",
  "request_id": "1234abcd",
  "time": "2026-10-05T09:58:02.000Z",
  "size": 48120,
  "modified": "2026-10-05T09:58:06.271Z",
  "method": "POST",
  "url": "/v1/chat/completions",
  "status": 200,
  "model": "gpt-5.1-codex"
}
```

- `kind` is `request` or `error`.
- `request_id` is the short ID from the name.
- `time` is the time in the name, which is the server's local time when the log was named, in UTC; `modified` is when the file last changed.
- `method`, `url`, `status` and `model` are read from the file: the method and URL from its `=== REQUEST INFO ===`, the status from its `=== RESPONSE ===`, and the model from the first `"model"` field in its request body, else from a Gemini URL, else from its answer. Each is `null` when it isn't found in the part of the file read (see the limits below).

### `GET /open-ferry/api/v1/request-logs`

Searches the logs, newest first.

| Parameter | Matches |
|---|---|
| `from`, `to` | The time in the log's name. Without either, all times. |
| `kind` | `request`, `error`, or `all` (default). |
| `path` | Logs whose URL contains this, ignoring case. |
| `status` | An exact status (`502`), or a class (`4xx`, `5xx`). |
| `model` | Logs naming a model that contains this, ignoring case, in a `"model"` field or Gemini URL. |
| `q` | Logs containing this text, ignoring ASCII case. |
| `limit` | Entries to return, 1 to 200, default 50. |
| `cursor` | From the previous page. |

```json
{
  "logs": [ { "name": "v1-chat-completions-2026-10-05T115802-1234abcd.log", "kind": "request", "...": "a log entry" } ],
  "next_cursor": "djEtY2hhdC1jb21wbGV0aW9ucy0yMDI2",
  "scanned": {"files": 214, "bytes": 18874368, "limit_reached": false},
  "request_log": true
}
```

**Searches are bounded.** Filtering by `from`, `to` and `kind` reads only names. Each search then opens at most 2,000 files and reads at most 64 MiB, and of a file over 1 MiB only its first and last 512 KiB, which is where the request's head and the answer's status are; `path`, `status`, `model` and `q` match only what was read. When a search stops at its limit before it has `limit` entries, `scanned.limit_reached` is `true` and `next_cursor` continues from the last file read, so "load more" searches on. At most 100,000 names are listed per search; beyond that the newest by name aren't certain to be found.

`request_log` is the config's `request-log`, so the app can say why there are only error logs.

### `GET /open-ferry/api/v1/request-logs/{name}`

Reads one log, in pieces.

| Parameter | |
|---|---|
| `offset` | The byte to start at, default 0. |
| `length` | Bytes to read, 1 to 4,194,304 (4 MiB), default 1,048,576 (1 MiB). |

```json
{
  "log": { "name": "v1-chat-completions-2026-10-05T115802-1234abcd.log", "...": "a log entry" },
  "offset": 0,
  "next_offset": null,
  "content": "=== REQUEST INFO ===\nVersion: 0.1.0\nURL: /v1/chat/completions\n..."
}
```

- `content` is the bytes as text. A piece ends before a character it would cut, so it may be up to three bytes shorter than `length`; bytes that aren't UTF-8 are shown as U+FFFD. For the exact bytes, download the log (below).
- `next_offset` is where the next piece starts, `null` at the end of the file.
- `name` must be a log's name as listed: no `/` or `\`, and named as a request or error log is. Other files of the log directory, such as `main.log`, aren't served (`not_found`).
- A log that is a symbolic link or other reparse point, isn't a plain file, or has another hard link answers 400 `invalid_log_file`, as the management API's log routes do.

### `GET /open-ferry/api/v1/request-logs/{name}/download`

Sends one log whole, byte for byte, as a file to save. Logs hold raw request and answer bodies, which may be binary, such as an image endpoint's multipart upload, so a download built from the pieces of `GET /request-logs/{name}` wouldn't be exact.

- **`name` is checked as for `GET /request-logs/{name}`:** it must be a log's name as listed, with no `/` or `\`, else 404 `not_found`; a log that is a symbolic link or other reparse point, isn't a plain file, or has another hard link answers 400 `invalid_log_file`. Access is checked as for every route, and every error is the usual JSON.
- **The answer is 200 with the file's bytes, exactly as on disk**, streamed: the server never holds the whole file in memory. Its headers:
  - `Content-Type: application/octet-stream`
  - `Content-Disposition: attachment; filename="<name>"`
  - `Content-Length`, the file's size when it was opened
  - `Cache-Control: no-store`
  - the dashboard's security headers (see "Serving the app").
- **The file is checked and read through one open handle**, so it can't be swapped for another file or a link between the check and the read. The first `Content-Length` bytes are sent; bytes added after the file was opened aren't. If the file gets shorter while it is sent, the connection is closed short of `Content-Length`, so the download fails rather than ending as a shorter file.
- The request log never writes a name a quoted header can't hold. Another file in the log directory might have one: each character of it that isn't printable ASCII, and each `"`, is then `_` in `filename`, and the exact name is in `filename*` (RFC 6266).

Scripts can also use the management API's `GET /v0/management/request-log-by-id/{id}` (any log, by request ID) and `GET /v0/management/request-error-logs/{name}` (error logs), which send a log as a file too.

---

## Client setup

### `GET /open-ferry/api/v1/client-setup`

What the app needs to write ready-made client configs, other than client keys, which come from the management API's `GET /v0/management/api-keys`.

```json
{
  "base_urls": [
    {"url": "http://127.0.0.1:8317", "source": "listen"},
    {"url": "http://localhost:8317", "source": "listen"},
    {"url": "https://proxy.example.com", "source": "config"}
  ],
  "tls": false,
  "safe_mode": false,
  "routes": [
    {"id": "claude-messages", "protocol": "claude", "method": "POST", "path": "/v1/messages", "base_path": "", "models": ["claude-sonnet-4-5", "gpt-5.1-codex"]},
    {"id": "codex-responses", "protocol": "codex", "method": "POST", "path": "/backend-api/codex/responses", "base_path": "/backend-api/codex", "models": ["claude-sonnet-4-5", "gpt-5.1-codex"]},
    {"id": "gemini-generate-content", "protocol": "gemini", "method": "POST", "path": "/v1beta/models/{model}:generateContent", "base_path": "", "models": ["claude-sonnet-4-5", "gpt-5.1-codex"]},
    {"id": "openai-chat-completions", "protocol": "openai", "method": "POST", "path": "/v1/chat/completions", "base_path": "/v1", "models": ["claude-sonnet-4-5", "gpt-5.1-codex"]},
    {"id": "openai-responses", "protocol": "openai-responses", "method": "POST", "path": "/v1/responses", "base_path": "/v1", "models": ["claude-sonnet-4-5", "gpt-5.1-codex"]}
  ],
  "models": [
    {"id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5", "owned_by": "anthropic", "providers": ["claude"], "context_length": 200000, "max_output_tokens": 64000},
    {"id": "gpt-5.1-codex", "display_name": "GPT 5.1 Codex", "owned_by": "openai", "providers": ["codex"], "context_length": 400000, "max_output_tokens": 128000}
  ]
}
```

- **`base_urls`** are the server's root as it sees itself, without a path; a client's base URL is one of them followed by its route's `base_path`. Those with `source` `listen` come from the config's `host`, `port` and `tls`: an empty `host`, `0.0.0.0` or `::` gives the loopback addresses and `localhost`, since the server can't know which of its other addresses a client reaches. The one with `source` `config` is `management.base-url`, when set, without any credentials, query or fragment in it. The app also knows its own origin, which may be another (a proxy in front).
- **`tls`** is the config's `tls.enable`.
- **`safe_mode`** is `true` while `api-keys` holds CLIProxyAPI's example keys and the proxy routes refuse service; client configs won't work until they are changed.
- **`routes`** are the proxy's entry points, each with the models a call to it can use right now: the models with a credential that can serve them, less those the route can't reach (the models only the image endpoints serve are on none of these), in `id` order. The proxy translates between formats, so today every other model is on every one of these routes; the lists are per route so that needn't stay true. `base_path` is what an SDK for that `protocol` takes after the root: the OpenAI SDK `/v1`, the Anthropic and Google Gen AI SDKs nothing.
- **`models`** describes each model on any route, by `id`: `display_name` (else the `id`), `owned_by`, the `providers` serving it, in order of preference, and `context_length` and `max_output_tokens` (`null` when unknown).

---

## Serving the app

Not routes the app calls, but what it can count on:

- **The app is at `/dashboard/`.** `/dashboard` redirects there. A path below it that isn't a file of the app answers `index.html`, for client-side routing, except below `/dashboard/assets/`, where a missing file is a 404.
- **Caching:** files under `/dashboard/assets/` are named by their content and get `Cache-Control: public, max-age=31536000, immutable`; `index.html` and every other file get `Cache-Control: no-cache`.
- **`GET /management.html` redirects** (302) to `/dashboard/` with the same query. CLIProxyAPI's safe-mode message sends users to `/management.html?safe-mode=configure`, so the app opens its API-key setup when it is loaded with `safe-mode=configure`.
- **Both answer an empty 404** while `management.disable-control-panel` is set.
- **A client the management API refuses for its address** (not local, while remote management isn't allowed, or banned) gets the same answer here: 403 with `{"error": "<upstream's text>"}`. The app itself needs no key; its API calls do.
- **The app is served while no management key is set**, so it can say how to set one; its API calls then answer 404 `management_disabled`.
- **Every answer from these paths and from the dashboard API carries** the headers below. (A CORS preflight, `OPTIONS`, is answered 204 by the server's CORS handling before it reaches these paths, as for every path.)
  - `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'`
  - `X-Content-Type-Options: nosniff`
  - `Referrer-Policy: no-referrer`
  - `X-Frame-Options: DENY`
- **A binary built without the app** (no `dashboard/dist` at build time) serves a short page at `/dashboard/` saying so and how to build it. The dashboard API works either way.
