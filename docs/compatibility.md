# Compatibility evidence

<!-- Written by .github/scripts/compatibility.py; don't edit by hand. -->

Open Ferry is a credited Rust port of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI).

## Legend

- **F [E…] — fixture-tested:** the exact linked Rust test compiles and passes offline in the required CI tests job; its scope says which assertions and boundary it exercises.
- **P — parity-tested:** a maintained translation suite is exercised by the separate offline parity CI job. A passing comparison allows only identical/equivalent outputs or recorded known differences; see the translation section for timing and limits.
- **live-tested:** requires a recorded real provider/client result. **There are no recorded live results or live-tested cells.**
- **U [U…] — unsupported:** the linked code explicitly refuses the capability.
- **? — unknown:** no evidence is mapped for this meaningful combination. Implementation or coverage of another credential is not proof.

## What is fixture-tested

Each fraction is tested capabilities / meaningful capabilities for that protocol and provider/credential row. These are fixture assertions, not tested application versions or general model support.

| Client protocol / transport | Provider / credential: capabilities tested |
|---|---|
| OpenAI Chat Completions / HTTP | Codex / API key: **1/9**; Claude / API key: **2/9**; xAI / API key: **2/9**; OpenAI-compatible upstream / API key: **4/9** |
| OpenAI Responses / HTTP | Codex / API key: **3/9**; Vertex AI / API key: **2/9**; Meta / API key: **3/9**; xAI / API key: **3/9**; OpenAI-compatible upstream / API key: **1/9**; Scripted route dispatcher / Provider credential not exercised: **1/9** |
| OpenAI Responses / WebSocket | Scripted route dispatcher / Provider credential not exercised: **3/10** |
| Claude Messages / HTTP | Claude / API key: **3/6**; Claude / OAuth sign-in; see scope limits: **2/6**; Meta / API key: **1/6**; xAI / API key: **1/6**; OpenAI-compatible upstream / API key: **3/6** |
| Gemini generateContent / HTTP | Gemini / API key: **3/9**; Vertex AI / API key: **3/9**; Vertex AI / Service account: **4/9** |
| Gemini Interactions / HTTP | Gemini via Interactions executor / API key: **3/9**; Vertex AI / API key: **1/9** |
| OpenAI Images / HTTP | Codex / API key: **3/3**; xAI / API key: **2/3**; OpenAI-compatible upstream / API key: **3/3**; Scripted route dispatcher / Provider credential not exercised: **3/3** |
| OpenAI Speech / HTTP | xAI / API key: **1/1**; Scripted route dispatcher / Provider credential not exercised: **1/1** |
| xAI TTS / HTTP | Scripted route dispatcher / Provider credential not exercised: **1/1** |
| OpenAI Videos / HTTP | xAI / API key: **1/2** |
| xAI Videos / HTTP | xAI / API key: **1/2** |
| Model discovery / HTTP | Scripted route dispatcher / Provider credential not exercised: **1/1** |

Protocols with no provider/credential fixture or refusal evidence yet: Legacy Completions / HTTP.

## Scope and client application versions

Client application versions are **not tested yet**. Protocol fixtures, synthetic client-version headers, and documented minimum versions do not establish a Codex CLI, Claude Code, or other real application recipe.
Future recorded recipes belong here with application/version, provider/model, credential type, protocol/transport, capabilities, artifact, and test date.

Executor fixtures exercise the named client format and a fake upstream. Public-route fixtures say so in their scope.
The **Scripted route dispatcher** row exercises a public route with fake dispatch; it does not exercise a provider account or provider credential. It is not promoted to any provider/credential row.
Text includes the request/response behavior described by the evidence; tools and reasoning mean only the named tool/field/event assertions. Audio and video scopes state direction. Token counts may be local estimates.

**Claude OAuth sign-in limits:** this route goes against Anthropic's terms; in testing only Haiku models worked. Accounts may be limited or suspended.
The synthetic OAuth fixture does not establish real Sonnet/Opus access. See [Claude subscriptions](claude-subscription.md#the-claude-sign-in). `claude-cli` runs the user's installed Claude Code and stores no Claude Code token.

## Counts and meaningful combinations

For each protocol, the generator defines the meaningful capability columns before considering evidence. Counts include every listed provider/credential row (including scripted dispatch) against that set, even when the row is hidden below.
A cell with several passing fixtures counts once. Dimensions outside that endpoint's set are omitted, not marked unsupported. For example, speech counts audio, and model discovery counts model list.

| Provider/credential evidence level | Meaningful cells |
|---|---:|
| fixture-tested | 66 |
| live-tested | 0 |
| unsupported | 2 |
| unknown | 972 |

Total: **1040** meaningful provider/credential capability cells. Translation evidence below is counted separately.

## Translation parity with CLIProxyAPI v8.0.38

Pinned reference: **v8.0.38**, commit `0db19025571684b198a2ef508f99b41c717337e8` (2026-10-21); [pin](../UPSTREAM.md#pinned-reference). Catalog reviewed **2026-10-28**.

**parity-tested** describes maintained suite coverage, not a newly recorded Go comparison for this checkout. A passing translator run compares the same inputs against the pinned Go implementation and permits only identical outputs, equivalence under recorded rules, or cases with recorded known differences; an unaccepted Different result fails the job. Build/setup errors also fail it.
The separate config-save suite has its own recorded certification-failure baseline; it is outside this translation table.

[PR parity CI](../.github/workflows/ci.yml) runs seed 1 with 25 random cases per generator only when translators, providers, core, the pin, parity tooling, manifests, or similar listed paths change; a docs-only PR does not trigger that comparison.
[Scheduled parity](../.github/workflows/parity-scheduled.yml) runs nightly at 06:23 UTC, and manually, with 2,000 random cases per generator by default. The existing tests job checks metadata on PRs, merge groups and manual runs when its Rust job runs; pushes to main only warm test caches. Metadata export does not execute Go.

This is translation evidence only: it proves no provider account, credential type, or public route, and fills none of the provider/credential cells.
Format names are registry identifiers: openai/openai-chat mean Chat Completions, openai-responses means Responses, claude means Messages, gemini means generateContent, interactions means Gemini Interactions, and codex is the Codex backend's Responses format.
Counts in links are maintained **hand-written cases**, including signature-relay cases, with seed 1 and zero random cases; CI's generated random cases are additional. Request compatibility variants are listed separately.

| Client protocol | claude | codex | gemini | interactions | openai | openai-responses |
|---|---|---|---|---|---|---|
| claude | ? (no direct suite) | P Request: [claude-request (25)](#p-claude-request)<br>P Request (compat): [claude-request-compat (25)](#p-claude-request-compat)<br>P Streamed response: [claude-stream (13)](#p-claude-stream)<br>P Whole response: [claude-non-stream (10)](#p-claude-non-stream) | P Request: [claude-to-gemini-request (45)](#p-claude-to-gemini-request)<br>P Request (compat): [claude-to-gemini-request-compat (45)](#p-claude-to-gemini-request-compat)<br>P Streamed response: [gemini-to-claude-stream (35)](#p-gemini-to-claude-stream)<br>P Whole response: [gemini-to-claude-non-stream (29)](#p-gemini-to-claude-non-stream) | P Request: [claude-to-interactions-request (35)](#p-claude-to-interactions-request)<br>P Request (compat): [claude-to-interactions-request-compat (35)](#p-claude-to-interactions-request-compat)<br>P Streamed response: [interactions-to-claude-stream (16)](#p-interactions-to-claude-stream)<br>P Whole response: [interactions-to-claude-non-stream (32)](#p-interactions-to-claude-non-stream) | P Request: [claude-to-chat-request (64)](#p-claude-to-chat-request)<br>P Request (compat): [claude-to-chat-request-compat (64)](#p-claude-to-chat-request-compat)<br>P Streamed response: [chat-to-claude-stream (72)](#p-chat-to-claude-stream)<br>P Whole response: [chat-to-claude-non-stream (33)](#p-chat-to-claude-non-stream) | ? (no direct suite) |
| completions | ? (no direct suite) | ? (no direct suite) | ? (no direct suite) | ? (no direct suite) | P Request: [completions-request (40)](#p-completions-request)<br>P Whole response: [completions-response (35)](#p-completions-response)<br>P Streamed response: [completions-stream-chunk (19)](#p-completions-stream-chunk) | ? (no direct suite) |
| gemini | P Request: [gemini-to-claude-request (42)](#p-gemini-to-claude-request)<br>P Streamed response: [claude-to-gemini-stream (9)](#p-claude-to-gemini-stream)<br>P Whole response: [claude-to-gemini-non-stream (19)](#p-claude-to-gemini-non-stream) | P Request: [gemini-to-codex-request (45)](#p-gemini-to-codex-request)<br>P Streamed response: [codex-to-gemini-stream (14)](#p-codex-to-gemini-stream)<br>P Whole response: [codex-to-gemini-non-stream (13)](#p-codex-to-gemini-non-stream) | P Request: [gemini-to-gemini-request (26)](#p-gemini-to-gemini-request)<br>P Streamed response: [gemini-to-gemini-stream (23)](#p-gemini-to-gemini-stream)<br>P Whole response: [gemini-to-gemini-non-stream (28)](#p-gemini-to-gemini-non-stream) | P Request: [gemini-to-interactions-request (84)](#p-gemini-to-interactions-request)<br>P Streamed response: [interactions-to-gemini-stream (11)](#p-interactions-to-gemini-stream)<br>P Whole response: [interactions-to-gemini-non-stream (16)](#p-interactions-to-gemini-non-stream) | P Request: [gemini-to-chat-request (45)](#p-gemini-to-chat-request)<br>P Streamed response: [chat-to-gemini-stream (52)](#p-chat-to-gemini-stream)<br>P Whole response: [chat-to-gemini-non-stream (32)](#p-chat-to-gemini-non-stream) | ? (no direct suite) |
| interactions | P Request: [interactions-to-claude-request (45)](#p-interactions-to-claude-request)<br>P Streamed response: [claude-to-interactions-stream (10)](#p-claude-to-interactions-stream)<br>P Whole response: [claude-to-interactions-non-stream (15)](#p-claude-to-interactions-non-stream) | P Request: [interactions-to-codex-request (45)](#p-interactions-to-codex-request)<br>P Streamed response: [codex-to-interactions-stream (22)](#p-codex-to-interactions-stream)<br>P Whole response: [codex-to-interactions-non-stream (15)](#p-codex-to-interactions-non-stream) | P Request: [interactions-to-gemini-request (144)](#p-interactions-to-gemini-request)<br>P Streamed response: [gemini-to-interactions-stream (20)](#p-gemini-to-interactions-stream)<br>P Whole response: [gemini-to-interactions-non-stream (19)](#p-gemini-to-interactions-non-stream) | P Request: [interactions-passthrough-request (6)](#p-interactions-passthrough-request)<br>P Streamed response: [interactions-passthrough-stream (2)](#p-interactions-passthrough-stream)<br>P Whole response: [interactions-passthrough-non-stream (5)](#p-interactions-passthrough-non-stream) | P Request: [interactions-to-chat-request (9)](#p-interactions-to-chat-request)<br>P Streamed response: [chat-to-interactions-stream (7)](#p-chat-to-interactions-stream)<br>P Whole response: [chat-to-interactions-non-stream (6)](#p-chat-to-interactions-non-stream) | P Request: [interactions-to-responses-request (11)](#p-interactions-to-responses-request)<br>P Streamed response: [responses-to-interactions-stream (5)](#p-responses-to-interactions-stream)<br>P Whole response: [responses-to-interactions-non-stream (4)](#p-responses-to-interactions-non-stream) |
| openai-chat | P Request: [chat-to-claude-request (77)](#p-chat-to-claude-request)<br>P Request (compat): [chat-to-claude-request-compat (77)](#p-chat-to-claude-request-compat)<br>P Streamed response: [claude-to-chat-stream (9)](#p-claude-to-chat-stream)<br>P Whole response: [claude-to-chat-non-stream (19)](#p-claude-to-chat-non-stream) | P Request: [chat-request (46)](#p-chat-request)<br>P Streamed response: [chat-stream (24)](#p-chat-stream)<br>P Whole response: [chat-non-stream (12)](#p-chat-non-stream) | P Request: [chat-to-gemini-request (67)](#p-chat-to-gemini-request)<br>P Streamed response: [gemini-to-chat-stream (30)](#p-gemini-to-chat-stream)<br>P Whole response: [gemini-to-chat-non-stream (27)](#p-gemini-to-chat-non-stream) | P Request: [chat-to-interactions-request (12)](#p-chat-to-interactions-request)<br>P Streamed response: [interactions-to-chat-stream (12)](#p-interactions-to-chat-stream)<br>P Whole response: [interactions-to-chat-non-stream (9)](#p-interactions-to-chat-non-stream) | P Request: [chat-to-chat-request (64)](#p-chat-to-chat-request)<br>P Streamed response: [chat-to-chat-stream (51)](#p-chat-to-chat-stream)<br>P Whole response: [chat-to-chat-non-stream (31)](#p-chat-to-chat-non-stream) | ? (no direct suite) |
| openai-responses | P Request: [responses-to-claude-request (116)](#p-responses-to-claude-request)<br>P Request (compat): [responses-to-claude-request-compat (116)](#p-responses-to-claude-request-compat)<br>P Streamed response: [claude-to-responses-stream (18)](#p-claude-to-responses-stream)<br>P Whole response: [claude-to-responses-non-stream (27)](#p-claude-to-responses-non-stream) | P Request: [responses-request (42)](#p-responses-request)<br>P Streamed response: [responses-stream (13)](#p-responses-stream)<br>P Whole response: [responses-non-stream (13)](#p-responses-non-stream) | P Request: [responses-to-gemini-request (37)](#p-responses-to-gemini-request)<br>P Streamed response: [gemini-to-responses-stream (49)](#p-gemini-to-responses-stream)<br>P Whole response: [gemini-to-responses-non-stream (25)](#p-gemini-to-responses-non-stream) | P Request: [responses-to-interactions-request (26)](#p-responses-to-interactions-request)<br>P Streamed response: [interactions-to-responses-stream (13)](#p-interactions-to-responses-stream)<br>P Whole response: [interactions-to-responses-non-stream (10)](#p-interactions-to-responses-non-stream) | P Request: [responses-to-chat-request (183)](#p-responses-to-chat-request)<br>P Streamed response: [chat-to-responses-stream (78)](#p-chat-to-responses-stream)<br>P Whole response: [chat-to-responses-non-stream (40)](#p-chat-to-responses-non-stream) | ? (no direct suite) |

Protocol helpers from the same registry:

| Client operation | Upstream format | Evidence |
|---|---|---|
| Codex model discovery | registry models | P model-list: [codex-models (21)](#p-codex-models) |
| reasoning settings | codex | P reasoning: [thinking-codex (47)](#p-thinking-codex) |
| reasoning settings | openai | P reasoning: [thinking-openai (33)](#p-thinking-openai) |

Parity count: **87 parity-tested suites**, covering **26** nonempty client/upstream translation cells and **3** protocol-helper entries. The other **10** direct-translation cells are unknown; they do not imply an indirect chain is unsupported.

## Provider and route fixture details

Only rows with fixture or refusal evidence are shown. Each table uses its meaningful capability set; absent provider/credential rows stay unknown.

### OpenAI Chat Completions / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Token counting |
|---|---|---|---|---|---|---|---|---|---|---|
| Codex | API key | F [E01](#e01) | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | F [E25](#e25) | F [E26](#e26) | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | F [E13](#e13) | F [E13](#e13) | ? | ? | ? | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | F [E18](#e18) | ? | ? | F [E18](#e18) | ? | ? | ? | F [E21](#e21) | F [E29](#e29) |

No evidence yet: Codex / OAuth; Codex / SIWC nested OAuth; Claude / OAuth sign-in; see scope limits; Claude via claude-cli / Installed Claude Code session; Gemini / API key; Gemini via Interactions executor / API key; Vertex AI / API key; Vertex AI / Service account; Vertex AI / Access token; Meta / API key; Meta / Access token; Scripted route dispatcher / Provider credential not exercised.

### OpenAI Responses / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Token counting |
|---|---|---|---|---|---|---|---|---|---|---|
| Codex | API key | F [E02](#e02) | F [E02](#e02) | ? | F [E24](#e24) | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | F [E33](#e33) | F [E33](#e33) | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | F [E12](#e12) | F [E12](#e12) | ? | ? | ? | ? | ? | F [E32](#e32) |
| xAI | API key | ? | F [E14](#e14) | ? | F [E14](#e14) | ? | ? | ? | ? | F [E31](#e31) |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | F [E21](#e21) | ? |
| Scripted route dispatcher | Provider credential not exercised | ? | ? | F [E49](#e49) | ? | ? | ? | ? | ? | ? |

No evidence yet: Codex / OAuth; Codex / SIWC nested OAuth; Claude / API key; Claude / OAuth sign-in; see scope limits; Claude via claude-cli / Installed Claude Code session; Gemini / API key; Gemini via Interactions executor / API key; Vertex AI / Service account; Vertex AI / Access token; Meta / Access token.

### OpenAI Responses / WebSocket

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Scripted route dispatcher | Provider credential not exercised | ? | F [E51](#e51) | F [E50](#e50) | ? | ? | ? | ? | ? | ? | F [E50](#e50), [E51](#e51) |

No evidence yet: Codex / OAuth; Codex / API key; Codex / SIWC nested OAuth; Claude / API key; Claude / OAuth sign-in; see scope limits; Claude via claude-cli / Installed Claude Code session; Gemini / API key; Gemini via Interactions executor / API key; Vertex AI / API key; Vertex AI / Service account; Vertex AI / Access token; Meta / API key; Meta / Access token; xAI / API key; OpenAI-compatible upstream / API key.

### Claude Messages / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Token counting |
|---|---|---|---|---|---|---|---|
| Claude | API key | F [E03](#e03) | F [E04](#e04) | ? | F [E27](#e27) | ? | ? |
| Claude | OAuth sign-in; see scope limits | F [E05](#e05) | ? | ? | ? | ? | F [E28](#e28) |
| Claude via claude-cli | Installed Claude Code session | ? | ? | U [U01](#u01) | ? | ? | U [U02](#u02) |
| Meta | API key | ? | ? | ? | ? | ? | F [E32](#e32) |
| xAI | API key | ? | ? | ? | ? | ? | F [E31](#e31) |
| OpenAI-compatible upstream | API key | F [E19](#e19) | F [E20](#e20) | ? | ? | ? | F [E30](#e30) |

No evidence yet: Codex / OAuth; Codex / API key; Codex / SIWC nested OAuth; Gemini / API key; Gemini via Interactions executor / API key; Vertex AI / API key; Vertex AI / Service account; Vertex AI / Access token; Meta / Access token; Scripted route dispatcher / Provider credential not exercised.

### Gemini generateContent / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Token counting |
|---|---|---|---|---|---|---|---|---|---|---|
| Gemini | API key | F [E06](#e06) | F [E06](#e06) | ? | ? | ? | ? | ? | ? | F [E07](#e07) |
| Vertex AI | API key | F [E09](#e09) | F [E09](#e09) | ? | ? | ? | F [E35](#e35) | ? | ? | ? |
| Vertex AI | Service account | F [E08](#e08) | F [E08](#e08) | ? | ? | ? | F [E34](#e34) | ? | ? | F [E08](#e08) |

No evidence yet: Codex / OAuth; Codex / API key; Codex / SIWC nested OAuth; Claude / API key; Claude / OAuth sign-in; see scope limits; Claude via claude-cli / Installed Claude Code session; Gemini via Interactions executor / API key; Vertex AI / Access token; Meta / API key; Meta / Access token; xAI / API key; OpenAI-compatible upstream / API key; Scripted route dispatcher / Provider credential not exercised.

### Gemini Interactions / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Token counting |
|---|---|---|---|---|---|---|---|---|---|---|
| Gemini via Interactions executor | API key | F [E43](#e43) | F [E10](#e10), [E44](#e44) | ? | F [E11](#e11) | ? | ? | ? | ? | ? |
| Vertex AI | API key | F [E42](#e42) | ? | ? | ? | ? | ? | ? | ? | ? |

No evidence yet: Codex / OAuth; Codex / API key; Codex / SIWC nested OAuth; Claude / API key; Claude / OAuth sign-in; see scope limits; Claude via claude-cli / Installed Claude Code session; Gemini / API key; Vertex AI / Service account; Vertex AI / Access token; Meta / API key; Meta / Access token; xAI / API key; OpenAI-compatible upstream / API key; Scripted route dispatcher / Provider credential not exercised.

### OpenAI Images / HTTP

| Provider | Credential | Streaming | Images in | Images out |
|---|---|---|---|---|
| Codex | API key | F [E38](#e38) | F [E37](#e37) | F [E36](#e36) |
| xAI | API key | ? | F [E16](#e16) | F [E15](#e15) |
| OpenAI-compatible upstream | API key | F [E41](#e41) | F [E40](#e40) | F [E39](#e39) |
| Scripted route dispatcher | Provider credential not exercised | F [E46](#e46) | F [E47](#e47) | F [E45](#e45) |

No evidence yet: Codex / OAuth; Codex / SIWC nested OAuth; Claude / API key; Claude / OAuth sign-in; see scope limits; Claude via claude-cli / Installed Claude Code session; Gemini / API key; Gemini via Interactions executor / API key; Vertex AI / API key; Vertex AI / Service account; Vertex AI / Access token; Meta / API key; Meta / Access token.

### OpenAI Speech / HTTP

| Provider | Credential | Audio |
|---|---|---|
| xAI | API key | F [E17](#e17) |
| Scripted route dispatcher | Provider credential not exercised | F [E48](#e48) |

No evidence yet: Codex / OAuth; Codex / API key; Codex / SIWC nested OAuth; Claude / API key; Claude / OAuth sign-in; see scope limits; Claude via claude-cli / Installed Claude Code session; Gemini / API key; Gemini via Interactions executor / API key; Vertex AI / API key; Vertex AI / Service account; Vertex AI / Access token; Meta / API key; Meta / Access token; OpenAI-compatible upstream / API key.

### xAI TTS / HTTP

| Provider | Credential | Audio |
|---|---|---|
| Scripted route dispatcher | Provider credential not exercised | F [E48](#e48) |

No evidence yet: Codex / OAuth; Codex / API key; Codex / SIWC nested OAuth; Claude / API key; Claude / OAuth sign-in; see scope limits; Claude via claude-cli / Installed Claude Code session; Gemini / API key; Gemini via Interactions executor / API key; Vertex AI / API key; Vertex AI / Service account; Vertex AI / Access token; Meta / API key; Meta / Access token; xAI / API key; OpenAI-compatible upstream / API key.

### OpenAI Videos / HTTP

| Provider | Credential | Images in | Video |
|---|---|---|---|
| xAI | API key | ? | F [E22](#e22), [E53](#e53) |

No evidence yet: Codex / OAuth; Codex / API key; Codex / SIWC nested OAuth; Claude / API key; Claude / OAuth sign-in; see scope limits; Claude via claude-cli / Installed Claude Code session; Gemini / API key; Gemini via Interactions executor / API key; Vertex AI / API key; Vertex AI / Service account; Vertex AI / Access token; Meta / API key; Meta / Access token; OpenAI-compatible upstream / API key; Scripted route dispatcher / Provider credential not exercised.

### xAI Videos / HTTP

| Provider | Credential | Images in | Video |
|---|---|---|---|
| xAI | API key | ? | F [E23](#e23) |

No evidence yet: Codex / OAuth; Codex / API key; Codex / SIWC nested OAuth; Claude / API key; Claude / OAuth sign-in; see scope limits; Claude via claude-cli / Installed Claude Code session; Gemini / API key; Gemini via Interactions executor / API key; Vertex AI / API key; Vertex AI / Service account; Vertex AI / Access token; Meta / API key; Meta / Access token; OpenAI-compatible upstream / API key; Scripted route dispatcher / Provider credential not exercised.

### Model discovery / HTTP

| Provider | Credential | Model list |
|---|---|---|
| Scripted route dispatcher | Provider credential not exercised | F [E52](#e52) |

No evidence yet: Codex / OAuth; Codex / API key; Codex / SIWC nested OAuth; Claude / API key; Claude / OAuth sign-in; see scope limits; Claude via claude-cli / Installed Claude Code session; Gemini / API key; Gemini via Interactions executor / API key; Vertex AI / API key; Vertex AI / Service account; Vertex AI / Access token; Meta / API key; Meta / Access token; xAI / API key; OpenAI-compatible upstream / API key.

## Regeneration and checks

Run from the repository root with Rust and Python 3.11+; no Go, credentials, live proxy, upstream checkout, or git history is needed:

```sh
python .github/scripts/compatibility.py
python .github/scripts/compatibility.py --check
python -I .github/scripts/test_compatibility.py
cargo run --locked -p open-ferry-parity -- --list-suites
```

The [fixture mapping](../.github/scripts/compatibility-evidence.json) names exact tests; generation/checking verify their source declarations and compiled test names, and run each with one pass and zero ignored tests. Required CI checks committed document bytes in the existing tests job.
The parity JSON export derives formats and directions from each registered translator's key and counts the registry's actual hand-written fixtures. It skips non-translation/configuration fixtures without opening the upstream tree.
Evidence identity is file path, fully qualified test name (or suite slug/label), pinned upstream version/commit, and catalog review date. Unrelated source edits and line shifts do not change the document. No file digest, line anchor, workspace version, or generation timestamp is used.

## Evidence catalog

All entries below identify the reference **v8.0.38**, commit `0db19025571684b198a2ef508f99b41c717337e8`, and catalog review date **2026-10-28**. This dates the mapping review, not a live run or a new parity result.

### E01

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/codex/executor/tests.rs::codex::executor::tests::execute_fills_empty_output_from_items`](../crates/open-ferry-providers/src/codex/executor/tests.rs).

Executor fixture: a Chat Completions request with a Codex API key receives the asserted text 'ok' reconstructed from a fake Responses event stream.

### E02

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/codex/executor/tests.rs::codex::executor::tests::stream_fills_empty_output_from_items`](../crates/open-ferry-providers/src/codex/executor/tests.rs).

Executor fixture: a Responses request with a Codex API key receives streamed events and an asserted completed response containing 'ok'. This does not open a client WebSocket.

### E03

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/claude/executor/tests.rs::claude::executor::tests::native_call_with_an_api_key`](../crates/open-ferry-providers/src/claude/executor/tests.rs).

Executor fixture: native Claude Messages request fields and API-key headers are asserted, and the response equals the fake upstream message bytes.

### E04

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/claude/executor/tests.rs::claude::executor::tests::native_stream_forwards_whole_events`](../crates/open-ferry-providers/src/claude/executor/tests.rs).

Executor fixture: a Claude API-key Messages stream preserves all eight fake SSE events and their framing.

### E05

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/claude/executor/tests.rs::claude::executor::tests::oauth_call_passes_the_client_through`](../crates/open-ferry-providers/src/claude/executor/tests.rs).

Executor fixture: synthetic Claude OAuth bearer, Messages text request, client headers, and tool declarations pass through. Only the request-side text behavior is mapped; it does not test a returned tool call or real model access. Its synthetic Sonnet model does not override the Haiku-only real testing limits documented above.

### E06

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/gemini/executor/tests.rs::gemini::executor::tests::streams_the_answer`](../crates/open-ferry-providers/src/gemini/executor/tests.rs).

Executor fixture: native Gemini text input with an API key receives fake text chunks; the SSE content type and streamGenerateContent URL are asserted.

### E07

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/gemini/executor/tests.rs::gemini::executor::tests::count_tokens_prepends_leading_user`](../crates/open-ferry-providers/src/gemini/executor/tests.rs).

Executor fixture: Gemini API-key token counting normalizes the request and forwards the fake upstream's totalTokens response. This is upstream counting, not a locally measured model tokenizer.

### E08

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/gemini/vertex/tests.rs::gemini::vertex::tests::service_accounts_call_their_project_with_a_cached_token`](../crates/open-ferry-providers/src/gemini/vertex/tests.rs).

Executor fixture: a fake service-account token endpoint and Vertex API verify text response, streaming chunks, token-count response, project URL, bearer headers, and reuse of the cached token.

### E09

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/gemini/vertex/tests.rs::gemini::vertex::tests::streams_lines_as_they_come`](../crates/open-ferry-providers/src/gemini/vertex/tests.rs).

Executor fixture: native Gemini input through a Vertex API key receives fake text chunks; stream URL and alternative response-format query are asserted.

### E10

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/gemini/interactions/tests.rs::gemini::interactions::tests::native_stream_passes_frames_with_their_usage`](../crates/open-ferry-providers/src/gemini/interactions/tests.rs).

Executor fixture: the Interactions executor with a Gemini API key preserves fake created/completed SSE frames and token usage. It asserts event framing rather than generated text.

### E11

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/gemini/interactions/tests.rs::gemini::interactions::tests::native_preserves_thinking_protocol_fields`](../crates/open-ferry-providers/src/gemini/interactions/tests.rs).

Executor fixture: native Interactions thinking_level and thinking_summaries request fields survive translation with an API key. This is request-side reasoning configuration, not returned reasoning text.

### E12

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/meta/tests.rs::meta::tests::apply_patch_goes_to_meta_as_a_function_and_comes_back_custom`](../crates/open-ferry-providers/src/meta/tests.rs).

Executor fixture: Responses custom apply_patch declarations, history, tool choice, returned calls and patch text round trip through a fake Meta upstream with an API key, both streamed and non-streamed.

### E13

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/xai/executor/tests.rs::xai::executor::tests::translates_for_a_chat_completions_client`](../crates/open-ferry-providers/src/xai/executor/tests.rs).

Executor fixture: a Chat Completions client format receives asserted text 'ok' and chat.completion.chunk output from a fake xAI Responses stream with an API key.

### E14

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/xai/executor/tests.rs::xai::executor::tests::stream_normalizes_reasoning_text_events`](../crates/open-ferry-providers/src/xai/executor/tests.rs).

Executor fixture: fake xAI reasoning_text events become Responses reasoning_summary events, with asserted text, event types, and ordering, for an API key.

### E15

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/xai/executor/tests/images.rs::xai::executor::tests::images::image_calls_post_to_generations_and_publish_usage`](../crates/open-ferry-providers/src/xai/executor/tests/images.rs).

Executor fixture: the OpenAI Images generation format with an xAI API key posts to the fake images endpoint and returns asserted base64 image data. This covers image output, not editing input.

### E16

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/xai/executor/tests/images.rs::xai::executor::tests::images::image_edits_post_to_edits`](../crates/open-ferry-providers/src/xai/executor/tests/images.rs).

Executor fixture: an OpenAI Images edit request with an input image URL and xAI API key reaches the fake edits endpoint with its image preserved.

### E17

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/xai/executor/speech/tests.rs::xai::executor::speech::tests::speech_posts_the_body_and_returns_the_audio`](../crates/open-ferry-providers/src/xai/executor/speech/tests.rs).

Executor fixture: the OpenAI Speech source format and /v1/audio/speech request-path metadata, with an API key and an already shaped xAI payload, post to fake /v1/tts and return asserted audio bytes with audio/mpeg content type. Audio direction: output. This covers the executor boundary, not the public Speech route's input conversion.

### E18

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests.rs::openai_compat::executor::tests::chat_request_goes_out_as_sent`](../crates/open-ferry-providers/src/openai_compat/executor/tests.rs).

Executor fixture: a Chat Completions request with an OpenAI-compatible API key receives asserted text 'ok'; the request's reasoning_effort is asserted as high. The streaming branch checks request forwarding only, so streaming is not promoted by this entry.

### E19

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests.rs::openai_compat::executor::tests::claude_client_gets_a_claude_message`](../crates/open-ferry-providers/src/openai_compat/executor/tests.rs).

Executor fixture: the Claude Messages input format is translated to fake upstream Chat Completions with an API key and returns an asserted Claude message with text 'ok'.

### E20

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests.rs::openai_compat::executor::tests::claude_client_streams_claude_events`](../crates/open-ferry-providers/src/openai_compat/executor/tests.rs).

Executor fixture: fake Chat Completions chunks with an OpenAI-compatible API key become asserted Claude Messages events and text deltas.

### E21

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests.rs::openai_compat::executor::tests::video_input`](../crates/open-ferry-providers/src/openai_compat/executor/tests.rs).

Executor fixture: Chat Completions and Responses video input URLs and data URLs with an API key are preserved in fake upstream requests, both streamed and non-streamed; the returned text is asserted. Video direction: input only; this does not establish video generation.

### E22

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/openai_videos/tests/native.rs::handlers::openai_videos::tests::native::videos_create_binds_retrieve_to_selected_auth`](../crates/open-ferry-server/src/handlers/openai_videos/tests/native.rs).

Server-route fixture: OpenAI video creation and retrieval against a fake xAI upstream stay bound to the selected API-key credential. This covers video job metadata and credential routing, not real generated video quality.

### E23

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/openai_videos/tests/native.rs::handlers::openai_videos::tests::native::xai_videos_native_create_binds_retrieve_to_selected_auth`](../crates/open-ferry-server/src/handlers/openai_videos/tests/native.rs).

Server-route fixture: native xAI video creation and retrieval against a fake xAI upstream stay bound to the selected API-key credential. This covers video job metadata and credential routing.

### E24

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/codex/executor/tests.rs::codex::executor::tests::drops_invalid_reasoning_encrypted_content`](../crates/open-ferry-providers/src/codex/executor/tests.rs).

Executor fixture: Codex API-key Responses requests retain valid encrypted reasoning and remove invalid reasoning IDs/content, on whole, streaming and compaction request paths. This asserts request-side reasoning handling.

### E25

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/claude/executor/tests.rs::claude::executor::tests::openai_call_reads_claudes_stream`](../crates/open-ferry-providers/src/claude/executor/tests.rs).

Executor fixture: a Chat Completions request through a Claude API key returns asserted text Hello there and the stop finish reason from fake Claude SSE.

### E26

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/claude/executor/tests.rs::claude::executor::tests::openai_stream_is_translated`](../crates/open-ferry-providers/src/claude/executor/tests.rs).

Executor fixture: fake Claude SSE through an API key becomes Chat Completions chunks with asserted concatenated text and finish reason.

### E27

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/claude/executor/tests.rs::claude::executor::tests::thinking_suffix_is_applied`](../crates/open-ferry-providers/src/claude/executor/tests.rs).

Executor fixture: a Claude API-key Messages request carries the asserted enabled thinking configuration with a 16384-token budget. This covers request-side reasoning settings.

### E28

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/claude/executor/tests.rs::claude::executor::tests::counts_tokens_with_anthropic`](../crates/open-ferry-providers/src/claude/executor/tests.rs).

Executor fixture: synthetic Claude OAuth Messages token counting calls the fake Anthropic count_tokens endpoint and returns its asserted input_tokens value. It does not prove live model access; the OAuth limits above still apply.

### E29

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests.rs::openai_compat::executor::tests::count_tokens`](../crates/open-ferry-providers/src/openai_compat/executor/tests.rs).

Executor fixture: an OpenAI-compatible API-key credential gets an asserted local tokenizer estimate for Chat Completions. No upstream tokenizer or credential validity is tested.

### E30

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests.rs::openai_compat::executor::tests::count_tokens_for_a_claude_client`](../crates/open-ferry-providers/src/openai_compat/executor/tests.rs).

Executor fixture: a Claude Messages request with an OpenAI-compatible API-key credential gets an asserted local input_tokens estimate, without an upstream count call.

### E31

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/xai/executor/tests.rs::xai::executor::tests::count_tokens_answers_in_the_clients_format_unsent`](../crates/open-ferry-providers/src/xai/executor/tests.rs).

Executor fixture: xAI API-key credentials get positive local token estimates in Responses and Claude Messages formats; the fake upstream receives no request.

### E32

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/meta/tests.rs::meta::tests::count_tokens_answers_in_the_clients_format`](../crates/open-ferry-providers/src/meta/tests.rs).

Executor fixture: Meta API-key credentials get positive local token estimates in Responses and Claude Messages formats; no Meta request is made.

### E33

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/gemini/vertex/tests.rs::gemini::vertex::tests::apply_patch_reuses_the_executor`](../crates/open-ferry-providers/src/gemini/vertex/tests.rs).

Executor fixture: Responses apply_patch tool declarations, returned custom-tool patch output, and streamed tool events are asserted through a Vertex API key and fake upstream.

### E34

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/gemini/vertex/tests.rs::gemini::vertex::tests::service_accounts_call_imagen_with_its_own_requests`](../crates/open-ferry-providers/src/gemini/vertex/tests.rs).

Executor fixture: a Vertex service account exchanges a fake token, calls the project Imagen predict endpoint, and returns asserted base64 image bytes in Gemini response shape. Image direction: output.

### E35

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/gemini/vertex/tests.rs::gemini::vertex::tests::api_keys_call_imagen_with_the_gemini_request`](../crates/open-ferry-providers/src/gemini/vertex/tests.rs).

Executor fixture: a Vertex API key routes Gemini text input to Imagen predict and returns asserted base64 prediction bytes. Image direction: output.

### E36

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/codex/executor/tests/images.rs::codex::executor::tests::images::tool_generation`](../crates/open-ferry-providers/src/codex/executor/tests/images.rs).

Executor fixture: a Codex API key turns an Images generation request into an image_generation Responses tool and returns asserted image data URL and metadata. Image direction: output.

### E37

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/codex/executor/tests/images.rs::codex::executor::tests::images::tool_json_edit`](../crates/open-ferry-providers/src/codex/executor/tests/images.rs).

Executor fixture: Codex API-key Images edits preserve input image URLs/data URLs and masks in the translated tool request, and return asserted base64 image output. This entry covers the input-image cell.

### E38

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/codex/executor/tests/images.rs::codex::executor::tests::images::tool_stream`](../crates/open-ferry-providers/src/codex/executor/tests/images.rs).

Executor fixture: a Codex API-key Images edit returns asserted partial-image and completed-image SSE events and base64 payloads.

### E39

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests/images.rs::openai_compat::executor::tests::images::images_generations_passthrough`](../crates/open-ferry-providers/src/openai_compat/executor/tests/images.rs).

Executor fixture: an OpenAI-compatible API key passes an Images generation request to a fake images endpoint and returns the exact asserted base64 image response. Image direction: output.

### E40

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests/images.rs::openai_compat::executor::tests::images::images_edits_multipart_rewrites_model`](../crates/open-ferry-providers/src/openai_compat/executor/tests/images.rs).

Executor fixture: an OpenAI-compatible API-key Images edit preserves asserted multipart image bytes and MIME type while rewriting the model. Image direction: input.

### E41

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests/images.rs::openai_compat::executor::tests::images::images_generations_streams_upstream`](../crates/open-ferry-providers/src/openai_compat/executor/tests/images.rs).

Executor fixture: an OpenAI-compatible API-key Images generation forwards the fake partial-generation and DONE SSE events with the asserted streaming request headers.

### E42

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/interactions/tests/native.rs::handlers::interactions::tests::native::a_vertex_model_uses_the_native_interactions_endpoint`](../crates/open-ferry-server/src/handlers/interactions/tests/native.rs).

Public-route fixture: POST /v1beta/interactions passes through the real credential manager and Vertex executor with an API key; input text, upstream key, native endpoint, and returned vertex-ok text are asserted.

### E43

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/interactions/tests/native.rs::handlers::interactions::tests::native::an_agent_uses_the_native_interactions_endpoint`](../crates/open-ferry-server/src/handlers/interactions/tests/native.rs).

Public-route fixture: POST /v1beta/interactions with agent and text input goes through the real Interactions executor and API-key credential; the selected agent, credential header, endpoint and response ID are asserted. This covers request-side text handling.

### E44

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/interactions/tests/native.rs::handlers::interactions::tests::native::an_agent_stream_passes_the_frames_through`](../crates/open-ferry-server/src/handlers/interactions/tests/native.rs).

Public-route fixture: an Interactions agent stream goes through the real Interactions executor and API-key credential; the HTTP SSE content type and exact returned frames are asserted.

### E45

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/images/tests.rs::handlers::images::tests::codex_generation_is_sent_on`](../crates/open-ferry-server/src/handlers/images/tests.rs).

Public-route fixture: POST /v1/images/generations selects Codex and returns asserted base64 output through a scripted dispatcher. Provider credentials are not exercised.

### E46

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/images/tests.rs::handlers::images::tests::xai_streams_are_written_as_completed_events`](../crates/open-ferry-server/src/handlers/images/tests.rs).

Public-route fixture: POST /v1/images/generations with stream=true writes exact completed-image SSE events and asserted base64/URL fields from scripted xAI dispatch. Provider credentials are not exercised.

### E47

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/images/tests.rs::handlers::images::tests::codex_form_edits_are_written_again`](../crates/open-ferry-server/src/handlers/images/tests.rs).

Public-route fixture: POST /v1/images/edits preserves asserted multipart PNG bytes, MIME type and fields in the scripted Codex dispatch. Provider credentials are not exercised.

### E48

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/openai_speech/tests.rs::handlers::openai_speech::tests::speech_endpoints_route_speech_models_to_the_executor`](../crates/open-ferry-server/src/handlers/openai_speech/tests.rs).

Public-route fixture: POST /v1/audio/speech and POST /v1/tts select xAI and return asserted ID3audio bytes and audio/mpeg content type from a scripted dispatcher. Audio direction: output; provider credentials are not exercised.

### E49

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/codex_client/tests.rs::handlers::codex_client::tests::responses_ready_tools_for_http_and_sse`](../crates/open-ferry-server/src/handlers/codex_client/tests.rs).

Public-route fixture: POST /v1/responses in HTTP and SSE modes prepares asserted collaboration-tool schemas and descriptions for scripted Codex dispatch. This covers request-side tool handling; no real client version or provider credential is exercised.

### E50

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/codex_client/tests.rs::handlers::codex_client::tests::websocket_readies_tools`](../crates/open-ferry-server/src/handlers/codex_client/tests.rs).

Public-route fixture: a real socket at /v1/responses accepts response.create and prepares asserted collaboration-tool schemas for scripted Codex dispatch. This covers request-side tool handling and the client WebSocket, without a provider credential or real client application.

### E51

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/responses_ws/tests.rs::handlers::responses_ws::tests::merges_transcript_for_non_passthrough_upstream`](../crates/open-ferry-server/src/handlers/responses_ws/tests.rs).

Public-route fixture: a real /v1/responses WebSocket emits asserted completed events over two turns and sends the asserted merged transcript via scripted Codex dispatch. No provider credential or upstream WebSocket is exercised.

### E52

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/models/codex/tests.rs::handlers::models::codex::tests::serves_the_codex_catalog`](../crates/open-ferry-server/src/handlers/models/codex/tests.rs).

Public-route fixture: GET /v1/models?client_version returns asserted Codex catalog entries, capabilities and token limits from a registered model list. Synthetic client-version headers are not a tested application recipe; provider credentials are not exercised.

### E53

fixture-tested; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Test: [`crates/open-ferry-server/src/handlers/openai_videos/tests/native.rs::handlers::openai_videos::tests::native::write_video_content_from_url`](../crates/open-ferry-server/src/handlers/openai_videos/tests/native.rs).

Public-route fixture: GET /openai/v1/videos/video_123/content goes through the real xAI executor and API-key credential, returning asserted video bytes, video/mp4 type and attachment disposition from fake upstreams. Video direction: output.

### U01

unsupported; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Refusal: [`crates/open-ferry-providers/src/claude_cli/prompt.rs`](../crates/open-ferry-providers/src/claude_cli/prompt.rs); `claude-cli: client tools aren't supported by claude-cli yet`.

The Claude Messages prompt builder rejects client tools, forcing tool choices, and tool-use/result blocks with a local invalid-request error. See also docs/claude-subscription.md.

### U02

unsupported; v8.0.38 / `0db19025571684b198a2ef508f99b41c717337e8`; reviewed 2026-10-28.

Refusal: [`crates/open-ferry-providers/src/claude_cli/executor.rs`](../crates/open-ferry-providers/src/claude_cli/executor.rs); `claude-cli: counting tokens isn't supported; Claude Code has no token count`.

The claude-cli executor's count_tokens method returns 501 for Claude Messages token counting. See also docs/claude-subscription.md.

### Parity suite evidence

Each slug is selectable with `--suite SLUG` in a real offline comparison. Labels come from the [translator registry](../tools/parity/src/translator.rs) and its [Interactions families](../tools/parity/src/interactions/mod.rs).

| Suite slug | Registry label | Hand-written cases |
|---|---|---:|
| <a id="p-gemini-to-claude-request"></a>`gemini-to-claude-request` | `claude/gemini/request` | 42 |
| <a id="p-claude-to-gemini-stream"></a>`claude-to-gemini-stream` | `claude/gemini/response` | 9 |
| <a id="p-claude-to-gemini-non-stream"></a>`claude-to-gemini-non-stream` | `claude/gemini/response-non-stream` | 19 |
| <a id="p-interactions-to-claude-request"></a>`interactions-to-claude-request` | `claude/interactions/request` | 45 |
| <a id="p-claude-to-interactions-stream"></a>`claude-to-interactions-stream` | `claude/interactions/response` | 10 |
| <a id="p-claude-to-interactions-non-stream"></a>`claude-to-interactions-non-stream` | `claude/interactions/response-non-stream` | 15 |
| <a id="p-chat-to-claude-request"></a>`chat-to-claude-request` | `claude/openai-chat/request` | 77 |
| <a id="p-chat-to-claude-request-compat"></a>`chat-to-claude-request-compat` | `claude/openai-chat/request-compat` | 77 |
| <a id="p-claude-to-chat-stream"></a>`claude-to-chat-stream` | `claude/openai-chat/response` | 9 |
| <a id="p-claude-to-chat-non-stream"></a>`claude-to-chat-non-stream` | `claude/openai-chat/response-non-stream` | 19 |
| <a id="p-responses-to-claude-request"></a>`responses-to-claude-request` | `claude/openai-responses/request` | 116 |
| <a id="p-responses-to-claude-request-compat"></a>`responses-to-claude-request-compat` | `claude/openai-responses/request-compat` | 116 |
| <a id="p-claude-to-responses-stream"></a>`claude-to-responses-stream` | `claude/openai-responses/response` | 18 |
| <a id="p-claude-to-responses-non-stream"></a>`claude-to-responses-non-stream` | `claude/openai-responses/response-non-stream` | 27 |
| <a id="p-codex-models"></a>`codex-models` | `codex-models/list` | 21 |
| <a id="p-claude-request"></a>`claude-request` | `codex/claude/request` | 25 |
| <a id="p-claude-request-compat"></a>`claude-request-compat` | `codex/claude/request-compat` | 25 |
| <a id="p-claude-stream"></a>`claude-stream` | `codex/claude/response` | 13 |
| <a id="p-claude-non-stream"></a>`claude-non-stream` | `codex/claude/response-non-stream` | 10 |
| <a id="p-gemini-to-codex-request"></a>`gemini-to-codex-request` | `codex/gemini/request` | 45 |
| <a id="p-codex-to-gemini-stream"></a>`codex-to-gemini-stream` | `codex/gemini/response` | 14 |
| <a id="p-codex-to-gemini-non-stream"></a>`codex-to-gemini-non-stream` | `codex/gemini/response-non-stream` | 13 |
| <a id="p-interactions-to-codex-request"></a>`interactions-to-codex-request` | `codex/interactions/request` | 45 |
| <a id="p-codex-to-interactions-stream"></a>`codex-to-interactions-stream` | `codex/interactions/response` | 22 |
| <a id="p-codex-to-interactions-non-stream"></a>`codex-to-interactions-non-stream` | `codex/interactions/response-non-stream` | 15 |
| <a id="p-chat-request"></a>`chat-request` | `codex/openai-chat/request` | 46 |
| <a id="p-chat-stream"></a>`chat-stream` | `codex/openai-chat/response` | 24 |
| <a id="p-chat-non-stream"></a>`chat-non-stream` | `codex/openai-chat/response-non-stream` | 12 |
| <a id="p-responses-request"></a>`responses-request` | `codex/openai-responses/request` | 42 |
| <a id="p-responses-stream"></a>`responses-stream` | `codex/openai-responses/response` | 13 |
| <a id="p-responses-non-stream"></a>`responses-non-stream` | `codex/openai-responses/response-non-stream` | 13 |
| <a id="p-completions-request"></a>`completions-request` | `completions/request` | 40 |
| <a id="p-completions-response"></a>`completions-response` | `completions/response` | 35 |
| <a id="p-completions-stream-chunk"></a>`completions-stream-chunk` | `completions/stream-chunk` | 19 |
| <a id="p-claude-to-gemini-request"></a>`claude-to-gemini-request` | `gemini/claude/request` | 45 |
| <a id="p-claude-to-gemini-request-compat"></a>`claude-to-gemini-request-compat` | `gemini/claude/request-compat` | 45 |
| <a id="p-gemini-to-claude-stream"></a>`gemini-to-claude-stream` | `gemini/claude/response` | 35 |
| <a id="p-gemini-to-claude-non-stream"></a>`gemini-to-claude-non-stream` | `gemini/claude/response-non-stream` | 29 |
| <a id="p-gemini-to-gemini-request"></a>`gemini-to-gemini-request` | `gemini/gemini/request` | 26 |
| <a id="p-gemini-to-gemini-stream"></a>`gemini-to-gemini-stream` | `gemini/gemini/response` | 23 |
| <a id="p-gemini-to-gemini-non-stream"></a>`gemini-to-gemini-non-stream` | `gemini/gemini/response-non-stream` | 28 |
| <a id="p-interactions-to-gemini-request"></a>`interactions-to-gemini-request` | `gemini/interactions/request` | 144 |
| <a id="p-gemini-to-interactions-stream"></a>`gemini-to-interactions-stream` | `gemini/interactions/response` | 20 |
| <a id="p-gemini-to-interactions-non-stream"></a>`gemini-to-interactions-non-stream` | `gemini/interactions/response-non-stream` | 19 |
| <a id="p-chat-to-gemini-request"></a>`chat-to-gemini-request` | `gemini/openai-chat/request` | 67 |
| <a id="p-gemini-to-chat-stream"></a>`gemini-to-chat-stream` | `gemini/openai-chat/response` | 30 |
| <a id="p-gemini-to-chat-non-stream"></a>`gemini-to-chat-non-stream` | `gemini/openai-chat/response-non-stream` | 27 |
| <a id="p-responses-to-gemini-request"></a>`responses-to-gemini-request` | `gemini/openai-responses/request` | 37 |
| <a id="p-gemini-to-responses-stream"></a>`gemini-to-responses-stream` | `gemini/openai-responses/response` | 49 |
| <a id="p-gemini-to-responses-non-stream"></a>`gemini-to-responses-non-stream` | `gemini/openai-responses/response-non-stream` | 25 |
| <a id="p-claude-to-interactions-request"></a>`claude-to-interactions-request` | `interactions/claude/request` | 35 |
| <a id="p-claude-to-interactions-request-compat"></a>`claude-to-interactions-request-compat` | `interactions/claude/request-compat` | 35 |
| <a id="p-interactions-to-claude-stream"></a>`interactions-to-claude-stream` | `interactions/claude/response` | 16 |
| <a id="p-interactions-to-claude-non-stream"></a>`interactions-to-claude-non-stream` | `interactions/claude/response-non-stream` | 32 |
| <a id="p-gemini-to-interactions-request"></a>`gemini-to-interactions-request` | `interactions/gemini/request` | 84 |
| <a id="p-interactions-to-gemini-stream"></a>`interactions-to-gemini-stream` | `interactions/gemini/response` | 11 |
| <a id="p-interactions-to-gemini-non-stream"></a>`interactions-to-gemini-non-stream` | `interactions/gemini/response-non-stream` | 16 |
| <a id="p-interactions-passthrough-request"></a>`interactions-passthrough-request` | `interactions/interactions/request` | 6 |
| <a id="p-interactions-passthrough-stream"></a>`interactions-passthrough-stream` | `interactions/interactions/response` | 2 |
| <a id="p-interactions-passthrough-non-stream"></a>`interactions-passthrough-non-stream` | `interactions/interactions/response-non-stream` | 5 |
| <a id="p-chat-to-interactions-request"></a>`chat-to-interactions-request` | `interactions/openai-chat/request` | 12 |
| <a id="p-interactions-to-chat-stream"></a>`interactions-to-chat-stream` | `interactions/openai-chat/response` | 12 |
| <a id="p-interactions-to-chat-non-stream"></a>`interactions-to-chat-non-stream` | `interactions/openai-chat/response-non-stream` | 9 |
| <a id="p-responses-to-interactions-request"></a>`responses-to-interactions-request` | `interactions/openai-responses/request` | 26 |
| <a id="p-interactions-to-responses-stream"></a>`interactions-to-responses-stream` | `interactions/openai-responses/response` | 13 |
| <a id="p-interactions-to-responses-non-stream"></a>`interactions-to-responses-non-stream` | `interactions/openai-responses/response-non-stream` | 10 |
| <a id="p-interactions-to-responses-request"></a>`interactions-to-responses-request` | `openai-responses/interactions/request` | 11 |
| <a id="p-responses-to-interactions-stream"></a>`responses-to-interactions-stream` | `openai-responses/interactions/response` | 5 |
| <a id="p-responses-to-interactions-non-stream"></a>`responses-to-interactions-non-stream` | `openai-responses/interactions/response-non-stream` | 4 |
| <a id="p-claude-to-chat-request"></a>`claude-to-chat-request` | `openai/claude/request` | 64 |
| <a id="p-claude-to-chat-request-compat"></a>`claude-to-chat-request-compat` | `openai/claude/request-compat` | 64 |
| <a id="p-chat-to-claude-stream"></a>`chat-to-claude-stream` | `openai/claude/response` | 72 |
| <a id="p-chat-to-claude-non-stream"></a>`chat-to-claude-non-stream` | `openai/claude/response-non-stream` | 33 |
| <a id="p-gemini-to-chat-request"></a>`gemini-to-chat-request` | `openai/gemini/request` | 45 |
| <a id="p-chat-to-gemini-stream"></a>`chat-to-gemini-stream` | `openai/gemini/response` | 52 |
| <a id="p-chat-to-gemini-non-stream"></a>`chat-to-gemini-non-stream` | `openai/gemini/response-non-stream` | 32 |
| <a id="p-interactions-to-chat-request"></a>`interactions-to-chat-request` | `openai/interactions/request` | 9 |
| <a id="p-chat-to-interactions-stream"></a>`chat-to-interactions-stream` | `openai/interactions/response` | 7 |
| <a id="p-chat-to-interactions-non-stream"></a>`chat-to-interactions-non-stream` | `openai/interactions/response-non-stream` | 6 |
| <a id="p-chat-to-chat-request"></a>`chat-to-chat-request` | `openai/openai-chat/request` | 64 |
| <a id="p-chat-to-chat-stream"></a>`chat-to-chat-stream` | `openai/openai-chat/response` | 51 |
| <a id="p-chat-to-chat-non-stream"></a>`chat-to-chat-non-stream` | `openai/openai-chat/response-non-stream` | 31 |
| <a id="p-responses-to-chat-request"></a>`responses-to-chat-request` | `openai/openai-responses/request` | 183 |
| <a id="p-chat-to-responses-stream"></a>`chat-to-responses-stream` | `openai/openai-responses/response` | 78 |
| <a id="p-chat-to-responses-non-stream"></a>`chat-to-responses-non-stream` | `openai/openai-responses/response-non-stream` | 40 |
| <a id="p-thinking-codex"></a>`thinking-codex` | `thinking/codex` | 47 |
| <a id="p-thinking-openai"></a>`thinking-openai` | `thinking/openai` | 33 |
