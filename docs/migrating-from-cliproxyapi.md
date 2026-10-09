# Moving from CLIProxyAPI

open-ferry reads CLIProxyAPI's config and auth directory as they are, so for most setups switching means running a different binary. This guide covers what you might notice when you do. It follows CLIProxyAPI v8.0.20, the version open-ferry is ported from. Every difference, down to the details, is listed in [UPSTREAM.md](../UPSTREAM.md).

## Switching over

Install open-ferry as the [README](../README.md#install) says, then run:

```sh
open-ferry migrate
```

The install scripts look for CLIProxyAPI too, and offer to run it when they find it.

`migrate` finds CLIProxyAPI, running or set up to run, and what starts it. It lists what carries over and what doesn't, anything that stops the switch, and each step it would take, then asks before it changes anything. `open-ferry migrate -dry-run` shows the same and changes nothing. If it can't tell which config CLIProxyAPI uses, name it with `-config`. The flags and the exit codes are in [docs/cli.md](cli.md#open-ferry-migrate).

open-ferry uses CLIProxyAPI's config and auth directory in place: nothing is copied into a new layout, and the config isn't rewritten. Each sign-in keeps a single refresh token, which both proxies can use, so switching back keeps working. With your yes, `migrate`:

1. **Backs up** the config, the `.env` files beside it and in CLIProxyAPI's working directory, and the whole auth directory, into a new folder, `open-ferry-migrate-<UTC time>`, beside the config, or beside the auth directory when the config is inside it. Each kind of file has a folder of its own, so that no name takes another's place: `config/<the config's name>`, `env/.env` and `env/working-dir.env`, `auth/`, and the record's copy, `migration.json`. The backup is placed by where the config and the auth directory really are, so a link can't put it inside the auth directory, where open-ferry would read its files as credentials, and the auth directory is copied from its real path. On Linux and macOS only you can open it (0700); on Windows it has the permissions of the folder it is in.
2. **Writes a record** of the switch, `migration.json`, beside open-ferry's installed config: `~/.config/open-ferry/` on Linux and macOS (root's, under `sudo`), `%APPDATA%\open-ferry\` on Windows. A copy goes in the backup. It holds paths, names and states, and no secrets: no command line, no environment and no file contents. [`migrate -undo`](#switching-back) reads it.
3. **Switches**, by what starts CLIProxyAPI:
   - **A service:** a systemd unit, yours or the system's; a launchd job, Homebrew's `brew services` job among them; a Windows service, a wrapper such as NSSM's included; or a scheduled task that runs CLIProxyAPI's binary itself, with its arguments and in its working directory (`%windir%\System32` for a task that names none). CLIProxyAPI's service is stopped and disabled, and its definition kept. open-ferry's is installed as [`open-ferry service install`](cli.md#open-ferry-service) installs it, with the same config and working directory, and started. A system service needs root or an administrator; without them, `migrate` says what to run.
   - **Anything else:** a launcher script or app, a scheduled task that runs a script, cron, or a process started by hand. These get the [drop-in switch](#the-drop-in-switch).
   - **A container** from CLIProxyAPI's image: `migrate` changes nothing, and prints the [Docker Compose](#docker-compose) steps for it.
4. **Checks that open-ferry answers** on the config's address and port, within 30 seconds, and undoes the switch when it doesn't, saying why. It asks `GET /`, which calls no provider and needs no key, and checks that open-ferry, not CLIProxyAPI, is the one answering.

**What stops it.** `migrate` changes nothing while any of these is so, and says what to do:

- CLIProxyAPI uses remote storage, cloud mode or Home mode (`PGSTORE_*`, `GITSTORE_*`, `OBJECTSTORE_*`, `DEPLOY=cloud`, `HOME_JWT` or `-home-jwt`). open-ferry needs the config and credentials as local files.
- Its config can't be read, or doesn't load in open-ferry; or its auth directory, or a file in it, can't be read, and so can't be backed up.
- For the drop-in switch: its start command has a flag open-ferry doesn't take, or CLIProxyAPI isn't running.
- A system service, without root or an administrator.
- A stopped systemd unit whose command line can't be told for sure: its unit file and drop-ins give another command than `systemctl show` does; it has more than one `ExecStart=` and isn't `Type=oneshot`; it has changed since systemd last read it (`NeedDaemonReload=yes`); or its `ExecStart=` uses a `$` variable or a `%` specifier, which systemd fills in only as it starts the unit. What `migrate` says names the unit file and what kind of difference it found, but not the command's arguments.
- A scheduled task that runs otherwise than open-ferry's would. open-ferry's task runs as you, only while you are logged on, without elevated rights, so a task that runs as a group, as another user or as no user named, with the highest rights (`HighestAvailable`), or with a stored password, S4U or a service account, stops it. So does more than one task that runs CLIProxyAPI's binary that way; a task whose directory, program or arguments use a variable `migrate` doesn't know (it knows `windir`, `SystemRoot`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `ProgramData`, `ProgramFiles`, `ProgramFiles(x86)`, `ProgramW6432`, `USERNAME`, `PUBLIC`, `TEMP` and `TMP`); and a task that runs CLIProxyAPI's command when CLIProxyAPI's working directory can't be read, so that whether the task started it can't be told.
- An earlier switch that hasn't been undone.

**Claude sign-ins.** If the auth directory holds credentials from CLIProxyAPI's Claude sign-in, `migrate` says how many. open-ferry serves them, but in our testing they get only the Haiku models, because open-ferry doesn't pose as Claude Code. If your clients use Claude Opus or Sonnet through them, they will lose those models. For the rest, use [`claude-cli`](claude-subscription.md), which runs your own Claude Code.

Read on if you use a sign-in other than Codex's or Claude's, a command-line flag other than the login ones, the management panel, or storage other than local files.

### The drop-in switch

When what starts CLIProxyAPI isn't a service open-ferry knows, `migrate` puts open-ferry in CLIProxyAPI's place. It renames CLIProxyAPI's binary to `<name>.cliproxyapi`, such as `cli-proxy-api.exe.cliproxyapi`, and puts open-ferry under the binary's name, so that whatever starts CLIProxyAPI starts open-ferry, with the same command line. Windows lets a running program be renamed but not replaced, so CLIProxyAPI runs on until it stops. Don't move or replace `<binary>` or `<binary>.cliproxyapi` by hand while a drop-in is in place: use `open-ferry migrate -undo`, which puts the binary back only if it is the one recorded.

On Linux and macOS, with an install receipt, what it puts there is a symbolic link to the installed open-ferry: the binary the install receipt names. `open-ferry update` replaces that binary, so the drop-in updates with it, and runs the new version from the next start. On Windows, and on Linux and macOS without an install receipt (there is nothing for a link to follow, and a link to wherever `migrate` runs from can go stale), it is a copy, which reports a new release (`open-ferry update`) but doesn't install it, as the updater replaces the installed open-ferry and not the copy. To update the copy, run `open-ferry migrate -undo`, then `open-ferry migrate` again. `migrate` says so in its plan, and `-json` gives `"drop_in": "symlink"` or `"copy"` ([docs/cli.md](cli.md#open-ferry-migrate)).

`migrate` records the SHA-256 of CLIProxyAPI's binary before it moves it, and `-undo` puts back only a file with that digest. Then `migrate` asks whether to stop CLIProxyAPI now. With your yes, or `-yes`, it is stopped: with SIGTERM on Linux and macOS, which CLIProxyAPI handles gracefully, and at once on Windows. If its launcher doesn't start it again within 3 seconds, `migrate` starts open-ferry in its place, with the same command line, in the same working directory, its output going to `open-ferry.log` in the backup. Without your yes, nothing is stopped: restart CLIProxyAPI through its launcher, and open-ferry starts in its place.

- **Updates put CLIProxyAPI back.** Anything that updates or reinstalls CLIProxyAPI's binary in place puts CLIProxyAPI back over open-ferry: a launcher's or tray app's updater, such as EasyCLIProxyAPI's; a package manager, such as Homebrew, Scoop or the AUR; or an install script. Turn its updates off, or run `open-ferry migrate` again after one.
- **open-ferry runs under CLIProxyAPI's file name.** It works the same under any name. To tell which one is running: open-ferry logs `open-ferry Version: ...` as it starts, and answers `GET /` with `open-ferry-ai-proxy`.
- **The start command has to suit open-ferry.** open-ferry reads CLIProxyAPI's command line as CLIProxyAPI does, but stops with its usage at a flag it doesn't take ([The command line](#the-command-line)), so `migrate` refuses until you take such a flag out of what starts CLIProxyAPI.

### Switching back

```sh
open-ferry migrate -undo
```

`-undo` reverses the switch its record describes. For a service, it stops and removes open-ferry's service, and turns CLIProxyAPI's back on as it was. For the drop-in switch, it moves CLIProxyAPI's binary back, removes open-ferry's link or copy, stops open-ferry, and starts CLIProxyAPI with the same command line, unless its launcher does. As the switch does, it shows its steps and asks first, unless `-yes` is given; `-dry-run` only shows them.

Only one `migrate` runs at a time, switch or `-undo`: a second finds a lock beside the record (`migration.json.lock`) held, says so, exits with 1 and changes nothing. Run it again when the first has finished. The lock is per record location, which follows `XDG_CONFIG_HOME`, `HOME` or `APPDATA`: a run that sees another location for the same CLIProxyAPI doesn't wait for the other, and fails when it moves CLIProxyAPI's binary, which never replaces a file, without changing CLIProxyAPI's binary.

CLIProxyAPI is not started until open-ferry has ended: `-undo` waits up to 20 seconds for open-ferry's service, as the service manager says, or its process, to end, and also that nothing answers as open-ferry on the config's address. If it doesn't end, `-undo` says what it saw and starts nothing. Turning CLIProxyAPI's service on is safe to repeat: a launchd job that is loaded already or a Windows service that runs already counts as done.

`-undo` marks the switch undone only when every step worked. If one failed, it says which, and the record stays open: run `open-ferry migrate -undo` again. Each step looks at what is there, so one that was done is not done twice. A drop-in's binary is put back only if it has the SHA-256 recorded at the switch; anything else is left where it is, with how to put CLIProxyAPI's back by hand, and the record stays open. A drop-in whose open-ferry was stopped but whose CLIProxyAPI isn't running leaves `migrate` with no command line to start it (the record holds none): it says so, exits with 1, and the record's status is `undone-not-started`. Start CLIProxyAPI as you do, then run `open-ferry migrate -undo` again: the record is closed (`undone`) once CLIProxyAPI's process is found running, or a server on the config's address answers that it is CLIProxyAPI. An HTTP answer from some other program (a 404, say) does not close it. The same holds when a switch fails and its own rollback does: the record stays open, as `switching`, and `-undo` finishes it.

If the record can't be read, `-undo` reads its copy in the backup instead, and says so. The record is written to a file beside it and renamed into place, so a failed write leaves the old one whole.

It leaves the config and the auth directory as they are: a token open-ferry refreshed works for CLIProxyAPI too. `-undo -restore` also copies the backed-up config, `.env` files and credentials back. `-restore` puts back the credentials as they were at the switch, so a token refreshed since is replaced by the older one, and a sign-in whose token was refreshed after the backup may need to be made again, as the old refresh token in the backup may be refused. Files added since the backup are left as they are.

`-restore` does not write under a running proxy. If CLIProxyAPI (or an open-ferry of the drop-in) runs on those files, the plan shows it, and `-undo` asks whether to stop it (`-yes` is the yes); without the yes nothing is stopped and nothing is copied. After a failed copy CLIProxyAPI is not started and the record stays open: run `-undo -restore` again, which copies, then starts. The record notes where each file, and its directory, really was at the switch; a file whose place leads somewhere else now (a symbolic link put there since) is not written, and `-undo` says so. After a service switch the files are copied only once CLIProxyAPI's manager says its service has stopped (systemd, launchd or the Windows service state; a scheduled task has no state to read, so it is disabled first, without a question; what runs is read after that, and the task is then ended and counts as stopped once none of its processes runs; the processes are read once more just before the copy, and CLIProxyAPI running then stops the copy). If the service runs, `-undo` asks first (`-yes` is the yes) and stops it through the manager, whether or not a process of it is seen. No process of the service is killed by its ID under the manager: if the manager can't stop it, can't say, or still says it runs 20 seconds after the stop, nothing is copied, CLIProxyAPI isn't started and the record stays open. Stop the service by hand, then run `-undo -restore` again. This holds for the first `-undo -restore` too, with the service still off as `migrate` left it. What still runs from CLIProxyAPI's binary once the manager says stopped is stopped by its identity. One race remains: `-undo` looks at where each file goes just before it writes it, but a program that swaps a directory for a symbolic link in the instant between that look and the write is not caught. A launcher or other process that restarts CLIProxyAPI at once can race the copy too: stop it first. So can a systemd timer, path or socket unit that starts CLIProxyAPI's unit, or any other program that starts its service: `-undo` can't see them, so stop or disable them first.

**CLIProxyAPI comments out open-ferry's own settings when it saves the config.** Back on CLIProxyAPI, a save from its management panel or API turns the sections only open-ferry reads, such as `routing.quota` and `management.separate-address`, into comments at the end of the file. If you switch to open-ferry again, set them again.

### By hand

To switch without `migrate`:

1. **Back up** your `config.yaml` and auth directory. open-ferry changes the config only when you save a change through the management API or the dashboard, and it saves credential files, as CLIProxyAPI does, when it refreshes a token or you sign in.
2. **Stop CLIProxyAPI.** Don't run both against the same auth directory at once. Each refreshes tokens on its own, and a refresh token one of them has used may then be refused to the other.
3. **Install open-ferry** as the [README](../README.md#install) says.
4. **Start it** where you started CLIProxyAPI, so that it finds `config.yaml` in the working directory, or pass the file with `-config`. It listens on the config's `port`, as before. To run it in the background, use [`open-ferry service install`](cli.md#open-ferry-service) in place of CLIProxyAPI's service.

If you run CLIProxyAPI's image with Docker Compose, see [Docker Compose](#docker-compose) instead of steps 3 and 4.

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
- **open-ferry's own routing settings**, `routing.strategy: quota` and the `routing.quota` section ([docs/routing.md](routing.md)), mean nothing to CLIProxyAPI: if you go back, it runs the `quota` strategy as round-robin, ignores the section, and comments it out when its management API writes a config in the v8 layout.
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
| `-tui`, `-standalone`, `-management-base-url` | As before, except that the OAuth tab offers only Codex and Claude, and the config and keys tabs can't save changes yet. `-standalone` still needs `management.secret-key` or `MANAGEMENT_PASSWORD`. With open-ferry's `management.separate-address` set, the TUI connects there unless `-management-base-url` or `management.base-url` says otherwise ([docs/cli.md](cli.md#the-terminal-ui)) |
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
| Speech endpoints | As before, for xAI API keys (`/v1/audio/speech` and `/v1/tts`, new in CLIProxyAPI v8.0.20); the audio is held in memory up to 256 MiB | |
| Realtime | Not yet: on the roadmap for v2 | |

## The management API

open-ferry serves the part of `/v0/management` (and its `/v8/management` names) that T3 Code's hub uses, and more. The routes, and how each differs, are in [The management API](../UPSTREAM.md#the-management-api). What you'll notice:

- **The control panel is open-ferry's own dashboard.** `/management.html` sends the browser to `/dashboard/`, keeping its query. The dashboard is built into the binary, so no panel is downloaded and `management.panel-github-repository` is ignored. `disable-control-panel` turns it off, as before, and a client the management API refuses for its address (another machine, without `allow-remote`) is refused the dashboard too, where CLIProxyAPI serves its panel to anyone. See [the dashboard](../UPSTREAM.md#added-in-open-ferry-the-dashboard).
- **The management API can have an address of its own.** open-ferry's `management.separate-address`, such as `127.0.0.1:8318`, serves the management API, the dashboard and the dashboard API there alone, and the proxy's port answers every one of their paths with a 404. CLIProxyAPI has no such setting. Back on CLIProxyAPI, the key is ignored and the management API is on the proxy's port again; a save through its management API of a file in the v8 layout turns the key into a comment at the end of the file, and its `PUT /v8/management/config.yaml` refuses a file with the key as an unknown field.
- **A config change through the API takes effect before the answer.** CLIProxyAPI answers first and reloads after; open-ferry saves the file, loads it again and then answers. A save keeps the file's comments and the settings open-ferry doesn't type, as CLIProxyAPI's does, replaces the file in one step and keeps the previous one as `config.yaml.bak`. A save that fails changes neither the file nor the running config, where CLIProxyAPI keeps the change in memory. Editing `config.yaml` by hand still works; the change is picked up on its own. See [Config writes](../UPSTREAM.md#config-writes).
- **A save never writes over a change made in the file.** One made after the config file changed on disk, before open-ferry loaded it, answers 409 `config_changed` and writes nothing; open-ferry loads the file, so the save made again keeps both changes, or, when the file is empty or doesn't load, keeps answering 409 until it is fixed. CLIProxyAPI writes its config over the file. See [The management API](../UPSTREAM.md#the-management-api).
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
