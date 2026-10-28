# Updates

open-ferry keeps itself up to date. By default it looks for a newer release every six hours, and when one is out it downloads it, checks its signature, and gets it ready for `open-ferry update` to install. The server never replaces its own binary and never restarts itself. In a supervised service in `auto` mode, the blue-green supervisor starts the staged version, checks it, and switches traffic to it automatically. `notify` only reports new releases. In the plain form, run `open-ferry update` to install one. You can turn this off, for good, in any of the ways below. CLIProxyAPI has nothing like it ([below](#cliproxyapi)).

## Turning updates off

Each of these turns automatic updates off. Only you turn them back on.

- **The command line.** Run:

  ```sh
  open-ferry update -mode off
  ```

  It writes `self-update.mode: off` into the config: the one given with `-config`, else `config.yaml` in the working directory, else the installed config, where `open-ferry init` writes one. For an install for the whole machine, without `-config` it is the machine's config instead, `/etc/open-ferry/config.yaml` on Linux or `C:\ProgramData\open-ferry\config.yaml` on Windows, and changing it needs root or an administrator. With `-config`, `-mode` changes only that file and needs no privilege for the machine records. A privileged process checks that the config and every directory above it are trusted before any read or write (see [the trust rules](cli.md#a-service-for-the-whole-machine)). It writes the file as the management API does: the new file must load first, its comments are kept, the old one is kept as `<file name>.bak`, and it is replaced in one step. A running server that reads that config follows the change at once, with no restart. `-mode notify` keeps the check but installs nothing, and `-mode auto` turns updates back on.
- **The config.** The setting lives in the config, at the top level in both layouts:

  ```yaml
  self-update:
    mode: off # auto (the default), notify or off
  ```

  A running server follows the change when it reloads the config, as it does for any change.
- **The environment.** `OPEN_FERRY_SELF_UPDATE=off` (or `notify`) lowers the mode, for containers, fleets and packages: `off` beats `notify`, which beats `auto`. It never raises it, so `auto` changes nothing, and a value that isn't a mode is ignored with a warning, so a typo can't turn updates on. Set it in the server's environment, or in the `.env` file in its working directory (beside the config, for [a service](cli.md#the-services-environment)). The server reads it at start.
- **The install scripts.** Install with `--no-auto-update` (`-NoAutoUpdate` for `install.ps1`), or with `OPEN_FERRY_INSTALL_SELF_UPDATE=off` (or `notify`, or `auto`) in the environment. Both machine installers honour `OPEN_FERRY_INSTALL_SELF_UPDATE` (`off`, `notify` or `auto`), with the command-line flag taking precedence. Both also honour `OPEN_FERRY_INSTALL_SYSTEM` and `OPEN_FERRY_INSTALL_MIGRATE`, though machine mode always skips migration. Real root ignores other `OPEN_FERRY_INSTALL_*` overrides, `FAKE_*` and tool configuration settings; elevated Windows machine installs ignore the download, GitHub CLI, install-directory, config-path and test overrides and all `FAKE_*` settings. Choose machine paths with command-line options. The script sets the mode in the config with `open-ferry update -mode`, whether it wrote the config or kept yours:

  ```sh
  curl -fsSL https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.sh | sh -s -- --no-auto-update
  ```

  ```powershell
  & ([scriptblock]::Create((irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1))) -NoAutoUpdate
  ```

  Every install ends with a line saying whether open-ferry keeps itself up to date, and the command that turns it off.
- **`open-ferry init`** writes `self-update` into a new config, with `mode: auto` and a comment saying how to turn it off, so the setting is there to change.
- **The dashboard.** Its Settings page has an Updates setting, On, Notify only or Off, which it saves with the others after a review. When `OPEN_FERRY_SELF_UPDATE` holds updates lower than the mode you choose, the page says so: the environment wins until you remove it there and restart the server.
- **The management API.** `PUT /v8/management/config/self-update/mode` with the management key and the body `"off"` (or `"notify"` or `"auto"`), a JSON string. The server follows it at once ([docs/dashboard-api.md](dashboard-api.md#updates)).

With updates off, open-ferry makes no update request of any kind, and stages and installs nothing. A check that is running when you turn them off stops where it is. A release staged before stays unused. `open-ferry update` still works when you run it, and says that automatic updates are off. `open-ferry update -check`, `open-ferry check` and the [status route](dashboard-api.md#get-open-ferryapiv1update) say whether updates are on, notify-only or off, and what set that: the default, the config or the environment.

## The modes

| `self-update.mode` | What the server does |
|---|---|
| `auto` (the default) | Looks for a newer release. When there is one, downloads it, checks it, and stages it in the [data directory](#files). In a supervised service, asks the supervisor to try it and switch traffic after its checks pass. In the plain form, logs once that `open-ferry update` installs it |
| `notify` | Looks for a newer release, and logs once for each new version. Downloads nothing else |
| `off` | Nothing: no request, no staging, no install |

`self-update.check-every` is how often it looks, a Go duration such as `12h`: 6 hours by default, and at least 1 hour (a shorter one is raised to it, with a warning). An install that [doesn't update itself](#which-installs-update-themselves) works as `notify` does in `auto`.

## What is requested, and how often

Besides the calls to your providers, the update check is the only request open-ferry makes without being asked. The first check is 5 to 10 minutes after the server starts, at random, and the next ones every `check-every`, give or take a tenth at random, so servers started together don't check together. The dashboard API's [check now](dashboard-api.md#post-open-ferryapiv1updatecheck) and `open-ferry update` check too.

A check downloads two files of the latest release:

- `https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/SHA256SUMS`, at most 64 KiB;
- `https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/SHA256SUMS.minisig`, at most 4 KiB.

Staging a release downloads one more, this system's archive: `https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/download/v<version>/open-ferry-<version>-<target>.tar.gz` (`.zip` on Windows), at most 256 MiB, within 15 minutes.

- Every request is HTTPS, and so is every redirect, of which it follows at most 10 (GitHub sends each file from its file host). Plain HTTP is allowed only to `127.0.0.1`, `::1` and `localhost`, for testing.
- It goes through the config's `proxy-url` when that is an `http` or `https` proxy, and never through a proxy from the environment, as the rest of open-ferry. A SOCKS proxy is refused, so with one the check fails.
- It sends the User-Agent `open-ferry/<version>`, and nothing about your setup. GitHub sees it as it sees any download.
- `OPEN_FERRY_UPDATE_BASE_URL` replaces `https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases`, for a mirror with GitHub's layout, as `OPEN_FERRY_INSTALL_BASE_URL` does for an unprivileged run of the install scripts (a privileged one ignores it). A mirror can't change what is installed: the signature is still checked.
- A build that trusts no release key ([below](#signed-releases)) makes no request at all.

## How a release is checked

Before anything is installed:

1. **The signature.** `SHA256SUMS.minisig` must be a [minisign](https://jedisct1.github.io/minisign/) signature of `SHA256SUMS` by a key built into the binary. Its trusted comment, which the signature covers, must be `open-ferry <version> SHA256SUMS`, and every archive the list names must be of that version, so an old signed list can't pass for a newer release.
2. **The version.** It must be newer than the installed one. The same version is up to date, an older one is never installed by an update, and a pre-release is ignored.
3. **The archive.** Its SHA-256 must be the one the signed list gives. Only the binary is taken out of it, and the whole archive is refused when an entry has an absolute path, a `..` part, a backslash or a drive letter, is a link, or is too large.
4. **The binary.** It is written to `versions/<version>/` in the data directory and run with `--version`, which must print `open-ferry <version>` within 30 seconds. One that fails is deleted, and automatic checks skip that version until a newer release; `open-ferry update` run by hand tries it again.

The signed list also covers the release's CycloneDX JSON SBOMs: one Rust dependency file per target (`open-ferry-sbom-rust-<target>.cdx.json`) and a separate production dashboard file (`open-ferry-sbom-dashboard.cdx.json`). They are release assets for inspection and scanning; automatic updates need only their target's archive. See [SECURITY.md](../SECURITY.md#signed-releases) for scope and manual verification.

## Which installs update themselves

Only a binary that `install.sh` or `install.ps1` put in place. Each writes `install-receipt.json` in the [data directory](#files), naming the binary it installed, and open-ferry replaces its binary only when:

- the receipt names the binary that runs;
- the binary is called `open-ferry` (`open-ferry.exe`);
- open-ferry can write to its directory;
- it doesn't run in a container: `/.dockerenv` or `/run/.containerenv` exists, or `container` or `KUBERNETES_SERVICE_HOST` is set. This updater check differs from the installer and privileged config checks, which use marker files, including `/var/run/secrets/kubernetes.io`, rather than these variables.

Any other install, such as the container image, a package manager's copy, a build from source, or a copy of the binary under another name or in another place, only says when a release is out, and why it doesn't install it. Update it the way you installed it: `docker pull` for the image, the install script for a binary. For an account install, the receipt is in the data directory of the user who ran the install script, and open-ferry reads it from that of the user it runs as, so a binary installed by one user and run as another only says when a release is out. A machine install keeps its receipt and records in `/var/lib/open-ferry` on Linux or `C:\ProgramData\open-ferry` on Windows. Privileged commands refuse records another account could change; commands that change machine records need root or an administrator. Deleting `install-receipt.json` has the same effect.

A system service installed before this version, as a plain systemd unit or Windows service, keeps using root's or LocalSystem's account records. Reinstall it with `open-ferry service install -system` (uninstall the old service first) for the supervised machine form. Rerunning either binary installer does not reinstall the service; it prints the service-install command.

A service that `open-ferry migrate` installs takes the same form as one from `open-ferry service install`: supervised when the install can update itself, plain otherwise, and `migrate` says why ([docs/cli.md](cli.md#open-ferry-migrate)).

If `install-self` cannot publish the receipt, it reports the failure and tries to restore the previous binary and receipt; the installer fails. If restoration fails too, it names the files to restore by hand.

**A drop-in made by `open-ferry migrate`.** On Linux and macOS with an install receipt, [`migrate`](migrating-from-cliproxyapi.md#the-drop-in-switch) puts a symbolic link under CLIProxyAPI's file name that leads to the installed open-ferry, so `open-ferry update` replaces the binary the link leads to, and the next start runs the new version. On Windows, and without an install receipt, it puts a copy there. The copy is another path than the receipt names, so it reports a new release but doesn't install it. To update it, run `open-ferry migrate -undo`, then `open-ferry migrate` again, after `open-ferry update` has updated the installed one. Only one `migrate` runs at a time: a second one exits with 1 and changes nothing. The lock is per record location, which follows `XDG_CONFIG_HOME`, `HOME` or `APPDATA`: a run that sees another location for the same CLIProxyAPI doesn't wait for the other, and fails when it moves CLIProxyAPI's binary, which never replaces a file, without changing CLIProxyAPI's binary. Don't move or replace `<binary>` or `<binary>.cliproxyapi` by hand while a drop-in is in place: use `open-ferry migrate -undo`, which puts the binary back only if it is the one recorded.

## Installing an update

```sh
open-ferry update
```

While a supervisor runs for this data directory, `open-ferry update` and `-rollback` switch workers on the same listener while existing requests finish. `-check` also shows the supervisor's phase, running version, rollback target, watch deadline and last outcome, and whether switching is disabled and why. `-cancel` asks it to cancel before the new worker takes over. Ctrl-C after acceptance stops following; the supervisor continues the switch. Updating, rolling back or cancelling a machine install needs root or an administrator.

The supervisor refuses an automatic request unless the mode is `auto`. Changing to `notify` leaves an accepted switch running. Of the modes, only `off` cancels an accepted switch, while cancellation is still possible.

Automatic checks skip releases that failed here or were rolled back from. When an attempt ends without a completed switch, automatic updates wait at least five hours before trying that version again. Manual requests are not held by this cooldown.

A timeout, a lost connection or an unreadable reply to a supervised update, rollback or cancellation leaves its outcome unknown. Run `open-ferry update -check` and do not resend the request. The command exits with 1; its JSON result is `outcome-unknown`.

The supervisor switches only while its directories and version binaries are safe from other accounts: owned and writable only by you and the system's own accounts, root on Linux and macOS, and SYSTEM, Administrators and TrustedInstaller on Windows. Otherwise it disables switching or refuses that version, and `open-ferry update -check` says why. On macOS this covers extended ACLs; on Linux, a user service also accepts a directory writable by your private group. [docs/cli.md](cli.md#what-it-installs) has the details.

In the plain form, it checks for the latest release, says what it would do, and asks. Then it downloads the release (or takes the copy the server staged), checks it as above, and puts it in place of the installed binary: it copies the new binary beside the old one and renames it over the old, so the path never holds half a binary. The old binary is kept in the data directory for a rollback. On Windows, which won't replace a running `.exe`, the old one is first renamed `open-ferry.exe.old`, removed at the next update. `-yes` installs without asking; without a terminal to ask in, and without `-yes`, it installs nothing. Its flags and exit codes are in [docs/cli.md](cli.md#open-ferry-update).

In the plain form, nothing is restarted: the running server goes on with the old binary until you restart it, and `open-ferry update -check` says that a restart is needed. `open-ferry update` says how to restart for your system:

| Running as | Restart |
|---|---|
| Your service on Linux | `systemctl --user restart open-ferry` (`sudo systemctl restart open-ferry` for the machine's) |
| Your service on macOS | `launchctl kickstart -k gui/$(id -u)/io.github.loft-902-co-llc.open-ferry` (`sudo launchctl kickstart -k system/io.github.loft-902-co-llc.open-ferry` for the machine's) |
| Your service on Windows | `schtasks /End /TN open-ferry`, then `schtasks /Run /TN open-ferry` (as an administrator, `sc.exe stop open-ferry`, then `sc.exe start open-ferry` for the machine's) |
| By hand | Stop it and start it again |

## Rolling back

```sh
open-ferry update -rollback
```

While a supervisor runs, it switches the service back to the version the supervisor kept, on the same listener, as [an update does](#installing-an-update). In the plain form, it puts back the version before the last update, from its copy in the data directory, after running it with `--version`. Nothing is downloaded, and nothing is restarted. Automatic checks skip the version you rolled back from until a newer release; `open-ferry update` installs it again. The data directory keeps one version back. For an older one, install it with the install script's `--version` (`-Version`).

## Files

For an account install, the data directory is `$XDG_DATA_HOME/open-ferry` when `XDG_DATA_HOME` is an absolute path, else `~/.local/share/open-ferry` (on macOS too), and `%LOCALAPPDATA%\open-ferry` on Windows. For a machine install it is `/var/lib/open-ferry` on Linux or `C:\ProgramData\open-ferry` on Windows, independent of the environment. It holds:

| File | What it is |
|---|---|
| `install-receipt.json` | Written by the install scripts ([above](#which-installs-update-themselves)). Updates never write it |
| `versions/<version>/open-ferry` | The binaries kept: the one staged for the next update, the one installed by the last update, and the one before it, for a rollback (`open-ferry.exe` on Windows) |
| `update-state.json` | What the last check found and what updates did. No secrets |
| `update.lock` | Held while a check, a stage, an update or a rollback runs, so two never overlap; the second says that another update is running |

The receipt, at most 16 KiB, is one JSON object with these fields, all required:

```json
{"format":1,"installer":"install.sh","version":"0.1.0","binary":"/home/you/.local/bin/open-ferry","target":"x86_64-unknown-linux-gnu","installed_at":"2026-10-08T12:00:00Z"}
```

`installer` is `install.sh` or `install.ps1`, `binary` the absolute path it wrote, and `installed_at` UTC. The file has no byte order mark.

The state is written whole and renamed into place; one that is missing or doesn't parse reads as empty, and the next check writes it again. A privileged process refuses untrusted records rather than treating them as empty:

```json
{
  "format": 1,
  "last_check": "2026-10-08T12:00:00Z",
  "last_result": "staged",
  "last_error": null,
  "latest": "0.2.0",
  "notified": "0.2.0",
  "staged": "0.2.0",
  "staged_sha256": "<the staged binary's SHA-256>",
  "previous": "0.1.0",
  "failed": [],
  "rolled_back": null,
  "last_switch": null
}
```

`last_result` is `up-to-date`, `update-available`, `staged`, `skipped`, `cannot-update` or `error`. `failed` lists up to 10 versions that failed their `--version` run here. After an update or a rollback, `last_switch` records it: `from`, `to`, `at`, `how` (`update` or `rollback`), the `binary` replaced, its `sha256`, `size` and `modified_ms`, and whether a `restart_needed`.

## Signed releases

The release workflow signs each release's `SHA256SUMS` with a minisign Ed25519 key, as `SHA256SUMS.minisig`, after the maintainer approves it, and checks the signature before it publishes anything ([RELEASING.md](../RELEASING.md#the-release-key)). The public keys are in [`release-keys.pub`](../release-keys.pub): at most two, the current one and, during a rotation, the next. The binary builds them in, and trusts no other. A build from a `release-keys.pub` with no key, as from a checkout before the first key is added, trusts no release: it makes no update request, and `open-ferry check` warns that it never updates.

To check a download by hand, put `SHA256SUMS` and `SHA256SUMS.minisig` beside the archive, and run, with `<key>` the key line of `release-keys.pub` (the one starting `RW`):

```sh
minisign -V -H -m SHA256SUMS -P <key>
sha256sum --check --ignore-missing SHA256SUMS      # shasum -a 256 on macOS
```

minisign prints `Signature and comment signature verified` and the trusted comment, which must name the version you downloaded: `Trusted comment: open-ferry 0.1.0 SHA256SUMS`. On Windows, check the archive's hash as the [README](../README.md#verify-the-download) shows. [SECURITY.md](../SECURITY.md#signed-releases) says what to do when a signature doesn't check out.

## Seeing what updates do

- **The server's log** says at start whether updates are on, notify-only or off, what set that and how often it looks; once for each new release; and, at warn, when a check fails. A failed check never stops the server.
- **`open-ferry update -check`** checks now and says what it found, and changes nothing; `-json` prints it as one JSON object.
- **`open-ferry check`** has a `self-update` finding: the mode and what set it, how often it looks, and whether this install updates itself. It makes no request.
- **The dashboard's About page** shows the status: the mode and what set it, the versions, the last and next check and what the last one found, and what to do next, such as a restart or `open-ferry update`. It has a Check now while updates aren't off. For a supervised install it also has Install now, for a newer release, and Roll back, to the version the supervisor kept. Each asks for confirmation, and the Updates card follows preparation, handover and the watch window. A plain install shows the command to run on the server instead.
- **The dashboard API** has the status, `GET /open-ferry/api/v1/update`, a check now, `POST /open-ferry/api/v1/update/check`, and, for a supervised install, `POST /open-ferry/api/v1/update/install` and `POST /open-ferry/api/v1/update/rollback` ([docs/dashboard-api.md](dashboard-api.md#updates)).

## Supervisor endpoint refusal codes

The local supervisor endpoint uses version 1 newline-delimited JSON. Refusals keep
`reason` as human text and may add an optional `code`, for example:

```json
{"type":"error","v":1,"code":"switching_disabled","reason":"switching is disabled; see the supervisor log"}
```

The codes callers distinguish are `nothing_to_cancel`, `switching_disabled`,
`no_previous` and `rollback_target_mismatch`. Cancellation maps `nothing_to_cancel`
to no switch in progress. Switching-disabled refusals use the same typed error as
local disabled policy. The two rollback refusals use the same typed errors as a
locally missing or changed rollback target, so the dashboard returns
`rollback_target_changed` and asks for fresh confirmation. A mismatch keeps the
requested version as `expected`; no current target is disclosed (`actual` is absent).

This optional field is additive, so the protocol version stays 1. Missing and
unknown codes remain generic refusals with the human reason. No released older
supervisor needs a text-matching fallback; callers never use `reason` to identify
these cases. Errors after acceptance remain terminal switch errors, regardless of
code.

## CLIProxyAPI

CLIProxyAPI doesn't update its own binary; it downloads its management panel's web page, and keeps that up to date unless `remote-management.disable-auto-update-panel` is on. open-ferry downloads no panel: its dashboard is built in. CLIProxyAPI's loader ignores the `self-update` section. Its management API's v0 writes to a config in the v8 layout comment the section out, and its v8 writes refuse a file that has it ([UPSTREAM.md](../UPSTREAM.md#added-in-open-ferry-updates)).
