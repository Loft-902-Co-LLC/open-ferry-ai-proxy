# Dashboard config saves and real-server checks

Each dashboard form keeps the config version it loaded. On a server that
returns an `ETag`, config writes use the v8 config API and send that version
as `If-Match`. Versioned forms read their values from the v8 disk snapshot
too, so file-watcher lag cannot pair old values with a newer tag. A successful
save advances the version for the next change in the same form. A review or background refresh never authorizes replacing a
concurrent edit. A stale save returns 412 `precondition_failed`: the dashboard
asks you to reload the latest config and warns that reloading loses unsaved
edits. Copy edits you want to keep before reloading. Earlier servers that
return no ETag remain usable, with unconditional saves.

Guarded setting, client-key, provider-key and config-backed credential-toggle
saves migrate a legacy config file to the v8 layout, preserving values and
comments. Unknown legacy fields
are retained as comments, as the v8 writer documents. The YAML editor also
uses the v8 route when a version is available. Credential files, usage prices
and ledger retention settings have separate storage, not config.yaml.

The `dashboard-real (ubuntu-latest)` CI job builds `dashboard/dist`, embeds it
in a debug `open-ferry`, and runs `npm run e2e:real` with Chromium headless
shell. It belongs to the required `check (ubuntu-latest)` aggregate. Pull
requests leave it to the merge queue; manual dispatch runs it on a branch.
Rust and dashboard build inputs enable it; unrelated documentation edits
skip it. Pushes to main warm its Rust and npm caches.

The suite starts its own binary on `127.0.0.1:18517` (or
`OPEN_FERRY_E2E_PORT`, never 8317), with disposable config and auth directories.
All outbound traffic goes to a dead loopback proxy except one fake provider
on a free loopback port with a per-key `proxy-url: direct` override. CSP stays
enforced, service workers stay blocked, and trace, screenshots, video and
Playwright DOM failure prompts stay off. CI uploads no artifacts from these
journeys because failure diagnostics could expose config text or keys.

For a local run, build the dashboard first, then the binary:

```powershell
cd dashboard
npm ci --ignore-scripts
npm run build
cd ..
cargo build -p open-ferry --bin open-ferry --locked
cd dashboard
$env:OPEN_FERRY_BIN = (Resolve-Path ../target/debug/open-ferry.exe).Path
npm run e2e:real
```

On Linux, set `OPEN_FERRY_BIN` to the absolute path of
`target/debug/open-ferry`. The spec starts and stops that binary itself.
