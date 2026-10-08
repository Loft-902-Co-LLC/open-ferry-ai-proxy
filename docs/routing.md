# Routing

How open-ferry picks a credential for each request, and the two routing settings of its own: routing by the quota the providers report, and a cap on long quota rests. CLIProxyAPI has neither; see [what it does with them](#cliproxyapi-and-these-settings).

The dashboard's Settings page sets both, and its Credentials page and Overview say when each capped rest is next checked.

## How a credential is picked

For each request, the credential manager takes the credentials that can serve the model and aren't resting (cooling down after a failure, turned off, or out of quota), then:

1. **Session affinity** (`routing.session-affinity`): a session already bound to one of them keeps it.
2. **Priority**: otherwise, only the credentials of the highest `priority` among them are considered.
3. **The strategy** (`routing.strategy`) picks one of those:
   - `round-robin` (the default) takes each in turn;
   - `fill-first` keeps taking the first until it rests;
   - `weighted-round-robin` takes each in turn in proportion to its `weight`;
   - `quota`, open-ferry's own, picks by the quota the providers report (below).

The first three are CLIProxyAPI's, and work as they do there. A failed request is retried on another credential as `routing.retry` says.

## Routing by quota

```yaml
routing:
  strategy: quota
  quota:
    prefer: soonest-reset # or most-left
    reserve-percent: 10
```

### What it reads

Each Claude and Codex response says how much of the account's quota is used, in its headers, and the credential manager keeps the last of these readings for each credential, as CLIProxyAPI does (the management API shows it as a credential's `quota`). The quota strategy reads these windows from it:

- **Claude**, and `claude-cli` entries, whose Claude Code reports the same: the 5-hour and 7-day windows, from `Anthropic-Ratelimit-Unified-5h-*` and `-7d-*` (the share used, and the reset). A window whose status is `rejected` counts as full.
- **Codex**: the primary and secondary windows, from `X-Codex-Primary-*` and `X-Codex-Secondary-*` (the percent used, and the reset time or the seconds until it).

Other windows, such as Claude's overage and per-model ones and Codex's per-model limits, aren't read. A window counts while its reset is ahead. One whose reset has passed says nothing of today's use, so it is ignored: a reading isn't aged otherwise, since a window's use only falls at its reset.

A credential's **binding window** is its fullest current one (of two as full, the one that resets later). It **has room** when every current window is below `100 - reserve-percent` percent used.

### No reading, or a stale one

A credential with no current window has room, with all of its allowance left. That is one whose responses carry none of these headers (most API keys, and Gemini, Vertex AI and OpenAI-compatible upstreams), one not used since open-ferry started, and one whose windows have all reset since its last reading.

### The order

Best first:

1. **Credentials with room come before those without.** The reserve is kept while another credential has room.
2. **Among those with room**, by `prefer`:
   - **`soonest-reset`** (the default): the earliest reset of the binding window first, so allowance that would otherwise go unused is used first; then the most left. Credentials with no reading come last: none of their allowance is known to be about to go unused, so they stay spare.
   - **`most-left`**: the most left in the binding window first, then the earliest reset. A credential with no reading counts as having all of it left, so it is tried, and read, on its first pick.
3. **When none has room**, the most left in the binding window first, then the earliest reset. The request is still served, by the credential likeliest to take it, rather than refused while the provider would still answer it. A credential the provider then refuses rests as it would under any strategy.
4. **Credentials that rank the same take turns**, as round-robin does.

Percentages are compared to a tenth of a percent and resets to the second. Session affinity still wins and priority still comes first, since the strategy only orders the ready credentials of one priority. Weights are ignored.

`reserve-percent` is 0 to 100; 0, the default, keeps nothing back, and a value outside the range is taken as its nearer end. `prefer` is read in any case; another value is `soonest-reset`. A change of `prefer` or `reserve-percent` alone keeps the strategy's turns.

The strategy's debug line holds the numbers it ranked by, never a credential's ID, key or email.

## A cap on long quota rests

```yaml
routing:
  quota:
    check-after: 1h
```

When a provider answers that a credential is out of quota, the credential, or the model on it, rests until the reset the provider gives, which can be days away. `routing.quota.check-after`, a Go duration such as `1h` or `90m`, caps that rest, under any strategy:

- **A rest longer than the cap lasts the cap instead.** A shorter one is left alone.
- **Then one request goes through, as a check.** While it is in flight, no other request picks the credential for that model (or at all, for a rest of the whole credential).
- **A success ends the rest.**
- **Another quota answer doubles the wait**, from the time of that answer: with `1h`, the checks come after 1, 2, 4, 8 hours and so on. The wait never passes the provider's reset: once doubling would reach it, the credential rests until the reset, and from then on the usual rules apply.
- **Any other answer**, such as a server error, leaves the rest as it is, so the next request checks again, once that answer's own cooldown is over. A streamed check holds its turn until the stream ends; a client that leaves the stream first lets the next request check.
- **`reset-quota`** (`POST /v0/management/reset-quota`, or the dashboard's reset) ends the rest, as it clears the credential's quota and cooldowns.

Off (empty, `0` or not a duration) is the default, and rests until the provider's reset, as CLIProxyAPI does. The setting takes effect without a restart. Turning it off holds no request back as a check from then on; a rest it already shortened still ends at its check time, and the credential's next answer is handled without the cap.

While a rest waits for its check, the credential's `next_retry_after` and `cooldowns` show the check time; while the check is in flight, they show the provider's reset.

### Across a restart

With `save-cooldown-status` on, a capped rest is saved with its cooldown, in the `.cds` file, under a key of open-ferry's own, `open_ferry_quota_check`, and put back at the next start, so the doubling goes on. A check that was in flight when the file was saved is due at once after a restart. A rest whose check was already due when it was saved, or comes due before the restart, has no cooldown left to carry it: it is lost, and the next quota answer starts again at the cap.

### In the management API

Each credential that `GET /v0/management/auth-files` lists (`GET /v8/management/credentials` in the v8 API), and the dashboard API's `claude-cli/entries` credential, has `quota_checks` while the cap holds one of its rests, and no such field otherwise:

```json
"quota_checks": [
  {
    "scope": "model",
    "model_key": "gpt-5.5",
    "state": "resting",
    "next_check_at": "2026-10-07T15:00:00.123456789Z",
    "provider_reset_at": "2026-10-11T09:30:00Z",
    "wait_seconds": 7200
  }
]
```

- **`scope`** is `credential` for a rest of the whole credential, or `model` for one model, named by its `model_key`, the key the credential's `cooldowns` use.
- **`state`** is `resting` until `next_check_at`, `due` from then until a request checks, and `checking` while that request is in flight.
- **`next_check_at`** is when the next check is let through, and **`provider_reset_at`** the reset the provider gave, both RFC 3339 in UTC.
- **`wait_seconds`** is the current wait, which the next quota answer doubles.

## CLIProxyAPI and these settings

Both settings are open-ferry's own, and CLIProxyAPI (v8.0.15 to v8.0.20) handles a config that uses them as follows:

- **It loads the file.** Its loader ignores fields it doesn't know, so `routing.quota` is ignored, and it runs a `strategy` it doesn't know as `round-robin` (`normalizedRoutingRuntimeState` and `newRoutingSelector` in `sdk/cliproxy/service_config.go`).
- **Its management API refuses `quota`**: `PUT /v0/management/routing/strategy` answers 400 `invalid strategy`, and a v8 config write that sets `routing.quota` answers 400 `invalid_config` (`ValidateV8Config` decodes strictly).
- **Its config writes take the section out.** A write through its management API to a config in the v8 layout, which `config.example.yaml` uses, or through its v8 API, comments out the sections it doesn't know, `routing.quota` among them, with a warning (`commentUnknownV8Sections` in `internal/config/config_v8.go`, which `NormalizeConfigLayout` runs for those writes). A config in the legacy layout keeps the section through its `/v0/management` writes.

open-ferry knows the section in both layouts, so its own saves keep it. A save writes only the keys the config sets, so a cleared setting leaves the file.
