# open-ferry-parity

Differential tests of open-ferry's translators against upstream [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI). The tool runs the same input through upstream's Go translators and through our Rust ports, then compares what they produce.

Config-save cases run concurrently on the available logical processors. Use `--jobs N` to limit case workers, or `--jobs 1` for the sequential path. Results, proofs, failure files and decision logs are recorded in the original case order. Other suites remain sequential. Case workers run their protection-ablation replay tails inline, so at most `N` cases and Go helpers execute at once; `--jobs 1` retains the original four-thread replay tail. At most twice the worker count is dispatched before recording, bounding buffered results and staging files.

One hundred and eight suites are covered so far:

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
| | Response, non-streaming | a whole Claude event stream, or a whole Messages response | one Chat Completions response |
| `claude::openai::responses` | Request | an OpenAI Responses request | the Claude Messages request |
| | Request, compatibility mode | the same | the Claude Messages request |
| | Response, streaming | a Claude event stream | Responses events |
| | Response, non-streaming | a whole Claude event stream, or a whole Messages response | one Responses response |
| `openai::responses` | Request | an OpenAI Responses request | the OpenAI Chat Completions request |
| | Response, streaming | a Chat Completions stream | Responses events |
| | Response, non-streaming | a Chat Completions response | one Responses response |
| `openai::claude` | Request | a Claude Messages request | the OpenAI Chat Completions request |
| | Request, compatibility mode | the same | the OpenAI Chat Completions request |
| | Response, streaming | a Chat Completions stream | Claude SSE events |
| | Response, non-streaming | a Chat Completions response | one Claude message |
| `openai::chat_completions` | Request | an OpenAI Chat Completions request | the same, for the upstream's model |
| | Response, streaming | a Chat Completions stream | each line's chunk, passed through |
| | Response, non-streaming | a Chat Completions response | the response, passed through |
| `codex::gemini` | Request | a Gemini `generateContent` request | the Codex request |
| | Response, streaming | a Codex event stream | Gemini responses |
| | Response, non-streaming | Codex's final event | one Gemini response |
| `claude::gemini` | Request | a Gemini `generateContent` request | the Claude Messages request |
| | Response, streaming | a Claude event stream | Gemini responses |
| | Response, non-streaming | a whole Claude event stream | one Gemini response |
| `openai::gemini` | Request | a Gemini `generateContent` request | the OpenAI Chat Completions request |
| | Response, streaming | a Chat Completions stream | Gemini responses |
| | Response, non-streaming | a Chat Completions response | one Gemini response |
| `signature` | Checks and replay decisions | one signature, a model and a target provider | every check's result and every replay decision |
| | Claude Messages sanitizers | a Claude Messages request, a model and options | each sanitizer's output and report |
| | Gemini sanitizer and validators | a Gemini request, the path of its `contents` and options | the sanitized request and each validator's error |
| `registry` | Requests | a request, its format and the provider's | the translated request, with reasoning summary settings carried over |
| | Streaming responses | a provider's event stream and both formats | each event's chunks, then the chunks given at the end |
| | Non-streaming responses | a provider's final event and both formats | the client's response |
| | Lookups and token counts | two formats and a token count | which translators are registered, and the count in the client's format |
| `completions` | Request | a legacy Completions request | the Chat Completions request |
| | Response | a Chat Completions response | the Completions response |
| | Stream chunks | Chat Completions stream chunks, each converted on its own | each chunk's Completions chunk, or none where upstream skips it |
| `gemini::gemini` | Request | a Gemini request | the same, with roles fixed, function responses named and safety settings added |
| | Response, streaming | a Gemini stream | each line's chunk, passed through |
| | Response, non-streaming | a Gemini response | the response, passed through |
| `gemini::claude` | Request | a Claude Messages request | the Gemini request |
| | Request, compatibility mode | the same | the Gemini request |
| | Response, streaming | a Gemini stream | Claude SSE events |
| | Response, non-streaming | a Gemini response | one Claude message |
| `gemini::openai::chat_completions` | Request | an OpenAI Chat Completions request | the Gemini request |
| | Response, streaming | a Gemini stream | Chat Completions chunks |
| | Response, non-streaming | a Gemini response | one Chat Completions response |
| `providers::codex::thinking` | Thinking settings | a request translated for Codex or Responses, the client's request, both formats, the provider and the model the request is bound to | the request with the model's suffix or the requested effort applied, or the error |
| `providers::openai_compat::thinking` | Thinking settings | the same, for a request translated for Chat Completions | the same |
| `gemini::openai::responses` | Request | an OpenAI Responses request | the Gemini request |
| | Response, streaming | a Gemini stream, the original request and the translated one | Responses events, then those given at the end |
| | Response, non-streaming | a whole Gemini response and both requests | one Responses response |
| `codex_models` (`open-ferry-core`) | Codex client model list | models registered for providers, a client version and settings | the list's length, SHA-256 and body, long strings hashed |
| `codex_client::multi_agent_v2` | Tools readied | a Codex client's Responses request, its user agent, the setting and the models registered | the request with its collaboration tools' messages readable and the models `spawn_agent` may pick in its description |
| | Request optimized | the same | the request as the Codex executor sends it to another upstream, and whether its namespace was renamed |
| | Agent messages | the same, and whether the model is a compatibility model | the request with its agent messages as user messages |
| | Namespace restored | an upstream's event and whether the request was optimized | the event's text with the namespace renamed back |
| `codex_client::orphan_delegation` | Orphan delegation outputs | a Codex sub-agent's Responses request, its `X-Openai-Subagent` header and the setting | the request with its unanswered delegation outputs as user messages |
| `payload` (`open-ferry-providers`) | Payload rules applied | a translated body, a config's payload rules, the executor, protocol, models, request path and headers of the call, and the paths to track | the body with the rules applied and the tracked paths they touched, or that the config or body can't be read |
| `observe::usage` (`open-ferry-core`) | Usage parsed from upstream responses | an upstream's response body or stream line and the parser for its protocol | the usage it reads, or none |
| | First-token events | an upstream's stream event and its protocol | whether it carries the first token |
| `session` (`open-ferry-core`) | Sessions requests name | a request's headers and body, and the connection's execution session | the session it names, with its parent, agent, client and whether it forked or is a subagent, or none |
| | Session identities derived | a request body, its client format and the caller scope | the identity derived for it, or none |
| `config::diff` (`open-ferry-core`) | Config change details | two configs | the change details logged when the config is reloaded |
| `config::save` (`open-ferry-core`) | Config file writes | a config file and the writes made to it: saves of a config, as read or changed, with and without the move to the v8 layout, nested strings set, and whole files written as the management API writes them | the file after each write, or why a write was refused |
| `manager::quota_signals` (`open-ferry-core`) | Quota snapshots of response headers | one response's headers, its provider and the snapshot before it | whether the snapshot changed, and the snapshot after it |
| `interactions::claude` and `claude::interactions` | Claude ↔ Interactions, 7 suites: the request both ways, Claude's also in compatibility mode, and the response both ways, streaming and non-streaming | a Claude Messages or Interactions request; an Interactions or Claude event stream, or a whole response | the request, events or response in the other format |
| `openai::interactions::chat_completions` | Chat Completions ↔ Interactions, 6 suites: the request and the response both ways, streaming and non-streaming | a Chat Completions or Interactions request; an Interactions or Chat Completions stream, or a whole response | the same, in the other format |
| `openai::interactions::responses` | Responses ↔ Interactions, 7 suites: the request and the response both ways, streaming and non-streaming, and the tool input error after an Interactions stream (`FinalizeToolInput`) | a Responses or Interactions request; an Interactions or Responses stream, or a whole response, with the client's request | the same, in the other format, and why an `apply_patch` call's input failed |
| `codex::interactions` | Interactions → Codex, 3 suites: the request and the response, streaming and non-streaming | an Interactions request; a Codex event stream, or Codex's final event | the Codex request; Interactions events or one interaction |
| `gemini::interactions` | Gemini ↔ Interactions and the Interactions passthrough, 9 suites: the request and the response both ways and passed through, streaming and non-streaming | a Gemini or Interactions request; an Interactions or Gemini stream, or a whole response | the same, in the other format or passed through, with a failed interaction as a Gemini error |
| `registry`, Interactions pairs | Requests and responses, within the registry suites above | each family's cases through its pairs, ten in all: `claude`, `openai` and `openai-response` to and from `interactions`, `interactions` to `codex`, `interactions` to and from `gemini`, and `interactions` to itself | as for the other registry rows |

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
| `--summary <file>` | | Also write a Markdown summary of the results to `<file>`: the upstream version, each suite's cases and outcomes, and the totals. In an existing file it replaces the part between the `<!-- parity-summary:start -->` and `<!-- parity-summary:end -->` lines, as in the [README](../../README.md#parity-with-cliproxyapi) |

The exit status is 0 when every case is identical, equivalent or a known difference (see below).

Use a repeatable `--suite <name>` to run selected suites by their directory
slugs, for example `--suite config-save --suite config-diff`. Unknown names
are errors; omitting the flag runs all suites. This offline filter cannot be
combined with `--live`. For a focused config-save sweep:

```sh
cargo run --release -p open-ferry-parity -- --upstream ../CLIProxyAPI --suite config-save --random 5000 --seed 10
```

## In CI

The existing required `check (ubuntu-latest)` includes offline parity on pull
requests and in the merge queue. Its path filter covers the translate, providers
and core crates, this tool, the upstream submodule pin, Cargo manifests and lock
file, the toolchain, the config example read by the cases, and its CI files.
Unrelated changes skip parity; a manual CI dispatch runs it. Every hand-written
case and `--random 25 --seed 1` run across all suites. Any unexpected difference
returns a nonzero status and fails the aggregate. The checked-in accepted cases
are the `known_difference` markers in `src/cases.rs`, `src/cases/**` and
`src/interactions/**/cases.rs`; the equivalence rules remain in `src/compare.rs`
and [UPSTREAM.md](../../UPSTREAM.md).

The fixed seed reproduces inputs. `settings-for-the-same-field` deliberately
supplies aliases that overwrite the same generation setting. Upstream iterates
a Go map, so its output can either match Rust's stable order or be a known
difference on separate runs. The identical/known counts can therefore vary;
the fixture's existing marker accepts either outcome. A tally regression test
checks that neither outcome can become an unexpected difference.

[Parity (scheduled)](../../.github/workflows/parity-scheduled.yml) runs daily at
06:23 UTC with `--random 2000` and the Actions run number as its printed seed.
This separate workflow fails visibly on a mismatch and is outside the required
checks. Its manual dispatch accepts optional `seed` and `random` inputs; an empty
seed uses the run number. For example:

```sh
gh workflow run parity-scheduled.yml --ref main -f seed=42 -f random=100
```

Both runs use the same reusable job. It checks out the exact
`reference/cliproxyapi` gitlink (currently `911f49cbd49f4fe2516979754a7488a3fd14fe64`,
v8.0.38), checks that the checkout matches, and uses Go 1.26.4, satisfying that
pin's `go 1.26.0`. Cargo uses CI's shared Rust dependency cache, filled by the
existing main-branch test job. The Go module downloads and the harness's own
`target/parity/go-build` cache are kept separately. No credentials or live
provider calls are used. The log and job summary show both commits, seed, random
count, each suite's case count, and identical, equivalent, known and different
totals.

On failure, `parity-failures-<run-id>-<attempt>` keeps the first 200 unexpected
cases per suite, or 1,000 for config-save frequency sweeps, with both outputs,
metadata and the log for 14 days. Download it from
the failed run's Actions page, or with `gh run download <run-id> -n <artifact>`.
To keep a failing case as a regression fixture:

1. Find its suite from the artifact directory and the corresponding hand-written
   cases function in `src/cases/**` or `src/interactions/**/cases.rs`.
2. Add a `Case::new` (request) or `Case::response` (response) with the saved
   `model`, exact `request_text` and `events`. Copy `translated_request` and
   `options` when present. Keep the request text rather than reserializing the
   parsed `request`, so whitespace and number spelling stay reproducible.
3. Give it a descriptive name, leave `known_difference` unset, and reproduce
   locally against the pinned checkout with the recorded seed and random count.
   After fixing the translator, rerun parity and commit the case with the fix.

## How it works

Upstream's translators are in `internal/` packages, which only code inside the CLIProxyAPI module can import. `go/main.go` is a small harness that reads JSON lines from stdin and writes each translation to stdout. A line can carry `options`, a JSON object of inputs that aren't part of the request, such as a signature's target provider. The tool builds it with `go build -overlay`, which adds the file to the module as `cmd/open-ferry-parity` at build time, along with each `go/parity_*.go`; such a file adds its translators to `go/main.go`'s from `init()`. Your checkout is not modified. The builds use a Go build cache of their own, `target/parity/go-build`, so parity runs in two checkouts at once don't mix up each other's harnesses; the first run in a checkout builds the CLIProxyAPI packages from scratch.

The legacy Completions conversions are unexported functions in `sdk/api/handlers/openai`, so the overlay also adds `go/openai/export.go` to that package to export them. They get a second harness, `go/completions/main.go`, built as `cmd/open-ferry-parity-completions`: importing that package registers translators `go/main.go` doesn't import, which would change what the registry suites see.

Two more harnesses exist for the same reason. The executor helpers (`internal/runtime/executor/helps`) run in `go/helps/main.go`, built as `cmd/open-ferry-parity-helps` with each `go/helps/parity_*.go`, since importing them registers the translator from Claude to the Gemini Interactions API; the payload, usage and first-token suites run there. The Gemini Interactions translators run in `go/interactions/main.go`, built as `cmd/open-ferry-parity-interactions` with each `go/interactions/parity_*.go`, one file per family of translator packages. The registry suites gain a family's pairs only through a blank import in that family's `go/parity_registry_*.go`, made in the same commit as the Rust registry gains them, so no file of `go/main.go`'s harness imports the helpers or an Interactions package any other way. `src/upstream.rs` sends each entry to its harness by its key.

Each translator gets:

- **Hand-written cases** (`src/cases.rs`, `src/cases/responses.rs` and `src/cases/chat.rs` for the Responses and Chat Completions translators, `src/cases/claude_chat.rs` and `src/cases/claude_responses.rs` for Chat Completions and Responses to Claude, `src/cases/openai_responses.rs` and `src/cases/openai_claude.rs` for Responses and Claude to Chat Completions, `src/cases/openai_chat.rs` for Chat Completions passed through, `src/cases/gemini.rs` for the translators from Gemini clients, `src/cases/to_gemini.rs` for the translators to Gemini, `src/cases/gemini_responses.rs` for Responses to Gemini, `src/cases/signature.rs`, `src/cases/registry.rs`, and `src/cases/completions.rs` for the legacy Completions conversions) for inputs the generator is unlikely to produce. For requests: duplicate keys, a 2 MiB image, a 100-level schema, a schema nested deeper than serde_json reads, numbers too large for a float, bodies that aren't objects, keys written with escapes, and for Claude, every tool choice and effort level against models with and without effort levels. For responses: parallel function calls with text arriving between them, web search, a policy error, a call ID that is empty, arguments replaced partway through a character, an `apply_patch` call whole, invalid and cut short, SSE noise, the client's model given in each place upstream looks for it, and whole Claude Messages responses, as Claude answers a call that doesn't stream, with every kind of block, loosely typed values, and bodies that aren't one. From Chat Completions: tool calls split every way, a call whose ID or name changes partway, `apply_patch` arguments cut inside an escape, usage before, with or after the finish reason, streams with no `[DONE]` or nothing but it, and tool names given back to a Claude client in another case or spacing than declared. From Gemini clients: function calls and responses paired by ID and in order, IDs padded or unknown, tool names Codex shortens and gives back, every kind of inline and file data in both spellings, both spellings of the system instruction, thinking levels and budgets against models with and without effort levels, and requests pretty-printed so derived call IDs differ. The Gemini response suites also read the other suites' Codex, Claude and Chat Completions streams and responses. For the registry: every other suite's hand-written cases through their own pair of formats, bodies and models of every type through pairs with no translator, every way each format sets and reads a reasoning summary, and every pair of formats looked up, including spellings upstream doesn't know. For the translators to Gemini: function responses to name from the calls before them, `contents` as an object or not a list, both keys for function declarations and both schema keys, safety settings given in each form, every thinking setting against models with and without budgets, Chat Completions media parts, signatures on tool calls, Google's tools and every `allowed_tools` choice; and streams with thoughts, signatures alone, calls continued without a name, inline data, two candidates, usage without a finish reason or the reverse, and creation times Go's parser accepts and rejects. For Responses to Gemini: signature carriers binding forwards, backwards and to nothing, malformed or oversized, signatures for another provider or that bypass the validator, every audio, video, file and image format, web search beside functions and with allowed domains, namespace calls, an `apply_patch` round trip, outputs missing or out of order, and every effort level; and streams that end every way (usage after the finish reason, split or under `cpaUsageMetadata`, `MAX_TOKENS`, `[DONE]` with or without a finish reason), `apply_patch` calls whole, repeated, conflicting, unnamed or not a patch, lines that aren't JSON with and without `apply_patch` declared, grounding with citations before or after the text, trailing and standalone signatures, wrapped chunks and requests, and several candidates. For the legacy Completions conversions: the examples from OpenAI's API reference, prompts of every type, numbers as text in each of Go's float forms, floats too large or not numbers, bodies and chunks that aren't JSON, are cut short or are SSE lines, and chunks upstream skips.
- **Random cases**, reproducible from the seed. The request generator (`src/generate.rs`) mixes well-formed requests with the sloppy input upstream tolerates: wrong value types, missing fields, unknown roles, near-miss reasoning signatures, case and Unicode edge cases, and JSON written pretty, compact or with escaped characters. The response generator (`src/generate/response.rs`) builds Codex event streams of reasoning, text, function calls and web searches, one item after another or interleaved. Events are sometimes dropped, repeated or loosely typed, and the final event lists more or fewer items than were streamed, or is an error, or is missing. The final event of each stream is also a non-streaming case.

  For the Responses translators (`src/generate/responses.rs`), requests mix the fields upstream sets, drops or renames with loosely typed values: required booleans as strings, blank and nearly blank call arguments, every role spelling, web search aliases in each place they're renamed, and cache breakpoints at every level. The event streams are the Codex streams above, with the model in `response.created` and `response.in_progress` removed or mistyped, and the event's `type` or `response` sometimes replaced, and the client's model given in the request, the translated request, the model parameter, or nowhere.

  For the Chat Completions translators (`src/generate/chat.rs`), requests mix every message role and content part with assistant tool calls in function and custom form, whose IDs are missing, empty or shared. Tool messages answer them by ID, by a shared ID, or by none, with content as text, parts, parts written as a JSON string, or other values. Tools are functions, custom tools and built-ins, named so that they need sanitizing or shortening, or collide once shortened. The event streams are the Codex streams above, for a request declaring the same tools, with `apply_patch` declared as a custom tool, also as a function, or in a namespace. Added to them are the events only this translator reads: `apply_patch` calls streamed as custom tool input, generated images, raw reasoning text, service tiers, creation times and models in various places, and messages of several parts whose URL citations, repeated and with odd ranges, come on every event that can carry them.

  For Chat Completions to Claude (`src/generate/claude_chat.rs`), requests are the Chat Completions requests above with what only the Claude translator reads added: `cache_control` markers on messages, parts and tools, valid or not; assistant `reasoning_content`; images and files as data URLs, including malformed ones; `top_p`, `stop` and token limits, loosely typed; user IDs that are present, blank or mistyped; every field that shows or hides reasoning summaries; Claude effort levels; and `allowed_tools` choices in each of their shapes, with `parallel_tool_calls`. The model is a Claude model with effort levels, one with budgets only, or one the catalog doesn't know. The event streams are Claude's: text, thinking, redacted thinking and tool use blocks streamed in deltas, with usage split between `message_start` and `message_delta`. Indexes and counts are sometimes missing or loosely typed, events are sometimes missing or swapped, and some lines aren't events at all. Each whole stream, framed as SSE, is also a non-streaming case, and so, for half as many streams, is the whole Messages response the stream builds (`src/generate/claude_native.rs`), as Claude answers a call that doesn't stream: written compact, pretty-printed or escaped, and now and then with a `type` or `content` that keeps it from being read as one.

  For Responses to Claude (`src/generate/claude_responses.rs`), requests carry `instructions` and system or developer items, message parts of every type with `cache_control` at every level, Codex agent messages, and reasoning items signed by Claude for the target model or another, by GPT, redacted, or not at all. Tool calls in function and custom form have call IDs that are missing, repeated or in need of cleaning, and namespaces; their outputs are paired, shuffled, repeated, orphaned or ahead of their call, with text, image and file parts. Web search calls carry queries and results in each of their shapes. Tools are functions, custom tools including `apply_patch`, namespaces, web searches with and without their options, built-ins Claude lacks and unknown types, declared at the top level or in `additional_tools` items, with long and colliding names. Every shape of `tool_choice`, reasoning effort and summary, token limit, service tier, output format and user ID appears, as do the fields a Responses response repeats from the request. The model has effort levels, budgets only, rejects an assistant prefill (some by a version that follows a provider namespace, is dotted or comes before a snapshot date), or is unknown. The event streams are Claude's: text with citations, thinking with signatures, redacted thinking, tool use (including `apply_patch` input) and web searches with their results, damaged as in the Chat Completions streams above, each answering a generated request (or none) so tool names are mapped back. Each whole stream, framed as SSE, is also a non-streaming case, and so, for half as many streams, is the whole Messages response the stream builds (`src/generate/claude_native.rs`), as Claude answers a call that doesn't stream: written compact, pretty-printed or escaped, and now and then with a `type` or `content` that keeps it from being read as one. Upstream finishes open tool calls by ranging over a Go map, so streams where two could be finished at once are drawn again, since upstream's order would be random.

  For Responses to Chat Completions (`src/generate/openai_responses.rs`), requests carry `instructions`, messages in every role with text, image, video and file parts, image details in every spelling, and `reasoning_content` on messages and calls. Reasoning items have summaries that are joined, blank, the placeholder text or not text at all, before a call, a message or nothing. Function and custom tool calls, sometimes in a namespace, have call IDs that are missing, padded or repeated; their outputs answer them in order, after an intervening message, more than once, without an ID or with no call at all, as text, as parts with images, or as parts written as JSON text. Custom tool input holds characters that need escaping, but is never an object or array, whose JSON text upstream copies into the arguments. The client's local shell has calls in its environment, with none or in a container, with actions whole, odd or missing, and outputs whose results upstream gives the model whole, beside function calls under the names the shell's calls are replayed under. Tools are functions, custom tools including `apply_patch`, namespaces whose qualified names are long, collide, or cut inside a character, `shell` tools local, in a container, in a namespace or with odd environments, tools named as the shell's function would be, built-ins and unknown types, declared at the top level or in `additional_tools` items. Every shape of `tool_choice`, the local shell's included, reasoning effort, `max_output_tokens`, `parallel_tool_calls` and `text.format` appears. The streams answering them are the Chat Completions streams below, calling the request's tools by the names the translated request gives them, by the names the client wrote, or by names neither declares, with arguments of the kind each tool takes: JSON for functions, `{"input": ...}` for custom tools, patches with escapes cut across chunks for `apply_patch`, and for the local shell, actions with limits of every number form and arguments that aren't an action for each reason upstream has, none with a key twice. The request is given as the client sent it, as translated, both, or neither, and the translated request sometimes holds only the fields a Responses response repeats, loosely typed. Namespaces are named so that cutting a call's full name never splits a character, since upstream cuts bytes there and every call to such a tool would read differently (see UPSTREAM.md). Each stream's whole response is also a non-streaming case.

  For Chat Completions passed through (`src/generate/openai_chat.rs`), requests are the Chat Completions requests above with the `model` the one asked for, another, missing or not a string. The streams are chunks as OpenAI-compatible providers send them: a role chunk or none, text and reasoning (`reasoning_content`, `reasoning` or `reasoning_details`) cut at character boundaries, tool calls whose ID, name and arguments come in separate chunks, interleaved with each other or with text, a second choice, a finish reason with usage, before it or without it, usage-only chunks, error events, `[DONE]` early, twice or never, and lines that aren't data. Whole responses carry the same in one message, its content sometimes a list of parts, with fields missing or of the wrong type. The other Chat Completions response suites answer with these streams and responses too.

  For the translators from Gemini clients (`src/generate/gemini.rs`), requests have contents in every role, with text, thought, inline and file data, function call and function response parts in their camelCase and snake_case spellings; IDs that are present, padded, blank, numeric or answer an earlier call; function declarations whose names are long, collide once cut or hold multi-byte characters, with `parameters` or `parametersJsonSchema` schemas whose types are in any case; tool configs in both spellings; generation configs with sampling values, token limits, stop sequences, modalities and thinking levels and budgets; service tiers and client user IDs. Each asks for a stream or not. They leave out what we write differently on purpose: a `temperature` or `topP` that isn't finite, a schema `type` that isn't a string or a property named `type`, and property names holding sjson path syntax. The responses are the Codex streams above answering a Gemini request that declares the tools they call, sometimes with a `created_at`; the Claude streams above; and Chat Completions streams whose arguments a lenient reader has to make sense of: keys without quotes, text around the object, missing commas. Upstream copies a Codex call's arguments that start with `{` but don't parse, and a Claude call's input that doesn't parse, into its output as they are, so that output isn't JSON; those arguments are replaced first, in the hand-written and registry cases too.

  For Claude to Chat Completions (`src/generate/openai_claude.rs`), requests are the Claude requests above with what only this translator reads added: `top_p`, `stop_sequences` and `user`, loosely typed, and tool choices that disable parallel calls. Each asks for a stream or not. The streams answering them call tools the Claude request declares in various cases, padded or with leading underscores, by the name as declared, in another case or spacing, or not at all, with arguments that are sometimes single-quoted for the translator to repair.

  Signatures (`src/generate/signature.rs`) are built with each provider's real layout, with fields left out or changed: Claude's classic, CAIS and CAQS protobuf envelopes in one or two base64 layers, Antigravity's Q form (a CAQS envelope for Google's thinking channel in two base64 layers) changed one field or one layer at a time, Gemini's envelopes around Tink payloads, UUIDs and tool blocks, GPT's Fernet tokens, random bytes for Grok and Kimi around their length and entropy limits, and SWE's `sealed.v1.` prefix. Some are then damaged with a cache prefix, whitespace, a cut, or a changed or inserted character. They appear in the thinking blocks and `tool_use` parts of the Claude requests above, in Gemini `contents` whose function calls and responses don't always pair up, and on their own with each target provider.

  For the registry (`src/generate/registry.rs`), requests are the generated requests of the other suites, sent through their own pair of formats or another, sometimes with another model and with reasoning summary settings set at the paths each format reads them from, in valid and invalid values. Half are instead sent through a translator that changes nothing, so only the summary settings carried between formats change the body: mostly the source format's settings, sometimes the target's to be written over, Claude thinking fields and token limits, and objects on the way that are something else, with models that do and don't think. The response cases are the other suites' generated streams and final events through their own pair, and every tenth through another pair, usually one with no translator, though never another suite's stream through the Gemini to Claude translator. A Codex call's arguments that start like JSON but aren't are replaced with `{}` on the way to Gemini, and to Interactions in a final event, as upstream copies them into its output as they are (see UPSTREAM.md); Interactions streams send them as text. Lookups are random pairs of formats, spelled as upstream knows them or not, with token counts.

  For the translators to Gemini (`src/generate/to_gemini.rs`), Gemini requests take their `contents` from the signature generator, with roles to fix and some function responses unnamed, now and then as an object; added to them are tools declared under either key with either schema key, a generation config with a `responseSchema`, a system instruction and safety settings. Claude requests are the Claude requests above with sampling settings, loosely typed, for a Gemini model. Chat Completions requests are the Chat Completions requests above with what only this translator reads: sampling and output settings, `n`, modalities, image settings, response formats, Google's tools, strict tools, `allowed_tools` choices, reasoning text, thought signatures in each place upstream looks, and video, audio, file and image parts. Their tool calls' arguments are always JSON, since upstream copies other text into invalid JSON (see UPSTREAM.md). The streams answer a request declaring tools whose names Gemini sanitizes, and call them by the sanitized name, the declared one, or, for Claude, in another case or with a leading underscore, or by names never declared. They hold text, thoughts, signatures alone, calls continued without a name, inline data, transcripts, a second candidate, finish reasons, creation times and usage, loosely typed. They come as `data:` lines or bare JSON with SSE noise, as Vertex AI's executor passes them; half of the Claude translator's come as bare JSON then `[DONE]`, as the Gemini executor passes them. Each stream's last chunk, whole, is also a non-streaming case.

  For Responses to Gemini (`src/generate/gemini_responses.rs`), requests carry tools of every kind (functions with their schema under each key, custom tools, `apply_patch`, namespaces, web search with allowed domains, built-in tools and tools in `additional_tools` items), every shape of `tool_choice`, and input items of every kind: messages with text and media parts of every type, system and developer messages before and after the conversation starts, tool calls with IDs under each key, missing, repeated or padded, their outputs paired, reordered, orphaned or missing, and reasoning items signed by Gemini, by other providers or in the translator's own signature carriers. Generation settings come loosely typed. The streams are Gemini's: thought, text and function call parts split across chunks, signed or not, with explicit part indexes, repeated or conflicting call snapshots, search grounding with queries, sources and citations, usage under either key, and every way a stream can end. Each comes with a generated request as the client's original, so tool names are mapped back, web search is switched on and request fields are echoed, and the whole stream is also a non-streaming case. They leave out what the port reads differently by design (see the generator's module docs): text that starts like JSON but isn't, a line or body that isn't JSON when `apply_patch` may be declared, integers out of `i64`'s range, file names Go's `filepath` reads by the OS's rules, carriers whose payload isn't UTF-8, and message IDs upstream's trailing signature cache would carry from one case to the next. In the registry cases, a Responses request sent to Gemini has call arguments that start like JSON but aren't replaced with `{}` first, as for Chat Completions; and another suite's stream sent from Gemini to Responses loses the lines that aren't JSON but that gjson reads in part (as `not json`, read as NaN), which upstream starts the response on and the port reads as nothing; when the request may declare `apply_patch`, it loses the other lines that aren't JSON too, and a response that isn't becomes `{}`.

  For the legacy Completions conversions (`src/generate/completions.rs`), requests carry each field upstream copies, or not, with every type gjson coerces: prompts that are text, batches of text or token IDs, objects, numbers or null; integers, floats and flags as numbers of every form, as text or as other types; and stop sequences in every shape. Chat Completions responses and runs of stream chunks hold choices with a message, a delta, both or neither, content and finish reasons of every type (including the string `"null"`), logprobs and indexes, and usage that is there, null, missing or not an object. A few bodies and chunks aren't JSON objects: empty, `[DONE]`, an SSE line or comment, or cut off before a field.

  For the Codex client model list (`go/parity_codex_models.go`, `src/cases/codex_models.rs` and `src/generate/codex_models.rs`), providers register models that have a template in the catalog, that only the static catalog knows, image models the list hides, and models of their own, under aliases and prefixed names, each with or without the fields the list reads: display names that sort or escape differently, descriptions, context sizes, reasoning levels and input modalities, sometimes marked explicit, and the same model from several providers. Client versions fall around the one that gets `max` and `ultra`. Both sides list the models sorted by ID, since upstream's order varies, and are told which models take `apply_patch`; which ones do, as the server decides it, is left to open-ferry's own tests.

  For Codex multi-agent v2 and orphan delegation (`go/parity_multi_agent.go`, `src/cases/multi_agent.rs` and `src/generate/multi_agent.rs`), requests carry collaboration tools at the top level, in namespaces nested up to two deep and in `additional_tools` items, with names and types that are padded, in another case, of another kind or not strings, now and then a name that uses the optimized namespace's; `spawn_agent` descriptions with and without model sections, indented or repeated, with and without the marker the list goes before, or not strings; `message` parameters marked `encrypted` in each way or not; and agent messages whose encrypted parts hold text, other types or nothing, with routing metadata. They come from Codex clients' user agents and others, with the setting on or off, and models registered with and without a catalog entry, display names that sort differently, odd descriptions and reasoning levels; the input cases are sometimes for a compatibility model. Optimizing runs without upstream's note that the Responses handler prepared the tools, as open-ferry keeps none. Sub-agents' requests hold delegation outputs with and without their call, with call IDs that are padded, blank, repeated or not strings; a request with an output that isn't a string is written compactly, since open-ferry quotes it as compact JSON where upstream copies the client's text (see UPSTREAM.md). Upstream events hold tool calls, outputs and namespaces with names to restore and names that only look like them, in arguments, inputs and outputs that are left alone, with escapes, numbers and keys Go's encoder writes its own way; a few are cut off or followed by more. The restored event is compared as text.

  For the payload rules (`go/helps/parity_payload.go`, `src/cases/payload.rs` and `src/generate/payload.rs`), configs hold defaults, overrides and filters whose model names, protocols, headers and conditions do and don't match the call, with paths through a body's objects and arrays (escaped dots, indexes, appends, wildcards, `#` projections and `#(query)` keys, under a root or not) and values of every kind of YAML scalar, sequence and mapping, and raw JSON. The bodies do and don't hold what a default would write, `disable-image-generation` meets image tools and `tool_choice` in each form, and Codex clients' tools go through the integer pass: Codex's tools and others, flat, in namespaces (named, nameless or nested) and in `additional_tools` items, in each tool format, with parameters at the top of the schema and inside objects, array items and union branches, typed as numbers, integers, strings or type lists. Upstream applies a rule's params in Go's map order, so each rule's params start with distinct plain keys, and the cases stay clear of what the payload module documents as a deviation.

  For usage and first-token events (`go/helps/parity_usage.go` and `go/helps/parity_ttft.go`, with `src/cases/usage.rs`, `src/cases/ttft.rs`, `src/generate/usage.rs` and `src/generate/ttft.rs`), answers and stream events of each protocol the parsers read carry counts that add up and counts that don't, buckets missing or under both their names, values written as gjson coerces them (floats, strings, booleans, null, numbers past `i64`), usage that isn't an object, service tiers in each place, the fields the first-token checks read missing, empty, blank, null or of another type, and every framing the parsers read: `data:` with and without a space, bare JSON, `[DONE]`, `event:` lines, blank lines, text after the JSON and lines cut short.

  For session reading (`go/parity_session.go`, `src/cases/session.rs` and `src/generate/session.rs`), requests name sessions, parents and agents in every header session affinity reads, in any case, given twice, blank, padded, around the 256-byte limit, and with Codex's turn metadata as JSON or not; and in every body key it reads, at the top, under `metadata` or `extra_body`, or in a nested `request` with and without `contents`, as text, numbers, booleans, objects, arrays or null, with control characters, and with Claude Code's `metadata.user_id` as JSON, in its legacy form or plain. IDs come from a small pool, so a parent is now and then its own child, and a connection's execution session comes and goes. Bodies are written compactly, as an object or array where an ID goes reads as its compact JSON here and as written by gjson (see UPSTREAM.md). Derived identities come from Chat Completions, Claude, Responses, Gemini (nested or not, with cached content) and Interactions bodies, read under each client format in any case and spacing and for several caller scopes, with instructions, user turns of text, media and other parts, roles in any case, `cache_control` keys, numbers Go reads as float64s and some it can't, and text around the body. Antigravity's format, out of scope, is a known difference.

  For the config change details (`go/parity_config_diff.go`, `src/cases/config_diff.rs` and `src/generate/config_diff.rs`), a config with a random mix of the settings open-ferry types is paired with itself, a fresh one, or itself with a few sections, key entry fields or OAuth channels changed or removed. Values aim at the diff's corners: padded and differently cased names that normalize to the same entry, URLs with user information, paths and odd hosts, values parsing clamps, raw payload JSON and model lists in another order. The hand-written cases also read the repository's own `config.example.yaml`, as it ships and with its examples uncommented, so both loaders are seen to read it alike. The generator leaves out what Go can't agree with itself on: empty lists and mappings, names that are the same once trimmed, and management keys Go hashes with a random salt.

  For the config writer (`go/parity_config_save.go`, `src/cases/config_save.rs` and `src/generate/config_save.rs`), each side writes the case's file into a temporary directory of its own and makes the case's writes to it in turn: upstream's `SaveConfigPreserveComments`, `SaveConfigPreserveCommentsUpdateNestedScalar` and the steps of the management API's `WriteConfig`, and ours. A save's config is the file as it stands, or a changed copy of it, read by each side's own loader. The hand-written files carry comments in every position (head, line and foot comments, on keys, values, list items and the document), every quoting and block scalar style, anchors and aliases, flow and block collections, indents of 2 and 4 and indentless lists, legacy, v8, mixed and historical v8 layouts, upstream's `config.example.yaml` and the repository's own with and without their examples uncommented, and the sections open-ferry doesn't type (`plugins`, `pprof`, `models` and unknown keys), with comments. The writes add, change and remove keys, grow and shrink lists, and move a file to the v8 layout and back. The random cases (`src/generate/config_save.rs`) build a file from a config of the config change details' generator (below) in the legacy or v8 layout, add the sections open-ferry doesn't type (`plugins` with plugin configs, `pprof`, `models`, `antigravity`, `devin`, `claude-header-defaults`, a Claude key's `cloak` and `fingerprint-profile`, and an unknown section), and write it in a random style: indents of 2 or 4, indentless lists, flow collections, every quoting and block scalar style, anchors and aliases, and comments in every position, some at the wrong indent. A quarter are in the writer's own style, so that what it keeps can be checked byte for byte. Each is followed by up to four writes: saves of the file as it stands or of a changed config (values changed, lists grown and shrunk, keys and OAuth channels added and removed), with and without the move to the v8 layout, nested string updates, and whole files. A save's config carries the file's untyped settings, as a management write's does, and on upstream's side its `plugins` are the file's, as upstream's would be: it changes the config it read from the file. Once a save may have left two OAuth channels in the file that are the same once trimmed and in lower case (the writer keeps the file's channel and adds the config's), the file isn't saved as it stands again: which one Go's loader keeps depends on its map order. Likewise, once a save may have left two names in a key's `headers` that are the same once trimmed, the file isn't saved again: which value Go's loader keeps depends on its map order, and a save over a file in the v8 layout rebuilds a provider's groups, without their comments, when the keys it loads from the file differ from the config's. Each file upstream writes is also read with its own `LoadConfig`, from a copy in a directory of its own, since `LoadConfig` may write to the file it reads. When that refuses it, the case ends there. open-ferry checks that what it writes loads before writing it, so it refuses such a write; when its loader refuses the bytes upstream wrote, the case counts as equivalent ("unloadable file refused, not written"), as with a nested string set under a key that holds a number, or an OAuth map emptied under a key with a line comment (Go writes the `{}` on the next line, at column 0). Where upstream writes a comment from a plugin's settings twice in a row, once from the plugin's decoded node and once from the file (a comment ending a plugin's settings that end the file), its second copy is dropped and the case counts as equivalent ("plugin comment written once, not twice"); only comment lines that repeat the ones just before them, each written once in the case's file or the files it writes, inside a `plugins` section. The rule also recognizes one identical comment-out prefix on both copies when migration archives that plugin comment. Two hand-written cases are known differences: upstream writes the `plugins` section from the config it decoded, with `plugins.dir` as `ResolvePluginsDir` cleaned it and the plugin IDs sorted and plain, where open-ferry, with no plugin host, keeps the file's.

  For the quota snapshot (`go/parity_quota_signals.go`, `src/cases/quota_signals.rs` and `src/generate/quota_signals.rs`), each side adds the case's headers in order to a response's, upstream with `http.Header.Add`, and observes them for the case's provider at a fixed time, over a snapshot taken before. Headers are Claude's and Codex's quota headers, Codex's limits named by marker, `X-Ratelimit-*`, `Retry-After` and headers that aren't quota's, in any case and repeated, sometimes more than the 64 a snapshot keeps. Values are numbers, words, empty, padded with ASCII or Unicode white space, around the 512-byte limit, holding a tab, non-ASCII, or bytes that aren't UTF-8 (given as hex). Providers are Claude's and Codex's in any case and padding, and some that aren't observed; Devin, which isn't ported, is left out of the random cases. A header value can't hold CR, LF or DEL here, so a tab stands for the control characters upstream's tests use.

`src/translator.rs` reads each translator's output as JSON. For the signature suites, both sides return a report of every check (`src/signature.rs` builds ours), with errors as their messages. A Claude stream becomes a list of `{"event", "data"}` frames, so frame order, event names and every field are compared. Tool IDs generated for calls that arrive without one (`toolu_<nanoseconds>_<counter>`, or in Claude requests `toolu_` and 24 random letters and digits) are masked on both sides before comparing, in Claude responses translated from Codex or Chat Completions alike. Each side's IDs become `toolu_(generated-1)`, `toolu_(generated-2)` and so on, in the order they first appear, so a reference to another call's ID still shows. An ID found anywhere in the case's input, including JSON held in a string, came from the client and is never masked. A Gemini stream translated to Claude numbers its `tool_use` IDs with a process-wide counter, and one translated to Chat Completions names its calls `<name>-<nanoseconds>-<counter>`; both are masked the same way. The Responses stream translator passes most lines through, so its output is read line by line: `=` for a line returned byte for byte, and otherwise the line's JSON. Chat Completions chunks are read the same way. A Chat Completions response's `created` falls back to the current time, which each side reads from its own clock, so within an hour of now it is masked on both sides. The same goes for every chunk translated from a Claude stream, and for the `created_at` of a Responses response translated from Claude: in each streamed event's `response`, and at the top level of a non-streaming one. A non-streaming Responses response translated from Chat Completions has its `created_at` masked the same way, and if the Chat Completions response had no ID, the one made up for it (`resp_<hex nanoseconds>_<counter>`) becomes `resp_(generated)` wherever the response repeats it. A Chat Completions to Claude stream is read as a list with an entry per chunk: a whole message, or the SSE frames it holds. A Gemini response stream is read as a list of its responses. Translated from Claude, a response's `createTime` is the current time, written in the server's zone by upstream and in UTC by us, so within an hour of now it is masked on both sides. Translated from Chat Completions, a response holding only function calls has them sorted, as upstream keeps them in a Go map whose order changes from run to run. In a Chat Completions request translated from Gemini, a call ID upstream derives from the client's JSON text (`call_` and 24 hex digits) is masked on both sides when that text isn't compact, as ours comes from the compact JSON; each distinct ID becomes `call_(derived-1)` and so on, so a tool message still has to answer the right call. A Responses response translated from Gemini has its `created_at` masked when it is the current time, and the IDs made up from the clock and a counter for a response without one (`resp_<hex nanoseconds>_<counter>`) and for its calls (`call_<nanoseconds>_<counter>`, also as `fc_` and `ctc_` item IDs) masked on both sides: the response's becomes `resp_(generated)` wherever it repeats, and each distinct call ID `call_(generated-1)` and so on, in the order it first appears. IDs found in the case's input are kept. Chat Completions passed through are compared as text. A Responses stream translated from Claude can close earlier tool calls at `content_block_start` and unfinished calls at `message_stop` in upstream Go map order. Each provider event's output is kept separate before comparison. Only complete independent groups (`added? delta* arguments/input.done item.done`, or `added? item.done` for tool search) are sorted by output index, with consecutive sequence slots retained. Function, custom-tool and tool-search payloads, group-internal order and source-event boundaries still compare. Interleaved groups are rejected. Registry single-frame chunks are reordered only within the matching provider event; split or incomplete chunks stay observable. Both `data:{json}` and `data: {json}` follow upstream's prefix-and-TrimSpace rule. A registry suite's output is read as the output of the translator the registry chose, so the masks and accepted deviations above apply to it too. For a stream, both sides also report the chunks each event gave and those given at the end, and whether an unfinished tool call's input failed to parse. Chunks of SSE frames are listed by their frames, so a frame split between chunks or left unended shows; other chunks are compared one by one. A non-streaming translation that failed (nil upstream, `None` here) reads differently from an empty response.

The comparison (`src/compare.rs`) walks both outputs. Every suite reads each number as written, `-0` and exponents included, in its cases' requests, in the outputs and in JSON held in a string, as the proxy hands each translator the client's JSON with its numbers as written and the translators keep a number's text where upstream copies it. Each case ends up in one of four groups:

- **identical**: the same JSON, including key order and number text.
- **equivalent**: the only differences are documented deviations (see UPSTREAM.md):
  - *tool parameter key order*: upstream sorts schema keys, we keep the client's order;
  - *tool parameter number text*: upstream writes a schema's numbers as Go writes a float64, we keep the client's text for the same number;
  - *embedded JSON re-serialized*: a string holds the same JSON, written compactly by us (in responses, a web search query, where Go also escapes `<`, `>` and `&`, the request fields a Responses response repeats as text when the client sent something else, and content, reasoning and call arguments from Chat Completions that aren't strings; in requests from Gemini clients, call arguments and results, and text, tool descriptions and stop sequences that aren't strings; in requests from Responses clients to Gemini, text that isn't a string, such as a tool output that answers no call). It is accepted only at the paths each translator lists in `Translator::embedded_json` (`src/translator.rs`), and only in the form listed there: the whole string, JSON inside unchanged text, or Go's escaping. A path holding `**` covers strings at any depth below it, such as the type hints the Gemini schema cleaner writes into descriptions. Request-side marker replays bind each span to its output path and to the byte positions changed by that replay; an equal structured value cannot witness an unchanged client string elsewhere in the same output. Anywhere else the string must match;
  - *cut at a character boundary*: upstream cut a name or ID in the middle of a character (written as U+FFFD), we cut before it;
  - *protobuf error prefix space*: the harness's protobuf-go build writes a non-breaking space after `proto:`, and we write a regular one;
  - *out-of-range number saturated*: Go converted a float too large for int64 to the minimum int64, as amd64 does, and we saturate, as arm64 does;
  - *made-up user ID left out*: upstream filled a Claude request's `metadata.user_id` for a client that sent none, and we don't. Upstream's ID is removed before comparing;
  - *createTime written in UTC*: a Gemini response's `createTime`, from a Codex response's `created_at`, is the same instant, written by upstream in the server's zone and by us in UTC;
  - *call ID derived from compact JSON*: a Chat Completions request translated from Gemini holds call IDs derived from a request whose text isn't compact, which are masked (see above).
  - *upstream lossy plugin re-encode*: Go replaced a source plugin string with an empty scalar and folded its text into an adjacent source comment. This class is counted separately in the summary, within Equivalent. It requires Rust's complete `plugins.configs` values to equal the source, each involved source comment to occur once, the scalar-to-comment changes to be inside that subtree, and every other output byte to match. A write with identical outputs starts a new source for later saves; differences during that write are never accepted. Changed values, comments, prefixes or bytes outside that shape stay Different; comment-only losses are not automatically accepted.
  - *upstream block scalar counterfactual*: the original Go run fails with a YAML error or writes a file its loader refuses. Only the writer's hazardous block scalar tokens/bodies are quoted, with their exact values, tags, anchors and comments retained and every other source byte unchanged. The rewritten Go run must succeed and all its outputs match under the existing rules; Rust's outputs on the two sources must be byte-identical at every step. This sibling class is counted separately within Equivalent. The original failure, rewritten source and all four output sets are saved under `target/parity/counterfactual/`, uploaded by CI even on success. Each whole-file `write` body is also a new source, quoted by the same rule; separate running configs stay unchanged. All four checks cover every step, including writes before and after saves. A failed proof stays Different. No marker or exclusion is used.
  - *retained config comments (protection ablation)*: six scoped parity-only switches cover the empty-mapping flush, canonical version, dropped key, inline empty-null flush, effective block-scalar quoting and scalar-body indentation. A minimal subset must reproduce every complete Go snapshot, including rejected bytes. Independently rerunning Go without parsed source/config comments must prove all protected values; parsed occurrence budgets and owners prove comments; faithful comment-free emission must match on loadable steps. Unloadable steps permit only source owners and use exact, nonoverlapping emitter-write bytes and offsets for ablated counts. Every protection in a certified subset is counted separately in the summary's deviation table. Unproved cases remain Different; proof and refusal artifacts are saved under `target/parity/counterfactual/`.
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

## Config-save baseline

To check a retained comment's place, the tool parses the exact input for one
write and reruns the pinned Go implementation's edits on a separate copy.
It numbers the original parts of that tree so it can follow a particular
key, value or list item through merges, reordering and v8 migration. Newly
created parts get no original number. The replay never reads Rust's output for
the step being checked. From the second step on, its input is Rust's previous
output, the same input the check uses.

A recorded Go transfer defines the comment's new place only if it lands on
the comment's own node, or on the key that now holds that node. It may also
change whether the comment is above, on or below the key. If Go drops the
comment or attaches it to an unrelated node, that attachment is ignored:
the comment instead keeps its original above/on/below position at its own
node's new place. This fallback covers key, value and list-item comments.
Saved proofs label transfers seen in this replay as `observed` and the other
choice as `fallback`. Deleted data or conflicting recorded destinations cannot
justify a move. Matching text
never finds the destination; it only checks that both parsers meant the same
source line after its original place and position in the comment block
selected the move.

The `retained-source-owner` rule can prove that our writer kept a comment at
its source owner when pinned Go moved or dropped it. A separate inventory must
find each source occurrence once, on the same node, key/value/item side and
head/line/foot slot, following each node Go's replay moved. A separate Go run
checks each comment against the exact input and edit. For each moved or
dropped comment, its place in the replay must have the source node's value and
tag in Go's real output; a key and its value are checked together. Comments Go
kept must stay at Go's real owners. A comment Go moved whose node, or for a
key its value, the save also rewrote stays Different. Keys with equal names
and equal values cannot be told apart. Ambiguous identities and missing,
duplicate or misplaced comments stay Different. All decoded values and tags
must match Go. The output must load and stay unchanged when our writer repeats
the operation. Check 4 compares faithful comment-free re-encodes; it does not
cover raw whitespace or key style. Every ownership rule also checks physical
blank lines and trailing whitespace. Output identical to Go's bytes passes
this layout check directly. Otherwise, regions are bounded by top-level keys
and code lines shared with equal counts by the source, Go and our output. A
blank run needs a source or Go run with the same previous or next position
key. Each key includes its region, code without leading indentation or an
explicit-key `? ` marker, and its in-region occurrence. Each run must be a
subsequence of one input run; its allowance comes from that run alone.
Separate runs may use different inputs. Trailing whitespace needs the same
code line, occurrence and suffix. A layout defect with no shared boundary
fails closed. Comment-only positions are keyed by their trimmed text. A blank
run needs a counterpart beside the same normalized code or trimmed comment,
counted by occurrence. An equal line inserted or removed earlier in the region
can shift that count, and comments with equal text share this limit. Only one
neighbour must match; a changed other neighbour remains allowed. The layout
check leaves out wider space before a comment, double spaces inside a line,
indentation and the final newline. Explicit-key syntax remains allowed because
yaml.v3 needs it to retain a comment on a key. Extra comments need the
existing Go archive or migration proof. The evidence lists the protections the
proof needed.

The extended proof checks alias-expanded source copies and physical comments
on aliases against a separate pinned-yaml.v3 inventory. Restored text must keep
every source copy at its bound owner. Unchanged comments retain Go's owners
and order. Source counts and independently computed archives bound all output
occurrences. The value, tag, faithful-byte, load and repeat checks still apply.
A step that differs from upstream's own history snapshot must also keep at least
`min(upstream's copies, the step source's copies)` of each comment text.
A new foot separator is accepted only when an exact single-line removal makes
pinned yaml.v3 move that source-owned foot. After removing all such separators,
the existing layout guard must pass.

A complete owner certificate can skip subset enumeration. Set the internal
environment variable `OPEN_FERRY_PARITY_FULL_SEARCH=1` to execute that search
too and compare decisions. `OPEN_FERRY_PARITY_DECISIONS` names a JSONL output
file containing case names, decisions, deviations and refusal kinds, without
config values. `OPEN_FERRY_PARITY_WORK_DIR` redirects scratch artifacts for
isolated comparisons. This custom directory must be new, empty, or carry the valid
`.open-ferry-parity-work-dir` marker written by a previous run. The tool-owned
default `target/parity` directory is always accepted and marked, including
restored Go caches and older unmarked output.
Nonempty unmarked custom directories are refused before any files are overwritten
or subfolders cleared; use a new empty custom directory for older unmarked scratch
output. Runs reset the work directory's `case-workers`, `counterfactual`,
`failures` and `live` subfolders as needed. These switches belong to the offline
tool only.
Full mode retains its subset-search diagnostics under `full_search`. Every
step also reruns the unchanged comparison rules on its exact preceding source.
Only steps those rules cannot prove need a new owner certificate and its repeat
check. The decision file includes certificate kinds and refusal reasons.
The optimized search evaluates at most four isolated Rust replays concurrently
and consumes them in the original mask order. Workers inherit the caller's
disabled protections explicitly. Full mode runs the sequential search, bypasses
the exact-input proof cache, and retains marked Go replays even when exact Go
bytes already rule out an owner repair. A pure scalar-loss refusal guard runs
before supplementary owner work. `OPEN_FERRY_PARITY_METRICS` names a JSON timing
file. Worker writer time is aggregate duration; `parallel_replay_wall` is the
corresponding wait time and must not be added to that worker duration.

Every line of a moved comment block must be kept at its final place, in source
order. Keeping part of a moved block stays Different. Complete blocks can
still pass: `random-32-223` keeps all 23 moved source lines, including two-line
blocks.

One correction can put comments written on a key's own physical line
immediately above that
same key after it moves, when Go's output can be loaded. It does not apply to
a key that stays where it was.
The relocation replay runs the edits on the parsed tree without the save's
write-and-reload step, so a comment that step drops still has a destination;
the archive replay includes that step.
If the real Go run stopped before a later write, no Go output can cross-check
the computed place. The tool still requires every source comment there.
When Go did write a loadable file, the existing relocation rule still requires
every comment Go kept at the place Go wrote it. Only the separate
`retained-source-owner` proof above can excuse Go moving or dropping a source
occurrence. The measured output in `random-30-947` still loses or misplaces
source occurrences and remains Different.
A fallback move also stays subject to the strict check when its text matches
a comment Go kept at that destination, or any line in an unloadable step’s
written trace. Equal text cannot prove that both source copies survived.

When a list uses an alias, both sides are compared with the alias filled in,
but comments are still counted where they are actually written. A comment on
an anchor does not let a copy appear on an alias. Filling in aliases does not
excuse moving a comment to another item or hiding a changed list.

Go's v8 migration turns unknown entries into archive comments. A separate
replay computes the exact new lines and where they must appear, using each
original key and its complete entry. It keeps the surrounding comments and
uses temporary markers to follow each generated line through writing and
parsing. Each computed line must appear exactly once at that place, in
addition to any identical comments already in the input. The archived entry
must parse back to the original values; a re-encode that loses data cannot
justify new lines. Altered text, extra copies, another place, an unarchived
key or an unreadable replay stays Different, even if Go wrote matching text.

All other checks still apply: values must agree with the separate run without
comments; disabling the smallest necessary set of existing comment-preserving
writer changes must reproduce Go's complete files; and bytes outside the
proved differences must stay unchanged. Missing or mismatched replay evidence
does not permit a difference. The evidence is saved with each proof.

The CI script passes `--baseline tools/parity/baseline/config-save.json`.
Without that option the tool keeps its strict exit verdict. The baseline
applies only after config-save certification fails: those cases remain
**Different** in the totals and artifacts. Harness failures, panics, unavailable
replays and every other suite remain fatal. A new check/reason/operation kind,
an excessive total rate, or any new Different case in seed 1 / random 25 fails.
No paths, comment text or byte offsets enter kind signatures. A case with
several failed checks contributes to several kind rows but only once to the
total budget. No-reproducing-subset kinds include all failed trials.

The classifier also compares the original Go failure boundary. If Rust
refuses a step that Go completed or got past, an ordinary Rust refusal is fatal,
not a baselined comment-placement kind. A failed wrapper's successful prefix is
`files.len() - 1` for `unloadable` (the last file failed reload), or `files.len()`
for `error`. Go must also have failed, with its successful prefix no longer than
the protected wrapper's; the same definition applies to both sides.

Comment-free Go oracle replies must have a valid wrapper and one snapshot per
successful step, or a consistent error/unloadable boundary; malformed replies
remain fatal as `comment_free_go_malformed`.
An internally consistent reply that stops before original Go is fatal as
`comment_free_go_early`. A successful run's prefix is its full files length;
an unloadable reply's prefix excludes its last, rejected snapshot. Thus an
oracle error or unloadable reply after original Go completed also stops early.
This reason is never a baseline kind. Inspect `original_go` and
`comment_free_go` in the refusal artifact, compare their successful prefixes,
and reproduce the cleaned case with the pinned Go harness. Investigate comment
stripping, loader behavior or harness drift; fix and rerun instead of adding
this refusal to the budget.

The committed budget is the maximum final-certifier observed total rate plus 20%,
rounded up to the next multiple of five per 1,000. It uses ten random-5000
config sweeps (30–39), random-500 config runs (11, 12, 38, 39), and the exact PR
profile. The 170/164 saved scheduled failures for seeds 11/12 add kind coverage
only: selected failures are not unbiased rate samples. The file records that
distinction. An observation below its recorded seed/profile count (or
with fewer PR case IDs) prints “Baseline can be lowered” and the refresh command.

The measured budget has **13 kinds** and a shared bound of **75 Different
per 1,000** config-save cases; the exact PR Different set is empty. The ten
random-5000 calibration seeds have 249 to 307 Different out of 5,109 cases
each. Seed 30 has 267 and seed 38 has 249. The selected historical sets have
168/170 and 161/164 Different; they contribute kinds, not rates.
Every recorded calibration count comes from a measured observation. Keeping
part of a moved block stays Different. Complete retained blocks and faithful
archives can still be proved,
as in `random-32-223`. The 20% margin above the maximum observed rate, rounded
up to five, gives the bound.
These counts describe the calibration sample; the separate kind check still
rejects an unseen failure kind. Earlier false-alarm and new-kind estimates
came from an older calibration and were not recomputed, so they are omitted.

### When the nightly fails

1. Download the run's `config-save-observation.json` artifact and saved cases.
2. Inspect each new kind's cases to distinguish a product regression from a
   newly seen existing limitation. Fix regressions; don't baseline them away.
3. Only for a reviewed existing limitation, refresh incrementally and open a PR
   explaining the added kinds or higher bound:

   ```sh
   cargo run --locked -p open-ferry-parity -- \
     --write-baseline tools/parity/baseline/config-save.json \
     --baseline tools/parity/baseline/config-save.json \
     --baseline-observation config-save-observation.json
   ```

Repeat `--baseline-observation` for additional runs. Incremental refresh unions
kinds and deduplicates calibration rows. It keeps the higher of the existing
bound and the new non-selected rows' bound, using the same margin and rounding.
It never lowers the bound and does not require ten new seeds. Selected replays
add kind coverage only. Fingerprints must match and non-certification failures
are refused. The existing exact PR case set is preserved; a supplied seed-1/25
observation must have exactly that set. Use a full refresh to lower the budget.

### Full refresh

Collect new observations with the unchanged verdict logic. This Bash example
collects ten full config profiles and the exact PR profile:

```sh
set -e
export CARGO_BUILD_JOBS=4 GOMAXPROCS=4 GOTOOLCHAIN=local
mkdir -p target/parity-calibration
for seed in {30..39}; do
  cargo run --locked --release -p open-ferry-parity -- \
    --upstream reference/cliproxyapi --suite config-save --seed "$seed" --random 5000 \
    --observation "target/parity-calibration/seed${seed}.json" || test "$?" -eq 1
done
cargo run --locked --release -p open-ferry-parity -- \
  --upstream reference/cliproxyapi --suite config-save --seed 1 --random 25 \
  --observation target/parity-calibration/pr.json || test "$?" -eq 1
# Also re-execute the full profiles and saved scheduled inputs recorded in the
# baseline, writing their observations into this same directory. For example:
# --replay-config-failures ARTIFACT/config-save --seed 11 --random 5000
# --observation target/parity-calibration/scheduled11.json
observations=()
for file in target/parity-calibration/*.json; do
  observations+=(--baseline-observation "$file")
done
cargo run --locked -p open-ferry-parity -- \
  --write-baseline tools/parity/baseline/config-save.json "${observations[@]}"
```

A strict collection run can exit 1 while writing a usable observation; refresh
still rejects observations containing any non-certification failure. Saved
inputs are **re-executed**, not certified from their old proof files. An unseen
seed below its profile's calibration minimum, or a known seed below its own
recorded count, prints the lowering message.

The writer sorts keys, kinds and calibration rows; input order does not affect
the bytes. It rejects non-certification failures and mismatched fingerprints.
The fingerprint contains only `classifier_version`, `upstream_pin` and
`generator_sha256` (normalized paths listed in `GENERATOR_PATHS` in
`src/baseline.rs`). The generator hash covers whole files in the source closure
of random config-save generation, including shared case construction. Unrelated
code in those files also changes the hash. It invalidates rate calibration when
the random distribution changes, with the probe-verified relabel exception below.
The closure test reviews imports, modules, includes, macro paths, external crate
paths and file/process/environment access. It also scans the crate for unhashed
implementations of hashed types. Existing shared `Generator` implementations in
other suites have private methods, apart from four reviewed signature helpers.
Random config generation uses only `Generator::new` and its RNG; a separate check
keeps those signature helpers out of the config generators, RNG and shared
constructors. A new visible method or trait implementation needs closure review.
Local types with the same name are separate types. This is a syntax check, not a Rust call graph; it
cannot fully resolve trait methods through generic bounds, deref coercions, or
arbitrary macro expansion. The source hash and committed case probes catch the
first edit that wires such a route into the random generator. Review that edit's
transitive inputs before permitting a relabel or refresh.
Hand-written
config-save case inputs are not fingerprinted: every PR and nightly profile runs
all of them, and the exact PR case set rejects any that becomes Different (the
committed `pr_different_cases` set is currently empty). Product, certifier,
harness, manifest and toolchain changes are judged by the verdict. In particular,
serde_json's `preserve_order` feature and locked version affect generated map
order but are excluded from this hash by policy: dependency changes go through
review and the lock file. A generator-only relabel must pass the probes below;
changed probe inputs or an upstream pin change require a full calibration refresh.
**Adding a kind or raising a bound must
be explained in the PR**; follow-up fixes lower the baseline rather than weaken
the certifier. The refresh command never silently updates a running CI budget.
The kind classifier has an explicit `classifier_version` in the fingerprint;
bump it when changing how diagnostic kinds are assigned.

### Fingerprint-only relabel

A full refresh records `random_case_probes`, a map from `seed-1-random-25` and
`seed-38-random-5000` to SHA-256 digests of their canonical random config-save
exports. Each compact JSON export includes IDs and every input field: model,
request, translated request, events, options and the known-difference marker.
Records stay in generator order. JSON framing has no platform line endings;
embedded input bytes, including deliberate CR/LF characters, are preserved.
The exporter is the same code used by refresh and relabel:

```sh
cargo run --locked -p open-ferry-parity -- \
  --export-config-cases pr-cases.json --seed 1 --random 25
cargo run --locked -p open-ferry-parity -- \
  --export-config-cases nightly-cases.json --seed 38 --random 5000
```

If an unrelated edit to a hashed file changes only the generator fingerprint,
run this offline command (no Go execution or upstream checkout is required):

```sh
cargo run --locked -p open-ferry-parity -- \
  --relabel-baseline tools/parity/baseline/config-save.json
```

It recomputes both exports and permits relabeling only if their bytes' digests
match the recorded probes. It changes only `generator_sha256`, preserving the
probes, kinds, bound, calibration rows, PR set, pin and classifier version. A
changed probe is named in the error and requires full refresh with matching
observations. The command refuses classifier-version or upstream-pin changes.
It also compares the generator sources embedded at build time with the current
on-disk sources, using the same LF-normalized hash. A stale binary refuses without
writing; rebuild with the `cargo run` command above. Relabel accepts no run flags:
its two probe profiles are fixed. A real-tree test independently compares the
committed probe digests with the compiled generator and requires a full refresh
if its case lists changed.
Legacy baselines without `random_case_probes` remain readable and valid for
ordinary assessment when their fingerprints match, but cannot be relabeled; a
full refresh must first record the probes. Incremental refresh preserves existing probes, including their
absence in a legacy baseline.

### Classifier-version reclassification

Version 4 records the new comment checks. Once an earlier write is proved, a
later write's failure becomes the one reported, so a case's diagnostic kind
can change. Reclassifying the previous saved proofs must still preserve every
case's kind set, count and fatal failures before collecting fresh results.

A classifier-version change requires reclassifying every saved observation with
the new classifier through `--import-config-failures` and its bound proof files,
as shown below. Write new observation copies; do not edit old fingerprints.
Compare every per-case kind set, the per-case and total counts, and
non-certification failures with the prior observations. Stop for review if any
of these change. Only then perform a full refresh under the new classifier,
keeping held-out observations outside the calibration set. The fingerprint-only
relabel command refuses this transition even when random probes match.

Historical fixtures can be imported with the committed classifier:

```sh
cargo run --locked -p open-ferry-parity -- --import-config-failures DIR/config-save \
  --import-proofs PROOFS --import-cases 5109 --seed 30 --random 5000 \
  --observation target/seed30.json
# Add --selected-failures for selected historical failures, using their replay
# count as --import-cases. Their rates are never used or accepted as full sweeps.
cargo run --locked -p open-ferry-parity -- \
  --baseline tools/parity/baseline/config-save.json \
  --baseline-observation target/seed30.json # repeat to check the entire corpus
```

Import attaches the current fingerprint: use it only after verifying that the
saved outputs came from the same product and generated inputs. When a classifier
changes, reclassification must follow the comparison and full-refresh procedure
above; matching probe bytes authorize only a generator-hash relabel, never a
classifier change. Saved proofs carrying both `original_go` and `comment_free_go`
receive the same early-oracle prefix check as live certification. Legacy proofs
missing either wrapper retain their previous classification behavior; import
does not reconstruct their missing oracle evidence. GitHub summaries
show each kind's count/rate and the total bound; the always-uploaded observation
can be reused in a reviewed refresh.

## Generated compatibility evidence

[`docs/compatibility.md`](../../docs/compatibility.md) is generated with
`python .github/scripts/compatibility.py`; `--check` fails on drift. The
small map in `.github/scripts/compatibility-evidence.json` identifies exact
Rust fixture tests, source paths, client formats, credentials and the
capabilities their assertions exercise. Generation and CI compile the real
libtest harnesses and run every mapped test by its exact name, requiring one
pass and zero ignored tests. Source declarations and refusal snippets must
also exist. `python -I .github/scripts/test_compatibility.py` checks these
conditions and byte-for-byte document freshness in the existing CI tests job.

Fixture revisions are LF-normalized source SHA-256 digests, with a stable
catalog review date; neither git history nor generation time affects output.
No live result or client application version is inferred. Translator-only
parity suites do not exercise credentials or the public server transport,
so they are not promoted to those matrix cells. The matrix instead maps the
existing provider and server fixtures, and leaves other combinations unknown.
The upstream version, commit and date come from `UPSTREAM.md`; generation
needs neither Go nor the upstream submodule.
