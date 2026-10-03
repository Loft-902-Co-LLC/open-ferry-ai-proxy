//! The translators under test, run on our side, and how to read each one's
//! output as JSON so ours and upstream's can be compared.

use std::cell::OnceCell;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
use open_ferry_translate::models::ModelCatalog;
use open_ferry_translate::openai::chat_completions::{
    OpenAIToOpenAIStream, convert_openai_request_to_openai,
    convert_openai_response_to_openai_non_stream,
};
use open_ferry_translate::openai::claude::{
    OpenAIToClaudeStream, convert_claude_request_to_openai,
    convert_claude_request_to_openai_with_compat, convert_openai_response_to_claude_non_stream,
};
use open_ferry_translate::openai::responses::{
    OpenAIToOpenAIResponsesStream,
    convert_openai_chat_completions_response_to_openai_responses_non_stream,
    convert_openai_responses_request_to_openai_chat_completions,
};
use open_ferry_translate::registry::{Format, Registry, ResponseContext, ResponseTransform};
use serde_json::{Value, json};

use crate::cases::Case;
use crate::compare::{self, Deviation, JsonAt, JsonForm};
use crate::signature;

/// How an empty non-streaming output reads, unlike any JSON a response holds.
const NO_OUTPUT: &str = "(no output)";

/// What a registry non-streaming case reads as when the translation failed:
/// upstream's registry returned nil, and ours `None`.
const FAILED: &str = "(failed)";

/// How the Responses stream translator's harness writes a line it returned unchanged.
const UNCHANGED: &str = "=";

/// What a response's `created` or `created_at` is replaced with when it is
/// the current time, which upstream and we each read from the clock.
const CREATED_NOW: &str = "(now)";

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
        }
    }

    /// Runs our port on `case`, returning its output in the form [`Self::read`] gives.
    pub fn run_rust(self, case: &Case) -> Result<Value, String> {
        let request = serde_json::from_str::<Value>(&case.request);
        let final_event = || {
            case.events
                .first()
                .and_then(|event| serde_json::from_str(event).ok())
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
                let translated = serde_json::from_str(&case.translated_request).unwrap_or_default();
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
                let translated = serde_json::from_str(&case.translated_request).unwrap_or_default();
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
                let translated = serde_json::from_str(&case.translated_request).unwrap_or_default();
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
                let translated = serde_json::from_str(&case.translated_request).unwrap_or_default();
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
                let translated = serde_json::from_str(&case.translated_request).unwrap_or_default();
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
                let translated = serde_json::from_str(&case.translated_request).unwrap_or_default();
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
                let translated = serde_json::from_str(&case.translated_request).unwrap_or_default();
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
        }
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
            | Self::OpenAIChatNonStream => &[],
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
                _ => None,
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
                _ => None,
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
                _ => None,
            },
            _ => None,
        }
    }

    /// Takes out of upstream's output what we leave out on purpose, returning
    /// the deviation that accounts for it.
    ///
    /// Upstream makes up a Claude `metadata.user_id` when the client sent
    /// none; we don't.
    pub fn drop_deliberate_omissions(self, case: &Case, go: &mut Value) -> Option<Deviation> {
        if let Some(native) = self.native(case) {
            return native.drop_deliberate_omissions(case, go);
        }
        if !matches!(
            self,
            Self::ClaudeChatRequest
                | Self::ClaudeChatRequestCompat
                | Self::ClaudeResponsesRequest
                | Self::ClaudeResponsesRequestCompat
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
    pub fn read(self, case: &Case, output: &[u8]) -> Option<Value> {
        let text = String::from_utf8_lossy(output);
        let native = self.native(case);
        let mut value = match self {
            Self::RegistryStream => return read_registry_stream(case, native, &text),
            Self::RegistryNonStream => return read_registry_non_stream(case, native, &text),
            Self::RegistryRequest if native.is_some() => return native?.read(case, output),
            Self::RegistryRequest | Self::RegistryLookup => {
                return serde_json::from_str(&text).ok();
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
            | Self::CompletionsResponse => return serde_json::from_str(&text).ok(),
            Self::ResponsesStream | Self::ChatStream => return read_lines(&text),
            Self::OpenAIChatStream => {
                let chunks: Vec<String> = serde_json::from_str(&text).ok()?;
                return Some(chunks.into_iter().map(Value::String).collect());
            }
            Self::OpenAIChatNonStream => return Some(Value::String(text.into_owned())),
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
                if text.is_empty() =>
            {
                return Some(NO_OUTPUT.into());
            }
            Self::ResponsesNonStream => return serde_json::from_str(&text).ok(),
            Self::ChatNonStream | Self::ClaudeChatNonStream => {
                let mut value: Value = serde_json::from_str(&text).ok()?;
                mask_time_now(&mut value, "created");
                return Some(value);
            }
            Self::ClaudeResponsesNonStream => {
                let mut value: Value = serde_json::from_str(&text).ok()?;
                mask_time_now(&mut value, "created_at");
                return Some(value);
            }
            Self::OpenAIResponsesNonStream => {
                let mut value: Value = serde_json::from_str(&text).ok()?;
                mask_time_now(&mut value, "created_at");
                mask_generated_response_id(&mut value, case);
                return Some(value);
            }
            Self::OpenAIClaudeStream => {
                let chunks: Vec<String> = serde_json::from_str(&text).ok()?;
                chunks
                    .iter()
                    .map(|chunk| match serde_json::from_str::<Value>(chunk) {
                        Ok(message) => json!({ "json": message }),
                        Err(_) => json!({ "sse": sse_frames(chunk) }),
                    })
                    .collect()
            }
            Self::OpenAIClaudeNonStream => serde_json::from_str(&text).ok()?,
            Self::ClaudeChatRequest
            | Self::ClaudeChatRequestCompat
            | Self::ClaudeResponsesRequest
            | Self::ClaudeResponsesRequestCompat => serde_json::from_str(&text).ok()?,
            Self::Stream => sse_frames(&text),
            Self::NonStream => serde_json::from_str(&text).ok()?,
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
fn read_lines(text: &str) -> Option<Value> {
    let lines: Vec<String> = serde_json::from_str(text).ok()?;
    let json = |text: &str| serde_json::from_str::<Value>(text).ok();
    let lines = lines
        .into_iter()
        .map(|line| {
            if line == UNCHANGED {
                Value::String(line)
            } else if let Some(data) = line.strip_prefix("data: ").and_then(json) {
                json!({ "data": data })
            } else if let Some(value) = json(&line) {
                json!({ "json": value })
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
fn read_chunks(text: &str) -> Option<Value> {
    let chunks: Vec<Option<String>> = serde_json::from_str(text).ok()?;
    let chunks = chunks
        .into_iter()
        .map(|chunk| match chunk {
            None => Value::Null,
            Some(chunk) => match serde_json::from_str(&chunk) {
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
        None => Some(serde_json::from_str(output).unwrap_or_else(|_| output.into())),
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
            | Translator::OpenAIResponsesStream,
        ) => all().map(String::as_str).collect::<String>(),
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
        )
    );
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

/// Splits SSE text into `{"event": …, "data": …}` frames. Anything that isn't
/// an `event: …\ndata: <JSON>\n\n` frame is kept as `{"unparsed": text}`, and
/// text after the last blank line as `{"unended": text}`, so they show up as
/// differences.
fn sse_frames(text: &str) -> Value {
    let mut frames = Vec::new();
    let mut rest = text;
    while let Some((frame, after)) = rest.split_once("\n\n") {
        let parsed = frame
            .strip_prefix("event: ")
            .and_then(|rest| rest.split_once("\ndata: "))
            .and_then(|(event, data)| {
                let data: Value = serde_json::from_str(data).ok()?;
                Some(json!({ "event": event, "data": data }))
            });
        frames.push(parsed.unwrap_or_else(|| json!({ "unparsed": frame })));
        rest = after;
    }
    if !rest.is_empty() {
        frames.push(json!({ "unended": rest }));
    }
    Value::Array(frames)
}

/// Replaces a response's creation time, held in `key`, if it is within an
/// hour of now.
fn mask_time_now(value: &mut Value, key: &str) {
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
fn mask_generated_tool_ids(value: &mut Value, from_client: &dyn Fn(&str) -> bool) {
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
fn input_text(case: &Case) -> String {
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
