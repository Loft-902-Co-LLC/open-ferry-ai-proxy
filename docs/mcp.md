# The MCP server: `open-ferry mcp`

`open-ferry mcp` serves open-ferry's commands for looking at and changing a setup ([docs/cli.md](cli.md#looking-at-and-changing-a-setup)) as the tools of a [Model Context Protocol](https://modelcontextprotocol.io) server, so a coding agent's app can check how the proxy is doing, change a setting, add a client key or sign in to a provider without editing the YAML or calling the management API itself. Each tool is one command, with the same checks, the same guardrails and the same JSON. CLIProxyAPI has no MCP server.

```
open-ferry mcp [--config PATH] [--management-key-file PATH]
```

The app starts it and talks to it on its standard input and output. Standard output carries the protocol only: the server logs nothing, and a failure to start goes to standard error. It ends, with exit code 0, when the app closes its input.

## Adding it to an app

Each app starts the server itself, so tell it the command:

- **Give `--config` with the config's full path.** The app picks the server's working directory, and without `--config` the server uses `config.yaml` there, else the installed config ([where that is](agents.md#where-its-files-are)).
- **The management key** is found as the commands find it ([the order](cli.md#the-management-key)): the config's `management.secret-key` when it is stored plain, then `MANAGEMENT_PASSWORD`, then a key file. When the config holds only the hash of the key, write the key to a file only you can read and add `--management-key-file <its path>`. Don't put the key itself in the app's settings.
- **`open-ferry` must be on the app's `PATH`**, or give its full path ([where the install scripts put it](agents.md#where-its-files-are)).

These are for you to run or edit; open-ferry never writes another app's settings.

### Claude Code

```sh
claude mcp add open-ferry -- open-ferry mcp --config /path/to/config.yaml
```

That adds it for you, in the project you are in; add `--scope user` after `add` to have it in every project. `claude mcp list` shows it, and `/mcp` in a session shows its tools.

### Codex

```sh
codex mcp add open-ferry -- open-ferry mcp --config /path/to/config.yaml
```

or, in `~/.codex/config.toml`:

```toml
[mcp_servers.open-ferry]
command = "open-ferry"
args = ["mcp", "--config", "/path/to/config.yaml"]
```

### Cursor

In `~/.cursor/mcp.json`, for every project, or `.cursor/mcp.json` in a project:

```json
{
  "mcpServers": {
    "open-ferry": {
      "command": "open-ferry",
      "args": ["mcp", "--config", "/path/to/config.yaml"]
    }
  }
}
```

### T3 Code

T3 Code runs agents such as Codex and Claude Code, and a thread's agent loads the MCP servers that agent is set up with. Add open-ferry to Codex's or Claude Code's settings as above, then start a new thread. Where your T3 Code has a page for MCP servers in its settings, add a command server there instead, with the command `open-ferry` and the arguments `mcp --config /path/to/config.yaml`.

## The tools

| Tool | Command | Arguments | Annotations |
|---|---|---|---|
| `status` | `status` | none | read-only, idempotent |
| `config_get` | `config get` | `path` | read-only, idempotent |
| `config_set` | `config set` | `path`; `value` or `from_file`; `string`, `confirm`, `expect_sha256` | destructive, idempotent |
| `config_unset` | `config unset` | `path`; `confirm`, `expect_sha256` | destructive, idempotent |
| `config_show` | `config show` | none | read-only, idempotent |
| `config_diff` | `config diff` | none | read-only, idempotent |
| `config_undo` | `config undo` | `confirm`, `expect_sha256`, `expect_backup_sha256` | destructive; refused with `changed_since` without `confirm` when the config was changed since the last backup, as by a hand edit |
| `config_replace` | `config replace` | `from_file`; `confirm`, `expect_sha256` | destructive, idempotent |
| `keys_list` | `keys list` | none | read-only, idempotent |
| `keys_add` | `keys add` | `generate` or `from_file`; `to_file`, `confirm`, `expect_sha256` | not destructive |
| `keys_remove` | `keys remove` | `index` or `from_file`; `confirm`, `expect_sha256` | destructive |
| `credentials_list` | `credentials list` | `state`, `provider` | read-only, idempotent |
| `credentials_enable` | `credentials enable` | `credential` | not destructive, idempotent |
| `credentials_disable` | `credentials disable` | `credential` | destructive, idempotent |
| `credentials_reset_quota` | `credentials reset-quota` | `credential` | not destructive, idempotent |
| `credentials_remove` | `credentials remove` | `credential`; `confirm` | destructive |
| `credentials_login` | `credentials login` | `provider`; `state`, `confirm` | not destructive, open world |
| `clients_setup` | `clients setup` | `client`; `model`, `shell`, `key_index` | read-only, idempotent |

The annotations are the protocol's hints: `readOnlyHint`, `destructiveHint` (given for the tools that aren't read-only), `idempotentHint` and `openWorldHint`, which is true only for `credentials_login`, as a sign-in goes to the provider. Each tool's input schema lists its arguments and refuses others; each tool's description, written for a model, says what it does and what needs `confirm`.

- **`path`** is a setting's path in the v8 config, dotted (`routing.strategy`, `server.port`), as the commands take it. A list is set whole, not one item by its index.
- **`value`** is any JSON. A secret is refused as `value`: it comes in a file, `from_file`.
- **`from_file`** is only for a secret: `config_set` refuses a file's value for a setting that holds none (`usage`), so give that as `value`. A file in the auth directory, or a credential file, is never read (`unsafe_file`): one with a PEM block (`-----BEGIN ...-----`), or, in JSON or YAML, any of the fields `access_token`, `refresh_token`, `id_token`, `private_key`, `client_secret`, `tokens` and `session_key` with a value, at any depth up to 32 levels, in any case and with or without `_`, `-` or `.` (`accessToken` and `access-token` too). A file nested deeper than 32 levels, or with a mapping key that isn't text (a number, say), can't be checked, so it is refused too. A call with `from_file` needs `confirm: true` (see below).
- **`from_file`** and **`to_file`** are paths on the machine the server runs on; give full paths, as the server's working directory is the app's choice.
- **`credential`** is a credential's `auth_index` from `credentials_list`, or its name.
- **`expect_sha256`** is the `config_sha256` a result that needed `confirm: true` gave: send it with `confirm: true` (see [What needs `confirm: true`](#what-needs-confirm-true)).
- **`expect_backup_sha256`**, for `config_undo`, is the `backup_sha256` its result that needed `confirm: true` gave: send it with `confirm: true` and `expect_sha256`.
- **`credentials_login`** takes two calls: the first, with `provider` (`codex`, or `claude` with `confirm: true`), returns a `url` for the user to open and a `state`, with `"status": "wait"`. The second, with the same `provider` and the `state`, waits up to 50 seconds for the sign-in to finish: `"status": "ok"`, or `"status": "wait"` while it is still going: not a tool error, but a result saying to call again with the same `state`.

Each call reads the config and looks for the server afresh, so a server started or stopped while the app runs is followed. With a server running for the config, a change goes through its management API; without one, the settings tools change the file, and the credentials tools say the server isn't running (`not_running`) and how to start it. A server on the config's address that runs another config file is never changed: the tools fail with `other_config`. One that runs another file holding the same bytes counts as running this config, as the server doesn't say which file it runs; a change then goes to the server's file, and the result's `note` says this file didn't change. Upstream's management routes take no precondition, so a save from the dashboard that lands between the check that the server runs this config and the change's request is overwritten when it is to the same setting (to any setting, for `config_replace`); `expect_sha256` can't catch that, and the result lists every setting that changed. A change is made only to the file it was worked out from, worked out again when the file changes meanwhile, and then made only if it needs no confirmation: `confirm: true` was given for the change as first worked out, so one that needs a confirmation once worked out again fails with `config_changed`. [docs/cli.md](cli.md#how-they-reach-the-server) has the details.

## What needs `confirm: true`

Without it, these change nothing, and the result is a tool error with `"error": "needs_confirmation"`, what they would change (`would.changes`, masked), why (`would.reasons`) and, for a change to the config, the SHA-256 of the config file it was worked out from (`would.config_sha256`). An agent should show that to the user and call again with `confirm: true` only when they agree.

**Send `expect_sha256` with `confirm: true`**, set to that `config_sha256`. The change is then made only to the config the user was shown: when the file changed since, by another write or a hand edit, the call fails with `config_changed`, nothing is changed, and the change isn't worked out again. Call again without `confirm` to see what it would change now, and show the user that. Without `expect_sha256`, `confirm: true` goes ahead with the change as worked out from the file at the second call, which can differ from what the user agreed to. The tools that take it are `config_set`, `config_unset`, `config_undo`, `config_replace`, `keys_add` and `keys_remove`. `config_undo`'s result also gives the SHA-256 of the backup it would put back (`would.backup_sha256`): send that as `expect_backup_sha256` too, as another write can change the backup and leave the config as it was, and the undo then fails with `config_changed`. `config_sha256` and `backup_sha256` are hashes of whole files, not secrets, and are safe to show.

- `credentials_remove` and `keys_remove`: they delete;
- `config_replace`: it replaces the whole config;
- `config_set`, `config_replace` and `keys_add` with `from_file`: it reads a file into the config (`keys_remove` needs it anyway);
- `credentials_login` for `claude`: that sign-in goes against Anthropic's terms ([why](claude-subscription.md#the-claude-sign-in));
- `keys_add` with `generate: true` and no `to_file`: the new key is returned, so it sits in the transcript;
- `config_set`, `config_unset`, `config_replace`, `config_undo` and `keys_remove`, when they change a sensitive setting:
  - `management.allow-remote`, `management.secret-key` and `management.separate-address`;
  - `server.host`, set to anything but `localhost` or a loopback address, or unset;
  - anything under `server.tls` or `server.trusted-proxies`;
  - the last client key in `access.api-keys`, removed (blank and repeated keys don't count, as the server ignores them).

## Results

A tool's result is the command's `--json` output as its structured content (`structuredContent`), and the same as text, followed by the command's text output. A failure is a tool error (`isError`) with the command's failure: `error` (a code such as `unknown_path`, `not_running` or `needs_confirmation`), `message`, and, when there is one, `hint` and `would`. A call to a tool that doesn't exist is a protocol error (`-32602`).

For example, `config_set` with `{"path": "routing.strategy", "value": "fill-first"}`:

```json
{
  "action": "set",
  "path": "routing.strategy",
  "changed": true,
  "via": "server",
  "changes": [{"path": "routing.strategy", "new": "fill-first"}],
  "undo": "Undo it with the config_undo tool."
}
```

## Secrets

- **No tool returns a secret already in the setup.** `keys_list` and `clients_setup` mask the client keys, and no tool reveals one: `open-ferry keys list --reveal --yes`, in a terminal, does. The management key, provider API keys, tokens and email addresses are masked wherever they appear, and every secret of the config, and of the config a change would make, and every token of the credential files in the auth directory, is then replaced by `[redacted]` wherever it still shows, in a URL's path too. A secret shorter than 8 characters is replaced only as a whole word, and only in strings: such a key can make the masking hide words that match it, and it is a weak key anyway. A config or a value that doesn't load is said not to, and where (a line, a setting), without quoting it. A value read with `from_file` for `config_set` shows only as `"[redacted]"` in what the tool would change and what it changed, with nothing of it shown, not its keys, its shape or its length (a key read for `keys_add` or `keys_remove` is a client key, masked as `keys_list` masks one); `config_replace`'s file is a whole config, and its changes are masked setting by setting.
- **A secret never comes in a call.** `config_set` refuses one as `value`, and `keys_add` and `keys_remove` take a key only from a file. The agent should ask the user to put a secret in a file and give its path, never to paste it into the conversation.
- **`keys_add` with `generate: true`** writes the new key to the new file `to_file` names (one that doesn't exist yet; on Linux and macOS only the user can read it) and returns its path as `key_file`. With `confirm: true` and no `to_file`, it returns the key itself as `key`, once. With `from_file`, the key is in a file already, and only its masked form is returned.

## Resources

| URI | What | Type |
|---|---|---|
| `open-ferry://docs/agents.md` | [docs/agents.md](agents.md): how an agent installs, sets up and changes open-ferry | `text/markdown` |
| `open-ferry://config` | The config the tools work on, masked, as YAML | `application/yaml` |

## The protocol

The server speaks MCP over standard input and output, as newline-delimited JSON-RPC, with the official Rust SDK, [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk). It answers protocol version `2025-11-25`, and the earlier `2025-06-18`, `2025-03-26` and `2024-11-05` to an app that asks for one. It offers tools and resources, and tells the app, in its instructions, to start with `status` and to ask the user before a `confirm: true`.
