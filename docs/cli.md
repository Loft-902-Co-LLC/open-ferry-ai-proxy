# Setting up from the command line: `init` and `check`

open-ferry takes CLIProxyAPI's flags ([migration guide](migrating-from-cliproxyapi.md#the-command-line)), and adds two subcommands of its own:

- **`open-ferry init`** writes a starting config, with new keys.
- **`open-ferry check`** looks over a setup before you start the proxy, and says how to fix what it finds.

A subcommand is read only as the first argument, and the flags after it are its own; `open-ferry init -h` and `open-ferry check -h` list them. With a flag first, the command line is read as before: `open-ferry -h init` prints the server's usage. CLIProxyAPI has no subcommands: it ignores a first argument that isn't a flag, and serves.

## A first setup

```sh
open-ferry init
open-ferry check -config <the path init printed>
open-ferry -config <the path init printed>
```

`init` prints the commands to run next, with the path filled in. Then add your upstream credentials: from the commented examples in the config, by signing in, or in the dashboard.

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
| `address` | Whether something already listens on `server.host` and `server.port`: an error, as the proxy couldn't start. It finds out by connecting to loopback (`127.0.0.1` or `::1`), never by binding the port. An empty host, `localhost`, `0.0.0.0` or `::` is checked on loopback. Any other host isn't checked, with a warning, as that would be a network call. A port of 0 is a warning, as the system would pick one |
| `dashboard` | The dashboard app is built into this binary. A binary built without it is a warning. With `management.disable-control-panel` on, it is fine either way |
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
