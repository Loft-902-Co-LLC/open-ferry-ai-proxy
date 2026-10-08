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
| `config_set` | `config set` | `path`; `value` or `from_file`; `string`, `confirm` | destructive, idempotent |
| `config_unset` | `config unset` | `path`; `confirm` | destructive, idempotent |
| `config_show` | `config show` | none | read-only, idempotent |
| `config_diff` | `config diff` | none | read-only, idempotent |
| `config_undo` | `config undo` | `confirm` | destructive |
| `config_replace` | `config replace` | `from_file`; `confirm` | destructive, idempotent |
| `keys_list` | `keys list` | none | read-only, idempotent |
| `keys_add` | `keys add` | `generate` or `from_file`; `to_file`, `confirm` | not destructive |
| `keys_remove` | `keys remove` | `index` or `from_file`; `confirm` | destructive |
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
- **`from_file`** and **`to_file`** are paths on the machine the server runs on; give full paths, as the server's working directory is the app's choice.
- **`credential`** is a credential's `auth_index` from `credentials_list`, or its name.
- **`credentials_login`** takes two calls: the first, with `provider` (`codex`, or `claude` with `confirm: true`), returns a `url` for the user to open and a `state`, with `"status": "wait"`. The second, with the same `provider` and the `state`, waits up to 50 seconds for the sign-in to finish: `"status": "ok"`, or a tool error with `"status": "wait"` while it is still going, when the call can be made again.

Each call reads the config and looks for the server afresh, so a server started or stopped while the app runs is followed. With a server running for the config, a change goes through its management API; without one, the settings tools change the file, and the credentials tools say the server isn't running (`not_running`) and how to start it. A server on the config's address that runs another config file is never changed: the tools fail with `other_config`. A change is made only to the file it was worked out from, worked out again when the file changes meanwhile. [docs/cli.md](cli.md#how-they-reach-the-server) has the details.

## What needs `confirm: true`

Without it, these change nothing, and the result is a tool error with `"error": "needs_confirmation"`, what they would change (`would.changes`, masked) and why (`would.reasons`). An agent should show that to the user and call again with `confirm: true` only when they agree.

- `credentials_remove` and `keys_remove`: they delete;
- `config_replace`: it replaces the whole config;
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

- **No tool returns a secret already in the setup.** `keys_list` and `clients_setup` mask the client keys, and no tool reveals one: `open-ferry keys list --reveal --yes`, in a terminal, does. The management key, provider API keys, tokens and email addresses are masked wherever they appear, and every secret of the config is then replaced by `[redacted]` wherever it still shows.
- **A secret never comes in a call.** `config_set` refuses one as `value`, and `keys_add` and `keys_remove` take a key only from a file. The agent should ask the user to put a secret in a file and give its path, never to paste it into the conversation.
- **`keys_add` with `generate: true`** writes the new key to the new file `to_file` names (one that doesn't exist yet; on Linux and macOS only the user can read it) and returns its path as `key_file`. With `confirm: true` and no `to_file`, it returns the key itself as `key`, once. With `from_file`, the key is in a file already, and only its masked form is returned.

## Resources

| URI | What | Type |
|---|---|---|
| `open-ferry://docs/agents.md` | [docs/agents.md](agents.md): how an agent installs, sets up and changes open-ferry | `text/markdown` |
| `open-ferry://config` | The config the tools work on, masked, as YAML | `application/yaml` |

## The protocol

The server speaks MCP over standard input and output, as newline-delimited JSON-RPC, with the official Rust SDK, [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk). It answers protocol version `2025-11-25`, and the earlier `2025-06-18`, `2025-03-26` and `2024-11-05` to an app that asks for one. It offers tools and resources, and tells the app, in its instructions, to start with `status` and to ask the user before a `confirm: true`.
