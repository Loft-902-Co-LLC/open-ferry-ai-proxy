# The dashboard API

The dashboard is a web app built into the binary and served at `/dashboard/`. It uses two APIs on its own origin:

- **The management API**, at `/v0/management/` (and `/v8/management/`), exactly as CLIProxyAPI has it: settings, credentials, client keys, sign-ins, logs.
- **The dashboard API**, described here, at `/open-ferry/api/v1/`, for what CLIProxyAPI has no route for: usage from the usage ledger, request-log search, client setup, the config's `claude-cli` entries, with their state and whether each is signed in, undoing the last config change, and open-ferry's own updates. The prefix is open-ferry's own, so the management namespace stays exactly upstream's.

By default all of these are served on the proxy's port, beside the proxy's routes. With open-ferry's `management.separate-address` set, such as `127.0.0.1:8318`, the app, the management API and the dashboard API are served at that address alone, so they still share one origin, and the proxy's port has none of them (see [Serving the app](#serving-the-app)).

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
| 404 | `not_found` | No such route, no such log, or no such `claude-cli` entry. On the proxy's port while `management.separate-address` is set, every path answers it, whatever the method, as the dashboard API is served only at that address. |
| 405 | `method_not_allowed` | The route exists, the method doesn't. |
| 409 | `no_backup` | `POST /config/undo`: there is no backup of the config file to undo to. |
| 409 | `changed_since` | `POST /config/undo`: the config file was changed since the last write that kept a backup (as by a hand edit), so the undo would lose that change too, and the body didn't send `force`. |
| 409 | `config_changed` | `POST /config/undo`: the config file or its backup isn't the one the body's `config_sha256` or `backup_sha256` names. |
| 409 | `updates_off` | `POST /update/check` while updates are off. |
| 409 | `already_preparing` | An install or rollback preparation is already running in this worker. |
| 409 | `switch_uncertain` | A previous request may have been accepted; read fresh supervisor status before explicitly retrying. |
| 409 | `switch_refused` | The supervisor refused the switch before acceptance; its reason is in `message`. |
| 409 | `switch_failed` | Preparation failed without a more specific code, the endpoint was untrusted or unreachable before sending, automatic updates were off, or the release was skipped. |
| 409 | `not_supervised` | This install has no supervisor; `message` gives command-line update and restart instructions. |
| 409 | `update_busy` | An update check or mutation holds the update lock; try again shortly. |
| 409 | `switching_disabled` | Switching is disabled; the request was not sent. The reason is fixed and omits policy paths. |
| 409 | `rollback_target_changed` | The confirmed rollback target changed or disappeared; read status and confirm again. Nothing was sent. |
| 409 | `already_latest` | The latest release is already running, or the running version is newer. Nothing was sent; show this as information. |
| 413 | `body_too_large` | The body is over 64 KiB. |
| 500 | `internal_error` | Something failed on the server; the message says what, without secrets. |
| 502 | `claude_cli_failed` | A `claude-cli` entry's Claude Code couldn't be run, or its answer wasn't what was expected. |
| 503 | `config_writer_unavailable` | `POST /config/undo`: the server can't write its config file. |
| 503 | `ledger_unavailable` | The usage ledger couldn't be opened. `GET /usage/ledger` says why. |
| 503 | `updates_unavailable` | The server runs no update checks, as the server `open-ferry -tui` starts doesn't. |
| 504 | `claude_cli_timeout` | A `claude-cli` entry's Claude Code didn't answer within 30 seconds. |

The management API's own errors keep upstream's shape, `{"error": "<text>"}`, but for one of open-ferry's: a save through `/v0/management` (a setting, a list, or a config API key's status) made after the config file changed on disk since the server loaded it, as by `open-ferry config` or a hand edit, answers 409 `{"error": "config_changed", "message": "..."}` and writes nothing. The server loads the file again, so the same save made again keeps that change. A file it can't load, such as an empty one or one with a YAML error, is left out, so the server and the management API keep the config they had, and each such save answers 409, its message saying to fix the file, until it is fixed.

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
      "cost": 0.0213,
      "access_provider": "config-inline",
      "is_native_key": true
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
- `access_provider` names the client authentication provider; `is_native_key` says whether the client key is native to the proxy. Both are `null` when unknown (rows from before this version or written by an older binary). New anonymous rows use `""` and `false`.

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
  "separate_management": false,
  "routes": [
    {"id": "claude-messages", "protocol": "claude", "method": "POST", "path": "/v1/messages", "base_path": "", "models": ["claude-sonnet-4-5", "gpt-5.1-codex"]},
    {"id": "codex-responses", "protocol": "codex", "method": "POST", "path": "/backend-api/codex/responses", "base_path": "/backend-api/codex", "models": ["claude-sonnet-4-5", "gpt-5.1-codex"]},
    {"id": "gemini-generate-content", "protocol": "gemini", "method": "POST", "path": "/v1beta/models/{model}:generateContent", "base_path": "", "models": ["claude-sonnet-4-5", "gpt-5.1-codex"]},
    {"id": "openai-chat-completions", "protocol": "openai", "method": "POST", "path": "/v1/chat/completions", "base_path": "/v1", "models": ["claude-sonnet-4-5", "gpt-5.1-codex"]},
    {"id": "openai-responses", "protocol": "openai-responses", "method": "POST", "path": "/v1/responses", "base_path": "/v1", "models": ["claude-sonnet-4-5", "gpt-5.1-codex"]}
  ],
  "models": [
    {"id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5", "owned_by": "anthropic", "providers": ["claude"], "created": 1759104000, "chat": true, "context_length": 200000, "max_output_tokens": 64000},
    {"id": "gpt-5.1-codex", "display_name": "GPT 5.1 Codex", "owned_by": "openai", "providers": ["codex"], "created": 1762992000, "chat": true, "context_length": 400000, "max_output_tokens": 128000}
  ]
}
```

- **`base_urls`** are the server's root as it sees itself, without a path; a client's base URL is one of them followed by its route's `base_path`. Those with `source` `listen` come from the config's `host`, `port` and `tls`: an empty `host`, `0.0.0.0` or `::` gives the loopback addresses and `localhost`, since the server can't know which of its other addresses a client reaches. The one with `source` `config` is `management.base-url`, when set, without any credentials, query or fragment in it, and only while `separate_management` is `false`: otherwise it is the management address's URL, which doesn't serve the proxy. The app also knows its own origin, which may be another (a proxy in front).
- **`tls`** is the config's `tls.enable`.
- **`safe_mode`** is `true` while `api-keys` holds CLIProxyAPI's example keys and the proxy routes refuse service; client configs won't work until they are changed.
- **`separate_management`** is `true` while the app is served at `management.separate-address`, apart from the proxy. The app's own origin is then the management address, which answers the proxy's routes with 404, so it isn't a base URL for a client: only `base_urls` are.
- **`routes`** are the proxy's entry points, each with the models a call to it can use right now: the models with a credential that can serve them, less those the route can't reach (the models only the image endpoints serve are on none of these), in `id` order. The proxy translates between formats, so today every other model is on every one of these routes; the lists are per route so that needn't stay true. `base_path` is what an SDK for that `protocol` takes after the root: the OpenAI SDK `/v1`, the Anthropic and Google Gen AI SDKs nothing.
- **`models`** describes each model on any route, by `id`: `display_name` (else the `id`), `owned_by`, the `providers` serving it, in order of preference, `created`, `chat`, and `context_length` and `max_output_tokens` (`null` when unknown).
  - **`created`** is when the model came out, in Unix seconds, as the model catalog has it; `null` when the catalog doesn't say, and for a model defined in the config (its `models` lists and `openai-compatibility`), whose registry entry has the time the config was loaded instead.
  - **`chat`** is `false` for a model that isn't a chat model, as far as its details tell: an image, video or speech model (by the list Codex clients hide them by, or one configured with `image: true`), or one whose details list output other than text (Gemini's image models, Imagen) or no `generateContent` among its methods (the embedding models).
  - The app suggests a model from these: for each setup, of the chat models on its route, the latest `created` (a `null` counts as oldest, and of equal dates the first in `id` order wins). Claude Code is offered the newest of Anthropic's models, Codex the newest of OpenAI's, when the route has one; a model counts as theirs by `owned_by`, or by a name starting `claude` or `gpt` after any prefix.

---

## claude-cli

### `GET /open-ferry/api/v1/claude-cli/entries`

The config's `claude-cli` entries, in the config's order, each with the credential the server made of it and how that is doing. The management API's `auth-files` list leaves out credentials made from the config, as CLIProxyAPI's does, so this is where the app finds an entry's state, cooldowns and quota. It runs nothing and reads no file.

```json
{
  "entries": [
    {
      "name": "claude-max-1",
      "prefix": "max1",
      "config_dir": "~/.claude-second",
      "disabled": false,
      "credential": {
        "id": "claude-cli:479b4a4c3660",
        "auth_index": "3734a62b508f0029",
        "provider": "claude-cli",
        "label": "claude-max-1",
        "status": "error",
        "status_message": "unauthorized",
        "unavailable": true,
        "next_retry_after": "2026-10-05T12:44:56.789012345Z",
        "cooldowns": [
          {"scope": "credential", "reason": "unauthorized", "retry_at": "2026-10-05T12:44:56.789012345Z", "remaining_seconds": 600, "http_status": 401}
        ],
        "quota": {
          "observed_at": "2026-10-05T12:30:01.5Z",
          "signals": {"Anthropic-Ratelimit-Unified-5h-Utilization": "0.4", "Anthropic-Ratelimit-Unified-5h-Reset": "1791212400"}
        },
        "success": 3,
        "failed": 1
      },
      "last_error": {"message": "Not signed in to Claude Code", "http_status": 401}
    },
    {"name": "spare", "prefix": "", "config_dir": "", "disabled": true, "credential": null, "last_error": null}
  ]
}
```

- **`name`, `prefix` and `config_dir`** are the entry's, trimmed, as the config has them: `config_dir` is neither expanded nor resolved, and is empty for an entry without one, whose Claude Code uses the `CLAUDE_CONFIG_DIR` open-ferry runs with, else its own default. **`disabled`** is the entry's switch.
- **`credential`** is the entry's credential as `GET /v0/management/auth-files` lists a credential file, with all its fields (the example shows some): its `status`, `status_message`, `unavailable`, `next_retry_after` and `cooldowns`, while `routing.quota.check-after` holds one of its quota rests its `quota_checks`, with each rest's next check (`next_check_at`, see [docs/routing.md](routing.md#in-the-management-api)), its counts and recent requests, and `quota`, the account's rate-limit windows from Claude Code's last report of them, named as Anthropic's `anthropic-ratelimit-unified-*` headers. Its times are as that route writes them, RFC 3339 in UTC to the nanosecond, not to the millisecond. Its `auth_index` is what `POST /v0/management/reset-quota` takes to reset its cooldowns. It is `null` for a disabled entry, which has no credential, and for an entry the server hasn't loaded yet.
- **`last_error`** is the credential's last failure, until a request succeeds: its `message`, Claude Code's error (from an error in the form of Anthropic's error body, its `error.message`) or open-ferry's when Claude Code ended without an answer, and the `http_status` it was answered with, `null` when none. It is `null` when there is no failure, or no credential.
- **Nothing else of the entry is passed on**: not its `command`, and nothing from its config directory, which the route doesn't read. Whether Claude Code is signed in is the next route's to say.

### `GET /open-ferry/api/v1/claude-cli/auth-status?name=<entry>`

Whether the Claude Code of the `claude-cli` entry `name` is signed in, and how. It runs the entry's `claude auth status --json` in the environment the entry's requests run in (its `config-dir` as `CLAUDE_CONFIG_DIR`), so it checks the account those requests use. See [docs/claude-subscription.md](claude-subscription.md).

```json
{"loggedIn": true, "authMethod": "claude.ai"}
```

- **`loggedIn`** is whether Claude Code is signed in. When it isn't, the answer is still a `200`, with `false`, even if Claude Code exits with an error.
- **`authMethod`** is how, in Claude Code's own words; empty when it doesn't say.
- **Nothing else is passed on.** Claude Code's answer also names the account's email, its organization and its config directory; they are neither returned nor logged.
- `name` is required, and matched without regard to case or surrounding spaces, as entry names are unique. An entry that isn't in the config is `404 not_found`, and a disabled one is checked all the same.
- Claude Code may take a few seconds. One that can't be run, or whose answer isn't the JSON expected, is `502 claude_cli_failed`; one that takes over 30 seconds is stopped, and the answer is `504 claude_cli_timeout`.

---

## Config

### `POST /open-ferry/api/v1/config/undo`

Undoes the last change to the config file. Every write of the config file (by the management API, the dashboard or `open-ferry config`) keeps the file it replaces beside it as `<config>.bak`, and records the SHA-256 of what it wrote in `<config>.sha256`; this puts that backup back in place of the file, and keeps the file it replaces as the new backup, so a second undo redoes the change. It takes the lock every management write takes, and the file's lock, `<config>.lock`, that every writer of the file takes (`open-ferry config` included), so it never interleaves with one, and the server loads the file again before it answers. The file's lock coordinates the writers that share the config's directory: two containers that each bind-mount only the config file get lock files of their own, and aren't coordinated, so mount the directory instead.

The body is optional. A JSON object with any of these fields says what the caller saw, so the undo puts back only what the caller was shown:

| Field | Type | Meaning |
|---|---|---|
| `config_sha256` | string | The SHA-256, in hex, the config file must have. Otherwise the answer is `409 config_changed`. |
| `backup_sha256` | string | The SHA-256, in hex, the backup must have. Otherwise the answer is `409 config_changed`. |
| `force` | boolean | Go ahead though the file was changed since the last write (see below). Default `false`. |

```json
{"config_sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08", "backup_sha256": "60303ae22b998861bce3b28f33eec1be758a213c86c93c076dbe9f558c11c752", "force": false}
```

```json
{"status": "ok"}
```

- **A file changed since the last write is kept unless forced.** When the file isn't what the last write recorded in `<config>.sha256` (it was edited by hand, or written by something that keeps no record, or the record is missing), undoing would lose that change too, so without `"force": true` the answer is `409 changed_since`. `config diff` (or the file itself) shows what the undo would put back.
- An empty body is the same as `{}`. An unknown field, a field of the wrong type, or a SHA-256 that isn't 64 hex characters is `400 invalid_request`.

- **One step only.** There is one backup, so undo goes back one write, and undo again goes forward again.
- With no backup the answer is `409 no_backup`; a server that can't write its config file (one embedded without a config writer) answers `503 config_writer_unavailable`. No refused undo changes anything.
- A backup that no longer loads as a config isn't put back: the answer is `500 internal_error`, and nothing changes.
- `open-ferry config undo` calls this route while a server is running for the config (see [docs/cli.md](cli.md#open-ferry-config)).

---

## Updates

open-ferry's own updates, which [docs/updates.md](updates.md) describes. CLIProxyAPI's `GET /v0/management/latest-version` is unchanged and still names upstream's latest release. Turning updates off or on is a config change: `PUT /v8/management/config/self-update/mode` with the body `"off"`, `"notify"` or `"auto"` (a JSON string, as every v8 write of one setting takes its value), which the server follows at once.

### `GET /open-ferry/api/v1/update`

What updates are doing. It reads the update state on disk and, when supervised, asks the local supervisor for its status. It makes no release or provider request.

```json
{
  "mode": "auto",
  "mode_source": "config",
  "updates": "on",
  "check_every_seconds": 21600,
  "running_version": "0.1.0",
  "installed_version": "0.1.0",
  "restart_needed": false,
  "target": "x86_64-unknown-linux-gnu",
  "latest_version": "0.1.1",
  "update_available": true,
  "staged_version": "0.1.1",
  "previous_version": null,
  "failed_versions": [],
  "rolled_back_version": null,
  "last_check": "2026-10-08T09:12:44Z",
  "last_result": "staged",
  "last_error": null,
  "next_check": "2026-10-08T15:20:02Z",
  "checking": false,
  "can_update_itself": true,
  "why_not": null,
  "why_not_code": null,
  "trusts_release_key": true,
  "notes": [],
  "supervisor": null,
  "switch_request": null
}
```

- **`mode`** is `auto`, `notify` or `off`, and **`updates`** says it as people do: `on`, `notify-only` or `off`. **`mode_source`** is what set it: `default`, `config` (`self-update.mode`) or `environment` (`OPEN_FERRY_SELF_UPDATE`, which can only lower the config's mode).
- **`installed_version`** is the version a restart runs: after `open-ferry update` has switched the binary, it differs from **`running_version`**, and **`restart_needed`** is `true`.
- **`latest_version`** and **`update_available`** are from the last check. **`staged_version`** is a release downloaded and checked, ready to switch to. **`previous_version`** is kept for `open-ferry update -rollback`. Supervised installs can switch without restarting the server.
- **`failed_versions`** failed their check here and are skipped until a newer release; **`rolled_back_version`** was rolled back from and is skipped too. `open-ferry update` run by hand tries either again.
- **`last_result`** is `up-to-date`, `update-available`, `cannot-update` (newer, but this install doesn't update itself), `staged`, `skipped` (a failed or rolled-back version) or `error`, with **`last_error`** saying what went wrong. **`next_check`** is `null` while updates are off; **`checking`** is whether a check runs now.
- **`can_update_itself`** is whether this install replaces its own binary. When it doesn't (a container, a binary the install scripts didn't put there, or one in a folder it can't write), the server only says when a release is out, **`why_not`** says why, and **`why_not_code`** is one of `container`, `no-receipt`, `bad-receipt`, `unknown-binary`, `other-name`, `other-binary` or `read-only`.
- **`trusts_release_key`** is `false` for a build with no release key, which checks nothing and never updates.
- **`notes`** are problems with the settings, such as an `OPEN_FERRY_SELF_UPDATE` or `self-update.check-every` that couldn't be used.

**`supervisor`** is `null` for a plain install. Update those from the command line with `open-ferry update` (or `open-ferry update -rollback`), then restart the server. A supervised install returns:

```json
{
  "form": "supervised",
  "phase": "watching",
  "running_version": "0.1.1",
  "rollback_target": "0.1.0",
  "watch_until": "2026-10-18T16:10:00Z",
  "switch": {"id": "7c1f", "how": "update", "from": "0.1.0", "to": "0.1.1"},
  "last_switch": {
    "how": "update", "auto": null, "from": "0.0.9", "to": "0.1.0",
    "result": "switched", "reason": null, "at": "2026-10-17T16:00:00Z"
  },
  "switching_disabled": null,
  "error": null,
  "notes": [],
  "stale": false,
  "snapshot_age_seconds": 0.01,
  "failed_reads": 0
}
```

- **`phase`** is the current switch status, such as `waiting-for-signins`, `loading`, `handing-over`, `taking-over`, `watching` or `cancelled`. Without a switch it is the supervisor's state (`idle`, `degraded`, `switching` or `stopping`); it is `unknown` before a live snapshot can be read.
- **`running_version`** is the supervisor's active worker version; while the request is served by a draining worker it can differ from the top-level running version. **`rollback_target`** is the version a rollback would select now, or `null`. During a watch it is the switch's source version. **`watch_until`** is the watch deadline or `null`.
- **`switch`** is the live supervisor switch snapshot, or `null`. It may include additional supervisor fields. **`last_switch`** is the newest finished switch from pending journal outcomes and state history, or `null`. Its `result` is an opaque kebab-case outcome; clients should display a readable fallback for unknown values. `reason`, `from` and `at` can be `null`. `auto` is `true` only when recorded as automatic, `false` when recorded as manual, and `null` when unknown.
- **`switching_disabled`** is the supervisor's reason switching is unavailable, or `null`. A policy-disabled status read sets it to the same fixed `switching is disabled` reason as the POST error. **`error`** says why the live supervisor could not be read. A failed read keeps the last snapshot available in this worker; it never changes a supervised install into a plain one. Other read errors never replace `switching_disabled`.
- **`notes`** lists fixed warnings about optional switch records. A missing or unreadable record does not discard live status or make it stale. Durable rollback and outcome fields are read independently.
- **`stale`** is true when the live supervisor read failed. **`snapshot_age_seconds`** is the cached snapshot's age, or `null` before any successful read. **`failed_reads`** counts consecutive unsuccessful status reads in this worker, shared by every tab that reads it; a successful read resets it. Show the read error instead of the cached phase. Stop treating a stale phase as running after five failed reads or a snapshot age of 30 seconds, whichever comes first. `unknown` is not a running phase. Keep polling a worker's `preparing` request even when its supervisor snapshot is stale. `cancelled` stays in progress until the switch finishes unwinding; the supervisor states `switching` and `stopping` also need observation.

**`switch_request`** describes the latest dashboard install or rollback request in this worker, or `null`:

```json
{"request_id": "46ca8ce60267177eaa54e796448145e0", "action": "install", "state": "preparing", "id": null, "reason": null, "pending": true}
```

`request_id` is an additive string field, currently 32 lowercase hex characters. Treat it as opaque. It is random, so it differs between requests and workers in practice; uniqueness is not guaranteed. It stays the same through preparation and the final request state. The POST 202 reply carries the same `request_id` as GET. Clients can distinguish a new request from an older request for the same action.

Failed and refused requests also carry **`code`**, using the conflict codes above, or `switch_ended` for the defensive accepted-failure case below. In particular, `already_latest` is informational, even though no switch started.

If a preparation task ends without recording a result, GET reports `state: "failed"` with reason `"preparing the switch stopped"`. This recovery is not a 409 response.

`state` is `preparing`, `accepted`, `refused`, `failed` or `unknown` (the supervisor did not answer and may have accepted). `id` is set after acceptance; `reason` explains refusal, failure or uncertainty. `pending` means preparation or observation of an accepted or uncertain request is still pending. The request remains until a newer request replaces it. This in-memory request state ends when the worker exits; the supervisor's live switch and durable last outcome remain available across a handover.

Poll every one or two seconds while preparing or switching; stop after it ends. Each dashboard status GET has a five-second timeout. Retry a failed status poll quietly during handover for at most five consecutive failures or 30 seconds, whichever comes first; then show the error with a retry action and stop polling. Never resend a switch request after an ambiguous timeout; read status first.

### `POST /open-ferry/api/v1/update/check`

Starts a check now, in the background, as the server's own checks run: in `auto` it downloads and checks a newer release and stages it; in `notify` it only looks. No body. The answer is `202`:

```json
{"check": "started"}
```

`"running"` instead of `"started"` means a check was running already, and no other started. `GET /update` shows how it went once `checking` is `false` again. While updates are off nothing is checked and the answer is `409 updates_off`; `open-ferry update -check` still checks by hand.

### `POST /open-ferry/api/v1/update/install` and `POST /open-ferry/api/v1/update/rollback`

Install takes no body. Rollback requires `{"target":"0.1.0"}`, the version shown when the user confirmed it. The target is checked under the update lock. If it changed or disappeared, read status and ask for confirmation again; do not silently select the new target.

Ask the supervisor to install the latest known staged release (downloading and checking it first if needed), or to roll back to the confirmed target. Both use the same management-key and address checks as the other update routes. Manual installs also work while automatic updates are off.

Manual installs may retry a version that failed here or was rolled back from. The confirmation warns about that history and still allows installation. A background check holding the update lock returns `409 update_busy` with `busy, try again shortly`.

The route waits about 900 ms for acceptance. If accepted, it returns `202`:

```json
{"accepted": true, "id": "7c1f", "request_id": "46ca8ce60267177eaa54e796448145e0"}
```

The supervisor owns the switch. The route does not wait for handover or watch confirmation, and disconnecting does not cancel it. If preparation or acceptance takes longer, the task continues in the background and the answer is `202`:

```json
{"preparing": true, "request_id": "46ca8ce60267177eaa54e796448145e0"}
```

At most one preparation runs per dashboard state. A second request while one runs returns `409 already_preparing`, with the message `already preparing`. `GET /update` reports eventual acceptance or refusal, including a refusal after the `202 preparing` response.

An acceptance timeout, a protocol error after sending the switch request, or a stream ending without an acceptance or refusal reply is ambiguous: the answer is `202 {"uncertain": true, "request_id": "..."}` and `switch_request.state` is `unknown`. Nothing is resent automatically. Another switch request returns `409 switch_uncertain` until a fresh supervisor status read succeeds. The request retains state `unknown` and its reason, with `pending: false`; the read alone cannot prove its outcome. Check the live switch and last outcome before deciding to retry. An unreachable endpoint means the request was not sent and is a known failure.

The client returns at acceptance. Later switch failures are observed through `supervisor.last_switch`, including its reason. As a defensive fallback for a client returning `Ended` directly, the route maps it to `202 {"failed": true, "reason": "...", "request_id": "..."}` and a failed `switch_request`; this is not expected from the current install or rollback path.

A supervisor refusal before the response returns `409 switch_refused` with its reason unchanged in `message`. A local preparation failure (download, verification, missing staged release, I/O or journal error) returns `409 switch_failed` with a short reason; detailed diagnostics go to the server log. A plain install returns `409 not_supervised` with command-line update and restart instructions. A server without update checks returns `503 updates_unavailable`, as for the existing routes.

Policy-disabled switching returns `409 switching_disabled`; a changed or removed rollback target returns `409 rollback_target_changed`; an already-running latest release returns `409 already_latest`. These are not sent. A pre-send timeout or a failed write before any byte is accepted is `Unreachable`, also not sent. Client preparation task failures are `Update` errors and not sent. `TimedOut`, `Stopped` and `Protocol` remain uncertain; read status rather than resending.

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
- **With `management.separate-address` set** (open-ferry's own setting), the app, `/management.html`, the dashboard API and the management API are served at that address and nowhere else, with the access rules above unchanged. A change to it takes a restart.
  - **The proxy's port** doesn't have their routes: `/dashboard/` and below, `/dashboard` and `/management.html` answer an empty 404, as while `management.disable-control-panel` is set; every path under `/open-ferry/`, the dashboard API's routes among them, 404 `not_found` ("no such route"), whatever the method, as a path that isn't a route does; and every path under `/v0/management/` and `/v8/management/` an empty 404, as while no management key is set. Everything else there is as before, the sign-in callback pages (`/anthropic/callback`, `/codex/callback`) among it: the management API's callback forwarders send the browser to them on the proxy's port.
  - **The management address** answers every other path, the proxy's routes, `/` and `/healthz` among them, with a 404, `404 page not found` in plain text. The link in safe mode's message, `/management.html?safe-mode=configure`, is then at the management address, not on the proxy's port that gives the message.
  - Both use `server.tls`, the same certificate and key.

## Sign in with ChatGPT binding

Start an explicit bind on the **authenticated** v8 management route:

```http
GET /v8/management/oauth/auth-url?provider=codex&mode=siwc-bind&name=codex-existing.json
Authorization: Bearer <management-key>
```

`name` is an existing top-level `*.json` filename, not a path. The target must be
a regular Codex credential with non-empty outer access and refresh tokens. Links
and Windows reparse points are refused. A successful start answers 200:

```json
{"status":"ok","url":"<authorize URL>","state":"<pending sign-in state>"}
```

Open `url`, then hand back the callback through either route:

```http
GET /v8/management/oauth/callback?state=<state>&code=<code>&client_id=<issued-client-id>
POST /v8/management/oauth/callback
```

The POST JSON is `{"state":"...","code":"...","client_id":"..."}`, with
optional `provider: "codex"`, or `{"redirect_url":"<full pasted redirect>"}`.
For bind sessions the pasted redirect accepts query or fragment fields, with
query fields taking precedence. GET, POST and `/codex/callback` all keep the
issued `client_id`; `is_webui=1` starts the loopback callback forwarder, which
keeps it too. An accepted management callback answers 200 `{"status":"ok"}`;
that means delivery, **not** successful persistence.

Poll `GET /v8/management/oauth/status?state=...`: `{"status":"wait"}` while
pending, `{"status":"ok"}` after persistence and credential sync, or
`{"status":"error","error":"<reason>"}` on failure. Cancel with
`DELETE /v8/management/oauth/session?state=...`, which answers
`{"status":"ok","cancelled":true}` when it removed the pending session.
Once persistence has been admitted, cancellation answers
`{"status":"ok","cancelled":false}` and the bind completes.
The callback wait is five minutes; exchange has a 60-second deadline.

Start errors use `{"error":"<reason>"}`:

| Status | Reason |
|---|---|
| 400 | `invalid SIWC bind target` (missing/unsafe name, unavailable or invalid file, links, non-Codex type, missing outer tokens), or `unsupported oauth mode` for a non-empty unrecognized mode |
| 503 | `credential store unavailable` or `server shutting down` |
| 429 | `too many oauth sessions` |
| 500 | Authorization URL or callback forwarder could not be started |

The existing callback validation errors remain: 400 for invalid state, missing
code/error or invalid pasted redirect; 404 for unknown/expired state; 409 for a
completed, failed or no-longer-pending flow. A bind callback without an issued
client ID fails the **session**, without exchanging or saving. Exchange failures,
timeouts, write-gate refusal, cancellation before admission, shutdown and a
target substituted, deleted or corrupted since start cannot bind. The selected
file is read through a validated no-follow handle inside the persistence primitive
under the credential lock. No handle remains open while waiting for sign-in,
so atomic saves remain possible. Outer account plus per-user fields (email or
ID-token user claims) detect substitution while allowing token rotations for
the same user. Every captured claim must remain present; a disappearing claim
fails closed, and retrying recaptures the current claims. A newly appearing
claim is allowed if the captured evidence and any file/grant fallback still
match. Email cannot mask changed subject/user or account claims.
When no per-user field exists, file identity plus an unchanged private SHA-256
fingerprint of the outer refresh grant are required, and
an atomic save requires restarting the bind. This is not ID-token verification
or SIWC-to-outer account matching. Missing or corrupt files are refused without
restoring the selection snapshot; the bind writer never follows a leaf link.
The auth directory must be trusted against uncooperative writers changing its
entries; other open-ferry writers participate in the credential lock. A leaf
link target is refused, as is a regular target with another auth-directory leaf
link resolving to its destination path or file identity. The final identity
check is best-effort under the trusted-directory rule, not an atomic
identity-conditional replacement: a writer bypassing the credential lock can
still swap the file between the check and replacement.
The bind can also fail if the filesystem's file-identity support changes; retry the bind.

Persistence admission occurs inside the locked bind, after the strict read and
identity checks. **Once admitted, cancellation is refused (`cancelled:false`)
and the bind completes.** The
operation's gate lease lasts through blocking persistence. Binding changes only `siwc` and
`use_siwc`, never the outer tokens or settings, and notifies credential sync so
the running service serves the saved credential. A post-save sync failure fails
the session but cannot undo the already saved file. Unknown nonempty modes and
`siwc-bind` with any provider other than Codex return 400. Ordinary Codex and
Claude sign-ins with no mode are unchanged. Bind persistence failures return a
fixed session error; only the server log reports the I/O category and numeric
OS code, never credential filenames.
The sync revision is reserved while the credential lock is still held, then
that lock is released before awaiting sync.

The device ID is a random v4 UUID generated by open-ferry and reused when valid;
it is sent only in the authorize URL and stored only per credential in
`siwc.device_id`, never as a session/request identity. The start response carries
it **only inside that private authorize URL**; there is no device-ID response
field, status/error exposure or log entry. The ID token's signature and nonce
are not verified, and the SIWC account is not matched to the outer account.
