# Setting up from the command line: `init`, `check` and `service`

open-ferry takes CLIProxyAPI's flags ([migration guide](migrating-from-cliproxyapi.md#the-command-line)), and adds subcommands of its own:

- **`open-ferry init`** writes a starting config, with new keys.
- **`open-ferry check`** looks over a setup before you start the proxy, and says how to fix what it finds.
- **`open-ferry service`** installs open-ferry as a background service, started when you log in or at boot and again when it fails, and removes it.
- **`open-ferry status`, `config`, `keys`, `credentials`, `clients` and `mcp`** look at a setup and change it, for you or a coding agent ([below](#looking-at-and-changing-a-setup)).

A subcommand is read only as the first argument, and the flags after it are its own; `open-ferry init -h`, `open-ferry check -h` and `open-ferry service -h` list them. With a flag first, the command line is read as before: `open-ferry -h init` prints the server's usage. CLIProxyAPI has no subcommands: it ignores a first argument that isn't a flag, and serves.

## A first setup

```sh
open-ferry init
open-ferry check -config <the path init printed>
open-ferry -config <the path init printed>
```

`init` prints the commands to run next, with the path filled in. Then add your upstream credentials: from the commented examples in the config, by signing in, or in the dashboard.

To keep the proxy running in the background instead, run `open-ferry service install`. Without `-config` it uses the installed config path, where `init` wrote the config.

## `open-ferry init`

```
open-ferry init [-config PATH] [-host HOST] [-port N] [-force]
```

| Flag | What it does |
|---|---|
| `-config PATH` | Write the config to `PATH`. By default, the installed config path (below) |
| `-host HOST` | The host the proxy listens on. By default `127.0.0.1`, so only this machine can reach it. `-host ""` listens on every interface |
| `-port N` | The port the proxy listens on, from 1 to 65535. By default 8317, the template's |
| `-force` | Replace an existing config. The old file is kept as `<file name>.bak` |

The installed config path is:

- on Linux and macOS, `$XDG_CONFIG_HOME/open-ferry/config.yaml`, or `~/.config/open-ferry/config.yaml` when `XDG_CONFIG_HOME` isn't set or isn't an absolute path;
- on Windows, `%APPDATA%\open-ferry\config.yaml`.

`init -h` shows it for your system. The proxy doesn't look there by itself: like CLIProxyAPI, it reads the file `-config` names, else `config.yaml` in the working directory. So start it with `-config`, as `init` says.

### What it writes

The config is `config.example.yaml`, the template built into the binary, with all of its comments, and four settings changed:

- `access.api-keys`: the template's example keys, which keep the proxy in safe mode, are replaced with one new client key;
- `management.secret-key`: a new management key;
- `server.host`: `127.0.0.1`, or `-host`;
- `server.port`: `-port`, or the template's.

The keys come from the operating system's random number generator:

- the client key is `sk-` and 32 random bytes in URL-safe base64, as the dashboard's new client keys are;
- the management key is 32 random bytes in hex.

Neither needs quoting in YAML or in a shell. Both are kept in the config file as they are, so keep the file private.

Before it writes, `init` loads the new config as the proxy would, and checks that it isn't in safe mode. It creates the file's directory when it is missing, and writes the file in one step, so a failure leaves no half-written config.

It won't replace an existing file unless you pass `-force`. With `-force`, the old file is kept as `<file name>.bak`, which replaces an older `.bak`.

**Who can read it.** On Linux and macOS the file is `0600`: only you can read it. A directory `init` creates is `0700`. On Windows, open-ferry doesn't set the file's permissions: it has the permissions it inherits from its folder. Under `%APPDATA%`, only you, administrators and SYSTEM can read it. If you write it elsewhere with `-config`, choose a folder only you can read.

### What it prints

On success, `init` prints to standard output (the keys here are shortened):

```
Wrote a new config to /home/you/.config/open-ferry/config.yaml
Only you can read it (0600).

Client key:     sk-3q2-7wA…
Management key: 9f86d081…

These keys aren't shown again; they are kept in the config file.
Clients send the client key as their API key.
The management key signs you in to the dashboard.

Dashboard: http://127.0.0.1:8317/dashboard/

Start the proxy with this config:
  open-ferry -config /home/you/.config/open-ferry/config.yaml
Check the setup first:
  open-ferry check -config /home/you/.config/open-ferry/config.yaml
```

- With `-force` over an existing file, a line after the first says `The old config is kept as <path>.bak`.
- On Windows the second line is `It has its folder's permissions: under %APPDATA%, only you, administrators and SYSTEM can read it.`
- With `-host ""`, a line warns `The proxy listens on every interface, so other machines can reach it.`
- The commands name the program as you ran it. A path with spaces or other special characters is quoted: with `'...'` on Linux and macOS, and with `"..."` on Windows, where a quoted program is run with PowerShell's `&`.

The keys are printed only here, once. To see them again, read the config file. To replace them, edit the file, use the dashboard, or run `init -force` for a whole new config.

### Exit codes

| Code | When |
|---|---|
| 0 | The config was written, or `-h` was asked for |
| 1 | It couldn't be written: the file exists and `-force` wasn't given, there is no installed config path (`APPDATA` or `HOME` isn't set), or a write failed. A line on standard error starting `init:` says why |
| 2 | Bad usage: an unknown flag, an argument that isn't a flag, a host with characters a host can't have, or a port outside 1 to 65535. The usage follows on standard error |

## `open-ferry check`

```
open-ferry check [-config PATH] [-json]
```

| Flag | What it does |
|---|---|
| `-config PATH` | Check this config. By default `config.yaml` in the working directory, as the proxy reads it |
| `-json` | Print one JSON object instead of a line for each finding |

`check` finds the config as the proxy does, and first loads the `.env` file in the working directory, as the proxy does at start, so a `MANAGEMENT_PASSWORD` there counts.

### What it checks

In this order, with the name each finding has:

| Check | What it looks at |
|---|---|
| `.env` | Only when the working directory's `.env` exists but doesn't load: a warning |
| `config` | The config exists and loads with the proxy's own loader. If it doesn't, that error is the only finding |
| `client keys` | `access.api-keys`. The template's example keys, which put the proxy in safe mode, are an error. No key at all is a warning, as any client that reaches the proxy can use it |
| `management key` | `management.secret-key`. Empty is a warning, unless `MANAGEMENT_PASSWORD` sets a key: without one, the management API and the dashboard's sign-in are off |
| `auth directory` | The auth directory can be read. One that doesn't exist yet is fine: the proxy makes it at start |
| `credential <file>` | Each `*.json` file in the auth directory, read as the proxy reads it. A file that isn't valid JSON, isn't a JSON object, or has a setting the proxy refuses (such as a weight that isn't a whole number) is an error. One the proxy skips (empty, with no `type`, or a Gemini CLI credential) is a warning. Only the files with a problem are listed, by name; their contents are never shown |
| `tls` | Only when `tls.enable` is on: the certificate and key load |
| `address` | Whether something already listens on `server.host` and `server.port`: an error, as the proxy couldn't start. It finds out by connecting to loopback (`127.0.0.1` or `::1`), never by binding the port. A connection that fails for any reason other than a refusal, a timeout or an address the machine doesn't have (such as `::1` with IPv6 off) is a warning that says check couldn't tell. An empty host, `localhost`, `0.0.0.0` or `::` is checked on loopback. Any other host isn't checked, with a warning, as that would be a network call. A port of 0 is a warning, as the system would pick one |
| `management address` | Only when `management.separate-address` is set: whether something already listens on that address, checked as for `address`, an error if so. When nothing does, it says the management API and the dashboard are served there alone, not on `server.port`, and that a change takes a restart. A port that is `server.port`'s, or an address that isn't host and port, fails the config's load, so `config` is the only finding |
| `management access` | Only when `management.separate-address` is set, and only as a warning where `management.allow-remote` doesn't fit the address. An address other machines can connect to (every interface, as in `:8318`, or one that isn't loopback) while `allow-remote` is off and `MANAGEMENT_PASSWORD` isn't set: they would be refused. Or a loopback address while `allow-remote` is on and `server.trusted-proxies` is empty: no other machine can connect, so `allow-remote` does nothing. A host name isn't judged, as that would take a lookup |
| `dashboard` | The dashboard app is built into this binary. A binary built without it is a warning. With `management.disable-control-panel` on, it is fine either way. With `management.separate-address` set, the URL it gives is on that address |
| `clock` | The system time, without the network: a time before this binary's build date (release binaries know it), a credential last refreshed more than 10 minutes after the system time, or one that expires more than 400 days after it, is a warning |
| `claude-cli <names>` | Each enabled `claude-cli` entry's Claude Code, once for each command, with `claude --version`, as the proxy checks it at start. A Claude Code older than 2.1.259, or one that can't be run, is an error. It never runs `claude auth status` |

`check` writes nothing, and never prints a key or a credential. Its only connections are those to loopback. It reads the files it checks, and runs each `claude-cli` command with `--version`.

### What it prints

One line for each finding: its level (`ok`, `warning` or `error`), padded to 7 characters, the check's name, a colon, and what it found. A warning or an error ends with `. Fix: ` and what to do. The last line counts the errors and warnings:

```
ok      config: config.yaml loads
ok      client keys: 1 key set
ok      management key: set
ok      auth directory: /home/you/.cli-proxy-api can be read; 2 credential files load (see below for the others)
error   credential broken.json: isn't valid JSON (line 1, column 15). Fix: remove it, or sign in again to write a new one
ok      address: nothing listens on 127.0.0.1:8317
ok      dashboard: built in, at http://127.0.0.1:8317/dashboard/
ok      clock: the system time, 2026-10-07T21:22:07Z, agrees with the credentials' times
1 error, no warnings
```

With `-json`, it prints one JSON object on one line instead. `status` is the worst level found, and a finding has a `fix` only when it isn't `ok`:

```json
{"config":"config.yaml","status":"error","errors":1,"warnings":0,"findings":[{"level":"ok","check":"config","message":"config.yaml loads"},{"level":"error","check":"credential broken.json","message":"isn't valid JSON (line 1, column 15)","fix":"remove it, or sign in again to write a new one"}]}
```

### Exit codes

| Code | When |
|---|---|
| 0 | No finding is an error; warnings are allowed. Also for `-h` |
| 1 | A finding is an error |
| 2 | It couldn't check: an unknown flag, an argument that isn't a flag, or a working directory that can't be read. A message on standard error says why |

So a script or a service can run `open-ferry check -config <path>` before it starts the proxy, and stop on a non-zero code.

## `open-ferry service`

```
open-ferry service install [-config PATH] [-system] [-dry-run]
open-ferry service uninstall [-system] [-dry-run]
open-ferry service status [-system]
```

`service` installs open-ferry as a background service, run by your system's own service manager: systemd on Linux, launchd on macOS, and Task Scheduler or the service manager on Windows. The service starts open-ferry when you log in, or at boot, and starts it again when it fails.

- **`install`** writes the service's definition and starts it.
- **`uninstall`** stops the service and removes what `install` made, and nothing else. Your config, the auth directory and the logs stay.
- **`status`** says where the service is defined, then shows what the service manager says about it.

| Flag | What it does |
|---|---|
| `-config PATH` | `install` only. The config the service runs with, made a full path. By default, the installed config path (see [`open-ferry init`](#open-ferry-init)) |
| `-system` | A service for the whole machine, started at boot as root, or LocalSystem on Windows, instead of one for you. It needs root or an administrator; see [below](#a-service-for-the-whole-machine) |
| `-dry-run` | `install` and `uninstall` only. Print the files it would write and the commands it would run, and change nothing. A dry run of `-system` works without root or an administrator |

The flags come after the action, and an argument after them is an error. `-h`, after `service` or after an action, prints the usage.

`install` refuses:

- a config that doesn't exist (make one with `open-ferry init`, or name another with `-config`);
- a config that doesn't load;
- a service that is already installed. To install it again, for a new config or a new binary, run `open-ferry service uninstall` first, adding `-system` if it is the machine's.

The service runs the binary that ran `install`, by its full path, as `open-ferry -config <config>`. If you move or replace that binary, install the service again.

### What it installs

| | Your service (default) | The machine's (`-system`) |
| --- | --- | --- |
| Linux | A systemd user unit, `~/.config/systemd/user/open-ferry.service`, run by `systemctl --user` | A systemd unit, `/etc/systemd/system/open-ferry.service`, started once the network is up |
| macOS | A launchd agent, `~/Library/LaunchAgents/io.github.loft-902-co-llc.open-ferry.plist`, loaded into your login session | A launchd daemon, `/Library/LaunchDaemons/io.github.loft-902-co-llc.open-ferry.plist` |
| Windows | A scheduled task, `open-ferry`, that runs as you when you log on | A Windows service, `open-ferry`, that runs as LocalSystem from boot |
| Its output | `journalctl --user -u open-ferry`; `~/Library/Logs/open-ferry.log`; `service.log` beside the config | `journalctl -u open-ferry`; `/Library/Logs/open-ferry.log`; `service.log` beside the config, and the System event log |

Every service runs open-ferry in the config's directory, so the `.env` file there is loaded. When open-ferry fails, systemd and the Windows service start it again after 5 seconds, and launchd no sooner than 10 seconds after it last started it. The scheduled task waits 5 seconds too, then longer, up to a minute, while open-ferry keeps failing soon after it starts. An open-ferry that exits cleanly isn't started again.

**On Linux**, a user unit runs while you are logged in. To start it at boot and keep it running after you log out, run `loginctl enable-linger` once. `systemctl --user` needs your user session's bus: over `su` or `sudo -u`, it may fail with "Failed to connect to bus"; log in as that user instead.

**On macOS**, the agent is loaded with `launchctl bootstrap gui/<your uid>`, which needs you to be logged in to the Mac's desktop. On a Mac no one is logged in to, as one you only reach over SSH may be, `install` fails; use `-system` there.

**On Windows**, the service is a scheduled task by default, not a Windows service. That is deliberate:

- **Your sign-ins live in your profile.** The config's `~` paths, the auth directory's among them, are the home of the account open-ferry runs as. A Windows service runs as a service account, whose profile isn't yours, so it wouldn't see your sign-ins.
- **A service that runs as you needs your password**, stored with the service manager, and changed there each time you change it.
- **A scheduled task runs as you** when you log on, with no password stored, and needs no administrator to register.

The task runs `open-ferry service run -config <config>`, which starts open-ferry with no window, its output going to `service.log` in the config's directory, and starts it again when it fails. `service.log` is moved to `service.log.1` as open-ferry starts, once it is past 10 MiB. Ending the task, or logging off, ends open-ferry with it. A console window may flash as the task starts. `service run` is only for the task and the Windows service to start; it isn't in the usage.

### A service for the whole machine

With `-system`, the service starts at boot, with no one logged in, as root on Linux and macOS and as LocalSystem on Windows. `install -system` and `uninstall -system` need root (`sudo`) or, on Windows, a terminal opened with "Run as administrator". Without them, `service` refuses and prints the command to run.

As the service runs with root's rights, `install -system` asks more of the config and the binary:

- **The auth directory must be a full path.** A `~` would be root's home, or LocalSystem's profile, not yours, and the default, `~/.cli-proxy-api`, is one. Set `auth-dir` to a path such as `/var/lib/open-ferry` or `C:\ProgramData\open-ferry\auth`, and sign in again there, or copy your credential files over.
- **Only root, or the administrators, may change the binary and the config.** Otherwise anyone who could change them could run code as root.
  - On Linux and macOS, the binary and the config, after following links, and every directory above them, must be owned by root and writable by no other user or group (a directory with the sticky bit, such as `/tmp`, doesn't count). `/usr/local/bin/open-ferry` and `/etc/open-ferry/config.yaml` will do on most systems. On an Intel Mac with Homebrew, `/usr/local/bin` is yours, so use a directory such as `/opt/open-ferry`; on systems where `/usr/local` is writable by the `staff` group, likewise.
  - On Windows, the binary and the config must be under `C:\Program Files`, `C:\Program Files (x86)` or `C:\Windows`. Copy them to `C:\Program Files\open-ferry`, for one. `C:\ProgramData` doesn't count: every user may add files there.
- **Run as root, you get no service of your own.** On Linux and macOS, `install` without `-system`, run as root, is refused: run it as the user the service is for, or add `-system`.

On Windows, `install -system` creates the service with `sc.exe`, to start at boot and again 5 seconds after any failure, an exit with an error included. The service writes to `service.log` beside the config, and the service manager logs its starts and stops in the System event log. The auth directory can be under `C:\ProgramData`, but every user may add files there unless you take that right away; for example, as an administrator:

```bat
icacls C:\ProgramData\open-ferry /inheritance:r /grant:r "SYSTEM:(OI)(CI)F" "Administrators:(OI)(CI)F"
```

### The service's environment

A service doesn't start from your shell, so it doesn't get your shell's environment:

- **Variables** the server reads, such as `MANAGEMENT_PASSWORD` or `CLAUDE_CODE_OAUTH_TOKEN`, go in the `.env` file beside the config. A variable the service manager already sets wins over the file.
- **`PATH`** is the service manager's, not your shell's: on Linux and macOS often only `/usr/bin` and `/bin` and their like, and for the Windows service the machine's. As `PATH` is always set, `.env` can't change it. If you use [`claude-cli`](claude-subscription.md), give each entry's `command` as the full path to `claude`, which `command -v claude` (or `where claude` on Windows) prints.
- **The working directory** is the config's directory.

### Uninstalling

`open-ferry service uninstall` stops the service, then removes its definition: the unit, the property list, the task or the Windows service. Add `-system` for the machine's. If the service wasn't running, it goes on. It never removes the config, the auth directory, `.env`, or the logs; remove those yourself if you want them gone.

### Exit codes

| Code | When |
|---|---|
| 0 | It did what was asked: the service was installed, removed or shown, or a dry run printed its plan. Also for `-h` |
| 1 | It didn't: the config is missing or doesn't load, the service is already installed (for `install`) or isn't (for `uninstall` and `status`), it needs root or an administrator, a path or the auth directory is refused for `-system`, or a command failed. A line on standard error starting `service:` says why |
| 2 | Bad usage: no action or an unknown one, a flag the action doesn't take, or an argument after the flags. The usage follows on standard error |

## Looking at and changing a setup

These commands look at a setup and change it, for you or for a coding agent, without editing the YAML or calling the management API by hand: `status`, `config`, `keys`, `credentials` and `clients`, and `mcp`, which serves the same actions as MCP tools ([docs/mcp.md](mcp.md)). CLIProxyAPI has none of them. [docs/agents.md](agents.md#look-at-it-and-change-it-the-commands-and-the-mcp-server) is the short version.

```
open-ferry status
open-ferry config get|set|unset|show|diff|undo|replace ...
open-ferry keys list|add|remove ...
open-ferry credentials list|enable|disable|reset-quota|remove|login ...
open-ferry clients setup <client> ...
open-ferry mcp
```

`open-ferry <command> --help` lists them all with their flags. A flag takes one dash or two (`-json`, `--json`), with its value after `=` or as the next argument; `--` ends the flags.

### Flags every command takes

| Flag | What it does |
|---|---|
| `--config PATH` | The config to work on. By default `config.yaml` in the working directory, else the installed config path (see [`init`](#open-ferry-init)) |
| `--management-key-file PATH` | A file whose first line is the management key (see [The management key](#the-management-key)) |
| `--json` | Print one JSON object on standard output, the same one the MCP tool returns, instead of text. A failure is printed as JSON on standard output too |
| `--yes`, `-y` | Go ahead with a change that needs a confirmation (see [What needs `--yes`](#what-needs---yes)) |
| `--help`, `-h` | Print the usage |

Like the proxy, they first load the `.env` file in the working directory, so a `MANAGEMENT_PASSWORD` there counts.

### How they reach the server

- **A server running for the config** is looked for at its management address: `management.separate-address` when it is set, else `server.host` and `server.port`, with `https` while `server.tls.enable` is on. Only loopback is tried: an empty host, `0.0.0.0` and `::` are reached on loopback, `localhost` on `127.0.0.1` and `::1`. A host name isn't looked up and an address that isn't loopback isn't tried, so the management key is never sent off the machine.
- **While one runs and takes the key**, a change goes through its management API: the server checks it, saves it with its config writer and applies it at once, as it does one made in the dashboard. The server is the only writer then, so a change here and one in the dashboard don't lose each other, but for one race:
- **One race is left on that path.** The management API's routes, as upstream has them, take no precondition: a request can't say which config it was worked out from. A command checks that the server runs the same bytes as the file just before it sends the change, but a save from the dashboard or the management API that lands between that check and the change's request isn't seen: the change is applied to the config as the server then holds it, so a save of another setting is kept, but one of the same setting (of any setting, for `config replace`) is overwritten, without asking, and `--expect-sha256` can't catch it. The window is about one request long. The report lists every setting that changed, the dashboard's too, and `config undo` puts back the file as it was before the change.
- **With no server**, with its management API off, or with no key to call it with, `config` and `keys` change the file with the same checks and the same writer. A server watching the file loads the change; one started later reads it. `credentials` needs the server, and says so with exit code 4 and how to start it.
- **A file change while a server runs** (with no key to call it with) is the server's to load, if it runs this config: without the key, the command can't ask it which config it runs. Until it has, and for good when the change doesn't load, a save of the server's own settings, from the dashboard or the management API, writes the settings it holds over the file, and so can undo the change. The note under such a change says so; give the commands the management key (`MANAGEMENT_PASSWORD` or a key file) so the change goes through the server instead. With the management API off, the server writes nothing.
- **A server that refuses the key** stops the command, with nothing changed: each refusal counts towards the server's ban of an address after five failed attempts in thirty minutes.
- **A server that runs another config**, such as one started from another file on the same port with the same key, is never changed or asked about. A server counts as running for the config only when the file it runs, as `GET /v0/management/config.yaml` gives it, holds the same bytes as the config here. Otherwise each command stops with `a server at ADDR runs another config` (`other_config`, exit code 1), nothing changed, and the hint to pass `--config` with the path of the config that server runs, or to give this config a port nothing else uses; `status` says `"management": "other_config"` and exits with 4.
- **A server that runs a copy of the config**, another file that holds the same bytes, counts as running it: the server doesn't say which file it runs, so the two can't be told apart until it writes. A change then goes to the server and the file it runs, and this file stays as it was; the report says so in its `note`, with `"changed": true` and the changes made to the server's config. Give `--config` the path of the file the server runs to work on that one.
- `config get`, `show` and `diff`, and `keys list`, read the file, which is what the server runs once it has loaded it.

Every write of the config keeps the file it replaces as `<config>.bak`, by these commands, the dashboard or the management API alike. Each holds an exclusive lock on `<config>.lock` from its read of the config to its write, so two never interleave, even from two processes; one that waits more than 5 seconds for it fails and writes nothing. The empty `.lock` file is left beside the config. Every change prints each setting it changed, with its old and new value masked, and that `open-ferry config undo` reverses it.

A change is made only to the file it was worked out from. When the file changes after it was read, by another write or a hand edit, the change is worked out again from the file as it is, with the same checks, and made only if it then needs no confirmation: a yes at the terminal, or `--yes`, was given for the change as first worked out, so one that needs a confirmation once worked out again is refused (`config_changed`, exit code 1), nothing changed, so you can look again. It is refused the same way if the file keeps changing.

### The management key

A command that calls the server looks for the management key in this order:

1. the config's `management.secret-key`, when it is stored plain, not as the bcrypt hash the server makes of it when `management.secret-key-hashing` is on;
2. `MANAGEMENT_PASSWORD`, from the environment or `.env`;
3. the first line of the file `--management-key-file` names, else of the file `OPEN_FERRY_MANAGEMENT_KEY_FILE` names.

The key is never taken from the command line, where process lists, shell history and agent transcripts would keep it: `--management-key`, `--management-password`, `--password`, `--secret-key` and `--api-key` are refused with exit code 2, as is any other flag whose name names a secret (`--token`, `--client-secret`, `--cookie` and the like). Such a flag isn't named in the error (it says `a flag that names a secret`), as its name can be a secret pasted where a flag goes. An unknown flag is named in the error without its value, and one with an odd or long name isn't repeated at all; an unknown subcommand or argument isn't repeated either (`an unknown command or argument after config`). With a hashed key in the config and no other, the settings commands change the file and the others say there is no key.

### What needs `--yes`

- deleting a credential (`credentials remove`) or a client key (`keys remove`);
- showing a secret in full (`keys list --reveal`, `clients setup --reveal`);
- replacing the whole config (`config replace`);
- a Claude sign-in (`credentials login claude`), which goes against Anthropic's terms ([why](claude-subscription.md#the-claude-sign-in));
- a change to a sensitive setting, by `config set`, `unset`, `replace` or `undo`, or `keys remove`:
  - `management.allow-remote`, `management.secret-key` and `management.separate-address`;
  - `server.host`, set to anything but `localhost` or a loopback address, or unset;
  - anything under `server.tls`;
  - anything under `server.trusted-proxies`, which decides whose forwarded addresses are believed, and so which clients count as local to the management API;
  - removing the last client key in `access.api-keys`. Keys are counted as the server counts them, trimmed and without empty or repeated ones, so `[""]` or `["  "]` counts as no key.

At a terminal, such a command asks, and goes ahead on `y`. When standard input or standard error isn't a terminal, as in a script or an agent's shell, or with `--from-stdin`, it changes nothing, exits with 3, and says what it would change:

```
Setting server.host needs --yes: server.host would be 0.0.0.0, which isn't loopback, so the proxy listens beyond this machine. Nothing was changed.
It would change:
  server.host: "127.0.0.1" -> "0.0.0.0"
to go ahead, run it again with --yes --expect-sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
```

With `--json`, the same is `{"error": "needs_confirmation", "message": ..., "hint": ..., "would": {"changes": [{"path": "server.host", "old": "127.0.0.1", "new": "0.0.0.0"}], "reasons": [...], "config_sha256": "9f86d081..."}}`.

`config_sha256` is the SHA-256 of the config file the change was worked out from. Give it back with `--expect-sha256 HASH`, which `config set`, `unset`, `undo` and `replace` and `keys add` and `remove` take, and the change is made only to that file: when the file changed since, by another write or a hand edit, the command is refused (`config_changed`, exit code 1), nothing changed, and the change isn't worked out again, so you can look at it again (run it without `--yes` for what it would change now, and its new `config_sha256`). Without `--expect-sha256`, `--yes` goes ahead with the change as worked out from the file when it runs, which can differ from the one you looked at. A script or an agent that shows someone what a change would do, then runs it with `--yes`, should always give it. `config_sha256`, and an undo's `backup_sha256`, are hashes of whole files, not secrets, and are safe to show.

### Secrets in what they print

Nothing they print holds a secret they weren't asked to show. Client keys, the management key, provider API keys, tokens and passwords are masked as the dashboard masks a client key (`sk-...mnop`: the last few characters, and the first three of a long one), a number or a boolean as a string is, since the server reads one there as a string key (`access.api-keys: [true]` shows as `["..."]`); a URL's user and password become `***`; email addresses are masked (`s***@e***.com`). Then every secret of the config, `MANAGEMENT_PASSWORD`, the key file and the credential files in the auth directory (their tokens and keys), and every secret of the config a change would make, is replaced by `[redacted]` wherever it still appears, as in a URL's path, the question at the terminal included. A config or a value that doesn't load is said not to, with the kind of problem and where it is (a line, a setting), never in the loader's words, which can quote a value.

A secret is never taken as an argument: `config set` reads one from standard input (`--from-stdin`) or a file (`--from-file`), and refuses one given inline with exit code 2. A value is a secret when its key names one (`secret-key`, `api-key`, `token`, `password` and the like), whether it is a string, a number or a boolean, or when it holds one, as a provider's list of keys does. A value `config set` reads from standard input or a file shows only as `"[redacted]"` wherever a change shows it (what it would change, the question at the terminal, and what it changed): one change at the setting given, with nothing of the value in it, not its keys, its shape or its length. A key `keys add` or `keys remove` reads is a client key, and shows as every client key does, masked as `keys list` masks it. `config replace` is the exception: its file is a whole config, and what it would change is shown masked setting by setting, as `config diff` shows it, so you can see what it does.

A file is read only for a secret, which is never shown: `config set` refuses a file's value for a setting that holds no secret, with exit code 2 (give that one as an argument). And no command reads a value from a file in the auth directory, or from a credential file anywhere: it is refused (`unsafe_file`, exit code 1), so a sign-in's tokens or a key are never copied into the config. A credential file is one that holds a PEM block (`-----BEGIN ...-----`, as a private key or a certificate is kept in), or, in JSON or YAML, any of these fields with a value, at any depth up to 32 levels of mappings and lists: `access_token`, `refresh_token`, `id_token`, `private_key`, `client_secret`, `tokens` and `session_key`, in any case and with or without `_`, `-` or `.` (so `accessToken`, `access-token` and `AccessToken` too). The refusal names the field, never its value. A JSON or YAML file nested deeper than 32 levels can't be checked, so it is refused too (`unsafe_file`, "too deeply nested to check").

### Exit codes

| Code | When |
|---|---|
| 0 | Done, or nothing needed changing. Also for `--help` |
| 1 | It failed or was refused: the server or the writer refused the change (`invalid_value`), a file a value comes from is in the auth directory or is a credential file (`unsafe_file`), the key was refused (`unauthorized`), the server at the config's address runs another config (`other_config`), the file changed while the change was worked out (`config_changed`), an undo would lose a change made since the last backup and wasn't confirmed (`changed_since`), a credential or key wasn't found, the file couldn't be read or written. Nothing was changed unless the output says so |
| 2 | Bad usage (`usage`): an unknown command or flag, or an unknown setting (`unknown_path`, with the nearest known one), or a secret given as an argument (`secret_in_argument`) |
| 3 | It needs `--yes` and didn't get it (`needs_confirmation`), or you answered no (`declined`). Nothing was changed |
| 4 | It needs the server, which isn't running (`not_running`); `status` exits with 4 when no server runs |

A failure prints its message and a hint on standard error; with `--json`, `{"error": <code>, "message": ..., "hint": ...}` on standard output.

### `open-ferry status`

```
open-ferry status [--config PATH] [--json]
```

Whether a server runs for the config, and how it is: its version, addresses, whether the management API took the key and where the key came from, its credentials by state, today's calls and errors (from the usage ledger), and the number of client keys. With no server, it says why and what the config holds, and exits with 4.

```
Config: /home/you/.config/open-ferry/config.yaml
Server: running, version 0.1.0
Proxy address: http://127.0.0.1:8317
Management API: reached with the key from the config's management.secret-key
Credentials: 3 (2 ready, 1 resting)
Calls today: 412 (3 errors)
Client keys: 1
```

```json
{
  "config": "/home/you/.config/open-ferry/config.yaml",
  "running": true,
  "version": "0.1.0",
  "address": "http://127.0.0.1:8317",
  "management_address": "http://127.0.0.1:8317",
  "management": "ok",
  "key_source": "config",
  "credentials": {"ready": 2, "resting": 1},
  "calls_today": {"requests": 412, "errors": 3},
  "client_keys": 1
}
```

`management` is `ok`, `no_key` (no key to call it with), `off` (the server has no management key), `refused`, or `other_config` (a server answers at the address, but runs another config, so `running` is `false`); `key_source` is `config`, `MANAGEMENT_PASSWORD` or `key-file`. What it couldn't find out, such as today's calls when the usage ledger is off, is under `notes`. With no server, it prints `"running": false` and a `reason`, such as `nothing answers at http://127.0.0.1:8317`.

### `open-ferry config`

```
open-ferry config get <path>
open-ferry config set <path> <value> | --from-stdin | --from-file FILE [--string]
open-ferry config unset <path>
open-ferry config show
open-ferry config diff
open-ferry config undo
open-ferry config replace --from-stdin | --from-file FILE
```

A path is the v8 config's, as the `/v8/management/config/` route and the dashboard's Settings page take it: keys joined by dots, such as `routing.strategy`, `server.port`, `routing.quota.prefer` or `access.api-keys`, or by `/` when a key holds a dot. CLIProxyAPI's settings and open-ferry's own are reached the same way. An unknown path is refused with exit code 2 and the nearest known one (`did you mean routing.strategy?`). A list is set whole, not one item at a time.

- **`get`** prints a setting, masked; a mapping as YAML. When it isn't set, it says so, with the default the server uses when it has one (`"set": false` and `"default"` in the JSON).
- **`set`** takes the value as YAML or JSON (`true`, `5`, `[a, b]`, `{"x": 1}`), or as a string, as it is, with `--string`. The server checks it as it checks a change from the dashboard; a value it would refuse is refused before anything is written. A secret must come with `--from-stdin` or `--from-file`.
- **`unset`** removes a setting, so its default applies.
- **`show`** prints the whole config, masked, as YAML (as JSON under `settings` with `--json`).
- **`diff`** prints what the last change made: each setting that differs between `<config>.bak` and the config.
- **`undo`** puts `<config>.bak` back, and keeps the config it replaces as the new `.bak`, so running it again redoes the change. With a server running, it goes through the dashboard API's `POST /open-ferry/api/v1/config/undo`, under the lock every management write takes. There is one backup, so it goes back one write.
  - When the config was changed since the last write that kept a backup, as by a hand edit, undoing loses that change too: without `--yes` it changes nothing, exits with 1 (`changed_since`) and lists what it would change; at a terminal it asks.
  - It puts back only the backup it showed, over the config it showed: their SHA-256 go with it, and when either changed after that, nothing is undone (`config_changed`, exit code 1).
  - When it needs `--yes`, its answer gives the SHA-256 of the backup it would put back as well (`would.backup_sha256`). Give it back with `--expect-backup-sha256 HASH`, besides `--expect-sha256`, and the undo puts back only that backup: another write can change the backup and leave the config as it was, and the undo is then refused (`config_changed`).
  - When `<config>.bak` holds the same bytes as the config, there is nothing to undo: it says so, calls no server and writes nothing.
- **`replace`** replaces the whole config with the YAML read from standard input or a file, after the same checks. It always needs `--yes`.

```
$ open-ferry config set routing.strategy fill-first
Setting routing.strategy: done, through the running server.
  routing.strategy: (not set) -> "fill-first"
Undo it with `open-ferry config undo`.
$ open-ferry config undo
Undid the last change: through the running server.
  routing.strategy: "fill-first" -> (not set)
Run `open-ferry config undo` again to redo it.
```

With `--json`, a change prints:

```json
{
  "action": "set",
  "path": "routing.strategy",
  "changed": true,
  "via": "server",
  "changes": [{"path": "routing.strategy", "new": "fill-first"}],
  "undo": "Undo it with `open-ferry config undo`."
}
```

`via` is `server` or `file`; a change made in the file has a `note` that says why. `changes` lists each setting it changed, with `old` and `new` masked, and leaves `old` or `new` out when the setting wasn't or isn't set. A change to what the config already holds prints `Nothing to change: the config already holds that.`, with `"changed": false`.

### `open-ferry keys`

```
open-ferry keys list [--reveal]
open-ferry keys add --generate [--to-file FILE] | --from-stdin | --from-file FILE
open-ferry keys remove <index> | --from-stdin | --from-file FILE
```

The client keys in `access.api-keys`, which clients send as their API key.

- **`list`** prints them masked, with their indexes. `--reveal --yes` prints them in full.
- **`add --generate`** makes a new key as `init` does (`sk-` and 32 random bytes in URL-safe base64), adds it, and prints it once, since you need it to set up a client. `--to-file FILE` writes it to `FILE` instead, which mustn't exist and which, on Linux and macOS, only you can read. `--from-stdin` and `--from-file` add a key you have.
- **`remove`** removes a key by its index from `list`, or the key read from standard input or a file. It needs `--yes`. Through a running server it removes the key by its place in the server's list, as the management API takes it (the key itself never goes in a URL), and reads the list again after: when another write moved the keys meanwhile, so that the keys gone aren't just this one, it says so (`key_list_changed`, exit code 1) with the keys gone, masked.
- In the file, `add` and `remove` change the list the file holds when the change is written, not the one read first: a key another write added or removed meanwhile stays so. A key to remove that is gone by then is `not_found`, and one to add that is there by then is `exists`, nothing changed.

```
$ open-ferry keys list
1 client key in access.api-keys, masked:
  0  sk-...mnop
$ open-ferry keys add --generate
Adding a client key: done, through the running server.
  access.api-keys: ["sk-...mnop"] -> ["sk-...mnop","sk-...ghmE"]
Undo it with `open-ferry config undo`.
The new client key, shown this once, so keep it now:
  sk-<the new key>
```

With `--json`, `list` prints `{"count": 1, "keys": [{"index": 0, "key": "sk-...mnop"}], "revealed": false}`, and `add` adds `index` (its place in the file after the change), `masked` and `key` (or `key_file`) to a change's fields.

The first change a running server saves can also write the settings it was using at their defaults into the file, which `changes` lists, with a note; that doesn't change what the server does.

### `open-ferry credentials`

```
open-ferry credentials list [--state STATE] [--provider PROVIDER]
open-ferry credentials enable <credential>
open-ferry credentials disable <credential>
open-ferry credentials reset-quota <credential>
open-ferry credentials remove <credential>
open-ferry credentials login codex|claude [--no-wait] [--state STATE]
```

The running server's credentials: its sign-ins and its API keys. Each needs the server. `<credential>` is a credential's `auth_index` from `list`, or its name.

- **`list`** prints, as the dashboard's Credentials page shows them, each credential's state, provider, `auth_index`, name and account (masked), cooldowns, quota and priority. A state is `ready`, `resting` (cooling down), `failing`, `refreshing`, `waiting`, `off` (disabled) or `unknown`.
- **`enable`** and **`disable`** turn a credential on or off; it is kept, and `disable` is reversed by `enable`.
- **`reset-quota`** clears a credential's cooldowns and quota state, so it can be picked again at once.
- **`remove`** deletes the credential's file. It needs `--yes`, and `config undo` doesn't bring it back.
- **`login codex`** starts a ChatGPT sign-in through the server, prints the address to open in a browser, and waits for it to finish, for up to five minutes; Ctrl-C cancels it. `--no-wait` prints the address and a `state` and returns; `--state STATE` waits for that sign-in. A sign-in still going when the wait ends exits with 1 and says how to wait again. `login claude` needs `--yes`, as signing in to Claude that way goes against Anthropic's terms; use [`claude-cli`](claude-subscription.md) instead, which signs in with your own Claude Code, never from here.

```
$ open-ferry credentials list
ready      codex        870dd779bc223fa9   codex-s***@e***.com.json  s***@e***.com
1 credential(s): 1 ready
$ open-ferry credentials disable 870dd779bc223fa9
codex-s***@e***.com.json is disabled.
Reverse it with `open-ferry credentials enable 870dd779bc223fa9`.
```

With `--json`, `list` prints `{"count": 1, "counts": {"ready": 1}, "credentials": [{"name": ..., "auth_index": ..., "provider": "codex", "account": "s***@e***.com", "state": "ready", "disabled": false, "source": "file", ...}]}`, and `disable` prints `{"action": "disable", "credential": ..., "auth_index": ..., "changed": true, "disabled": true, "undo": ...}`.

### `open-ferry clients setup`

```
open-ferry clients setup <client> [--model MODEL] [--shell posix|powershell] [--key-index N] [--reveal]
```

Prints what the dashboard's client setups give for `<client>`, one of `openai-python`, `openai-node`, `codex`, `claude-code` and `curl`: the base URL, the environment variables, and the config snippet or sample request, with a model the server serves (else `--model`, else `<model>`) and the client key masked. `--reveal --yes` fills the key in. It only prints: it never writes another program's config, such as `~/.codex` or `~/.claude`. With no server running, it works from the config alone.

### `open-ferry mcp`

```
open-ferry mcp [--config PATH] [--management-key-file PATH]
```

Serves these commands as tools to an agent's app over the Model Context Protocol, on standard input and output; see [docs/mcp.md](mcp.md). It logs nothing on standard output, which carries the protocol, and ends when its input does.

## The terminal UI

`open-ferry -tui` is CLIProxyAPI's terminal UI, a client of the management API ([migration guide](migrating-from-cliproxyapi.md#the-command-line)). On its own it connects to `-management-base-url`, else to the config's `management.base-url`, else, when `management.separate-address` is set, to that address, else to 127.0.0.1 at `server.port`. A management address on every interface, such as `:8318`, is reached on 127.0.0.1, and with `https` while `tls.enable` is on. With `-standalone`, it waits for the server it starts at the management address when one is set, as the management API is served only there, and at `server.port` otherwise.
