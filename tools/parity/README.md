# open-ferry-parity

Differential tests of open-ferry's translators against upstream [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI). The tool sends the same Claude requests through upstream's Go translator and through our Rust port, then compares the Codex requests they produce.

Only the Claude → Codex request translator (`codex::claude`) is covered so far.

## Running it

You need Go and a CLIProxyAPI checkout. Use the version pinned in [UPSTREAM.md](../../UPSTREAM.md) or one close to it; translator changes made upstream since the pin show up as differences.

```sh
git clone https://github.com/router-for-me/CLIProxyAPI ../CLIProxyAPI
cargo run --release -p open-ferry-parity -- --upstream ../CLIProxyAPI
```

Options:

| Flag | Default | |
|---|---|---|
| `--random <n>` | 5000 | Random cases to generate |
| `--seed <n>` | 1 | Seed for the random cases |
| `--show <n>` | 10 | Kinds of difference to print |
| `--go <path>` | `go` | Go binary |
| `--live <url>` | | Send a few requests through a running CLIProxyAPI instead (see below) |
| `--model <name>` | | Model for `--live` |

The exit status is 0 when every case is identical, equivalent or a known difference (see below).

## How it works

Upstream's translators are in `internal/` packages, which only code inside the CLIProxyAPI module can import. `go/main.go` is a small harness that reads JSON lines from stdin and writes each translation to stdout. The tool builds it with `go build -overlay`, which adds the file to the module as `cmd/open-ferry-parity` at build time. Your checkout is not modified.

Each run uses:

- **Hand-written cases** (`src/cases.rs`) for inputs the generator is unlikely to produce: duplicate keys, a 2 MiB image, a 100-level schema, numbers too large for a float.
- **Random cases** (`src/generate.rs`), reproducible from the seed. The generator mixes well-formed requests with the sloppy input upstream tolerates: wrong value types, missing fields, unknown roles, near-miss reasoning signatures, case and Unicode edge cases, and JSON written pretty, compact or with escaped characters.

The comparison (`src/compare.rs`) walks both outputs. Each case ends up in one of four groups:

- **identical**: the same JSON, including key order and number text.
- **equivalent**: the only differences are documented deviations (see UPSTREAM.md):
  - *tool parameter key order*: upstream sorts schema keys, we keep the client's order;
  - *embedded JSON re-serialized*: a string holds the same JSON, written compactly by us;
  - *cut at a character boundary*: upstream cut a name or ID in the middle of a character (written as U+FFFD), we cut before it.
- **known**: a hand-written case marked with `known_difference`, such as behaviour not ported yet.
- **different**: anything else. Failing cases are written to `target/parity/failures/` with the request and both outputs.

## Live mode

The offline comparison shows that our output has the same meaning as upstream's. Live mode checks that the real API agrees. open-ferry has no server yet, so this tests the translators, not two proxies. A few realistic requests (`src/live.rs`) are translated both ways, and both Codex bodies are posted to a running CLIProxyAPI's `/v1/responses`. That endpoint applies the same Responses-to-Codex handling to both bodies.

```sh
export OPEN_FERRY_PARITY_API_KEY=...   # a client key from the proxy's api-keys
cargo run --release -p open-ferry-parity -- --upstream ../CLIProxyAPI \
  --live http://127.0.0.1:8317 --model gpt-5.5
```

Model output varies between runs, so live mode compares the shape of each reply: HTTP status, the final event, the output item types and the function call names. It also checks that each reply is the kind the case asks for, such as a function call or a JSON object. There are 7 cases, 2 requests each, all with low reasoning effort. Bodies and replies are saved to `target/parity/live/`.
