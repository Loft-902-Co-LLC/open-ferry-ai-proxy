# open-ferry-parity

Differential tests of open-ferry's translators against upstream [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI). The tool runs the same input through upstream's Go translators and through our Rust ports, then compares what they produce.

Six translators are covered so far:

| Module | Translator | Input | Output |
|---|---|---|---|
| `codex::claude` | Request | a Claude Messages request | the Codex request |
| | Response, streaming | a Codex event stream | Claude SSE events |
| | Response, non-streaming | Codex's final event | one Claude message |
| `codex::openai::responses` | Request | an OpenAI Responses request | the Codex request |
| | Response, streaming | a Codex event stream | Responses events |
| | Response, non-streaming | Codex's final event | one Responses response |

## Running it

You need Go and a CLIProxyAPI checkout. Use the version pinned in [UPSTREAM.md](../../UPSTREAM.md) or one close to it; translator changes made upstream since the pin show up as differences.

```sh
git clone https://github.com/router-for-me/CLIProxyAPI ../CLIProxyAPI
cargo run --release -p open-ferry-parity -- --upstream ../CLIProxyAPI
```

Options:

| Flag | Default | |
|---|---|---|
| `--random <n>` | 5000 | Random cases to generate per translator |
| `--seed <n>` | 1 | Seed for the random cases |
| `--show <n>` | 10 | Kinds of difference to print |
| `--go <path>` | `go` | Go binary |
| `--live <url>` | | Send a few requests through a running CLIProxyAPI instead (see below) |
| `--model <name>` | | Model for `--live` |

The exit status is 0 when every case is identical, equivalent or a known difference (see below).

## How it works

Upstream's translators are in `internal/` packages, which only code inside the CLIProxyAPI module can import. `go/main.go` is a small harness that reads JSON lines from stdin and writes each translation to stdout. The tool builds it with `go build -overlay`, which adds the file to the module as `cmd/open-ferry-parity` at build time. Your checkout is not modified.

Each translator gets:

- **Hand-written cases** (`src/cases.rs`, and `src/cases/responses.rs` for the Responses translators) for inputs the generator is unlikely to produce. For requests: duplicate keys, a 2 MiB image, a 100-level schema, numbers too large for a float, bodies that aren't objects, keys written with escapes. For responses: parallel function calls with text arriving between them, web search, a policy error, a call ID that is empty, arguments replaced partway through a character, SSE noise, and the client's model given in each place upstream looks for it.
- **Random cases**, reproducible from the seed. The request generator (`src/generate.rs`) mixes well-formed requests with the sloppy input upstream tolerates: wrong value types, missing fields, unknown roles, near-miss reasoning signatures, case and Unicode edge cases, and JSON written pretty, compact or with escaped characters. The response generator (`src/generate/response.rs`) builds Codex event streams of reasoning, text, function calls and web searches, one item after another or interleaved. Events are sometimes dropped, repeated or loosely typed, and the final event lists more or fewer items than were streamed, or is an error, or is missing. The final event of each stream is also a non-streaming case.

  For the Responses translators (`src/generate/responses.rs`), requests mix the fields upstream sets, drops or renames with loosely typed values: required booleans as strings, blank and nearly blank call arguments, every role spelling, web search aliases in each place they're renamed, and cache breakpoints at every level. The event streams are the Codex streams above, with the model in `response.created` and `response.in_progress` removed or mistyped, and the event's `type` or `response` sometimes replaced, and the client's model given in the request, the translated request, the model parameter, or nowhere.

`src/translator.rs` reads each translator's output as JSON. A Claude stream becomes a list of `{"event", "data"}` frames, so frame order, event names and every field are compared. Tool IDs generated for calls that arrive without one (`toolu_<nanoseconds>_<counter>`) are masked on both sides before comparing. The Responses stream translator passes most lines through, so its output is read line by line: `=` for a line returned byte for byte, and otherwise the line's JSON.

The comparison (`src/compare.rs`) walks both outputs. Each case ends up in one of four groups:

- **identical**: the same JSON, including key order and number text.
- **equivalent**: the only differences are documented deviations (see UPSTREAM.md):
  - *tool parameter key order*: upstream sorts schema keys, we keep the client's order;
  - *embedded JSON re-serialized*: a string holds the same JSON, written compactly by us (in responses, only a web search query, where Go also escapes `<`, `>` and `&`);
  - *cut at a character boundary*: upstream cut a name or ID in the middle of a character (written as U+FFFD), we cut before it.
- **known**: a hand-written case marked with `known_difference`, such as behaviour not ported yet. A random case that may reach such behaviour is marked too. So far that is only a thinking signature upstream could replay to a Grok model.
- **different**: anything else. Failing cases are written to `target/parity/failures/<translator>/` (such as `claude-request` or `responses-stream`) with the input and both outputs.

## Live mode

The offline comparison shows that our output has the same meaning as upstream's. Live mode checks that the real API agrees. open-ferry has no server yet, so this tests the translators, not two proxies. A few realistic requests (`src/live.rs`) are translated both ways, and both Codex bodies are posted to a running CLIProxyAPI's `/v1/responses`. That endpoint applies the same Responses-to-Codex handling to both bodies.

```sh
export OPEN_FERRY_PARITY_API_KEY=...   # a client key from the proxy's api-keys
cargo run --release -p open-ferry-parity -- --upstream ../CLIProxyAPI \
  --live http://127.0.0.1:8317 --model gpt-5.5
```

Model output varies between runs, so live mode compares the shape of each reply: HTTP status, the final event, the output item types and the function call names. It also checks that each reply is the kind the case asks for, such as a function call or a JSON object. There are 7 cases, 2 requests each, all with low reasoning effort.

The replies are real Codex event streams, so each one is then run through every response translator, upstream's and ours, which must agree exactly. For the non-streaming translators, an empty `output` in the final event is filled with the streamed items first, as upstream's executor does. This sends no further requests. Bodies, replies and any failing response cases are saved to `target/parity/live/`.
