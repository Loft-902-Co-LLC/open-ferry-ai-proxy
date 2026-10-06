//! The translators under test, run on our side, and how to read each one's
//! output as JSON so ours and upstream's can be compared.

use std::cell::OnceCell;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use open_ferry_translate::claude::gemini::{
    ClaudeToGeminiStream, convert_claude_response_to_gemini_non_stream,
    convert_gemini_request_to_claude,
};
use open_ferry_translate::claude::openai::chat_completions::{
    ClaudeToOpenAIChatCompletionsStream,
    convert_claude_response_to_openai_chat_completions_non_stream,
    convert_openai_chat_completions_request_to_claude,
    convert_openai_chat_completions_request_to_claude_with_compat,
};
use open_ferry_translate::claude::openai::responses::{
    ClaudeToOpenAIResponsesStream, convert_claude_response_to_openai_responses_non_stream,
    convert_openai_responses_request_to_claude,
    convert_openai_responses_request_to_claude_with_compat,
};
use open_ferry_translate::codex::claude::{
    CodexToClaudeStream, convert_claude_request_to_codex,
    convert_claude_request_to_codex_with_compat, convert_codex_response_to_claude_non_stream,
};
use open_ferry_translate::codex::gemini::{
    CodexToGeminiStream, convert_codex_response_to_gemini_non_stream,
    convert_gemini_request_to_codex,
};
use open_ferry_translate::codex::openai::chat_completions::{
    CodexToOpenAIChatCompletionsStream,
    convert_codex_response_to_openai_chat_completions_non_stream,
    convert_openai_chat_completions_request_to_codex,
};
use open_ferry_translate::codex::openai::responses::{
    CodexToOpenAIResponsesStream, convert_codex_response_to_openai_responses_non_stream,
    convert_openai_responses_request_to_codex,
};
use open_ferry_translate::completions::{
    convert_chat_completions_response_to_completions,
    convert_chat_completions_stream_chunk_to_completions,
    convert_completions_request_to_chat_completions,
};
use open_ferry_translate::gemini::claude::{
    GeminiToClaudeStream, convert_claude_request_to_gemini,
    convert_claude_request_to_gemini_with_compat, convert_gemini_response_to_claude_non_stream,
};
use open_ferry_translate::gemini::gemini::{
    convert_gemini_request_to_gemini, passthrough_gemini_response_non_stream,
    passthrough_gemini_response_stream,
};
use open_ferry_translate::gemini::openai::chat_completions::{
    GeminiToOpenAIStream, convert_gemini_response_to_openai_non_stream,
    convert_openai_request_to_gemini,
};
use open_ferry_translate::gemini::openai::responses::{
    GeminiToOpenAIResponsesStream, convert_gemini_response_to_openai_responses_non_stream,
    convert_openai_responses_request_to_gemini,
};
use open_ferry_translate::json::exact;
use open_ferry_translate::models::ModelCatalog;
use open_ferry_translate::openai::chat_completions::{
    OpenAIToOpenAIStream, convert_openai_request_to_openai,
    convert_openai_response_to_openai_non_stream,
};
use open_ferry_translate::openai::claude::{
    OpenAIToClaudeStream, convert_claude_request_to_openai,
    convert_claude_request_to_openai_with_compat, convert_openai_response_to_claude_non_stream,
};
use open_ferry_translate::openai::gemini::{
    OpenAIToGeminiStream, convert_gemini_request_to_openai,
    convert_openai_response_to_gemini_non_stream,
};
use open_ferry_translate::openai::responses::{
    OpenAIToOpenAIResponsesStream,
    convert_openai_chat_completions_response_to_openai_responses_non_stream,
    convert_openai_responses_request_to_openai_chat_completions,
};
use open_ferry_translate::registry::{Format, Registry, ResponseContext, ResponseTransform};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::cases::Case;
use crate::codex_models;
use crate::compare::{self, Deviation, JsonAt, JsonForm, Numbers};
use crate::config_diff;
use crate::config_save;
use crate::interactions::{self, Stage};
use crate::multi_agent;
use crate::payload;
use crate::raw_json::{self, Raw};
use crate::signature;
use crate::ttft;
use crate::usage;

/// How an empty non-streaming output reads, unlike any JSON a response holds.
pub(crate) const NO_OUTPUT: &str = "(no output)";

/// What a registry non-streaming case reads as when the translation failed:
/// upstream's registry returned nil, and ours `None`.
const FAILED: &str = "(failed)";

/// How the Responses stream translator's harness writes a line it returned unchanged.
pub(crate) const UNCHANGED: &str = "=";

/// What a response's `created` or `created_at` is replaced with when it is
/// the current time, which upstream and we each read from the clock.
pub(crate) const CREATED_NOW: &str = "(now)";

/// What the clock time and count in a response ID made up for a response
/// without one are replaced with (see [`mask_generated_response_id`]).
const GENERATED_RESPONSE_ID: &str = "(generated)";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Translator {
    /// Claude Messages request → Codex request.
    Request,
    /// The same in compatibility mode, which keeps more thinking blocks.
    RequestCompat,
    /// Codex event stream → Claude SSE events.
    Stream,
    /// The final Codex event → one Claude message.
    NonStream,
    /// OpenAI Responses request → Codex request.
    ResponsesRequest,
    /// Codex event stream → Responses events.
    ResponsesStream,
    /// The final Codex event → one Responses response.
    ResponsesNonStream,
    /// OpenAI Chat Completions request → Codex request.
    ChatRequest,
    /// Codex event stream → Chat Completions chunks.
    ChatStream,
    /// The final Codex event → one Chat Completions response.
    ChatNonStream,
    /// OpenAI Chat Completions request → Claude Messages request.
    ClaudeChatRequest,
    /// The same in compatibility mode, which keeps assistant reasoning.
    ClaudeChatRequestCompat,
    /// Claude event stream → Chat Completions chunks.
    ClaudeChatStream,
    /// A whole Claude event stream → one Chat Completions response.
    ClaudeChatNonStream,
    /// OpenAI Responses request → Claude Messages request.
    ClaudeResponsesRequest,
    /// The same in compatibility mode, which keeps more thinking blocks.
    ClaudeResponsesRequestCompat,
    /// Claude event stream → Responses events.
    ClaudeResponsesStream,
    /// A whole Claude event stream → one Responses response.
    ClaudeResponsesNonStream,
    /// OpenAI Responses request → OpenAI Chat Completions request.
    OpenAIResponsesRequest,
    /// Chat Completions stream → Responses events.
    OpenAIResponsesStream,
    /// A whole Chat Completions response → one Responses response.
    OpenAIResponsesNonStream,
    /// Claude Messages request → OpenAI Chat Completions request.
    OpenAIClaudeRequest,
    /// The same in compatibility mode, which passes on all thinking.
    OpenAIClaudeRequestCompat,
    /// Chat Completions stream → Claude SSE events, or a Claude message per
    /// chunk for a client that didn't ask for a stream.
    OpenAIClaudeStream,
    /// A whole Chat Completions response → one Claude message.
    OpenAIClaudeNonStream,
    /// Chat Completions request → Chat Completions request, its model replaced.
    OpenAIChatRequest,
    /// Chat Completions stream → the same chunks, passed through.
    OpenAIChatStream,
    /// A whole Chat Completions response, passed through.
    OpenAIChatNonStream,
    /// Gemini request → Codex request.
    CodexGeminiRequest,
    /// Codex event stream → Gemini responses.
    CodexGeminiStream,
    /// The final Codex event → one Gemini response.
    CodexGeminiNonStream,
    /// Gemini request → Claude Messages request.
    ClaudeGeminiRequest,
    /// Claude event stream → Gemini responses.
    ClaudeGeminiStream,
    /// A whole Claude event stream → one Gemini response.
    ClaudeGeminiNonStream,
    /// Gemini request → Chat Completions request.
    OpenAIGeminiRequest,
    /// Chat Completions stream → Gemini responses.
    OpenAIGeminiStream,
    /// A whole Chat Completions response → one Gemini response.
    OpenAIGeminiNonStream,
    /// Gemini request → Gemini request, normalized.
    GeminiGeminiRequest,
    /// Gemini stream → the same payloads, passed through.
    GeminiGeminiStream,
    /// A whole Gemini response, passed through.
    GeminiGeminiNonStream,
    /// Claude Messages request → Gemini request.
    GeminiClaudeRequest,
    /// The same in compatibility mode, which keeps thinking blocks.
    GeminiClaudeRequestCompat,
    /// Gemini stream → Claude SSE events.
    GeminiClaudeStream,
    /// A whole Gemini response → one Claude message.
    GeminiClaudeNonStream,
    /// OpenAI Chat Completions request → Gemini request.
    GeminiChatRequest,
    /// Gemini stream → Chat Completions chunks.
    GeminiChatStream,
    /// A whole Gemini response → one Chat Completions response.
    GeminiChatNonStream,
    /// One reasoning signature → every check and replay decision on it.
    SignatureInspect,
    /// A Claude Messages request → its signed history stripped and sanitized.
    ClaudeMessagesSignatures,
    /// A Gemini request → its thought signatures sanitized and validated.
    GeminiSignatures,
    /// A request through the translator registry, between the pair of
    /// formats in the case's options (see [`registry_formats`]).
    RegistryRequest,
    /// A provider's event stream through the translator registry.
    RegistryStream,
    /// A provider's whole response through the translator registry.
    RegistryNonStream,
    /// Which translators the registry has for a pair, and a token count.
    RegistryLookup,
    /// Legacy Completions request → Chat Completions request.
    CompletionsRequest,
    /// A Chat Completions response → a legacy Completions response.
    CompletionsResponse,
    /// Chat Completions stream chunks → legacy Completions chunks, each one
    /// on its own.
    CompletionsStreamChunk,
    /// OpenAI Responses request → Gemini request.
    GeminiResponsesRequest,
    /// Gemini stream → Responses events.
    GeminiResponsesStream,
    /// A whole Gemini response → one Responses response.
    GeminiResponsesNonStream,
    /// A Gemini Interactions translator, whose methods delegate to its
    /// family's (see [`crate::interactions`]).
    Interactions(interactions::Kind),
    /// A request translated for Codex or Responses → its thinking setting
    /// applied, for the model in the case's options (see
    /// [`crate::cases::thinking`]).
    ThinkingCodex,
    /// A request translated for Chat Completions → its thinking setting
    /// applied.
    ThinkingOpenAI,
    /// Registered models → the Codex client model list, summarized (see
    /// `go/parity_codex_models.go`).
    CodexModels,
    /// A Codex client's Responses request → its collaboration tools readied
    /// at the Responses API boundary (see `go/parity_multi_agent.go`).
    MultiAgentPrepare,
    /// A Codex client's Responses request → as the Codex executor sends it
    /// to another upstream, and whether its namespace was renamed.
    MultiAgentOptimize,
    /// A Codex client's Responses request → its agent messages rewritten for
    /// another format or a compatibility model.
    MultiAgentInput,
    /// A Codex sub-agent's Responses request → its orphan delegation outputs
    /// as user messages.
    MultiAgentOrphan,
    /// An upstream's event → the optimized namespace renamed back, as text.
    MultiAgentRestore,
    /// A translated body and payload rules → the body with the rules
    /// applied, and the tracked paths they touched (see
    /// `go/helps/parity_payload.go`).
    Payload,
    /// An upstream's response body or stream line → the usage parsed from
    /// it (see `go/helps/parity_usage.go`).
    Usage,
    /// An upstream's stream event → whether it carries the first token
    /// (see `go/helps/parity_ttft.go`).
    Ttft,
    /// Two configs → the change details logged on reload (see
    /// `go/parity_config_diff.go`).
    ConfigDiff,
    /// A config file and writes to it → the file after each write (see
    /// `go/parity_config_save.go`).
    ConfigSave,
}

impl Translator {
    /// The harness's name for the translator (see `go/main.go`, and
    /// `go/completions/main.go` for the `completions/` ones).
    pub fn key(self) -> &'static str {
        match self {
            Self::Request => "codex/claude/request",
            Self::RequestCompat => "codex/claude/request-compat",
            Self::Stream => "codex/claude/response",
            Self::NonStream => "codex/claude/response-non-stream",
            Self::ResponsesRequest => "codex/openai-responses/request",
            Self::ResponsesStream => "codex/openai-responses/response",
            Self::ResponsesNonStream => "codex/openai-responses/response-non-stream",
            Self::ChatRequest => "codex/openai-chat/request",
            Self::ChatStream => "codex/openai-chat/response",
            Self::ChatNonStream => "codex/openai-chat/response-non-stream",
            Self::ClaudeChatRequest => "claude/openai-chat/request",
            Self::ClaudeChatRequestCompat => "claude/openai-chat/request-compat",
            Self::ClaudeChatStream => "claude/openai-chat/response",
            Self::ClaudeChatNonStream => "claude/openai-chat/response-non-stream",
            Self::ClaudeResponsesRequest => "claude/openai-responses/request",
            Self::ClaudeResponsesRequestCompat => "claude/openai-responses/request-compat",
            Self::ClaudeResponsesStream => "claude/openai-responses/response",
            Self::ClaudeResponsesNonStream => "claude/openai-responses/response-non-stream",
            Self::OpenAIResponsesRequest => "openai/openai-responses/request",
            Self::OpenAIResponsesStream => "openai/openai-responses/response",
            Self::OpenAIResponsesNonStream => "openai/openai-responses/response-non-stream",
            Self::OpenAIClaudeRequest => "openai/claude/request",
            Self::OpenAIClaudeRequestCompat => "openai/claude/request-compat",
            Self::OpenAIClaudeStream => "openai/claude/response",
            Self::OpenAIClaudeNonStream => "openai/claude/response-non-stream",
            Self::OpenAIChatRequest => "openai/openai-chat/request",
            Self::OpenAIChatStream => "openai/openai-chat/response",
            Self::OpenAIChatNonStream => "openai/openai-chat/response-non-stream",
            Self::CodexGeminiRequest => "codex/gemini/request",
            Self::CodexGeminiStream => "codex/gemini/response",
            Self::CodexGeminiNonStream => "codex/gemini/response-non-stream",
            Self::ClaudeGeminiRequest => "claude/gemini/request",
            Self::ClaudeGeminiStream => "claude/gemini/response",
            Self::ClaudeGeminiNonStream => "claude/gemini/response-non-stream",
            Self::OpenAIGeminiRequest => "openai/gemini/request",
            Self::OpenAIGeminiStream => "openai/gemini/response",
            Self::OpenAIGeminiNonStream => "openai/gemini/response-non-stream",
            Self::GeminiGeminiRequest => "gemini/gemini/request",
            Self::GeminiGeminiStream => "gemini/gemini/response",
            Self::GeminiGeminiNonStream => "gemini/gemini/response-non-stream",
            Self::GeminiClaudeRequest => "gemini/claude/request",
            Self::GeminiClaudeRequestCompat => "gemini/claude/request-compat",
            Self::GeminiClaudeStream => "gemini/claude/response",
            Self::GeminiClaudeNonStream => "gemini/claude/response-non-stream",
            Self::GeminiChatRequest => "gemini/openai-chat/request",
            Self::GeminiChatStream => "gemini/openai-chat/response",
            Self::GeminiChatNonStream => "gemini/openai-chat/response-non-stream",
            Self::SignatureInspect => "signature/inspect",
            Self::ClaudeMessagesSignatures => "signature/claude-messages",
            Self::GeminiSignatures => "signature/gemini",
            Self::RegistryRequest => "registry/request",
            Self::RegistryStream => "registry/response",
            Self::RegistryNonStream => "registry/response-non-stream",
            Self::RegistryLookup => "registry/lookup",
            Self::CompletionsRequest => "completions/request",
            Self::CompletionsResponse => "completions/response",
            Self::CompletionsStreamChunk => "completions/stream-chunk",
            Self::GeminiResponsesRequest => "gemini/openai-responses/request",
            Self::GeminiResponsesStream => "gemini/openai-responses/response",
            Self::GeminiResponsesNonStream => "gemini/openai-responses/response-non-stream",
            Self::Interactions(kind) => kind.key(),
            Self::ThinkingCodex => "thinking/codex",
            Self::ThinkingOpenAI => "thinking/openai",
            Self::CodexModels => "codex-models/list",
            Self::MultiAgentPrepare => "multi-agent/prepare",
            Self::MultiAgentOptimize => "multi-agent/optimize",
            Self::MultiAgentInput => "multi-agent/input",
            Self::MultiAgentOrphan => "multi-agent/orphan",
            Self::MultiAgentRestore => "multi-agent/restore",
            Self::Payload => "payload/apply",
            Self::Usage => "usage/parse",
            Self::Ttft => "ttft/token-event",
            Self::ConfigDiff => "config-diff/details",
            Self::ConfigSave => "config-save/steps",
        }
    }

    /// A short name for directories.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Request => "claude-request",
            Self::RequestCompat => "claude-request-compat",
            Self::Stream => "claude-stream",
            Self::NonStream => "claude-non-stream",
            Self::ResponsesRequest => "responses-request",
            Self::ResponsesStream => "responses-stream",
            Self::ResponsesNonStream => "responses-non-stream",
            Self::ChatRequest => "chat-request",
            Self::ChatStream => "chat-stream",
            Self::ChatNonStream => "chat-non-stream",
            Self::ClaudeChatRequest => "chat-to-claude-request",
            Self::ClaudeChatRequestCompat => "chat-to-claude-request-compat",
            Self::ClaudeChatStream => "claude-to-chat-stream",
            Self::ClaudeChatNonStream => "claude-to-chat-non-stream",
            Self::ClaudeResponsesRequest => "responses-to-claude-request",
            Self::ClaudeResponsesRequestCompat => "responses-to-claude-request-compat",
            Self::ClaudeResponsesStream => "claude-to-responses-stream",
            Self::ClaudeResponsesNonStream => "claude-to-responses-non-stream",
            Self::OpenAIResponsesRequest => "responses-to-chat-request",
            Self::OpenAIResponsesStream => "chat-to-responses-stream",
            Self::OpenAIResponsesNonStream => "chat-to-responses-non-stream",
            Self::OpenAIClaudeRequest => "claude-to-chat-request",
            Self::OpenAIClaudeRequestCompat => "claude-to-chat-request-compat",
            Self::OpenAIClaudeStream => "chat-to-claude-stream",
            Self::OpenAIClaudeNonStream => "chat-to-claude-non-stream",
            Self::OpenAIChatRequest => "chat-to-chat-request",
            Self::OpenAIChatStream => "chat-to-chat-stream",
            Self::OpenAIChatNonStream => "chat-to-chat-non-stream",
            Self::CodexGeminiRequest => "gemini-to-codex-request",
            Self::CodexGeminiStream => "codex-to-gemini-stream",
            Self::CodexGeminiNonStream => "codex-to-gemini-non-stream",
            Self::ClaudeGeminiRequest => "gemini-to-claude-request",
            Self::ClaudeGeminiStream => "claude-to-gemini-stream",
            Self::ClaudeGeminiNonStream => "claude-to-gemini-non-stream",
            Self::OpenAIGeminiRequest => "gemini-to-chat-request",
            Self::OpenAIGeminiStream => "chat-to-gemini-stream",
            Self::OpenAIGeminiNonStream => "chat-to-gemini-non-stream",
            Self::GeminiGeminiRequest => "gemini-to-gemini-request",
            Self::GeminiGeminiStream => "gemini-to-gemini-stream",
            Self::GeminiGeminiNonStream => "gemini-to-gemini-non-stream",
            Self::GeminiClaudeRequest => "claude-to-gemini-request",
            Self::GeminiClaudeRequestCompat => "claude-to-gemini-request-compat",
            Self::GeminiClaudeStream => "gemini-to-claude-stream",
            Self::GeminiClaudeNonStream => "gemini-to-claude-non-stream",
            Self::GeminiChatRequest => "chat-to-gemini-request",
            Self::GeminiChatStream => "gemini-to-chat-stream",
            Self::GeminiChatNonStream => "gemini-to-chat-non-stream",
            Self::SignatureInspect => "signature-inspect",
            Self::ClaudeMessagesSignatures => "signature-claude-messages",
            Self::GeminiSignatures => "signature-gemini",
            Self::RegistryRequest => "registry-request",
            Self::RegistryStream => "registry-stream",
            Self::RegistryNonStream => "registry-non-stream",
            Self::RegistryLookup => "registry-lookup",
            Self::CompletionsRequest => "completions-request",
            Self::CompletionsResponse => "completions-response",
            Self::CompletionsStreamChunk => "completions-stream-chunk",
            Self::GeminiResponsesRequest => "responses-to-gemini-request",
            Self::GeminiResponsesStream => "gemini-to-responses-stream",
            Self::GeminiResponsesNonStream => "gemini-to-responses-non-stream",
            Self::Interactions(kind) => kind.slug(),
            Self::ThinkingCodex => "thinking-codex",
            Self::ThinkingOpenAI => "thinking-openai",
            Self::CodexModels => "codex-models",
            Self::MultiAgentPrepare => "multi-agent-prepare",
            Self::MultiAgentOptimize => "multi-agent-optimize",
            Self::MultiAgentInput => "multi-agent-input",
            Self::MultiAgentOrphan => "multi-agent-orphan",
            Self::MultiAgentRestore => "multi-agent-restore",
            Self::Payload => "payload",
            Self::Usage => "usage",
            Self::Ttft => "ttft",
            Self::ConfigDiff => "config-diff",
            Self::ConfigSave => "config-save",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Request => "Claude -> Codex request",
            Self::RequestCompat => "Claude -> Codex request, compatibility mode",
            Self::Stream => "Codex -> Claude response, streaming",
            Self::NonStream => "Codex -> Claude response, non-streaming",
            Self::ResponsesRequest => "Responses -> Codex request",
            Self::ResponsesStream => "Codex -> Responses response, streaming",
            Self::ResponsesNonStream => "Codex -> Responses response, non-streaming",
            Self::ChatRequest => "Chat Completions -> Codex request",
            Self::ChatStream => "Codex -> Chat Completions response, streaming",
            Self::ChatNonStream => "Codex -> Chat Completions response, non-streaming",
            Self::ClaudeChatRequest => "Chat Completions -> Claude request",
            Self::ClaudeChatRequestCompat => {
                "Chat Completions -> Claude request, compatibility mode"
            }
            Self::ClaudeChatStream => "Claude -> Chat Completions response, streaming",
            Self::ClaudeChatNonStream => "Claude -> Chat Completions response, non-streaming",
            Self::ClaudeResponsesRequest => "Responses -> Claude request",
            Self::ClaudeResponsesRequestCompat => "Responses -> Claude request, compatibility mode",
            Self::ClaudeResponsesStream => "Claude -> Responses response, streaming",
            Self::ClaudeResponsesNonStream => "Claude -> Responses response, non-streaming",
            Self::OpenAIResponsesRequest => "Responses -> Chat Completions request",
            Self::OpenAIResponsesStream => "Chat Completions -> Responses response, streaming",
            Self::OpenAIResponsesNonStream => {
                "Chat Completions -> Responses response, non-streaming"
            }
            Self::OpenAIClaudeRequest => "Claude -> Chat Completions request",
            Self::OpenAIClaudeRequestCompat => {
                "Claude -> Chat Completions request, compatibility mode"
            }
            Self::OpenAIClaudeStream => "Chat Completions -> Claude response, streaming",
            Self::OpenAIClaudeNonStream => "Chat Completions -> Claude response, non-streaming",
            Self::OpenAIChatRequest => "Chat Completions passthrough request",
            Self::OpenAIChatStream => "Chat Completions passthrough response, streaming",
            Self::OpenAIChatNonStream => "Chat Completions passthrough response, non-streaming",
            Self::CodexGeminiRequest => "Gemini -> Codex request",
            Self::CodexGeminiStream => "Codex -> Gemini response, streaming",
            Self::CodexGeminiNonStream => "Codex -> Gemini response, non-streaming",
            Self::ClaudeGeminiRequest => "Gemini -> Claude request",
            Self::ClaudeGeminiStream => "Claude -> Gemini response, streaming",
            Self::ClaudeGeminiNonStream => "Claude -> Gemini response, non-streaming",
            Self::OpenAIGeminiRequest => "Gemini -> Chat Completions request",
            Self::OpenAIGeminiStream => "Chat Completions -> Gemini response, streaming",
            Self::OpenAIGeminiNonStream => "Chat Completions -> Gemini response, non-streaming",
            Self::GeminiGeminiRequest => "Gemini -> Gemini request",
            Self::GeminiGeminiStream => "Gemini passthrough response, streaming",
            Self::GeminiGeminiNonStream => "Gemini passthrough response, non-streaming",
            Self::GeminiClaudeRequest => "Claude -> Gemini request",
            Self::GeminiClaudeRequestCompat => "Claude -> Gemini request, compatibility mode",
            Self::GeminiClaudeStream => "Gemini -> Claude response, streaming",
            Self::GeminiClaudeNonStream => "Gemini -> Claude response, non-streaming",
            Self::GeminiChatRequest => "Chat Completions -> Gemini request",
            Self::GeminiChatStream => "Gemini -> Chat Completions response, streaming",
            Self::GeminiChatNonStream => "Gemini -> Chat Completions response, non-streaming",
            Self::SignatureInspect => "Signature checks and replay decisions",
            Self::ClaudeMessagesSignatures => "Claude Messages signature sanitizers",
            Self::GeminiSignatures => "Gemini thought signature sanitizer and validators",
            Self::RegistryRequest => "Translator registry, requests",
            Self::RegistryStream => "Translator registry, streaming responses",
            Self::RegistryNonStream => "Translator registry, non-streaming responses",
            Self::RegistryLookup => "Translator registry, lookups and token counts",
            Self::CompletionsRequest => "Completions -> Chat Completions request",
            Self::CompletionsResponse => "Chat Completions -> Completions response",
            Self::CompletionsStreamChunk => "Chat Completions -> Completions stream chunks",
            Self::GeminiResponsesRequest => "Responses -> Gemini request",
            Self::GeminiResponsesStream => "Gemini -> Responses response, streaming",
            Self::GeminiResponsesNonStream => "Gemini -> Responses response, non-streaming",
            Self::Interactions(kind) => kind.title(),
            Self::ThinkingCodex => "Thinking settings for Codex and Responses",
            Self::ThinkingOpenAI => "Thinking settings for Chat Completions",
            Self::CodexModels => "Codex client model list",
            Self::MultiAgentPrepare => "Codex multi-agent v2 tools readied",
            Self::MultiAgentOptimize => "Codex multi-agent v2 request optimized",
            Self::MultiAgentInput => "Codex agent messages for other formats",
            Self::MultiAgentOrphan => "Codex orphan delegation outputs",
            Self::MultiAgentRestore => "Codex multi-agent v2 namespace restored",
            Self::Payload => "Payload rules applied",
            Self::Usage => "Usage parsed from upstream responses",
            Self::Ttft => "First-token events",
            Self::ConfigDiff => "Config change details",
            Self::ConfigSave => "Config file writes",
        }
    }

    /// Runs our port on `case`, returning its output in the form [`Self::read`] gives.
    /// The request, and a registry case's translated request, are read with
    /// their numbers as the translator keeps them (see [`Self::numbers`]).
    pub fn run_rust(self, case: &Case) -> Result<Value, String> {
        let numbers = self.numbers();
        let request = numbers.parse(&case.request);
        let final_event = || {
            case.events
                .first()
                .and_then(|event| exact::from_str(event).ok())
                .unwrap_or_default()
        };
        match self {
            Self::Request => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_claude_request_to_codex(&case.model, &request))
            }
            Self::RequestCompat => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_claude_request_to_codex_with_compat(
                    &case.model,
                    &request,
                ))
            }
            Self::SignatureInspect => Ok(signature::inspect(
                &case.model,
                &case.request,
                &case.options,
            )),
            Self::ClaudeMessagesSignatures => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(signature::claude_messages(
                    &case.model,
                    &request,
                    &case.options,
                ))
            }
            Self::GeminiSignatures => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(signature::gemini(&request, &case.options))
            }
            Self::Stream => {
                let mut stream = CodexToClaudeStream::new(&request.unwrap_or_default());
                let output: String = case
                    .events
                    .iter()
                    .map(|line| stream.translate_line(line.as_bytes()))
                    .collect();
                Ok(self
                    .read(case, output.as_bytes())
                    .expect("streams always read"))
            }
            Self::NonStream => {
                let output = convert_codex_response_to_claude_non_stream(
                    &request.unwrap_or_default(),
                    &final_event(),
                );
                let output = output.map(|value| value.to_string()).unwrap_or_default();
                self.read(case, output.as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::ResponsesRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_openai_responses_request_to_codex(
                    &case.model,
                    request,
                ))
            }
            Self::ResponsesStream => {
                let translated = numbers.parse(&case.translated_request).unwrap_or_default();
                let stream = CodexToOpenAIResponsesStream::new(
                    &case.model,
                    &request.unwrap_or_default(),
                    &translated,
                );
                // Written as the harness writes upstream's output.
                let lines: Vec<String> = case
                    .events
                    .iter()
                    .map(|line| {
                        let output = stream.translate_line(line.as_bytes());
                        if *output == *line.as_bytes() {
                            UNCHANGED.to_owned()
                        } else {
                            String::from_utf8_lossy(&output).into_owned()
                        }
                    })
                    .collect();
                let output = serde_json::to_vec(&lines).expect("strings serialize");
                Ok(self.read(case, &output).expect("streams always read"))
            }
            Self::ResponsesNonStream => {
                let output = convert_codex_response_to_openai_responses_non_stream(final_event());
                let output = output.map(|value| value.to_string()).unwrap_or_default();
                self.read(case, output.as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::ChatRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_openai_chat_completions_request_to_codex(
                    &case.model,
                    &request,
                    true,
                ))
            }
            Self::ChatStream => {
                let mut stream = CodexToOpenAIChatCompletionsStream::new(
                    &case.model,
                    &request.unwrap_or_default(),
                );
                // Written as the harness writes upstream's output.
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .filter_map(|line| stream.translate_line(line.as_bytes()))
                    .map(|chunk| chunk.to_string())
                    .collect();
                let output = serde_json::to_vec(&chunks).expect("strings serialize");
                Ok(self.read(case, &output).expect("streams always read"))
            }
            Self::ChatNonStream => {
                let output = convert_codex_response_to_openai_chat_completions_non_stream(
                    &request.unwrap_or_default(),
                    &final_event(),
                );
                let output = output.map(|value| value.to_string()).unwrap_or_default();
                self.read(case, output.as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::ClaudeChatRequest | Self::ClaudeChatRequestCompat => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let convert = if self == Self::ClaudeChatRequest {
                    convert_openai_chat_completions_request_to_claude
                } else {
                    convert_openai_chat_completions_request_to_claude_with_compat
                };
                let output = convert(&case.model, &request, true, ModelCatalog::embedded());
                // Read back, so generated IDs are masked as upstream's are.
                Ok(self
                    .read(case, output.to_string().as_bytes())
                    .expect("requests always read"))
            }
            Self::ClaudeChatStream => {
                let mut stream = ClaudeToOpenAIChatCompletionsStream::new(&case.model);
                // Written as the harness writes upstream's output.
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .filter_map(|line| stream.translate_line(line.as_bytes()))
                    .map(|chunk| chunk.to_string())
                    .collect();
                let output = serde_json::to_vec(&chunks).expect("strings serialize");
                Ok(self.read(case, &output).expect("streams always read"))
            }
            Self::ClaudeChatNonStream => {
                let body = case.events.first().map_or(&b""[..], |body| body.as_bytes());
                let output = convert_claude_response_to_openai_chat_completions_non_stream(body);
                self.read(case, output.to_string().as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::ClaudeResponsesRequest | Self::ClaudeResponsesRequestCompat => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let convert = if self == Self::ClaudeResponsesRequest {
                    convert_openai_responses_request_to_claude
                } else {
                    convert_openai_responses_request_to_claude_with_compat
                };
                let output = convert(&case.model, &request, true, ModelCatalog::embedded());
                // Read back, so generated IDs are masked as upstream's are.
                Ok(self
                    .read(case, output.to_string().as_bytes())
                    .expect("requests always read"))
            }
            Self::ClaudeResponsesStream => {
                // An original request that isn't JSON counts as absent, as
                // upstream's pickRequestJSON skips it.
                let translated = numbers.parse(&case.translated_request).unwrap_or_default();
                let mut stream = ClaudeToOpenAIResponsesStream::new(
                    &case.model,
                    &request.unwrap_or_default(),
                    &translated,
                );
                // The SSE frames for every line, then those for the stream's end,
                // as the harness collects upstream's.
                let mut output: String = case
                    .events
                    .iter()
                    .map(|line| stream.translate_line(line.as_bytes()))
                    .collect();
                output.push_str(&stream.finalize_tool_input());
                Ok(self
                    .read(case, output.as_bytes())
                    .expect("streams always read"))
            }
            Self::OpenAIResponsesRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let output = convert_openai_responses_request_to_openai_chat_completions(
                    &case.model,
                    &request,
                    true,
                );
                // Read back, as upstream's output is.
                Ok(self
                    .read(case, output.to_string().as_bytes())
                    .expect("requests always read"))
            }
            Self::OpenAIResponsesStream => {
                // An original request that isn't JSON counts as absent, as
                // upstream's pickRequestJSON skips it.
                let translated = numbers.parse(&case.translated_request).unwrap_or_default();
                let mut stream = OpenAIToOpenAIResponsesStream::new(
                    &case.model,
                    &request.unwrap_or_default(),
                    &translated,
                );
                // The SSE frames for every line, then those for the stream's end,
                // as the harness collects upstream's.
                let mut output: String = case
                    .events
                    .iter()
                    .map(|line| stream.translate_line(line.as_bytes()))
                    .collect();
                output.push_str(&stream.finalize_tool_input());
                Ok(self
                    .read(case, output.as_bytes())
                    .expect("streams always read"))
            }
            Self::OpenAIResponsesNonStream => {
                let translated = numbers.parse(&case.translated_request).unwrap_or_default();
                let body = case.events.first().map_or(&b""[..], |body| body.as_bytes());
                let output =
                    convert_openai_chat_completions_response_to_openai_responses_non_stream(
                        &request.unwrap_or_default(),
                        &translated,
                        body,
                    );
                self.read(case, output.to_string().as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::OpenAIClaudeRequest | Self::OpenAIClaudeRequestCompat => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let convert = if self == Self::OpenAIClaudeRequest {
                    convert_claude_request_to_openai
                } else {
                    convert_claude_request_to_openai_with_compat
                };
                let stream = case.options["stream"].as_bool().unwrap_or(false);
                Ok(convert(&case.model, &request, stream))
            }
            Self::OpenAIClaudeStream => {
                let mut stream = OpenAIToClaudeStream::new(&request.unwrap_or_default());
                // Written as the harness writes upstream's output.
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .flat_map(|line| stream.translate_line(line.as_bytes()))
                    .collect();
                let output = serde_json::to_vec(&chunks).expect("strings serialize");
                Ok(self.read(case, &output).expect("streams always read"))
            }
            Self::OpenAIClaudeNonStream => {
                let output = convert_openai_response_to_claude_non_stream(
                    &request.unwrap_or_default(),
                    &final_event(),
                );
                self.read(case, output.to_string().as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::OpenAIChatRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_openai_request_to_openai(&case.model, request))
            }
            Self::OpenAIChatStream => {
                let mut stream = OpenAIToOpenAIStream::new();
                // Written as the harness writes upstream's output, empty
                // chunks included.
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .filter_map(|line| stream.translate_line(line.as_bytes()))
                    .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                    .collect();
                let output = serde_json::to_vec(&chunks).expect("strings serialize");
                Ok(self.read(case, &output).expect("streams always read"))
            }
            Self::OpenAIChatNonStream => {
                let body = case.events.first().map_or(&b""[..], |body| body.as_bytes());
                let output = convert_openai_response_to_openai_non_stream(body);
                Ok(self.read(case, output).expect("bodies always read"))
            }
            Self::CodexGeminiRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_gemini_request_to_codex(&case.model, &request))
            }
            Self::ClaudeGeminiRequest | Self::OpenAIGeminiRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let stream = case.options["stream"].as_bool().unwrap_or(false);
                let output = if self == Self::ClaudeGeminiRequest {
                    convert_gemini_request_to_claude(
                        &case.model,
                        &request,
                        stream,
                        ModelCatalog::embedded(),
                    )
                } else {
                    convert_gemini_request_to_openai(&case.model, &request, stream)
                };
                // Read back, so derived IDs become upstream's.
                Ok(self
                    .read(case, output.to_string().as_bytes())
                    .expect("requests always read"))
            }
            Self::CodexGeminiStream | Self::ClaudeGeminiStream | Self::OpenAIGeminiStream => {
                let original = request.unwrap_or_default();
                let mut translate: LineTranslator = match self {
                    Self::CodexGeminiStream => {
                        let mut stream = CodexToGeminiStream::new(&case.model, &original);
                        Box::new(move |line| stream.translate_line(line))
                    }
                    Self::ClaudeGeminiStream => {
                        let mut stream = ClaudeToGeminiStream::new(&case.model);
                        Box::new(move |line| stream.translate_line(line))
                    }
                    _ => {
                        let mut stream = OpenAIToGeminiStream::new();
                        Box::new(move |line| stream.translate_line(line))
                    }
                };
                // Written as the harness writes upstream's output.
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .flat_map(|line| translate(line.as_bytes()))
                    .map(|chunk| chunk.to_string())
                    .collect();
                let output = serde_json::to_vec(&chunks).expect("strings serialize");
                Ok(self.read(case, &output).expect("streams always read"))
            }
            Self::CodexGeminiNonStream => {
                let output = convert_codex_response_to_gemini_non_stream(
                    &case.model,
                    &request.unwrap_or_default(),
                    &final_event(),
                );
                let output = output.map(|value| value.to_string()).unwrap_or_default();
                self.read(case, output.as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::ClaudeGeminiNonStream | Self::OpenAIGeminiNonStream => {
                let body = case.events.first().map_or(&b""[..], |body| body.as_bytes());
                let output = if self == Self::ClaudeGeminiNonStream {
                    convert_claude_response_to_gemini_non_stream(&case.model, body)
                } else {
                    convert_openai_response_to_gemini_non_stream(body)
                };
                self.read(case, output.to_string().as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::GeminiGeminiRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let stream = case.options["stream"].as_bool().unwrap_or(false);
                Ok(convert_gemini_request_to_gemini(
                    &case.model,
                    request,
                    stream,
                ))
            }
            Self::GeminiGeminiStream => {
                // Written as the harness writes upstream's output, empty
                // chunks included.
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .filter_map(|line| passthrough_gemini_response_stream(line.as_bytes()))
                    .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                    .collect();
                let output = serde_json::to_vec(&chunks).expect("strings serialize");
                Ok(self.read(case, &output).expect("streams always read"))
            }
            Self::GeminiGeminiNonStream => {
                let body = case.events.first().map_or(&b""[..], |body| body.as_bytes());
                let output = passthrough_gemini_response_non_stream(body);
                Ok(self.read(case, output).expect("bodies always read"))
            }
            Self::GeminiClaudeRequest | Self::GeminiClaudeRequestCompat => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let convert = if self == Self::GeminiClaudeRequest {
                    convert_claude_request_to_gemini
                } else {
                    convert_claude_request_to_gemini_with_compat
                };
                let stream = case.options["stream"].as_bool().unwrap_or(false);
                Ok(convert(
                    &case.model,
                    &request,
                    stream,
                    ModelCatalog::embedded(),
                ))
            }
            Self::GeminiClaudeStream => {
                let mut stream = GeminiToClaudeStream::new(&request.unwrap_or_default());
                // The output for every line, joined, as the harness collects
                // upstream's.
                let output: String = case
                    .events
                    .iter()
                    .map(|line| stream.translate(line.as_bytes()))
                    .collect();
                Ok(self
                    .read(case, output.as_bytes())
                    .expect("streams always read"))
            }
            Self::GeminiClaudeNonStream => {
                let output = convert_gemini_response_to_claude_non_stream(
                    &request.unwrap_or_default(),
                    &final_event(),
                );
                self.read(case, output.to_string().as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::GeminiChatRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let stream = case.options["stream"].as_bool().unwrap_or(false);
                Ok(convert_openai_request_to_gemini(
                    &case.model,
                    &request,
                    stream,
                ))
            }
            Self::GeminiChatStream => {
                let mut stream = GeminiToOpenAIStream::new(&request.unwrap_or_default());
                // Written as the harness writes upstream's output.
                let chunks: Vec<String> = case
                    .events
                    .iter()
                    .flat_map(|line| stream.translate(line.as_bytes()))
                    .map(|chunk| chunk.to_string())
                    .collect();
                let output = serde_json::to_vec(&chunks).expect("strings serialize");
                Ok(self.read(case, &output).expect("streams always read"))
            }
            Self::GeminiChatNonStream => {
                let output = convert_gemini_response_to_openai_non_stream(
                    &request.unwrap_or_default(),
                    &final_event(),
                );
                self.read(case, output.to_string().as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::RegistryRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let (from, to) = registry_formats(case);
                let stream = case.options["stream"].as_bool().unwrap_or(false);
                let output = if case.options["identity"].as_bool().unwrap_or(false) {
                    let registry = Registry::new();
                    registry.register(
                        from.clone(),
                        to.clone(),
                        Some(Arc::new(|_, body, _| body)),
                        ResponseTransform::default(),
                    );
                    registry.translate_request(&from, &to, &case.model, request, stream)
                } else {
                    Registry::global().translate_request(&from, &to, &case.model, request, stream)
                };
                // Read back, so generated IDs are masked as upstream's are.
                Ok(self
                    .read(case, output.to_string().as_bytes())
                    .expect("requests always read"))
            }
            Self::RegistryStream => {
                let (from, to) = registry_formats(case);
                let original = request.unwrap_or_default();
                let translated = numbers.parse(&case.translated_request).unwrap_or_default();
                let context = ResponseContext {
                    model: &case.model,
                    original_request: &original,
                    request: &translated,
                };
                let mut stream = Registry::global().response_stream(&from, &to, &context);
                let text = |chunks: Vec<Vec<u8>>| -> Vec<String> {
                    chunks
                        .iter()
                        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                        .collect()
                };
                // Written as the harness writes upstream's output.
                let events: Vec<Vec<String>> = case
                    .events
                    .iter()
                    .map(|line| text(stream.translate(line.as_bytes())))
                    .collect();
                let finish = text(stream.finish());
                let report = json!({
                    "events": events,
                    "finish": finish,
                    "failed": stream.tool_input_error().is_some(),
                });
                Ok(self
                    .read(case, report.to_string().as_bytes())
                    .expect("streams always read"))
            }
            Self::RegistryNonStream => {
                let (from, to) = registry_formats(case);
                let original = request.unwrap_or_default();
                let translated = numbers.parse(&case.translated_request).unwrap_or_default();
                let context = ResponseContext {
                    model: &case.model,
                    original_request: &original,
                    request: &translated,
                };
                let body = case
                    .events
                    .first()
                    .map(|body| body.as_bytes().to_vec())
                    .unwrap_or_default();
                let output = Registry::global()
                    .translate_non_stream(&from, &to, &context, body)
                    .map(|output| String::from_utf8_lossy(&output).into_owned());
                self.read(case, json!({ "output": output }).to_string().as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::RegistryLookup => {
                let (from, to) = registry_formats(case);
                let registry = Registry::global();
                let count = case.options["count"].as_i64().unwrap_or(0);
                let body = case.request.as_bytes().to_vec();
                let token_count = registry.translate_token_count(&from, &to, count, body);
                Ok(json!({
                    "request": registry.has_request_transformer(&from, &to),
                    "response": registry.has_response_transformer(&from, &to),
                    "stream": registry.has_stream_response_transformer(&from, &to),
                    "non_stream": registry.has_non_stream_response_transformer(&from, &to),
                    "token_count": String::from_utf8_lossy(&token_count),
                }))
            }
            Self::ClaudeResponsesNonStream => {
                let translated = numbers.parse(&case.translated_request).unwrap_or_default();
                let body = case.events.first().map_or(&b""[..], |body| body.as_bytes());
                let output = convert_claude_response_to_openai_responses_non_stream(
                    &request.unwrap_or_default(),
                    &translated,
                    body,
                );
                let output = match output {
                    Value::Null => String::new(),
                    output => output.to_string(),
                };
                self.read(case, output.as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::CompletionsRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                Ok(convert_completions_request_to_chat_completions(&request))
            }
            Self::CompletionsResponse => {
                let body = case.events.first().map_or(&b""[..], |body| body.as_bytes());
                Ok(convert_chat_completions_response_to_completions(body))
            }
            Self::CompletionsStreamChunk => {
                // Written as the harness writes upstream's output.
                let chunks: Vec<Option<String>> = case
                    .events
                    .iter()
                    .map(|chunk| {
                        convert_chat_completions_stream_chunk_to_completions(chunk.as_bytes())
                            .map(|chunk| chunk.to_string())
                    })
                    .collect();
                let output = serde_json::to_vec(&chunks).expect("strings serialize");
                Ok(self.read(case, &output).expect("chunks always read"))
            }
            Self::GeminiResponsesRequest => {
                let request = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let output =
                    convert_openai_responses_request_to_gemini(&case.model, &request, true);
                // Read back, as upstream's output is.
                Ok(self
                    .read(case, output.to_string().as_bytes())
                    .expect("requests always read"))
            }
            Self::GeminiResponsesStream => {
                // An original request that isn't JSON counts as absent, as
                // upstream's pickRequestJSON skips it.
                let translated = numbers.parse(&case.translated_request).unwrap_or_default();
                let mut stream = GeminiToOpenAIResponsesStream::new(
                    &case.model,
                    &request.unwrap_or_default(),
                    &translated,
                );
                // The SSE frames for every line, then those for the stream's end,
                // as the harness collects upstream's.
                let mut output: String = case
                    .events
                    .iter()
                    .map(|line| stream.translate_line(line.as_bytes()))
                    .collect();
                output.push_str(&stream.finalize_tool_input());
                Ok(self
                    .read(case, output.as_bytes())
                    .expect("streams always read"))
            }
            Self::GeminiResponsesNonStream => {
                let translated = numbers.parse(&case.translated_request).unwrap_or_default();
                let body = case.events.first().map_or(&b""[..], |body| body.as_bytes());
                let output = convert_gemini_response_to_openai_responses_non_stream(
                    &request.unwrap_or_default(),
                    &translated,
                    body,
                );
                let output = output.map(|value| value.to_string()).unwrap_or_default();
                self.read(case, output.as_bytes())
                    .ok_or_else(|| "output is not JSON".to_owned())
            }
            Self::Interactions(kind) => kind.run(case),
            Self::ThinkingCodex | Self::ThinkingOpenAI => {
                let mut body = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let apply = if self == Self::ThinkingCodex {
                    open_ferry_providers::codex::thinking::apply_with_model_info
                } else {
                    open_ferry_providers::openai_compat::thinking::apply_with_model_info
                };
                let option = |key: &str| case.options[key].as_str().unwrap_or_default();
                let error = apply(
                    &mut body,
                    option("source").as_bytes(),
                    &case.model,
                    option("from"),
                    option("to"),
                    option("provider"),
                    crate::cases::thinking::model_info(&case.options["model_info"]),
                )
                .err();
                Ok(object([("body", body), ("error", json!(error))]))
            }
            Self::CodexModels => Ok(codex_models::list(&case.options)),
            Self::MultiAgentPrepare
            | Self::MultiAgentOptimize
            | Self::MultiAgentInput
            | Self::MultiAgentOrphan => {
                let body = request
                    .map_err(|err| format!("case {} is not valid JSON: {err}", case.name))?;
                let run = match self {
                    Self::MultiAgentPrepare => multi_agent::prepare,
                    Self::MultiAgentOptimize => multi_agent::optimize,
                    Self::MultiAgentInput => multi_agent::input,
                    _ => multi_agent::orphan,
                };
                Ok(run(body, &case.options))
            }
            Self::MultiAgentRestore => Ok(multi_agent::restore(&case.request, &case.options)),
            Self::Payload => payload::apply(case),
            Self::Usage => usage::parse(case),
            Self::Ttft => ttft::token_event(case),
            Self::ConfigDiff => config_diff::details(case),
            Self::ConfigSave => config_save::steps(case),
        }
    }

    /// How this translator keeps the numbers in JSON it reads, which is how
    /// its cases' requests are read for it and JSON in its output's strings is
    /// compared (see [`Numbers`]): exactly as written, for every suite, as
    /// the proxy hands each translator the client's JSON. Each suite's `read`
    /// keeps them as written too.
    pub fn numbers(self) -> Numbers {
        Numbers::AsWritten
    }

    /// Where this translator writes JSON it read compactly while upstream
    /// copies the JSON's text, and the form it takes there. Each place is a
    /// documented deviation (see UPSTREAM.md and the ported module's docs).
    /// JSON in any other string must match upstream's exactly.
    pub fn embedded_json(self, case: &Case) -> &'static [JsonAt] {
        use JsonForm::{GoEscaped, InText, Whole};
        match self {
            // Those of the translator the registry runs.
            Self::RegistryRequest | Self::RegistryStream | Self::RegistryNonStream => self
                .native(case)
                .map_or(&[], |native| native.embedded_json(case)),
            // Function call arguments, a tool result that falls back to its
            // raw content, and a text that isn't a string, also when a system
            // reminder wraps it.
            Self::Request | Self::RequestCompat => &[
                ("$.input[*].arguments", Whole),
                ("$.input[*].output", Whole),
                ("$.input[*].output[*].text", Whole),
                ("$.input[*].content[*].text", InText),
            ],
            // A web search's query, which Go's encoder escapes.
            Self::Stream => &[("$[*].data.delta.partial_json", GoEscaped)],
            // Reasoning summary and message content parts that aren't strings,
            // joined into one text.
            Self::NonStream => &[
                ("$.content[*].thinking", InText),
                ("$.content[*].text", InText),
            ],
            // A tool message's content that is neither a string nor an array,
            // a tool output part it doesn't recognize, and call arguments,
            // custom tool input and text that aren't strings.
            Self::ChatRequest => &[
                ("$.input[*].arguments", Whole),
                ("$.input[*].input", Whole),
                ("$.input[*].output", Whole),
                ("$.input[*].output[*].text", Whole),
                ("$.input[*].content[*].text", Whole),
            ],
            // The schema in a structured output instruction, a tool message's
            // content that can't be converted, and values read as text that
            // aren't strings: text, tool descriptions, stop sequences and the
            // reasoning effort.
            Self::ClaudeChatRequest | Self::ClaudeChatRequestCompat => &[
                ("$.system[*].text", InText),
                ("$.messages[*].content[*].text", Whole),
                ("$.messages[*].content[*].content", Whole),
                ("$.messages[*].content[*].content[*].text", Whole),
                ("$.tools[*].description", Whole),
                ("$.stop_sequences[*]", Whole),
                ("$.output_config.effort", Whole),
            ],
            // The schema in a structured output instruction, a tool output
            // that has no part Claude can carry, and values read as text that
            // aren't strings: text, reasoning summaries (joined into one
            // thinking text), image URLs, custom tool input and tool
            // descriptions.
            Self::ClaudeResponsesRequest | Self::ClaudeResponsesRequestCompat => &[
                ("$.system[*].text", InText),
                ("$.tools[*].description", Whole),
                ("$.messages[*].content", Whole),
                ("$.messages[*].content[*].text", Whole),
                ("$.messages[*].content[*].thinking", InText),
                ("$.messages[*].content[*].source.url", Whole),
                ("$.messages[*].content[*].input.input", Whole),
                ("$.messages[*].content[*].content", Whole),
                ("$.messages[*].content[*].content[*].text", Whole),
                ("$.messages[*].content[*].content[*].source.url", Whole),
            ],
            // The request fields a response repeats, when the client sent
            // something other than a string.
            Self::ClaudeResponsesStream => &[
                ("$[*].data.response.instructions", Whole),
                ("$[*].data.response.previous_response_id", Whole),
                ("$[*].data.response.prompt_cache_key", Whole),
                ("$[*].data.response.safety_identifier", Whole),
            ],
            Self::ClaudeResponsesNonStream => &[
                ("$.instructions", Whole),
                ("$.previous_response_id", Whole),
                ("$.prompt_cache_key", Whole),
                ("$.safety_identifier", Whole),
            ],
            // Values read as text that aren't strings: the prompt and model,
            // and in responses the ID, model, text and finish reasons.
            Self::CompletionsRequest => &[("$.model", Whole), ("$.messages[*].content", Whole)],
            Self::CompletionsResponse => &[
                ("$.id", Whole),
                ("$.model", Whole),
                ("$.choices[*].text", Whole),
                ("$.choices[*].finish_reason", Whole),
            ],
            Self::CompletionsStreamChunk => &[
                ("$[*].id", Whole),
                ("$[*].model", Whole),
                ("$[*].choices[*].text", Whole),
                ("$[*].choices[*].finish_reason", Whole),
            ],
            // Values read as text that aren't strings: message content and
            // its text, reasoning, image URLs, call arguments and tool
            // outputs, and the names and descriptions of tools.
            Self::OpenAIResponsesRequest => &[
                ("$.messages[*].content", InText),
                ("$.messages[*].content[*].text", Whole),
                ("$.messages[*].content[*].image_url.url", Whole),
                ("$.messages[*].reasoning_content", InText),
                ("$.messages[*].role", Whole),
                ("$.messages[*].tool_calls[*].function.arguments", Whole),
                ("$.messages[*].tool_calls[*].function.name", Whole),
                ("$.tools[*].function.description", Whole),
                ("$.tool_choice.function.name", Whole),
            ],
            // Values read as text that aren't strings: call arguments, text
            // (also when a system reminder wraps it), a tool result's content,
            // stop sequences and the user.
            Self::OpenAIClaudeRequest | Self::OpenAIClaudeRequestCompat => &[
                ("$.messages[*].content", InText),
                ("$.messages[*].content[*].text", InText),
                ("$.messages[*].tool_calls[*].function.arguments", Whole),
                ("$.stop[*]", Whole),
                ("$.user", Whole),
            ],
            // Values read as text that aren't strings: the instructions a
            // response repeats, and content and reasoning, each delta on its
            // own and joined into one text.
            Self::OpenAIResponsesStream => &[
                ("$[*].data.response.instructions", Whole),
                ("$[*].data.delta", Whole),
                ("$[*].data.text", InText),
                ("$[*].data.part.text", InText),
                ("$[*].data.item.content[*].text", InText),
                ("$[*].data.item.summary[*].text", InText),
                ("$[*].data.response.output[*].content[*].text", InText),
                ("$[*].data.response.output[*].summary[*].text", InText),
            ],
            // Content and call arguments that aren't strings, and so the
            // input of a custom tool call whose arguments are an object
            // without one.
            Self::OpenAIResponsesNonStream => &[
                ("$.output[*].content[*].text", Whole),
                ("$.output[*].arguments", Whole),
                ("$.output[*].input", Whole),
            ],
            // Call arguments and outputs, and text and tool descriptions read
            // from a value that isn't a string.
            Self::CodexGeminiRequest => &[
                ("$.input[*].arguments", Whole),
                ("$.input[*].output", Whole),
                ("$.input[*].content[*].text", Whole),
                ("$.tools[*].description", Whole),
            ],
            // A tool result taken from the whole response or a result that
            // isn't a string, and text, tool descriptions and stop sequences
            // read from a value that isn't a string. System text is joined.
            Self::ClaudeGeminiRequest => &[
                ("$.messages[*].content[*].content", Whole),
                ("$.messages[*].content[*].text", InText),
                ("$.tools[*].description", Whole),
                ("$.stop_sequences[*]", Whole),
            ],
            // Call arguments, a tool message's content, and text, tool
            // descriptions and stop sequences read from a value that isn't a
            // string. A message's text parts are joined.
            Self::OpenAIGeminiRequest => &[
                ("$.messages[*].tool_calls[*].function.arguments", Whole),
                ("$.messages[*].content", InText),
                ("$.messages[*].content[*].text", InText),
                ("$.tools[*].function.description", Whole),
                ("$.stop[*]", Whole),
            ],
            // Content and reasoning that aren't strings.
            Self::OpenAIGeminiStream => &[("$[*].candidates[*].content.parts[*].text", Whole)],
            Self::OpenAIGeminiNonStream => &[("$.candidates[*].content.parts[*].text", Whole)],
            // Text that isn't a string, such as a reasoning item's content.
            Self::CodexGeminiStream => &[("$[*].candidates[*].content.parts[*].text", Whole)],
            Self::CodexGeminiNonStream => &[("$.candidates[*].content.parts[*].text", Whole)],
            Self::ClaudeGeminiStream | Self::ClaudeGeminiNonStream => &[],
            // Values read as text that aren't strings: text (also when a
            // system reminder wraps it), tool names, a tool result stored as
            // text, and the hint the schema cleaner adds for a type that is
            // an object or array.
            Self::GeminiClaudeRequest | Self::GeminiClaudeRequestCompat => &[
                ("$.contents[*].parts[*].text", InText),
                ("$.systemInstruction.parts[*].text", InText),
                ("$.contents[*].parts[*].functionCall.name", Whole),
                ("$.contents[*].parts[*].functionResponse.name", Whole),
                (
                    "$.contents[*].parts[*].functionResponse.response.result",
                    Whole,
                ),
                ("$.tools[*].functionDeclarations[*].name", Whole),
                (
                    "$.tools[*].functionDeclarations[*].parametersJsonSchema**.description",
                    InText,
                ),
            ],
            // Values read as text that aren't strings: text, tool names and
            // the reasoning effort, a tool message's content stored as a
            // result's text, and the hint the schema cleaner adds for a type
            // that is an object or array.
            Self::GeminiChatRequest => &[
                ("$.contents[*].parts[*].text", InText),
                ("$.systemInstruction.parts[*].text", InText),
                ("$.contents[*].parts[*].functionCall.name", Whole),
                ("$.contents[*].parts[*].functionResponse.name", Whole),
                (
                    "$.contents[*].parts[*].functionResponse.response.result",
                    Whole,
                ),
                ("$.generationConfig.thinkingConfig.thinkingLevel", Whole),
                ("$.tools[*].functionDeclarations[*].name", Whole),
                (
                    "$.tools[*].functionDeclarations[*].parametersJsonSchema**.description",
                    InText,
                ),
            ],
            // A call's name that isn't a string, given to the unnamed
            // response that answers it.
            Self::GeminiGeminiRequest => &[("$.contents[*].parts[*].functionResponse.name", Whole)],
            // A function call's arguments, which we stream compactly.
            Self::GeminiClaudeStream => &[("$[*].data.delta.partial_json", Whole)],
            // A function call's arguments, written compactly.
            Self::GeminiChatStream => &[(
                "$[*].json.choices[*].delta.tool_calls[*].function.arguments",
                Whole,
            )],
            Self::GeminiChatNonStream => &[(
                "$.choices[*].message.tool_calls[*].function.arguments",
                Whole,
            )],
            Self::ResponsesRequest
            | Self::ResponsesStream
            | Self::ResponsesNonStream
            | Self::ChatStream
            | Self::ChatNonStream
            | Self::ClaudeChatStream
            | Self::ClaudeChatNonStream
            | Self::SignatureInspect
            | Self::ClaudeMessagesSignatures
            | Self::GeminiSignatures
            | Self::RegistryLookup
            | Self::OpenAIClaudeStream
            | Self::OpenAIClaudeNonStream
            | Self::OpenAIChatRequest
            | Self::OpenAIChatStream
            | Self::OpenAIChatNonStream
            | Self::GeminiGeminiStream
            | Self::GeminiGeminiNonStream
            | Self::GeminiClaudeNonStream
            | Self::ThinkingCodex
            | Self::ThinkingOpenAI
            | Self::CodexModels
            | Self::MultiAgentPrepare
            | Self::MultiAgentOptimize
            | Self::MultiAgentInput
            | Self::MultiAgentOrphan
            | Self::MultiAgentRestore
            | Self::Payload
            | Self::Usage
            | Self::Ttft
            | Self::ConfigDiff
            | Self::ConfigSave => &[],
            Self::GeminiResponsesRequest => GEMINI_RESPONSES_REQUEST_JSON,
            Self::GeminiResponsesStream => GEMINI_RESPONSES_STREAM_JSON,
            Self::GeminiResponsesNonStream => GEMINI_RESPONSES_NON_STREAM_JSON,
            Self::Interactions(kind) => kind.embedded_json(case),
        }
    }

    /// For a registry case, the translator the registry runs for the case's
    /// pair of formats, whose output this one reads as its own. `None` for a
    /// pair with none, or with a request translator of the case's own.
    fn native(self, case: &Case) -> Option<Self> {
        let pair = (
            case.options["from"].as_str().unwrap_or_default(),
            case.options["to"].as_str().unwrap_or_default(),
        );
        match self {
            Self::RegistryRequest if case.options["identity"].as_bool() != Some(true) => match pair
            {
                ("claude", "codex") => Some(Self::Request),
                ("openai-response", "codex") => Some(Self::ResponsesRequest),
                ("openai", "codex") => Some(Self::ChatRequest),
                ("openai", "claude") => Some(Self::ClaudeChatRequest),
                ("openai-response", "claude") => Some(Self::ClaudeResponsesRequest),
                ("claude", "openai") => Some(Self::OpenAIClaudeRequest),
                ("openai", "openai") => Some(Self::OpenAIChatRequest),
                ("openai-response", "openai") => Some(Self::OpenAIResponsesRequest),
                ("gemini", "codex") => Some(Self::CodexGeminiRequest),
                ("gemini", "claude") => Some(Self::ClaudeGeminiRequest),
                ("gemini", "openai") => Some(Self::OpenAIGeminiRequest),
                ("gemini", "gemini") => Some(Self::GeminiGeminiRequest),
                ("claude", "gemini") => Some(Self::GeminiClaudeRequest),
                ("openai", "gemini") => Some(Self::GeminiChatRequest),
                ("openai-response", "gemini") => Some(Self::GeminiResponsesRequest),
                (from, to) => {
                    interactions::Kind::native(Stage::Request, from, to).map(Self::Interactions)
                }
            },
            Self::RegistryStream => match pair {
                ("codex", "claude") => Some(Self::Stream),
                ("codex", "openai-response") => Some(Self::ResponsesStream),
                ("codex", "openai") => Some(Self::ChatStream),
                ("claude", "openai") => Some(Self::ClaudeChatStream),
                ("claude", "openai-response") => Some(Self::ClaudeResponsesStream),
                ("openai", "claude") => Some(Self::OpenAIClaudeStream),
                ("openai", "openai") => Some(Self::OpenAIChatStream),
                ("openai", "openai-response") => Some(Self::OpenAIResponsesStream),
                ("codex", "gemini") => Some(Self::CodexGeminiStream),
                ("claude", "gemini") => Some(Self::ClaudeGeminiStream),
                ("openai", "gemini") => Some(Self::OpenAIGeminiStream),
                ("gemini", "gemini") => Some(Self::GeminiGeminiStream),
                ("gemini", "claude") => Some(Self::GeminiClaudeStream),
                ("gemini", "openai") => Some(Self::GeminiChatStream),
                ("gemini", "openai-response") => Some(Self::GeminiResponsesStream),
                (from, to) => {
                    interactions::Kind::native(Stage::Stream, from, to).map(Self::Interactions)
                }
            },
            Self::RegistryNonStream => match pair {
                ("codex", "claude") => Some(Self::NonStream),
                ("codex", "openai-response") => Some(Self::ResponsesNonStream),
                ("codex", "openai") => Some(Self::ChatNonStream),
                ("claude", "openai") => Some(Self::ClaudeChatNonStream),
                ("claude", "openai-response") => Some(Self::ClaudeResponsesNonStream),
                ("openai", "claude") => Some(Self::OpenAIClaudeNonStream),
                ("openai", "openai") => Some(Self::OpenAIChatNonStream),
                ("openai", "openai-response") => Some(Self::OpenAIResponsesNonStream),
                ("codex", "gemini") => Some(Self::CodexGeminiNonStream),
                ("claude", "gemini") => Some(Self::ClaudeGeminiNonStream),
                ("openai", "gemini") => Some(Self::OpenAIGeminiNonStream),
                ("gemini", "gemini") => Some(Self::GeminiGeminiNonStream),
                ("gemini", "claude") => Some(Self::GeminiClaudeNonStream),
                ("gemini", "openai") => Some(Self::GeminiChatNonStream),
                ("gemini", "openai-response") => Some(Self::GeminiResponsesNonStream),
                (from, to) => {
                    interactions::Kind::native(Stage::NonStream, from, to).map(Self::Interactions)
                }
            },
            _ => None,
        }
    }

    /// Takes out of upstream's output what we leave out on purpose, returning
    /// the deviations that account for it.
    ///
    /// Upstream makes up a Claude `metadata.user_id` when the client sent
    /// none; we don't. The Gemini to Chat Completions translator derives call
    /// IDs from JSON text, which we write compactly: where that gives a
    /// different ID, [`Self::read`] turns ours into upstream's, and that is
    /// accounted for here.
    pub fn drop_deliberate_omissions(self, case: &Case, go: &mut Value) -> Vec<Deviation> {
        if self == Self::ConfigSave {
            let unloadable = config_save::drop_unloadable(go);
            let repeated = config_save::drop_repeated_plugin_comments(case, go);
            return unloadable.into_iter().chain(repeated).collect();
        }
        self.drop_one_omission(case, go).into_iter().collect()
    }

    /// [`Self::drop_deliberate_omissions`] for the translators, which leave
    /// out one thing at most.
    fn drop_one_omission(self, case: &Case, go: &mut Value) -> Option<Deviation> {
        if let Some(native) = self.native(case) {
            return native.drop_one_omission(case, go);
        }
        if let Self::Interactions(kind) = self {
            return kind.drop_deliberate_omissions(case, go);
        }
        if self == Self::OpenAIGeminiRequest {
            let ids = derived_call_ids(&case.request);
            return contains_string(go, &|text| ids.iter().any(|(_, upstream)| upstream == text))
                .then_some(Deviation::CompactCallIdSource);
        }
        if !matches!(
            self,
            Self::ClaudeChatRequest
                | Self::ClaudeChatRequestCompat
                | Self::ClaudeResponsesRequest
                | Self::ClaudeResponsesRequestCompat
                | Self::ClaudeGeminiRequest
        ) {
            return None;
        }
        let request: Value = serde_json::from_str(&case.request).ok()?;
        let client_id = |value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .is_some_and(|id| !id.trim().is_empty())
        };
        let metadata_id = request.get("metadata").and_then(|meta| meta.get("user_id"));
        if client_id(metadata_id) || client_id(request.get("user")) {
            return None;
        }
        let metadata = go.get_mut("metadata")?.as_object_mut()?;
        metadata.shift_remove("user_id")?;
        Some(Deviation::SyntheticUserId)
    }

    /// Reads a translator's raw output as JSON, or `None` if it isn't the
    /// kind of output the translator should produce.
    ///
    /// A Claude stream becomes an array of `{"event", "data"}` frames, and a
    /// Responses or Chat Completions stream an array with an entry per line
    /// (see [`read_lines`]). The Chat Completions to Claude stream is an array
    /// with an entry per chunk: `{"json": …}` for a whole message, or
    /// `{"sse": [frame, …]}`. A passed-through Chat Completions stream or
    /// response is kept as text. An empty non-streaming output (no response)
    /// reads as [`NO_OUTPUT`]. In Claude responses and requests, tool IDs
    /// generated for calls without one are masked, since they hold a timestamp
    /// or random letters; IDs found in `case`'s input are not (see
    /// [`mask_generated_tool_ids`]). So is a Chat Completions response's
    /// `created`, or a Responses response's `created_at` from a Claude stream
    /// or a Chat Completions response, when it is the current time, and a
    /// response ID made up for a Chat Completions response without one (see
    /// [`mask_generated_response_id`]).
    ///
    /// A Gemini stream is an array with an entry per chunk, read as JSON or
    /// kept as text if it isn't JSON. A Gemini response's `createTime` from a
    /// Claude stream is masked when it is the current time (see
    /// [`mask_create_time_now`]), and the function calls in a chunk from a
    /// Chat Completions stream are sorted (see [`sort_function_calls`]). In
    /// a Chat Completions request from a Gemini one, each call ID we derive
    /// from compact JSON where upstream derives another from the client's
    /// text becomes upstream's (see [`derived_call_ids`]).
    pub fn read(self, case: &Case, output: &[u8]) -> Option<Value> {
        let text = String::from_utf8_lossy(output);
        let native = self.native(case);
        let mut value = match self {
            Self::RegistryStream => return read_registry_stream(case, native, &text),
            Self::RegistryNonStream => return read_registry_non_stream(case, native, &text),
            Self::Interactions(kind) => return kind.read(case, output),
            Self::RegistryRequest if native.is_some() => return native?.read(case, output),
            Self::RegistryRequest | Self::RegistryLookup => {
                return exact::from_str(&text).ok();
            }
            Self::Request
            | Self::RequestCompat
            | Self::ResponsesRequest
            | Self::OpenAIResponsesRequest
            | Self::OpenAIClaudeRequest
            | Self::OpenAIClaudeRequestCompat
            | Self::OpenAIChatRequest
            | Self::SignatureInspect
            | Self::ClaudeMessagesSignatures
            | Self::GeminiSignatures
            | Self::ChatRequest
            | Self::CompletionsRequest
            | Self::CompletionsResponse
            | Self::CodexGeminiRequest
            | Self::GeminiGeminiRequest
            | Self::GeminiClaudeRequest
            | Self::GeminiClaudeRequestCompat
            | Self::GeminiChatRequest
            | Self::GeminiResponsesRequest
            | Self::ThinkingCodex
            | Self::ThinkingOpenAI
            | Self::CodexModels
            | Self::MultiAgentPrepare
            | Self::MultiAgentOptimize
            | Self::MultiAgentInput
            | Self::MultiAgentOrphan
            | Self::MultiAgentRestore
            | Self::Payload
            | Self::Usage
            | Self::Ttft
            | Self::ConfigDiff
            | Self::ConfigSave => return exact::from_str(&text).ok(),
            Self::OpenAIGeminiRequest => {
                let mut value: Value = exact::from_str(&text).ok()?;
                replace_compact_call_ids(&mut value, case);
                return Some(value);
            }
            Self::CodexGeminiStream | Self::ClaudeGeminiStream | Self::OpenAIGeminiStream => {
                let chunks: Vec<String> = serde_json::from_str(&text).ok()?;
                let chunks = chunks
                    .into_iter()
                    .map(|chunk| {
                        let mut value = exact::from_str(&chunk).unwrap_or(Value::String(chunk));
                        match self {
                            Self::ClaudeGeminiStream => mask_create_time_now(&mut value),
                            Self::OpenAIGeminiStream => sort_function_calls(&mut value),
                            _ => {}
                        }
                        value
                    })
                    .collect();
                return Some(Value::Array(chunks));
            }
            Self::ResponsesStream | Self::ChatStream => return read_lines(&text),
            Self::OpenAIChatStream | Self::GeminiGeminiStream => {
                let chunks: Vec<String> = serde_json::from_str(&text).ok()?;
                return Some(chunks.into_iter().map(Value::String).collect());
            }
            Self::OpenAIChatNonStream | Self::GeminiGeminiNonStream => {
                return Some(Value::String(text.into_owned()));
            }
            Self::GeminiClaudeStream => {
                let mut frames = sse_frames_ended_by(&text, "\n\n\n");
                mask_tool_use_counters(&mut frames);
                return Some(frames);
            }
            Self::GeminiChatStream => {
                let mut lines = read_lines(&text)?;
                mask_function_call_ids(&mut lines);
                return Some(lines);
            }
            Self::OpenAIResponsesStream => return Some(sse_frames(&text)),
            Self::CompletionsStreamChunk => return read_chunks(&text),
            Self::ClaudeChatStream => {
                let mut lines = read_lines(&text)?;
                for line in lines.as_array_mut().into_iter().flatten() {
                    if let Some(chunk) = line.get_mut("json") {
                        mask_time_now(chunk, "created");
                    }
                }
                return Some(lines);
            }
            Self::ClaudeResponsesStream => {
                let mut frames = sse_frames(&text);
                for frame in frames.as_array_mut().into_iter().flatten() {
                    if let Some(response) = frame.pointer_mut("/data/response") {
                        mask_time_now(response, "created_at");
                    }
                }
                return Some(frames);
            }
            Self::NonStream
            | Self::ResponsesNonStream
            | Self::ChatNonStream
            | Self::ClaudeChatNonStream
            | Self::ClaudeResponsesNonStream
            | Self::OpenAIResponsesNonStream
            | Self::OpenAIClaudeNonStream
            | Self::CodexGeminiNonStream
            | Self::ClaudeGeminiNonStream
            | Self::OpenAIGeminiNonStream
            | Self::GeminiClaudeNonStream
            | Self::GeminiChatNonStream
                if text.is_empty() =>
            {
                return Some(NO_OUTPUT.into());
            }
            Self::ResponsesNonStream
            | Self::CodexGeminiNonStream
            | Self::OpenAIGeminiNonStream
            | Self::GeminiClaudeNonStream => return exact::from_str(&text).ok(),
            Self::ClaudeGeminiNonStream => {
                let mut value: Value = exact::from_str(&text).ok()?;
                mask_create_time_now(&mut value);
                return Some(value);
            }
            Self::GeminiChatNonStream => {
                let mut value: Value = exact::from_str(&text).ok()?;
                mask_function_call_ids(&mut value);
                return Some(value);
            }
            Self::ChatNonStream | Self::ClaudeChatNonStream => {
                let mut value: Value = exact::from_str(&text).ok()?;
                mask_time_now(&mut value, "created");
                return Some(value);
            }
            Self::ClaudeResponsesNonStream => {
                let mut value: Value = exact::from_str(&text).ok()?;
                mask_time_now(&mut value, "created_at");
                return Some(value);
            }
            Self::OpenAIResponsesNonStream => {
                let mut value: Value = exact::from_str(&text).ok()?;
                mask_time_now(&mut value, "created_at");
                mask_generated_response_id(&mut value, case);
                return Some(value);
            }
            Self::OpenAIClaudeStream => {
                let chunks: Vec<String> = serde_json::from_str(&text).ok()?;
                chunks
                    .iter()
                    .map(|chunk| match exact::from_str(chunk) {
                        Ok(message) => object([("json", message)]),
                        Err(_) => object([("sse", sse_frames(chunk))]),
                    })
                    .collect()
            }
            Self::OpenAIClaudeNonStream => exact::from_str(&text).ok()?,
            Self::ClaudeChatRequest
            | Self::ClaudeChatRequestCompat
            | Self::ClaudeResponsesRequest
            | Self::ClaudeResponsesRequestCompat
            | Self::ClaudeGeminiRequest => exact::from_str(&text).ok()?,
            Self::Stream => sse_frames(&text),
            Self::NonStream => exact::from_str(&text).ok()?,
            Self::GeminiResponsesStream => {
                let mut frames = sse_frames(&text);
                mask_gemini_responses_ids(&mut frames, case);
                return Some(frames);
            }
            Self::GeminiResponsesNonStream if text.is_empty() => {
                return Some(NO_OUTPUT.into());
            }
            Self::GeminiResponsesNonStream => {
                let mut value: Value = exact::from_str(&text).ok()?;
                mask_gemini_responses_ids(&mut value, case);
                return Some(value);
            }
        };
        let input = OnceCell::new();
        mask_generated_tool_ids(&mut value, &|id| {
            input.get_or_init(|| input_text(case)).contains(id)
        });
        Some(value)
    }
}

/// Reads the Responses stream translator's output: a JSON array with a
/// string per output line, or `=` for a line returned unchanged. A changed
/// line becomes `{"data": …}` for an SSE data line, `{"json": …}` for a bare
/// JSON line, and `{"line": text}` for anything else.
pub(crate) fn read_lines(text: &str) -> Option<Value> {
    let lines: Vec<String> = serde_json::from_str(text).ok()?;
    let json = |text: &str| exact::from_str(text).ok();
    let lines = lines
        .into_iter()
        .map(|line| {
            if line == UNCHANGED {
                Value::String(line)
            } else if let Some(data) = line.strip_prefix("data: ").and_then(json) {
                object([("data", data)])
            } else if let Some(value) = json(&line) {
                object([("json", value)])
            } else {
                json!({ "line": line })
            }
        })
        .collect();
    Some(Value::Array(lines))
}

/// Reads the Completions chunk translator's output: a JSON array with a
/// string per chunk, or `null` for a chunk that was skipped. Each chunk is
/// read as JSON, or kept as text if it isn't JSON.
pub(crate) fn read_chunks(text: &str) -> Option<Value> {
    let chunks: Vec<Option<String>> = serde_json::from_str(text).ok()?;
    let chunks = chunks
        .into_iter()
        .map(|chunk| match chunk {
            None => Value::Null,
            Some(chunk) => match exact::from_str(&chunk) {
                Ok(value) => value,
                Err(_) => Value::String(chunk),
            },
        })
        .collect();
    Some(Value::Array(chunks))
}

/// Reads a registry non-streaming report, `{"output": text}`, as `native`
/// reads its output, or as a body passed through as it is, which need not be
/// JSON. `{"output": null}`, a failed translation, reads as [`FAILED`].
fn read_registry_non_stream(case: &Case, native: Option<Translator>, text: &str) -> Option<Value> {
    let report: Value = serde_json::from_str(text).ok()?;
    let output = match &report["output"] {
        Value::Null => return Some(FAILED.into()),
        output => output.as_str()?,
    };
    match native {
        Some(native) => native.read(case, output.as_bytes()),
        None if output.is_empty() => Some(NO_OUTPUT.into()),
        None => Some(exact::from_str(output).unwrap_or_else(|_| output.into())),
    }
}

/// The pair of formats in a registry case's options: for a request, the
/// client's format and the provider's; for a response, the provider's and the
/// client's.
fn registry_formats(case: &Case) -> (Format, Format) {
    let format = |key: &str| Format::new(case.options[key].as_str().unwrap_or_default().to_owned());
    (format("from"), format("to"))
}

/// Reads a registry stream's report, `{"events": [[chunk, …] for each
/// event], "finish": [chunk, …], "failed": bool}`, as `native`'s harness entry
/// would have written its chunks, followed by an entry for what the registry
/// adds: the chunks each event and the stream's end gave, and whether the
/// stream failed. A stream with no translator reads as the Responses stream
/// translator's does, its chunks passed through.
///
/// The SSE translators' chunks are read joined, as their harness entries
/// write them, so for those each chunk is listed by its frames (see
/// [`sse_chunk`]): where a chunk splits a frame, or a frame isn't ended,
/// shows. The other translators' chunks are read one by one, so they are only
/// counted.
fn read_registry_stream(case: &Case, native: Option<Translator>, text: &str) -> Option<Value> {
    let report: Value = serde_json::from_str(text).ok()?;
    let chunks = |value: &Value| -> Option<Vec<String>> {
        value
            .as_array()?
            .iter()
            .map(|chunk| chunk.as_str().map(str::to_owned))
            .collect()
    };
    let events: Vec<Vec<String>> = report["events"]
        .as_array()?
        .iter()
        .map(chunks)
        .collect::<Option<_>>()?;
    let finish = chunks(&report["finish"])?;
    let failed = report["failed"].as_bool()?;
    let all = || events.iter().flatten().chain(&finish);
    let output = match native {
        Some(
            Translator::Stream
            | Translator::ClaudeResponsesStream
            | Translator::OpenAIResponsesStream
            | Translator::GeminiClaudeStream
            | Translator::GeminiResponsesStream,
        ) => all().map(String::as_str).collect::<String>(),
        Some(Translator::Interactions(kind)) if kind.joins_stream() => {
            all().map(String::as_str).collect::<String>()
        }
        Some(Translator::ResponsesStream) | None => {
            let mut lines: Vec<&str> = Vec::new();
            for (event, chunks) in case.events.iter().zip(&events) {
                lines.extend(chunks.iter().map(
                    |chunk| {
                        if chunk == event { UNCHANGED } else { chunk }
                    },
                ));
            }
            lines.extend(finish.iter().map(String::as_str));
            serde_json::to_string(&lines).expect("strings serialize")
        }
        Some(_) => serde_json::to_string(&all().collect::<Vec<_>>()).expect("strings serialize"),
    };
    let mut value = match native {
        Some(native) => native.read(case, output.as_bytes())?,
        None => read_lines(&output)?,
    };
    let sse = matches!(
        native,
        Some(
            Translator::Stream
                | Translator::ClaudeResponsesStream
                | Translator::OpenAIResponsesStream
                | Translator::GeminiClaudeStream
                | Translator::GeminiResponsesStream
        )
    ) || matches!(native, Some(Translator::Interactions(kind)) if kind.joins_stream());
    let shape = |chunks: &[String]| -> Value {
        if sse {
            chunks.iter().map(|chunk| sse_chunk(chunk)).collect()
        } else {
            chunks.len().into()
        }
    };
    let shapes: Vec<Value> = events.iter().map(|chunks| shape(chunks)).collect();
    value.as_array_mut()?.push(json!({ "registry": {
        "chunks": shapes,
        "finish": shape(&finish),
        "tool_input_failed": failed,
    } }));
    Some(value)
}

/// An SSE chunk as a list of its frames, each given by its `event:` line
/// (or `(no event)`), then `{"unended": text}` for any text after the last
/// blank line. The frames' data is compared elsewhere.
fn sse_chunk(chunk: &str) -> Value {
    let mut frames = Vec::new();
    let mut rest = chunk;
    while let Some((frame, after)) = rest.split_once("\n\n") {
        let event = frame.lines().find(|line| line.starts_with("event:"));
        frames.push(event.unwrap_or("(no event)").into());
        rest = after;
    }
    if !rest.is_empty() {
        frames.push(json!({ "unended": rest }));
    }
    Value::Array(frames)
}

/// Splits SSE text into `{"event": …, "data": …}` frames, each number in
/// the data kept as written. Anything that isn't an `event: …\ndata:
/// <JSON>\n\n` frame is kept as `{"unparsed": text}`, and text after the
/// last blank line as `{"unended": text}`, so they show up as differences.
pub(crate) fn sse_frames(text: &str) -> Value {
    let mut frames = Vec::new();
    let mut rest = text;
    while let Some((frame, after)) = rest.split_once("\n\n") {
        let parsed = frame
            .strip_prefix("event: ")
            .and_then(|rest| rest.split_once("\ndata: "))
            .and_then(|(event, data)| {
                let data = exact::from_str(data).ok()?;
                Some(object([("event", event.into()), ("data", data)]))
            });
        frames.push(parsed.unwrap_or_else(|| json!({ "unparsed": frame })));
        rest = after;
    }
    if !rest.is_empty() {
        frames.push(json!({ "unended": rest }));
    }
    Value::Array(frames)
}

/// An object of `fields`, in order. Unlike `json!`, which re-reads a
/// `Value` it is given, it keeps each number as written.
pub(crate) fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

/// Splits SSE text whose frames each end with `end` into `{"event": …,
/// "data": …}` frames, as [`sse_frames`] does for frames ending with a blank
/// line. Anything else is kept as `{"unparsed": text}`.
pub(crate) fn sse_frames_ended_by(text: &str, end: &str) -> Value {
    let frames = text
        .split_terminator(end)
        .map(|frame| {
            frame
                .strip_prefix("event: ")
                .and_then(|rest| rest.split_once("\ndata: "))
                .and_then(|(event, data)| {
                    let data = exact::from_str(data).ok()?;
                    Some(object([("event", event.into()), ("data", data)]))
                })
                .unwrap_or_else(|| json!({ "unparsed": frame }))
        })
        .collect();
    Value::Array(frames)
}

/// Replaces the count at the end of the IDs the Gemini to Claude stream
/// translator makes up for tool calls, `<name>-<count>`, with
/// `(generated-<n>)` for the `n`th call. The count is process-wide, upstream's
/// and ours alike, so it depends on the cases run before.
fn mask_tool_use_counters(frames: &mut Value) {
    let mut n = 0;
    for frame in frames.as_array_mut().into_iter().flatten() {
        let Some(block) = frame.pointer_mut("/data/content_block") else {
            continue;
        };
        if block["type"] != "tool_use" {
            continue;
        }
        let Some(Value::String(id)) = block.get_mut("id") else {
            continue;
        };
        if let Some((name, count)) = id.rsplit_once('-')
            && !count.is_empty()
            && count.bytes().all(|b| b.is_ascii_digit())
        {
            n += 1;
            *id = format!("{name}-(generated-{n})");
        }
    }
}

/// Replaces the clock time and count in the IDs the Gemini to Chat
/// Completions translators make up for tool calls,
/// `<name>-<unix nanos>-<count>`, with `(generated-<n>)` for the `n`th
/// distinct ID, wherever an item of a `tool_calls` list has one.
fn mask_function_call_ids(value: &mut Value) {
    fn generated(id: &str) -> Option<&str> {
        let (rest, count) = id.rsplit_once('-')?;
        let (name, nanos) = rest.rsplit_once('-')?;
        let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        (nanos.len() >= 16 && digits(nanos) && !count.is_empty() && digits(count)).then_some(name)
    }
    fn mask(value: &mut Value, seen: &mut Vec<String>) {
        match value {
            Value::Array(items) => items.iter_mut().for_each(|item| mask(item, seen)),
            Value::Object(fields) => {
                if let Some(Value::Array(calls)) = fields.get_mut("tool_calls") {
                    for call in calls {
                        let Some(Value::String(id)) = call.get_mut("id") else {
                            continue;
                        };
                        let Some(name) = generated(id) else {
                            continue;
                        };
                        let name = name.to_owned();
                        let n = match seen.iter().position(|seen| seen == id) {
                            Some(index) => index + 1,
                            None => {
                                seen.push(id.clone());
                                seen.len()
                            }
                        };
                        *id = format!("{name}-(generated-{n})");
                    }
                }
                fields.values_mut().for_each(|field| mask(field, seen));
            }
            _ => {}
        }
    }
    mask(value, &mut Vec::new());
}

/// Replaces a response's creation time, held in `key`, if it is within an
/// hour of now.
pub(crate) fn mask_time_now(value: &mut Value, key: &str) {
    let Some(created) = value.get_mut(key) else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    if created
        .as_u64()
        .is_some_and(|created| created.abs_diff(now) < 3600)
    {
        *created = CREATED_NOW.into();
    }
}

/// Replaces a Gemini response's `createTime`, an RFC 3339 time, if it is
/// within an hour of now. Upstream writes it in the local time zone and we
/// in UTC, so both are read as instants.
pub(crate) fn mask_create_time_now(value: &mut Value) {
    let Some(created) = value.get_mut("createTime") else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let current = created
        .as_str()
        .and_then(compare::rfc3339_seconds)
        .and_then(|created| u64::try_from(created).ok())
        .is_some_and(|created| created.abs_diff(now) < 3600);
    if current {
        *created = CREATED_NOW.into();
    }
}

/// Sorts the parts of each candidate in a Gemini chunk by their JSON text, if
/// every one is a function call. A Chat Completions stream's calls come out
/// together when it finishes; upstream keeps them in a Go map until then, so
/// their order changes from run to run, and we write them in the order of
/// their index.
fn sort_function_calls(chunk: &mut Value) {
    let candidates = chunk.get_mut("candidates").and_then(Value::as_array_mut);
    for candidate in candidates.into_iter().flatten() {
        let Some(parts) = candidate
            .pointer_mut("/content/parts")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        if parts.iter().all(|part| part.get("functionCall").is_some()) {
            parts.sort_by_cached_key(Value::to_string);
        }
    }
}

/// One of our Gemini response stream translators, taking a line at a time.
type LineTranslator<'a> = Box<dyn FnMut(&[u8]) -> Vec<Value> + 'a>;

/// Whether `text` is JSON written compactly, as we write it.
/// The call IDs the Gemini to Chat Completions translator derives from a
/// function call's or response's JSON, where ours differ from upstream's, as
/// (ours, upstream's). Upstream hashes the JSON as the client wrote it, and a
/// name that's an object or array as its text; we hash them written
/// compactly. Elsewhere the two match, so the IDs must too.
fn derived_call_ids(request: &str) -> Vec<(String, String)> {
    let mut ids = Vec::new();
    let Some(root) = raw_json::parse(request) else {
        return ids;
    };
    let Some(Raw::Array(contents, _)) = root.get("contents") else {
        return ids;
    };
    for (message_index, content) in contents.iter().enumerate() {
        let Some(Raw::Array(parts, _)) = content.get("parts") else {
            continue;
        };
        for (part_index, part) in parts.iter().enumerate() {
            let mut derive = |kind: &str, holder: &Raw<'_>, payload: Option<&Raw<'_>>| {
                let name = holder.get("name");
                let ours = derived_call_id(
                    kind,
                    message_index,
                    part_index,
                    &gjson_string(name, true),
                    &payload
                        .map(|payload| payload.value().to_string())
                        .unwrap_or_default(),
                );
                let upstream = derived_call_id(
                    kind,
                    message_index,
                    part_index,
                    &gjson_string(name, false),
                    payload.map_or("", Raw::text),
                );
                if ours != upstream {
                    ids.push((ours, upstream));
                }
            };
            if let Some(call) = part.get("functionCall") {
                derive("call", call, call.get("args"));
            }
            if let Some(response) = part.get("functionResponse") {
                let body = response.get("response");
                derive(
                    "response",
                    response,
                    body.map(|body| body.get("content").unwrap_or(body)),
                );
            }
        }
    }
    ids
}

/// `deterministicToolCallID`.
fn derived_call_id(
    kind: &str,
    message_index: usize,
    part_index: usize,
    name: &str,
    payload: &str,
) -> String {
    let digest = Sha256::digest(format!(
        "{kind}|{message_index}|{part_index}|{name}|{payload}"
    ));
    let hex: String = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("call_{hex}")
}

/// gjson `String()`: a string's value, an integer as written, another number
/// as Go formats its float, `""` for null or nothing, and other values as
/// their text, or with `compact` an object or array as we write it.
fn gjson_string(value: Option<&Raw<'_>>, compact: bool) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let text = value.text();
    match value.value() {
        Value::Object(_) | Value::Array(_) if compact => value.value().to_string(),
        Value::String(string) => string,
        Value::Null => String::new(),
        Value::Number(_) => {
            let digits = text.strip_prefix('-').unwrap_or(text);
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return text.to_owned();
            }
            match text.parse::<f64>() {
                Ok(float) => open_ferry_translate::go::format_float(float),
                Err(_) => text.to_owned(),
            }
        }
        _ => text.to_owned(),
    }
}

/// Replaces each call ID we derive from compact JSON with the one upstream
/// derives from the client's text (see [`derived_call_ids`]). Every other ID
/// is compared as it is.
fn replace_compact_call_ids(value: &mut Value, case: &Case) {
    fn replace(value: &mut Value, ids: &[(String, String)]) {
        match value {
            Value::String(text) => {
                if let Some((_, upstream)) = ids.iter().find(|(ours, _)| ours == text) {
                    *text = upstream.clone();
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| replace(item, ids)),
            Value::Object(fields) => fields.values_mut().for_each(|field| replace(field, ids)),
            _ => {}
        }
    }
    let ids = derived_call_ids(&case.request);
    if !ids.is_empty() {
        replace(value, &ids);
    }
}

/// Whether any string in `value` satisfies `matches`.
fn contains_string(value: &Value, matches: &dyn Fn(&str) -> bool) -> bool {
    match value {
        Value::String(text) => matches(text),
        Value::Array(items) => items.iter().any(|item| contains_string(item, matches)),
        Value::Object(fields) => fields.values().any(|field| contains_string(field, matches)),
        _ => false,
    }
}

/// Replaces the made-up part of a response ID, `resp_<unix nanos in hex>_<count>`,
/// which upstream and we each take from the clock and a counter, wherever the
/// response repeats it: in its `id` and in the IDs of its items, such as
/// `rs_<hex>_<count>` and `msg_resp_<hex>_<count>_0`. An `id` found in
/// `case`'s input is kept as it is.
fn mask_generated_response_id(value: &mut Value, case: &Case) {
    fn replace(value: &mut Value, generated: &str) {
        match value {
            Value::String(text) if text.contains(generated) => {
                *text = text.replace(generated, GENERATED_RESPONSE_ID);
            }
            Value::Array(items) => items.iter_mut().for_each(|item| replace(item, generated)),
            Value::Object(fields) => fields
                .values_mut()
                .for_each(|field| replace(field, generated)),
            _ => {}
        }
    }
    let Some(id) = value.get("id").and_then(Value::as_str) else {
        return;
    };
    let Some(generated) = id.strip_prefix("resp_") else {
        return;
    };
    let made_up = generated.split_once('_').is_some_and(|(nanos, count)| {
        !nanos.is_empty()
            && nanos
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && !count.is_empty()
            && count.bytes().all(|b| b.is_ascii_digit())
    });
    if !made_up || input_text(case).contains(id) {
        return;
    }
    let generated = generated.to_owned();
    replace(value, &generated);
}

/// What the `n`th distinct tool ID generated in an output is replaced with
/// before comparing.
fn generated_tool_id(n: usize) -> String {
    format!("toolu_(generated-{n})")
}

/// Replaces `toolu_<unix nanos>_<counter>` and `toolu_` with 24 random letters
/// and digits, the IDs upstream and we generate for a call that has none.
///
/// Each distinct ID gets the next [`generated_tool_id`] in the order it first
/// appears, so a result keeps pointing at its call: a result that refers to
/// another call shows up as a difference. Keys are visited in the output's
/// order. An ID for which `from_client` is true is kept as it is, so a
/// changed client ID is a difference too.
pub(crate) fn mask_generated_tool_ids(value: &mut Value, from_client: &dyn Fn(&str) -> bool) {
    fn mask(value: &mut Value, from_client: &dyn Fn(&str) -> bool, seen: &mut Vec<String>) {
        match value {
            Value::String(id) if is_generated_tool_id(id) && !from_client(id) => {
                let n = match seen.iter().position(|seen| seen == id) {
                    Some(index) => index + 1,
                    None => {
                        seen.push(id.clone());
                        seen.len()
                    }
                };
                *id = generated_tool_id(n);
            }
            Value::Array(items) => {
                for item in items {
                    mask(item, from_client, seen);
                }
            }
            Value::Object(fields) => {
                for field in fields.values_mut() {
                    mask(field, from_client, seen);
                }
            }
            _ => {}
        }
    }
    mask(value, from_client, &mut Vec::new());
}

/// A case's input as one text, to look for the IDs the client sent: the
/// request, the translated request, the events and the options as written,
/// and every string in the JSON found in them, with escapes decoded. JSON
/// held in those strings is read too, since a call's arguments can carry
/// an ID that the output then holds as a string of its own.
pub(crate) fn input_text(case: &Case) -> String {
    let options = case.options.to_string();
    let mut text = String::new();
    [&case.request, &case.translated_request, &options]
        .into_iter()
        .chain(&case.events)
        .for_each(|input| push_strings(input, &mut text));
    text
}

/// Appends `input`, then the strings in the JSON embedded in it, each on a
/// line of its own.
fn push_strings(input: &str, text: &mut String) {
    fn push_value(value: &Value, text: &mut String) {
        match value {
            Value::String(string) => push_strings(string, text),
            Value::Array(items) => items.iter().for_each(|item| push_value(item, text)),
            Value::Object(fields) => fields.values().for_each(|field| push_value(field, text)),
            _ => {}
        }
    }
    text.push_str(input);
    text.push('\n');
    for (_, value) in compare::json_parts(input) {
        if let Some(value) = value {
            push_value(&value, text);
        }
    }
}

fn is_generated_tool_id(text: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let Some(rest) = text.strip_prefix("toolu_") else {
        return false;
    };
    let random = rest.len() == 24 && rest.bytes().all(|b| b.is_ascii_alphanumeric());
    random
        || rest
            .split_once('_')
            .is_some_and(|(nanos, counter)| digits(nanos) && digits(counter))
}

/// Where the Responses → Gemini request translator writes JSON it read
/// compactly while upstream copies the JSON's text (see
/// [`Translator::embedded_json`]).
const GEMINI_RESPONSES_REQUEST_JSON: &[JsonAt] = &[];

/// The same for the Gemini → Responses stream translator.
const GEMINI_RESPONSES_STREAM_JSON: &[JsonAt] = &[];

/// The same for the Gemini → Responses non-streaming translator.
const GEMINI_RESPONSES_NON_STREAM_JSON: &[JsonAt] = &[];

/// Masks what the Gemini → Responses translators take from the clock and
/// their counters, in a stream's frames or a whole response: `created_at`
/// when it is the current time, a response ID made up for a response
/// without one (`resp_<unix nanos in hex>_<count>`), wherever an item ID
/// repeats it, and call IDs made up for calls (`call_<unix nanos>_<count>`,
/// the nanos in decimal in a stream and in hex in a whole response), also
/// as `fc_` and `ctc_` item IDs. IDs found in `case`'s input are kept.
fn mask_gemini_responses_ids(value: &mut Value, case: &Case) {
    let input = OnceCell::new();
    let from_client = |id: &str| input.get_or_init(|| input_text(case)).contains(id);

    // The response, or each frame's.
    let mut responses: Vec<&mut Value> = match &mut *value {
        Value::Array(frames) => frames
            .iter_mut()
            .filter_map(|frame| frame.pointer_mut("/data/response"))
            .collect(),
        response => vec![response],
    };
    for response in &mut responses {
        mask_time_now(response, "created_at");
    }
    let generated = responses
        .iter()
        .filter_map(|response| response.get("id").and_then(Value::as_str))
        .find_map(|id| {
            let generated = id.strip_prefix("resp_")?;
            (is_made_up_call_suffix(generated) && !from_client(id)).then(|| generated.to_owned())
        });

    // Call IDs first: on a coarse clock a call ID can repeat the response
    // ID's made-up part when both counters happen to agree.
    let mut seen = Vec::new();
    mask_made_up_call_ids(value, &from_client, &mut seen);
    if let Some(generated) = generated {
        replace_in_strings(value, &generated, GENERATED_RESPONSE_ID);
    }
}

/// Whether `text` is `<hex>_<decimal>`, the made-up part of an ID.
fn is_made_up_call_suffix(text: &str) -> bool {
    text.split_once('_').is_some_and(|(nanos, count)| {
        !nanos.is_empty()
            && nanos
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && !count.is_empty()
            && count.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Replaces `from` with `to` in every string in `value`.
fn replace_in_strings(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(text) if text.contains(from) => *text = text.replace(from, to),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| replace_in_strings(item, from, to)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| replace_in_strings(field, from, to)),
        _ => {}
    }
}

/// Replaces each made-up call ID, `call_<nanos>_<count>` alone or after
/// `fc_` or `ctc_`, with `call_(generated-<n>)`, numbering IDs in the order
/// they first appear, so an item keeps pointing at its call.
fn mask_made_up_call_ids(
    value: &mut Value,
    from_client: &dyn Fn(&str) -> bool,
    seen: &mut Vec<String>,
) {
    match value {
        Value::String(text) => {
            let (prefix, id) = ["fc_", "ctc_", ""]
                .into_iter()
                .find_map(|prefix| Some((prefix, text.strip_prefix(prefix)?)))
                .expect("the empty prefix always matches");
            let made_up = id.strip_prefix("call_").is_some_and(is_made_up_call_suffix);
            if !made_up || from_client(id) {
                return;
            }
            let n = match seen.iter().position(|seen| seen == id) {
                Some(index) => index + 1,
                None => {
                    seen.push(id.to_owned());
                    seen.len()
                }
            };
            *text = format!("{prefix}call_(generated-{n})");
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| mask_made_up_call_ids(item, from_client, seen)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| mask_made_up_call_ids(field, from_client, seen)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_text_becomes_frames() {
        let text = "event: a\ndata: {\"x\":1}\n\nevent: b\ndata: not json\n\n";
        assert_eq!(
            sse_frames(text),
            json!([
                { "event": "a", "data": { "x": 1 } },
                { "unparsed": "event: b\ndata: not json" }
            ])
        );
        assert_eq!(sse_frames(""), json!([]));
        // A last frame without its blank line differs from one with it.
        let text = "event: a\ndata: {\"x\":1}";
        assert_eq!(
            sse_frames(text),
            json!([{ "unended": "event: a\ndata: {\"x\":1}" }])
        );
        assert_ne!(
            sse_frames(text),
            sse_frames("event: a\ndata: {\"x\":1}\n\n")
        );
    }

    #[test]
    fn responses_lines_read_by_kind() {
        let text = json!(["=", "data: {\"x\":1}", "{\"y\":2}", "data: [DONE]"]).to_string();
        assert_eq!(
            read_lines(&text),
            Some(json!([
                "=",
                { "data": { "x": 1 } },
                { "json": { "y": 2 } },
                { "line": "data: [DONE]" }
            ]))
        );
        assert_eq!(read_lines("not json"), None);
    }

    #[test]
    fn only_a_current_created_time_is_masked() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut current = json!({ "created": now - 5 });
        mask_time_now(&mut current, "created");
        assert_eq!(current["created"], CREATED_NOW);
        let mut past = json!({ "created": 1_700_000_000 });
        mask_time_now(&mut past, "created");
        assert_eq!(past["created"], 1_700_000_000);
        let mut other_key = json!({ "created_at": now, "created": now });
        mask_time_now(&mut other_key, "created_at");
        assert_eq!(
            other_key,
            json!({ "created_at": CREATED_NOW, "created": now })
        );
    }

    #[test]
    fn responses_stream_creation_times_are_masked() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let created = json!({ "type": "response.created", "response": { "created_at": now } });
        let delta = json!({ "type": "response.output_text.delta", "created_at": now });
        let text = format!(
            "event: response.created\ndata: {created}\n\nevent: response.output_text.delta\ndata: {delta}\n\n"
        );
        let case = Case::response("created", "{}", Vec::new());
        let frames = Translator::ClaudeResponsesStream
            .read(&case, text.as_bytes())
            .unwrap();
        assert_eq!(frames[0]["data"]["response"]["created_at"], CREATED_NOW);
        assert_eq!(frames[1]["data"]["created_at"], now);
        let body = json!({ "id": "msg_1", "created_at": now, "output": [] }).to_string();
        let response = Translator::ClaudeResponsesNonStream
            .read(&case, body.as_bytes())
            .unwrap();
        assert_eq!(response["created_at"], CREATED_NOW);
    }

    #[test]
    fn made_up_response_ids_are_masked() {
        let case = Case::response("generated", "{}", vec!["{}".into()]);
        let read = |output: Value| {
            Translator::OpenAIResponsesNonStream
                .read(&case, output.to_string().as_bytes())
                .unwrap()
        };
        let id = "resp_18a2b3c4d5e6f708_12";
        let response = read(json!({
            "id": id,
            "output": [
                { "id": "rs_18a2b3c4d5e6f708_12", "type": "reasoning" },
                { "id": "msg_resp_18a2b3c4d5e6f708_12_0", "type": "message" },
                { "call_id": "call_resp_18a2b3c4d5e6f708_12_0_1", "name": "18a2b3c4d5e6f708_12" }
            ]
        }));
        assert_eq!(
            response,
            json!({
                "id": "resp_(generated)",
                "output": [
                    { "id": "rs_(generated)", "type": "reasoning" },
                    { "id": "msg_resp_(generated)_0", "type": "message" },
                    { "call_id": "call_resp_(generated)_0_1", "name": "(generated)" }
                ]
            })
        );

        // Not made up: other forms, and an ID the provider sent.
        for id in ["resp_1", "resp_18A2_1", "resp_18a2_", "chatcmpl-1"] {
            assert_eq!(read(json!({ "id": id }))["id"], id);
        }
        let sent = Case::response(
            "sent",
            "{}",
            vec![json!({ "id": "resp_ab_1", "choices": [] }).to_string()],
        );
        let response = Translator::OpenAIResponsesNonStream
            .read(&sent, br#"{"id":"resp_ab_1"}"#)
            .unwrap();
        assert_eq!(response["id"], "resp_ab_1");
    }

    #[test]
    fn chat_to_claude_chunks_read_by_kind() {
        let case = Case::response("chunks", "{}", Vec::new());
        let chunks = json!([
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "{\"id\":\"x\"}",
            "event: a\ndata: {"
        ]);
        let read = Translator::OpenAIClaudeStream
            .read(&case, chunks.to_string().as_bytes())
            .unwrap();
        assert_eq!(
            read,
            json!([
                { "sse": [{ "event": "message_stop", "data": { "type": "message_stop" } }] },
                { "json": { "id": "x" } },
                { "sse": [{ "unended": "event: a\ndata: {" }] }
            ])
        );
    }

    #[test]
    fn only_generated_tool_ids_are_masked() {
        let mut value = json!({ "ids": [
            "toolu_1759400000000000000_3",
            "toolu_01ABC",
            "toolu_1_x",
            "toolu_aZ09aZ09aZ09aZ09aZ09aZ09",
            "toolu_aZ09aZ09aZ09aZ09aZ09aZ0_"
        ] });
        mask_generated_tool_ids(&mut value, &|_| false);
        assert_eq!(
            value["ids"],
            json!([
                "toolu_(generated-1)",
                "toolu_01ABC",
                "toolu_1_x",
                "toolu_(generated-2)",
                "toolu_aZ09aZ09aZ09aZ09aZ09aZ0_"
            ])
        );
    }

    #[test]
    fn each_generated_id_keeps_its_number() {
        let mut value = json!({
            "messages": [
                { "content": [
                    { "type": "tool_use", "id": "toolu_aZ09aZ09aZ09aZ09aZ09aZ09" },
                    { "type": "tool_use", "id": "toolu_1759400000000000000_3" }
                ] },
                { "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1759400000000000000_3" },
                    { "type": "tool_result", "tool_use_id": "toolu_aZ09aZ09aZ09aZ09aZ09aZ09" }
                ] }
            ]
        });
        mask_generated_tool_ids(&mut value, &|_| false);
        let ids: Vec<&str> = [
            "/messages/0/content/0/id",
            "/messages/0/content/1/id",
            "/messages/1/content/0/tool_use_id",
            "/messages/1/content/1/tool_use_id",
        ]
        .into_iter()
        .map(|pointer| value.pointer(pointer).and_then(Value::as_str).unwrap())
        .collect();
        assert_eq!(
            ids,
            [
                "toolu_(generated-1)",
                "toolu_(generated-2)",
                "toolu_(generated-2)",
                "toolu_(generated-1)"
            ]
        );
    }

    /// A Claude request with a call and its result.
    fn call_and_result(call_id: &str, result_id: &str, text: &str) -> String {
        json!({ "messages": [
            { "role": "assistant", "content": [
                { "type": "tool_use", "id": call_id, "name": "lookup", "input": {} }
            ] },
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": result_id, "content": text }
            ] }
        ] })
        .to_string()
    }

    #[test]
    fn a_result_for_another_generated_id_is_a_difference() {
        let case = Case::new("generated", "claude-sonnet-4-5", "{}");
        let (a, b) = ("toolu_1759400000000000000_3", "toolu_1759400000000000001_4");
        let read = |output: String| {
            Translator::ClaudeChatRequest
                .read(&case, output.as_bytes())
                .unwrap()
        };
        let go = read(call_and_result(a, a, "done"));
        // The same call under another generated ID.
        let rust = read(call_and_result(b, b, "done"));
        assert_eq!(go, rust);
        assert_eq!(
            go["messages"][1]["content"][0]["tool_use_id"],
            "toolu_(generated-1)"
        );

        let rust = read(call_and_result(a, b, "done"));
        let differences = compare::compare(&go, &rust, &[]).differences;
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].path, "$.messages[1].content[0].tool_use_id");
        assert_eq!(differences[0].rust, r#""toolu_(generated-2)""#);
    }

    #[test]
    fn only_ids_we_derive_from_compact_json_become_upstream_ids() {
        let read = |request: &str, id: Option<&str>| {
            let case = Case::new("derived", "gpt-5", request);
            let mut rust = convert_gemini_request_to_openai(
                "gpt-5",
                &serde_json::from_str(request).unwrap(),
                false,
            );
            let call = &mut rust["messages"][0]["tool_calls"][0]["id"];
            let ours = call.as_str().unwrap().to_owned();
            if let Some(id) = id {
                *call = id.into();
            }
            let read = Translator::OpenAIGeminiRequest
                .read(&case, rust.to_string().as_bytes())
                .unwrap();
            (ours, read["messages"][0]["tool_calls"][0]["id"].clone())
        };
        // Whitespace outside the hashed JSON changes nothing, so the ID is
        // compared as it is, and a wrong one stays wrong.
        let compact_args =
            r#"{ "contents":[{"role":"model","parts":[{"functionCall":{"name":"f","args":{}}}]}]}"#;
        let (ours, id) = read(compact_args, None);
        assert_eq!(ours, "call_15109fbc38df8528f7e437db");
        assert_eq!(id, ours);
        let wrong = "call_0123456789abcdef01234567";
        assert_eq!(read(compact_args, Some(wrong)).1, wrong);
        // Arguments written with spaces: our ID becomes upstream's.
        let spaced_args = r#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"f","args":{ "a" : 1 }}}]}]}"#;
        let (ours, id) = read(spaced_args, None);
        assert_eq!(ours, derived_call_id("call", 0, 0, "f", r#"{"a":1}"#));
        assert_eq!(id, derived_call_id("call", 0, 0, "f", r#"{ "a" : 1 }"#));
        assert_eq!(read(spaced_args, Some(wrong)).1, wrong);
        // A name that's an object is hashed as its text too.
        assert_eq!(
            derived_call_ids(
                r#"{"contents":[{"parts":[{"functionResponse":{"name":{ "n": 1.50 },"response":{"content":[ 1 ]}}}]}]}"#
            ),
            [(
                derived_call_id("response", 0, 0, r#"{"n":1.50}"#, "[1]"),
                derived_call_id("response", 0, 0, r#"{ "n": 1.50 }"#, "[ 1 ]"),
            )]
        );
        assert_eq!(gjson_string(raw_json::parse("1.50").as_ref(), false), "1.5");
        assert_eq!(gjson_string(raw_json::parse("-7").as_ref(), false), "-7");
        assert_eq!(
            gjson_string(raw_json::parse("1e400").as_ref(), false),
            "+Inf"
        );
        // Halfway between two shortest decimals: gjson rounds to even.
        assert_eq!(
            gjson_string(raw_json::parse("2156163594508435.25").as_ref(), false),
            "2156163594508435.2"
        );
    }

    #[test]
    fn client_tool_ids_are_not_masked() {
        let client = "toolu_aaaaaaaaaaaaaaaaaaaaaaaa";
        let request = json!({ "input": [
            { "type": "function_call", "call_id": client, "name": "lookup", "arguments": "{}" },
            { "type": "function_call_output", "call_id": client, "output": client }
        ] });
        let case = Case::new("client-id", "claude-sonnet-4-5", request.to_string());
        let read = |output: String| {
            Translator::ClaudeResponsesRequest
                .read(&case, output.as_bytes())
                .unwrap()
        };
        let go = read(call_and_result(client, client, client));
        assert_eq!(go["messages"][1]["content"][0]["tool_use_id"], client);

        // A result that answers some other ID, even one that looks generated.
        let other = "toolu_bbbbbbbbbbbbbbbbbbbbbbbb";
        let rust = read(call_and_result(client, other, other));
        let paths: Vec<String> = compare::compare(&go, &rust, &[])
            .differences
            .into_iter()
            .map(|difference| difference.path)
            .collect();
        assert_eq!(
            paths,
            [
                "$.messages[1].content[0].tool_use_id",
                "$.messages[1].content[0].content"
            ]
        );
    }

    #[test]
    fn client_ids_are_found_anywhere_in_the_input() {
        let id = "toolu_cccccccccccccccccccccccc";
        let found = |case: &Case| input_text(case).contains(id);
        // Within a longer string.
        let request = json!({ "input": [{ "call_id": format!(" {id} ") }] });
        assert!(found(&Case::new("padded", "", request.to_string())));
        // Escaped, in JSON held in a string.
        let escaped = format!("toolu_{}u0063{}", '\\', "c".repeat(23));
        let arguments = format!(r#"{{"id":"{escaped}"}}"#);
        let request = json!({ "input": [{ "arguments": arguments }] }).to_string();
        assert!(!request.contains(id));
        assert!(found(&Case::new("escaped", "", request)));
        // In the provider's events.
        let events = vec![format!("event: x\ndata: {}\n\n", json!({ "id": id }))];
        assert!(found(&Case::response("event", "{}", events)));
        assert!(!found(&Case::response("none", "{}", Vec::new())));
    }
}
