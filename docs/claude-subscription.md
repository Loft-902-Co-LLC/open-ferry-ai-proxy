# Claude subscriptions

open-ferry can serve Claude models in three ways:

- **`claude-api-key`**: with an Anthropic API key, billed to that key. This is the plain way, and nothing on this page applies to it.
- **`claude-cli`**: with your own Claude subscription, through your own installed Claude Code. open-ferry runs it for each request; Claude Code signs itself in and makes every request to Anthropic. This page is mostly about it.
- **The Claude sign-in** (`-claude-login`): don't use it. [The last section](#the-claude-sign-in) says why.

## Why not sign in to Claude directly

Anthropic's terms don't allow a tool such as open-ferry to sign in to a Claude subscription and call the API with its tokens. Their [legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use), under "Authentication and credential use", says that developers "may not collect, store, or intermediate Claude.ai credentials or session tokens". In our testing, a subscription signed in that way gets only the Haiku models.

The same page doesn't prevent "an end user from signing in to the unmodified Claude Code binary with their own Claude subscription". That is what `claude-cli` relies on. open-ferry runs the `claude` you installed, as it is, with `claude -p`. It passes the request in and reads the answer out. It never reads, copies or stores Claude Code's credentials, runs no sign-in, and sends no request to Anthropic itself.

## Set it up

1. **Install Claude Code**, version 2.1.259 or newer, as [Anthropic's setup guide](https://code.claude.com/docs/en/setup) says. Check it with `claude --version`.
2. **Sign in** with your subscription:

   ```sh
   claude auth login
   ```

   `claude auth status` tells you whether it worked.
3. **Add more accounts**, if you have them, each in a directory of its own ([Anthropic's guide](https://code.claude.com/docs/en/authentication)):

   ```sh
   CLAUDE_CONFIG_DIR=~/.claude-second claude auth login
   ```

   Then give that directory as the entry's `config-dir`.
4. **Or use a long-lived token** instead of a sign-in. Run `claude setup-token`, and set the token it prints as `CLAUDE_CODE_OAUTH_TOKEN` in the environment open-ferry runs in. open-ferry leaves it in Claude Code's environment, for entries without a `config-dir` only, and never reads it. An entry with a `config-dir` uses that directory's sign-in.
5. **Add an entry** to `config.yaml`, as below.

## Config

`claude-cli` is a list at the top level of `config.yaml`:

```yaml
claude-cli:
  - name: claude-max-1          # required and unique
  - name: claude-max-2
    config-dir: "~/.claude-second"
    command: ""                 # the claude to run; empty finds it on PATH
    system-prompt: replace      # or append
    max-concurrency: 2          # requests at once; more wait for a turn
    timeout: 10m                # the most one request may take
    prefix: "max2"              # calls to "max2/claude-sonnet-5-5" go to this entry
    models:                     # empty serves the built-in Claude models
      - name: "claude-sonnet-5-5"
        alias: "sonnet"
    excluded-models:
      - "*haiku*"
```

- **`command`**: the path to `claude`. Empty finds `claude` on `PATH`; on Windows, `claude.exe`, else the `claude.cmd` npm installs.
- **`config-dir`**: the account's `CLAUDE_CONFIG_DIR`. Empty uses the `CLAUDE_CONFIG_DIR` open-ferry runs with, else Claude Code's default.
- **`system-prompt`**: `replace`, the default, gives Claude Code the client's system prompt in place of its own. `append` keeps Claude Code's and adds the client's to it.
- **`prefix`, `models`, `excluded-models`, `priority`, `weight` and `disabled`** mean what they mean for `claude-api-key`.

At start, and when the list changes, open-ferry runs each entry's `claude --version` and logs a warning when it is older than 2.1.259 or can't be run. The dashboard API's [`claude-cli/auth-status`](dashboard-api.md#claude-cli) route tells you whether an entry's Claude Code is signed in.

## What to expect

- **Claude Code is an agent, not the raw API.** Its own tools, MCP servers, slash commands and session files are off, and it answers in one turn. Still, its answers can differ from the API's for the same request, most of all in `append` mode.
- **No client tools yet.** A request with tools, a tool choice other than `none` or `auto`, or tool calls and results in its messages gets a 400.
- **Earlier turns go as a transcript.** A conversation of several turns goes to Claude Code as one message holding all of it, not as separate turns. A new turn only adds to the transcript, so Claude Code's prompt cache still works.
- **Sampling settings are ignored**: `temperature`, `top_p`, `top_k`, `stop_sequences` and metadata. `max_tokens`, the thinking budget and the effort are passed on.
- **Thinking comes back without its text.** Claude Code doesn't pass on what the model thought, so a client that asks for thinking gets thinking blocks with their signatures and empty text. A client that doesn't ask gets none, even when the model thought.
- **Each request starts a process.** Short requests took 2 to 4.5 seconds in our testing.
- **Claude Code's own overhead counts toward your plan's limits.** Each request writes about 590 tokens of Claude Code's own to the prompt cache. Usage is reported as Claude Code gives it, with these tokens in it.
- **Token counting isn't supported**: `/v1/messages/count_tokens` answers 501.
- **Quota readings work.** Claude Code reports the account's rate-limit windows, and open-ferry reads them as it reads the Claude API's rate-limit headers. When a window runs out, the entry cools down until it resets.
- **Some failures stop Claude Code at once**, rather than waiting through its retries: a failed sign-in, an account on hold, billing, a rate limit or an unknown model. Each gets the status the API would give.

## For your own use only

Use `claude-cli` only with your own accounts, for yourself. Anthropic's page says that plan limits "assume ordinary, individual usage of Claude Code and the Agent SDK". Don't share the proxy's API keys that reach these entries with anyone else.

## The Claude sign-in

open-ferry still has the Claude sign-in it ported from CLIProxyAPI: `-claude-login`, and signing in to Claude from the dashboard, the TUI and the management API. We advise against it.

- It signs in with Claude Code's OAuth client, stores the Claude.ai tokens it gets in the auth directory, and sends requests to Anthropic with them.
- Anthropic's terms forbid third-party tools from doing this (see their [legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use), "Authentication and credential use").
- In our testing, only the Haiku models work through it.
- Anthropic may limit or suspend an account used this way. If you use it, that risk is yours.

For your subscription, use [`claude-cli`](#set-it-up). For an API key, use `claude-api-key`.
