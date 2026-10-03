# open-ferry-parity

Differential tests of open-ferry's translators against upstream [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI). The tool runs the same input through upstream's Go translators and through our Rust ports, then compares what they produce.

Twenty-five suites are covered so far:

| Module | Translator | Input | Output |
|---|---|---|---|
| `codex::claude` | Request | a Claude Messages request | the Codex request |
| | Request, compatibility mode | the same | the Codex request |
| | Response, streaming | a Codex event stream | Claude SSE events |
| | Response, non-streaming | Codex's final event | one Claude message |
| `codex::openai::responses` | Request | an OpenAI Responses request | the Codex request |
| | Response, streaming | a Codex event stream | Responses events |
| | Response, non-streaming | Codex's final event | one Responses response |
| `codex::openai::chat_completions` | Request | an OpenAI Chat Completions request | the Codex request |
| | Response, streaming | a Codex event stream | Chat Completions chunks |
| | Response, non-streaming | Codex's final event | one Chat Completions response |
| `claude::openai::chat_completions` | Request | an OpenAI Chat Completions request | the Claude Messages request |
| | Request, compatibility mode | the same | the Claude Messages request |
| | Response, streaming | a Claude event stream | Chat Completions chunks |
| | Response, non-streaming | a whole Claude event stream | one Chat Completions response |
| `claude::openai::responses` | Request | an OpenAI Responses request | the Claude Messages request |
| | Request, compatibility mode | the same | the Claude Messages request |
| | Response, streaming | a Claude event stream | Responses events |
| | Response, non-streaming | a whole Claude event stream | one Responses response |
| `signature` | Checks and replay decisions | one signature, a model and a target provider | every check's result and every replay decision |
| | Claude Messages sanitizers | a Claude Messages request, a model and options | each sanitizer's output and report |
| | Gemini sanitizer and validators | a Gemini request, the path of its `contents` and options | the sanitized request and each validator's error |
| `registry` | Requests | a request, its format and the provider's | the translated request, with reasoning summary settings carried over |
| | Streaming responses | a provider's event stream and both formats | each event's chunks, then the chunks given at the end |
| | Non-streaming responses | a provider's final event and both formats | the client's response |
| | Lookups and token counts | two formats and a token count | which translators are registered, and the count in the client's format |

## Running it

You need Go and a CLIProxyAPI checkout. Use the version pinned in [UPSTREAM.md](../../UPSTREAM.md) or one close to it; translator changes made upstream since the pin show up as differences.

```sh
git clone https://github.com/router-for-me/CLIProxyAPI ../CLIProxyAPI
cargo run --release -p open-ferry-parity -- --upstream ../CLIProxyAPI
```

Options:

| Flag | Default | |
|---|---|---|
| `--random <n>` | 5000 | Random cases to generate per suite |
| `--seed <n>` | 1 | Seed for the random cases |
| `--show <n>` | 10 | Kinds of difference to print |
| `--go <path>` | `go` | Go binary |
| `--live <url>` | | Send a few requests through a running CLIProxyAPI instead (see below) |
| `--model <name>` | | Model for `--live` |

The exit status is 0 when every case is identical, equivalent or a known difference (see below).

## How it works

Upstream's translators are in `internal/` packages, which only code inside the CLIProxyAPI module can import. `go/main.go` is a small harness that reads JSON lines from stdin and writes each translation to stdout. A line can carry `options`, a JSON object of inputs that aren't part of the request, such as a signature's target provider. The tool builds it with `go build -overlay`, which adds the file to the module as `cmd/open-ferry-parity` at build time. Your checkout is not modified.

Each translator gets:

- **Hand-written cases** (`src/cases.rs`, `src/cases/responses.rs` and `src/cases/chat.rs` for the Responses and Chat Completions translators, `src/cases/claude_chat.rs` and `src/cases/claude_responses.rs` for Chat Completions and Responses to Claude, `src/cases/signature.rs`, and `src/cases/registry.rs`) for inputs the generator is unlikely to produce. For requests: duplicate keys, a 2 MiB image, a 100-level schema, a schema nested deeper than serde_json reads, numbers too large for a float, bodies that aren't objects, keys written with escapes, and for Claude, every tool choice and effort level against models with and without effort levels. For responses: parallel function calls with text arriving between them, web search, a policy error, a call ID that is empty, arguments replaced partway through a character, an `apply_patch` call whole, invalid and cut short, SSE noise, and the client's model given in each place upstream looks for it. For the registry: every other suite's hand-written cases through their own pair of formats, bodies and models of every type through pairs with no translator, every way each format sets and reads a reasoning summary, and every pair of formats looked up, including spellings upstream doesn't know.
- **Random cases**, reproducible from the seed. The request generator (`src/generate.rs`) mixes well-formed requests with the sloppy input upstream tolerates: wrong value types, missing fields, unknown roles, near-miss reasoning signatures, case and Unicode edge cases, and JSON written pretty, compact or with escaped characters. The response generator (`src/generate/response.rs`) builds Codex event streams of reasoning, text, function calls and web searches, one item after another or interleaved. Events are sometimes dropped, repeated or loosely typed, and the final event lists more or fewer items than were streamed, or is an error, or is missing. The final event of each stream is also a non-streaming case.

  For the Responses translators (`src/generate/responses.rs`), requests mix the fields upstream sets, drops or renames with loosely typed values: required booleans as strings, blank and nearly blank call arguments, every role spelling, web search aliases in each place they're renamed, and cache breakpoints at every level. The event streams are the Codex streams above, with the model in `response.created` and `response.in_progress` removed or mistyped, and the event's `type` or `response` sometimes replaced, and the client's model given in the request, the translated request, the model parameter, or nowhere.

  For the Chat Completions translators (`src/generate/chat.rs`), requests mix every message role and content part with assistant tool calls in function and custom form, whose IDs are missing, empty or shared. Tool messages answer them by ID, by a shared ID, or by none, with content as text, parts, parts written as a JSON string, or other values. Tools are functions, custom tools and built-ins, named so that they need sanitizing or shortening, or collide once shortened. The event streams are the Codex streams above, for a request declaring the same tools, with `apply_patch` declared as a custom tool, also as a function, or in a namespace. Added to them are the events only this translator reads: `apply_patch` calls streamed as custom tool input, generated images, raw reasoning text, and service tiers, creation times and models in various places.

  For Chat Completions to Claude (`src/generate/claude_chat.rs`), requests are the Chat Completions requests above with what only the Claude translator reads added: `cache_control` markers on messages, parts and tools, valid or not; assistant `reasoning_content`; images and files as data URLs, including malformed ones; `top_p`, `stop` and token limits, loosely typed; user IDs that are present, blank or mistyped; every field that shows or hides reasoning summaries; Claude effort levels; and `allowed_tools` choices in each of their shapes, with `parallel_tool_calls`. The model is a Claude model with effort levels, one with budgets only, or one the catalog doesn't know. The event streams are Claude's: text, thinking, redacted thinking and tool use blocks streamed in deltas, with usage split between `message_start` and `message_delta`. Indexes and counts are sometimes missing or loosely typed, events are sometimes missing or swapped, and some lines aren't events at all. Each whole stream, framed as SSE, is also a non-streaming case.

  For Responses to Claude (`src/generate/claude_responses.rs`), requests carry `instructions` and system or developer items, message parts of every type with `cache_control` at every level, Codex agent messages, and reasoning items signed by Claude for the target model or another, by GPT, redacted, or not at all. Tool calls in function and custom form have call IDs that are missing, repeated or in need of cleaning, and namespaces; their outputs are paired, shuffled, repeated, orphaned or ahead of their call, with text, image and file parts. Web search calls carry queries and results in each of their shapes. Tools are functions, custom tools including `apply_patch`, namespaces, web searches with and without their options, built-ins Claude lacks and unknown types, declared at the top level or in `additional_tools` items, with long and colliding names. Every shape of `tool_choice`, reasoning effort and summary, token limit, service tier, output format and user ID appears, as do the fields a Responses response repeats from the request. The model has effort levels, budgets only, rejects an assistant prefill, or is unknown. The event streams are Claude's: text with citations, thinking with signatures, redacted thinking, tool use (including `apply_patch` input) and web searches with their results, damaged as in the Chat Completions streams above, each answering a generated request (or none) so tool names are mapped back. Each whole stream, framed as SSE, is also a non-streaming case. Upstream finishes open tool calls by ranging over a Go map, so streams where two could be finished at once are drawn again, since upstream's order would be random.

  Signatures (`src/generate/signature.rs`) are built with each provider's real layout, with fields left out or changed: Claude's classic, CAIS and CAQS protobuf envelopes in one or two base64 layers, Gemini's envelopes around Tink payloads, UUIDs and tool blocks, GPT's Fernet tokens, random bytes for Grok and Kimi around their length and entropy limits, and SWE's `sealed.v1.` prefix. Some are then damaged with a cache prefix, whitespace, a cut, or a changed or inserted character. They appear in the thinking blocks and `tool_use` parts of the Claude requests above, in Gemini `contents` whose function calls and responses don't always pair up, and on their own with each target provider.

  For the registry (`src/generate/registry.rs`), requests are the generated requests of the other suites, sent through their own pair of formats or another, sometimes with another model and with reasoning summary settings set at the paths each format reads them from, in valid and invalid values. Half are instead sent through a translator that changes nothing, so only the summary settings carried between formats change the body: mostly the source format's settings, sometimes the target's to be written over, Claude thinking fields and token limits, and objects on the way that are something else, with models that do and don't think. The response cases are the other suites' generated streams and final events through their own pair, and every tenth through a pair with no translator. Lookups are random pairs of formats, spelled as upstream knows them or not, with token counts.

`src/translator.rs` reads each translator's output as JSON. For the signature suites, both sides return a report of every check (`src/signature.rs` builds ours), with errors as their messages. A Claude stream becomes a list of `{"event", "data"}` frames, so frame order, event names and every field are compared. Tool IDs generated for calls that arrive without one (`toolu_<nanoseconds>_<counter>`, or in Claude requests `toolu_` and 24 random letters and digits) are masked on both sides before comparing. Each side's IDs become `toolu_(generated-1)`, `toolu_(generated-2)` and so on, in the order they first appear, so a reference to another call's ID still shows. An ID found anywhere in the case's input, including JSON held in a string, came from the client and is never masked. The Responses stream translator passes most lines through, so its output is read line by line: `=` for a line returned byte for byte, and otherwise the line's JSON. Chat Completions chunks are read the same way. A Chat Completions response's `created` falls back to the current time, which each side reads from its own clock, so within an hour of now it is masked on both sides. The same goes for every chunk translated from a Claude stream, and for the `created_at` of a Responses response translated from Claude: in each streamed event's `response`, and at the top level of a non-streaming one. A registry suite's output is read as the output of the translator the registry chose, so the masks and accepted deviations above apply to it too. For a stream, both sides also report the chunks each event gave and those given at the end, and whether an unfinished tool call's input failed to parse. Chunks of SSE frames are listed by their frames, so a frame split between chunks or left unended shows; other chunks are compared one by one. A non-streaming translation that failed (nil upstream, `None` here) reads differently from an empty response.

The comparison (`src/compare.rs`) walks both outputs. Each case ends up in one of four groups:

- **identical**: the same JSON, including key order and number text.
- **equivalent**: the only differences are documented deviations (see UPSTREAM.md):
  - *tool parameter key order*: upstream sorts schema keys, we keep the client's order;
  - *embedded JSON re-serialized*: a string holds the same JSON, written compactly by us (in responses, a web search query, where Go also escapes `<`, `>` and `&`, and the request fields a Responses response repeats as text when the client sent something else). It is accepted only at the paths each translator lists in `Translator::embedded_json` (`src/translator.rs`), and only in the form listed there: the whole string, JSON inside unchanged text, or Go's escaping. Anywhere else the string must match;
  - *cut at a character boundary*: upstream cut a name or ID in the middle of a character (written as U+FFFD), we cut before it;
  - *protobuf error prefix space*: the harness's protobuf-go build writes a non-breaking space after `proto:`, and we write a regular one;
  - *out-of-range number saturated*: Go converted a float too large for int64 to the minimum int64, as amd64 does, and we saturate, as arm64 does;
  - *made-up user ID left out*: upstream filled a Claude request's `metadata.user_id` for a client that sent none, and we don't. Upstream's ID is removed before comparing.
- **known**: a hand-written case marked with `known_difference`, such as behaviour not ported yet.
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
