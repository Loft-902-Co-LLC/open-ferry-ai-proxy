# Compatibility evidence

<!-- Written by .github/scripts/compatibility.py; don't edit by hand. -->

Open Ferry is a credited Rust port of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI).
The pinned reference is **v8.0.38**, commit `0db19025571684b198a2ef508f99b41c717337e8` (2026-10-21); see [UPSTREAM.md](../UPSTREAM.md#pinned-reference).

This matrix describes **client protocols and transports**, providers, credential types, and individual capabilities separately.
Evidence is deliberately narrow: executor fixtures exercise the named client format and a fake upstream, without a real client application or provider account.
They do not establish every model, every request shape, or the public HTTP/WebSocket route. Evidence that exercises a server route says so explicitly.
A provider's upstream WebSocket test alone does not prove a client's WebSocket transport.

## Legend and scope

- **F [E…] — fixture-tested**: the linked, exact Rust test is run offline by the generator's check in required CI. Each reference identifies its source, fixture revision, review date, and the assertions it covers.
- **live-tested**: requires a recorded result from a real provider and client. **There are no recorded live results, and no live-tested cells.**
- **U [U…] — unsupported**: explicit refusal code is linked and checked for existence. A refusal is not a successful capability test.
- **? — unknown**: no evidence mapped for this exact combination. Implementation, translator parity, or support for another credential type does not promote it to tested.

Text means text request/response handling; streaming means incremental protocol events; tools means only the tool behavior described by its evidence.
Reasoning means the named reasoning fields or events, not model quality. Images in/out are separate; audio and video evidence specifies its direction.
Token counting may be local estimation; its evidence states which. Unknown also covers dimensions that are not meaningful for an endpoint; it is not a claim of refusal.

**Claude OAuth sign-in limits:** this route goes against Anthropic's terms; in testing only Haiku models worked. Accounts may be limited or suspended.
The synthetic OAuth fixture below does not establish real Sonnet/Opus access. See [Claude subscriptions](claude-subscription.md#the-claude-sign-in).
`claude-cli` instead runs the user's installed Claude Code and stores no Claude Code token.

## Client application versions

Client application versions are **not tested yet**. No Codex CLI, Claude Code, or other application version is claimed by these protocol fixtures.
A future recorded client recipe should add application/version, provider/model, credential type, protocol/transport, capabilities, evidence artifact, and test date here.
A documented minimum version or a synthetic User-Agent is not a tested client recipe.

## Cell counts

Counts cover capability cells in the protocol tables only, including unknown combinations.

| Evidence level | Cells |
|---|---:|
| fixture-tested | 33 |
| live-tested | 0 |
| unsupported | 2 |
| unknown | 2110 |

Total: 2145 cells (13 protocol/transport entries × 15 provider/credential entries × 11 capabilities).

## OpenAI Chat Completions / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | F [E01](#e01) | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | F [E13](#e13) | F [E13](#e13) | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | F [E18](#e18) | ? | ? | F [E18](#e18) | ? | ? | ? | F [E21](#e21) | ? | ? | ? |

## Legacy Completions / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |

## OpenAI Responses / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | F [E02](#e02) | F [E02](#e02) | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | F [E12](#e12) | F [E12](#e12) | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | F [E14](#e14) | ? | F [E14](#e14) | ? | ? | ? | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | F [E21](#e21) | ? | ? | ? |

## OpenAI Responses / WebSocket

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |

## Claude Messages / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | F [E03](#e03) | F [E04](#e04) | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | F [E05](#e05) | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | U [U01](#u01) | ? | ? | ? | ? | ? | ? | U [U02](#u02) | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | F [E19](#e19) | F [E20](#e20) | ? | ? | ? | ? | ? | ? | ? | ? | ? |

## Gemini generateContent / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | F [E06](#e06) | F [E06](#e06) | ? | ? | ? | ? | ? | ? | ? | F [E07](#e07) | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | F [E09](#e09) | F [E09](#e09) | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | F [E08](#e08) | F [E08](#e08) | ? | ? | ? | ? | ? | ? | ? | F [E08](#e08) | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |

## Gemini Interactions / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | F [E10](#e10) | ? | F [E11](#e11) | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |

## OpenAI Images / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | ? | ? | ? | F [E16](#e16) | F [E15](#e15) | ? | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |

## OpenAI Speech / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | ? | ? | ? | ? | ? | F [E17](#e17) | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |

## xAI TTS / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |

## OpenAI Videos / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | ? | ? | ? | ? | ? | ? | F [E22](#e22) | ? | ? | ? |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |

## xAI Videos / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | ? | ? | ? | ? | ? | ? | F [E23](#e23) | ? | ? | ? |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |

## Model discovery / HTTP

| Provider | Credential | Text | Streaming | Tool calls | Reasoning | Images in | Images out | Audio | Video | Model list | Token counting | WebSocket |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Codex | OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Codex | SIWC nested OAuth | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude | OAuth sign-in; see limits above | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Claude via claude-cli | Installed Claude Code session | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Gemini via Interactions executor | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Service account | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Vertex AI | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| Meta | Access token | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| xAI | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |
| OpenAI-compatible upstream | API key | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? | ? |

## Evidence catalog

Catalog review date: **2026-10-28** (the mapping review, not a live run or a generation timestamp).
Fixture versions below are SHA-256 digests of the linked Rust source file after LF normalization, so they also work in shallow CI checkouts.
The review date stays fixed until the mapping is reviewed; source changes update the fixture revision and require regeneration.
The upstream pin above identifies the port's reference, not an executed parity comparison for these provider fixtures.

### E01

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:1de550e55aaa158e29e7ae7137da787fec89ba2565075766de93a7003d84b0bc`.

Test: [`crates/open-ferry-providers/src/codex/executor/tests.rs::codex::executor::tests::execute_fills_empty_output_from_items`](../crates/open-ferry-providers/src/codex/executor/tests.rs#L511).

Executor fixture: a Chat Completions request with a Codex API key receives the asserted text 'ok' reconstructed from a fake Responses event stream.

### E02

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:1de550e55aaa158e29e7ae7137da787fec89ba2565075766de93a7003d84b0bc`.

Test: [`crates/open-ferry-providers/src/codex/executor/tests.rs::codex::executor::tests::stream_fills_empty_output_from_items`](../crates/open-ferry-providers/src/codex/executor/tests.rs#L534).

Executor fixture: a Responses request with a Codex API key receives streamed events and an asserted completed response containing 'ok'. This does not open a client WebSocket.

### E03

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:afda0b0dda298813afd36981f0b2bf8d83b9e136c538ca34414c68f0106296de`.

Test: [`crates/open-ferry-providers/src/claude/executor/tests.rs::claude::executor::tests::native_call_with_an_api_key`](../crates/open-ferry-providers/src/claude/executor/tests.rs#L298).

Executor fixture: native Claude Messages request fields and API-key headers are asserted, and the response equals the fake upstream message bytes.

### E04

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:afda0b0dda298813afd36981f0b2bf8d83b9e136c538ca34414c68f0106296de`.

Test: [`crates/open-ferry-providers/src/claude/executor/tests.rs::claude::executor::tests::native_stream_forwards_whole_events`](../crates/open-ferry-providers/src/claude/executor/tests.rs#L587).

Executor fixture: a Claude API-key Messages stream preserves all eight fake SSE events and their framing.

### E05

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:afda0b0dda298813afd36981f0b2bf8d83b9e136c538ca34414c68f0106296de`.

Test: [`crates/open-ferry-providers/src/claude/executor/tests.rs::claude::executor::tests::oauth_call_passes_the_client_through`](../crates/open-ferry-providers/src/claude/executor/tests.rs#L345).

Executor fixture: synthetic Claude OAuth bearer, Messages text request, client headers, and tool declarations pass through. Only the request-side text behavior is mapped; it does not test a returned tool call or real model access. Its synthetic Sonnet model does not override the Haiku-only real testing limits documented above.

### E06

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:a5f4293dd913478200fa4abcd12e6aebae533245f224c699fa5c6912f3de5420`.

Test: [`crates/open-ferry-providers/src/gemini/executor/tests.rs::gemini::executor::tests::streams_the_answer`](../crates/open-ferry-providers/src/gemini/executor/tests.rs#L633).

Executor fixture: native Gemini text input with an API key receives fake text chunks; the SSE content type and streamGenerateContent URL are asserted.

### E07

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:a5f4293dd913478200fa4abcd12e6aebae533245f224c699fa5c6912f3de5420`.

Test: [`crates/open-ferry-providers/src/gemini/executor/tests.rs::gemini::executor::tests::count_tokens_prepends_leading_user`](../crates/open-ferry-providers/src/gemini/executor/tests.rs#L222).

Executor fixture: Gemini API-key token counting normalizes the request and forwards the fake upstream's totalTokens response. This is upstream counting, not a locally measured model tokenizer.

### E08

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:72ae6b0d8d29835f9102e68fbf5d2e3097f9b729fd58e345df943114840faa77`.

Test: [`crates/open-ferry-providers/src/gemini/vertex/tests.rs::gemini::vertex::tests::service_accounts_call_their_project_with_a_cached_token`](../crates/open-ferry-providers/src/gemini/vertex/tests.rs#L475).

Executor fixture: a fake service-account token endpoint and Vertex API verify text response, streaming chunks, token-count response, project URL, bearer headers, and reuse of the cached token.

### E09

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:72ae6b0d8d29835f9102e68fbf5d2e3097f9b729fd58e345df943114840faa77`.

Test: [`crates/open-ferry-providers/src/gemini/vertex/tests.rs::gemini::vertex::tests::streams_lines_as_they_come`](../crates/open-ferry-providers/src/gemini/vertex/tests.rs#L790).

Executor fixture: native Gemini input through a Vertex API key receives fake text chunks; stream URL and alternative response-format query are asserted.

### E10

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:94d07013dc703a1add19012aa0fce9a888fa9f5126eaf99046da4a76525acef7`.

Test: [`crates/open-ferry-providers/src/gemini/interactions/tests.rs::gemini::interactions::tests::native_stream_passes_frames_with_their_usage`](../crates/open-ferry-providers/src/gemini/interactions/tests.rs#L714).

Executor fixture: the Interactions executor with a Gemini API key preserves fake created/completed SSE frames and token usage. It asserts event framing rather than generated text.

### E11

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:94d07013dc703a1add19012aa0fce9a888fa9f5126eaf99046da4a76525acef7`.

Test: [`crates/open-ferry-providers/src/gemini/interactions/tests.rs::gemini::interactions::tests::native_preserves_thinking_protocol_fields`](../crates/open-ferry-providers/src/gemini/interactions/tests.rs#L633).

Executor fixture: native Interactions thinking_level and thinking_summaries request fields survive translation with an API key. This is request-side reasoning configuration, not returned reasoning text.

### E12

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:0e75fdb7f74a20816c9d3ec4cf699928c557539c5ab5ba74d9d428f5f57bd667`.

Test: [`crates/open-ferry-providers/src/meta/tests.rs::meta::tests::apply_patch_goes_to_meta_as_a_function_and_comes_back_custom`](../crates/open-ferry-providers/src/meta/tests.rs#L1097).

Executor fixture: Responses custom apply_patch declarations, history, tool choice, returned calls and patch text round trip through a fake Meta upstream with an API key, both streamed and non-streamed.

### E13

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:7885feccc28799154328f23a527355759dfabef47974db3b06158492287978c0`.

Test: [`crates/open-ferry-providers/src/xai/executor/tests.rs::xai::executor::tests::translates_for_a_chat_completions_client`](../crates/open-ferry-providers/src/xai/executor/tests.rs#L1142).

Executor fixture: a Chat Completions client format receives asserted text 'ok' and chat.completion.chunk output from a fake xAI Responses stream with an API key.

### E14

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:7885feccc28799154328f23a527355759dfabef47974db3b06158492287978c0`.

Test: [`crates/open-ferry-providers/src/xai/executor/tests.rs::xai::executor::tests::stream_normalizes_reasoning_text_events`](../crates/open-ferry-providers/src/xai/executor/tests.rs#L1067).

Executor fixture: fake xAI reasoning_text events become Responses reasoning_summary events, with asserted text, event types, and ordering, for an API key.

### E15

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:c00590719d657152cb8463e5b324601eba4f69befb006fe4096d95e3dbb68491`.

Test: [`crates/open-ferry-providers/src/xai/executor/tests/images.rs::xai::executor::tests::images::image_calls_post_to_generations_and_publish_usage`](../crates/open-ferry-providers/src/xai/executor/tests/images.rs#L32).

Executor fixture: the OpenAI Images generation format with an xAI API key posts to the fake images endpoint and returns asserted base64 image data. This covers image output, not editing input.

### E16

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:c00590719d657152cb8463e5b324601eba4f69befb006fe4096d95e3dbb68491`.

Test: [`crates/open-ferry-providers/src/xai/executor/tests/images.rs::xai::executor::tests::images::image_edits_post_to_edits`](../crates/open-ferry-providers/src/xai/executor/tests/images.rs#L132).

Executor fixture: an OpenAI Images edit request with an input image URL and xAI API key reaches the fake edits endpoint with its image preserved.

### E17

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:792a1b0d4dbe6dea04c0f4779ce369bb8dfa965ccbd8b0cfc12d8648675e28a9`.

Test: [`crates/open-ferry-providers/src/xai/executor/speech/tests.rs::xai::executor::speech::tests::speech_posts_the_body_and_returns_the_audio`](../crates/open-ferry-providers/src/xai/executor/speech/tests.rs#L207).

Executor fixture: the OpenAI Speech source format and /v1/audio/speech request-path metadata, with an API key and an already shaped xAI payload, post to fake /v1/tts and return asserted audio bytes with audio/mpeg content type. Audio direction: output. This covers the executor boundary, not the public Speech route's input conversion.

### E18

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:fe11f1d780704cfb2f4cc80e25cbd88c065dd633e8f5f7721bbae07a620164e7`.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests.rs::openai_compat::executor::tests::chat_request_goes_out_as_sent`](../crates/open-ferry-providers/src/openai_compat/executor/tests.rs#L1287).

Executor fixture: a Chat Completions request with an OpenAI-compatible API key receives asserted text 'ok'; the request's reasoning_effort is asserted as high. The streaming branch checks request forwarding only, so streaming is not promoted by this entry.

### E19

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:fe11f1d780704cfb2f4cc80e25cbd88c065dd633e8f5f7721bbae07a620164e7`.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests.rs::openai_compat::executor::tests::claude_client_gets_a_claude_message`](../crates/open-ferry-providers/src/openai_compat/executor/tests.rs#L1587).

Executor fixture: the Claude Messages input format is translated to fake upstream Chat Completions with an API key and returns an asserted Claude message with text 'ok'.

### E20

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:fe11f1d780704cfb2f4cc80e25cbd88c065dd633e8f5f7721bbae07a620164e7`.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests.rs::openai_compat::executor::tests::claude_client_streams_claude_events`](../crates/open-ferry-providers/src/openai_compat/executor/tests.rs#L1611).

Executor fixture: fake Chat Completions chunks with an OpenAI-compatible API key become asserted Claude Messages events and text deltas.

### E21

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:fe11f1d780704cfb2f4cc80e25cbd88c065dd633e8f5f7721bbae07a620164e7`.

Test: [`crates/open-ferry-providers/src/openai_compat/executor/tests.rs::openai_compat::executor::tests::video_input`](../crates/open-ferry-providers/src/openai_compat/executor/tests.rs#L1877).

Executor fixture: Chat Completions and Responses video input URLs and data URLs with an API key are preserved in fake upstream requests, both streamed and non-streamed; the returned text is asserted. Video direction: input only; this does not establish video generation.

### E22

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:aa8d9d496e8247e52512585b876e943392f4bc275833f7a66a679595a6a89d0e`.

Test: [`crates/open-ferry-server/src/handlers/openai_videos/tests/native.rs::handlers::openai_videos::tests::native::videos_create_binds_retrieve_to_selected_auth`](../crates/open-ferry-server/src/handlers/openai_videos/tests/native.rs#L315).

Server-route fixture: OpenAI video creation and retrieval against a fake xAI upstream stay bound to the selected API-key credential. This covers video job metadata and credential routing, not real generated video quality.

### E23

fixture-tested; reviewed 2026-10-28; fixture revision `sha256:aa8d9d496e8247e52512585b876e943392f4bc275833f7a66a679595a6a89d0e`.

Test: [`crates/open-ferry-server/src/handlers/openai_videos/tests/native.rs::handlers::openai_videos::tests::native::xai_videos_native_create_binds_retrieve_to_selected_auth`](../crates/open-ferry-server/src/handlers/openai_videos/tests/native.rs#L347).

Server-route fixture: native xAI video creation and retrieval against a fake xAI upstream stay bound to the selected API-key credential. This covers video job metadata and credential routing.

### U01

unsupported; reviewed 2026-10-28; fixture revision `sha256:cff9cb8bbbebd636d14b1c604f1a88b00de0e4d1ea1cdf446c1dfc2e609fc0eb`.

Refusal: [`crates/open-ferry-providers/src/claude_cli/prompt.rs`](../crates/open-ferry-providers/src/claude_cli/prompt.rs#L172); `claude-cli: client tools aren't supported by claude-cli yet`.

The Claude Messages prompt builder rejects client tools, forcing tool choices, and tool-use/result blocks with a local invalid-request error. See also docs/claude-subscription.md.

### U02

unsupported; reviewed 2026-10-28; fixture revision `sha256:bcae953c32f6f0c65f7f68845b39808656eac369320e858bf16da3e8b5595da7`.

Refusal: [`crates/open-ferry-providers/src/claude_cli/executor.rs`](../crates/open-ferry-providers/src/claude_cli/executor.rs#L592); `claude-cli: counting tokens isn't supported; Claude Code has no token count`.

The claude-cli executor's count_tokens method returns 501 for Claude Messages token counting. See also docs/claude-subscription.md.

## Regeneration and checks

Run from the repository root (Rust and Python 3.11+; no Go or upstream checkout needed):

```sh
python .github/scripts/compatibility.py
python .github/scripts/compatibility.py --check
python -I .github/scripts/test_compatibility.py
```

The small hand-kept mapping is [compatibility-evidence.json](../.github/scripts/compatibility-evidence.json).
Generation and checking validate each source declaration, compile the referenced crate's test harness, require its exact test name, and run it with one pass and zero ignored tests.
Missing, renamed, ignored, failing, or conditionally absent tests fail the check. Explicit refusal snippets must still exist, and every dimension and evidence ID is validated.
Required CI runs these checks in the existing tests job, alongside the workspace tests; it also checks the committed document byte for byte.
Translation-only parity suites are not promoted to provider/credential/transport evidence. Their separate results remain in the [parity tool](../tools/parity/README.md).
Extending coverage means reviewing and mapping an actual test, then regenerating; implemented combinations without mapped assertions remain unknown.
