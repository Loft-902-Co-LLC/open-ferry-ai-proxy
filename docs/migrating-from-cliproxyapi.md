# Moving from CLIProxyAPI

open-ferry reads CLIProxyAPI's config and auth directory as they are, so for most setups switching means running a different binary. This guide covers what you might notice when you do. It follows CLIProxyAPI v8.0.15, the version open-ferry is ported from. Every difference, down to the details, is listed in [UPSTREAM.md](../UPSTREAM.md).

## Switching over

1. **Back up** your `config.yaml` and auth directory. open-ferry changes the config only when you save a change through the management API or the dashboard, and it saves credential files, as CLIProxyAPI does, when it refreshes a token or you sign in.
2. **Stop CLIProxyAPI.** Don't run both against the same auth directory at once. Each refreshes tokens on its own, and a refresh token one of them has used may then be refused to the other.
3. **Install open-ferry** as the [README](../README.md#install) says.
4. **Start it** where you started CLIProxyAPI, so that it finds `config.yaml` in the working directory, or pass the file with `-config`. It listens on the config's `port`, as before.
5. **Read on** if you use a sign-in other than Codex's or Claude's, a command-line flag other than the login ones, the management panel, or storage other than local files. If you run CLIProxyAPI's image with Docker Compose, see [Docker Compose](#docker-compose) instead of steps 3 and 4.

## Docker Compose

open-ferry's image, `ghcr.io/loft-902-co-llc/open-ferry`, keeps the paths of CLIProxyAPI's: the config is `/CLIProxyAPI/config.yaml`, the auth directory `/root/.cli-proxy-api`, the logs `/CLIProxyAPI/logs`, and the port 8317. `/CLIProxyAPI/CLIProxyAPI` is a link to the binary, so a `command:` written for CLIProxyAPI's image still runs. To switch a Compose file written for CLIProxyAPI's image, such as its `docker-compose.yml`:

1. **Change the image** to `ghcr.io/loft-902-co-llc/open-ferry:latest`, or to a version, such as `ghcr.io/loft-902-co-llc/open-ferry:0.1.0`. If the file takes the image from `CLI_PROXY_IMAGE`, set that variable to open-ferry's image instead.
2. **Remove the `build:` section.** It builds CLIProxyAPI from a checkout; open-ferry's image is built from its release archives.
3. **Remove the sign-in callback ports** (8085, 1455, 54545, 51121 and 11451). open-ferry's callback servers listen on the container's own loopback, which a published port doesn't reach. The README says [how to sign in](../README.md#run-it-in-a-container) instead.
4. **Remove the plugins volume and `DEPLOY`.** Neither plugins nor the cloud deploy mode are ported.
5. **Consider mounting the config's directory** rather than the file, if you save the config from the dashboard or the management API. With the file mounted on its own, open-ferry writes a save over it in place, as CLIProxyAPI does, and leaves the backup, `config.yaml.bak`, inside the container. With its directory mounted, each save replaces the file in one step and the backup is kept beside it. Mount a directory that holds `config.yaml`, and name the file in the command:

   ```yaml
   command: ["open-ferry", "-config", "/CLIProxyAPI/config/config.yaml"]
   volumes:
     - ./config:/CLIProxyAPI/config
   ```

6. **Set `TZ`** if you want local times in the logs. CLIProxyAPI's image sets `Asia/Shanghai`; open-ferry's is in UTC.

The auth directory and log mounts stay as they are, as do `CLI_PROXY_CONFIG_PATH`, `CLI_PROXY_AUTH_PATH` and `CLI_PROXY_LOG_PATH`, and the config's empty `server.host`, which a container needs. Consider publishing 8317 on the host's 127.0.0.1 only (`"127.0.0.1:8317:8317"`), unless other machines use the proxy. open-ferry's own [`docker-compose.yml`](../docker-compose.yml) is the result of these steps, with the port so published.

## Config and auth directory

**What stays the same:**
- **Both config layouts load:** the legacy one and the v8 one. Changes to the config or the auth directory are picked up without a restart, as before.
- **The auth directory** is the config's `auth-dir`, by default `~/.cli-proxy-api`, with the same credential files.
- **The management key** is still `management.secret-key` or `MANAGEMENT_PASSWORD`. A config in the legacy layout has the management settings under `remote-management:`, which is still read; where both blocks set a key, `management:` wins.
- **A `.env` file** in the working directory is still loaded at start, and a variable already set in the environment still wins over it. On Windows it wins whatever the case of its name, and a file that starts with a UTF-8 byte order mark loads, where CLIProxyAPI refuses it.
- **Log files** go to the same place: `logs` under `WRITABLE_PATH` if it's set, else the working directory's `logs` if that directory exists and can be written to, else `logs` in the auth directory.
- **Session affinity** (`routing.session-affinity`, `session-affinity-ttl` and `session-affinity-subagents`) works as before: a conversation stays on the credential that served it, and moves only when that one fails or isn't ready. The bindings are held in memory, hashed, and never logged; see [Session affinity](../UPSTREAM.md#session-affinity).
- **Safe mode** still holds: while `api-keys` lists the example keys from CLIProxyAPI's `config.example.yaml`, the proxy refuses service until you change them. open-ferry's own [`config.example.yaml`](../config.example.yaml), in each release archive, keeps those keys, so a config copied from it is refused too until you change them. There's no warning page at `/`, though: the proxy's routes answer 403 with CLIProxyAPI's message, whose link, `/management.html?safe-mode=configure`, opens the dashboard's API key setup.

**What differs:**
- **Loading the config never writes it.** CLIProxyAPI replaces a plain `secret-key` with its bcrypt hash in the file; open-ferry leaves the file alone and compares the plain key as written. A file that already holds a hash works too. The file is written only when you save a change through the management API or the dashboard. See [the foundation](../UPSTREAM.md#the-foundation).
- **The `models` catalog sources are files** (`catalog`, `codex-catalog`). An absolute path is read at start and again when the file changes; an empty source is the catalog built into the binary. No catalog is downloaded: a URL source logs a warning and the catalog in use stays, and `-local-model` changes nothing. `devin-catalog` is ignored.
- **Settings for features open-ferry doesn't have are read and ignored**:
  - the per-credential limits `credential-concurrency` and `credential-in-flight`;
  - plugins, pprof and LAN discovery;
  - the Antigravity and Devin sections;
  - the client impersonation ("cloaking") settings.

  The full list is in the docs of `crates/open-ferry-core/src/config/mod.rs`. open-ferry's [`config.example.yaml`](../config.example.yaml) leaves these settings out.
- **Credential files of providers open-ferry doesn't serve** are left alone in the auth directory, but nothing is served from them. CLIProxyAPI hands a credential of an unknown type to its OpenAI-compatible executor.
- **No remote storage or cloud mode.** The config and credentials must be local files. The Postgres, git and object stores, the cloud deploy mode and Home mode aren't ported, so these variables are ignored:
  - `DEPLOY=cloud`;
  - `PGSTORE_*`, `GITSTORE_*` and `OBJECTSTORE_*`;
  - `HOME_JWT`.

## The command line

The binary is `open-ferry` (`open-ferry.exe` on Windows), not `cli-proxy-api`, so update scripts and service definitions. Flags are read as CLIProxyAPI reads them, with one or two dashes.

| Flag | In open-ferry |
|---|---|
| `-config`, `-no-browser`, `-oauth-callback-port` | As before |
| `-codex-login`, `-codex-device-login`, `-claude-login` | As before. `-claude-login` goes against Anthropic's terms and, in our testing, gets only the Haiku models ([why](claude-subscription.md#the-claude-sign-in)) |
| `-local-model` | Accepted, but only logs a line: no catalog is downloaded, so the built-in catalogs are used unless `models` names a catalog file |
| `-antigravity-login`, `-kimi-login`, `-kimi-ai-login`, `-devin-login` | Not available: those providers aren't supported (see [below](#providers-and-sign-ins)) |
| `-xai-login`, `-meta-login` | Not available: use an API key (see [below](#providers-and-sign-ins)) |
| `-vertex-import`, `-vertex-import-prefix` | Not available: use the management API's `vertex/import` route, or a `vertex-api-key` entry in the config |
| `-password` | As before: a management password for clients on the same machine (127.0.0.1 and ::1), and the `/keep-alive` endpoint that stops the server after 10 seconds without a call |
| `-tui`, `-standalone`, `-management-base-url` | As before, except that the OAuth tab offers only Codex and Claude, and the config and keys tabs can't save changes yet. `-standalone` still needs `management.secret-key` or `MANAGEMENT_PASSWORD` |
| `-home-jwt`, `-home-disable-cluster-discovery` | Not available: Home mode is on the roadmap for v4 |
| `-discover`, `-discover-timeout`, `-discover-json`, `-discover-service-type` | Not available |

An unknown flag stops open-ferry with its usage and exit code 2, as CLIProxyAPI does.

Exit codes differ in a few places:
- A working directory that can't be read, a config that won't load, or an auth directory that can't be resolved exits with 1; CLIProxyAPI logs it and exits with 0.
- A failed sign-in exits with 1, and with 13 when its callback port is in use. CLIProxyAPI exits with 0 for the first.

## Providers and sign-ins

open-ferry signs in only with each provider's own OAuth flow, and doesn't pose as another company's app ([why](../README.md#non-goals)). So some of CLIProxyAPI's providers and sign-ins aren't there:

| In CLIProxyAPI | In open-ferry | Instead |
|---|---|---|
| Codex and Claude sign-ins; OpenAI-compatible, Gemini, Gemini Interactions and Vertex AI keys and service accounts | Supported | |
| Meta | API keys and access tokens; no sign-in | A `meta-api-key` entry |
| xAI | API keys; no sign-in, and no Grok CLI chat proxy | An `xai-api-key` entry |
| Kimi | Not planned | Kimi's API key, through `openai-compatibility` or `claude-api-key` |
| Gemini CLI, and the AI Studio relay | Not planned | A Gemini API key (`gemini-api-key`) or Vertex AI |
| Antigravity, Devin | Not planned | None in open-ferry |
| Claude "cloaking" and the Claude Code request profile | Not planned | Requests carry Claude's documented headers only |
| Plugins | Not ported | Maybe in v3, after a survey ([roadmap](../ROADMAP.md#v3-plugins-if-they-are-worth-it)) |
| Image and video endpoints | Video: as before, for xAI API keys (`/v1/videos` and its routes, `/openai/v1/videos`, a video and its `/content`); at most 10,000 videos are remembered with the key that made them. Images: as before, through Codex, xAI API keys and `openai-compatibility` providers (`/v1/images/generations` and `/v1/images/edits`); an edit's `multipart/form-data` form is read in memory and answers 413 past the body limit, where CLIProxyAPI writes a large one to temporary files | |
| Realtime | Not yet: on the roadmap for v2 | |

## The management API

open-ferry serves the part of `/v0/management` (and its `/v8/management` names) that T3 Code's hub uses, and more. The routes, and how each differs, are in [The management API](../UPSTREAM.md#the-management-api). What you'll notice:

- **The control panel is open-ferry's own dashboard.** `/management.html` sends the browser to `/dashboard/`, keeping its query. The dashboard is built into the binary, so no panel is downloaded and `management.panel-github-repository` is ignored. `disable-control-panel` turns it off, as before, and a client the management API refuses for its address (another machine, without `allow-remote`) is refused the dashboard too, where CLIProxyAPI serves its panel to anyone. See [the dashboard](../UPSTREAM.md#added-in-open-ferry-the-dashboard).
- **A config change through the API takes effect before the answer.** CLIProxyAPI answers first and reloads after; open-ferry saves the file, loads it again and then answers. A save keeps the file's comments and the settings open-ferry doesn't type, as CLIProxyAPI's does, replaces the file in one step and keeps the previous one as `config.yaml.bak`. A save that fails changes neither the file nor the running config, where CLIProxyAPI keeps the change in memory. Editing `config.yaml` by hand still works; the change is picked up on its own. See [Config writes](../UPSTREAM.md#config-writes).
- **Also not ported:**
  - `quota/providers` and `quota/reset`;
  - the plugin routes;
  - other providers' sign-in routes (`kimi-auth-url` and the like). On v8, `oauth/auth-url` answers 404 `provider_not_found` for them.
- **Version checks are open-ferry's.** `latest-version` reports open-ferry's latest release. `X-CPA-VERSION`, `X-CPA-COMMIT` and `X-CPA-BUILD-DATE` describe the open-ferry build.
- **Config reads:**
  - The v8 reads show a plain management key as a bcrypt hash, which differs from one run to the next.
  - `GET config` leaves out the sections open-ferry ignores, and writes an empty list as `null`.
  - `model-definitions` knows only the channels open-ferry serves; others answer 400 `unknown channel`.
  - See [Config and info reads](../UPSTREAM.md#config-and-info-reads).
- **Credential files:**
  - An upload must hold a credential open-ferry serves.
  - A delete removes only `*.json` files at the top of the auth directory.
  - Names are checked as Windows requires them, on every system.
  - See [Credential files](../UPSTREAM.md#credential-files).
- **Sign-ins through the API:**
  - A callback goes to its login in memory, so no `.oauth-*` files appear in the auth directory.
  - The callback forwarders listen on 127.0.0.1 only, so a browser on another machine can't reach them, nor can one outside a container through a published port. Forward the port over SSH, or send the URL the browser lands on to the `oauth-callback` route.
  - The `state` is longer than CLIProxyAPI's.
  - See [OAuth logins](../UPSTREAM.md#oauth-logins).
- **Times are in UTC** in `auth-files` and in usage records, where CLIProxyAPI writes local time.
- **The usage queue isn't served over RESP** (the Redis protocol on the server's port); use the `usage-queue` route ([Usage statistics](../UPSTREAM.md#usage-statistics)).

## Requests to providers

These are the differences most likely to show in practice. The rest are in [Deviations so far](../UPSTREAM.md#deviations-so-far) and [xAI](../UPSTREAM.md#xai).

- **User agent.** OpenAI-compatible, Gemini and Vertex AI upstreams get `User-Agent: open-ferry/<version>`, unless the client sent its own.
- **Custom headers.** A credential's custom `headers` can't set a header that says which client is calling, such as `User-Agent`, `X-App`, `X-Stainless-*`, `Originator` or a session ID. Such a header is dropped, with a warning naming it.
- **Prompt caching.** No session ID or `prompt_cache_key` is made up for a request. One the client sends is still passed on, but if you relied on the keys CLIProxyAPI derives, you may see fewer prompt cache hits. Session affinity still derives a session for a request that names none, but only to pick its credential: it isn't sent upstream.
- **Secrets are redacted.** A secret the request was sent with is replaced with `[redacted]` in provider errors and answers before they reach the client.
- **Redirects.** Requests follow redirects only to the same origin.

## Logs

- **Request logs mask more:** cookies and management keys among others. A compressed body is decoded, or replaced by a one-line placeholder. OAuth callbacks aren't logged. See [Request logs](../UPSTREAM.md#request-logs).
- **Usage records** keep their times in UTC (see above).
- **The usage ledger**, `open-ferry-usage.sqlite3`, is made in the log directory at start, for the dashboard, so the directory appears even when nothing else is logged there. See [the dashboard](../UPSTREAM.md#added-in-open-ferry-the-dashboard).
