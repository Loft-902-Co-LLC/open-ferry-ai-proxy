// Ported from CLIProxyAPI internal/runtime/executor/helps/usage_helpers.go
// (NewUsageReporter, NewExecutorUsageReporter, publishWithOutcome,
// EnsurePublished, publishAttemptRecord, PublishAdditionalModel,
// buildAdditionalModelRecord, buildRecordForModel,
// failFromErrors, latency, warnModelSubstitution, authIndexForLog,
// resolveUsageSource, resolveUsageAuthType, StreamUsageBuffer.Publish,
// StreamUsageBuffer.PublishFailure), the usage reporting of
// internal/runtime/executor/claude_executor_execute.go,
// claude_executor_stream.go, codex_executor_execute.go,
// codex_executor_stream.go, codex_executor_terminal.go
// (observeCodexTokenEvent), codex_executor_request.go
// (publishCodexImageToolUsage, codexImageGenerationToolModel),
// codex_websockets_executor.go,
// xai_websockets_executor.go,
// xai_executor_execute.go, xai_executor_stream.go, xai_executor_media.go
// (executeImages, executeVideos), xai_executor_speech.go (executeSpeech),
// gemini_executor.go (including executeInteractions and
// executeInteractionsStream), gemini_vertex_executor.go and
// openai_compat_executor.go, internal/redisqueue/plugin.go (HandleUsage's
// gate), sdk/cliproxy/auth/token_fingerprint.go (AccessTokenSHA256,
// accessTokenForFingerprint), sdk/cliproxy/auth/conductor_execution.go
// (requestedModelAliasFromOptions, generateFromOptions),
// and sdk/cliproxy/usage/manager.go (ServiceTierFromContext,
// GenerateFromContext, GenerateEnabled) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The usage reporter: the [`Tap`] that turns each executor call of a
//! client's call into one usage record.
//!
//! The tap sees each executor call's attempts, their answers and how the
//! call ended, and reads the answer as the executor would have:
//! - An answer read whole (a non-streaming call) is kept, up to
//!   [`BODY_BOUND`], and read when the call completes: Claude's as JSON, or
//!   line by line when it came as an event stream; Gemini's, Gemini
//!   Interactions' and an OpenAI-compatible provider's as JSON; Codex's
//!   lines up to its
//!   `response.completed` or `response.incomplete`; a Codex compaction's
//!   as OpenAI JSON.
//! - A stream is read line by line as it comes, each line up to
//!   [`LINE_BOUND`]: the latest token counts and the model it names are
//!   kept. A Codex stream, and each message on a Codex WebSocket, is read
//!   event by event, and its counts are those of its first
//!   `response.completed`, `response.incomplete` or `response.done`.
//!   Reading stops at the protocol's terminal marker, as upstream's
//!   executors stop reading there: an OpenAI-compatible stream's `[DONE]`,
//!   Claude's `message_stop`, a Codex terminal event. Nothing after it
//!   changes the counts or the model, whether it comes later in the same
//!   chunk or in a later one.
//!
//! The record is published once per executor call, when it ends: with the
//! counts read, or with none when the answer named none, or as a failure
//! with the call's error. A call canceled before its answer came is a
//! failure with status 499 and the body `context canceled`. So is a Claude
//! stream canceled before its `message_stop`, with the counts it had read
//! (upstream's `StreamUsageBuffer.PublishFailure`). Each record keeps the
//! time to first token (see [`super::ttft`]), the latest answer's headers
//! with credentials masked, and the model the answer named. When that model
//! is not the one asked for, a warning is logged, each time, naming the
//! provider (`unknown` when the call names none). A Codex WebSocket's time
//! to first token starts
//! when its request is sent on the open connection ([`Tap::request_sent`]),
//! not at the dial.
//!
//! A Codex call over HTTP, a stream or not, makes a second record when its
//! terminal event has the image generation tool's counts
//! (`response.tool_usage.image_gen`, upstream's
//! `publishCodexImageToolUsage`): after the call's own record, one for the
//! model of the request's first `image_generation` tool (`gpt-image-2`
//! when it names none), with the same request, but its own execution ID
//! and no response model, unless the tool's model is the one sent. It is
//! skipped when its counts are all zero; its event alone still makes the
//! call's record when the call's own counts are missing.
//!
//! An xAI answer is read as a Codex one, but as upstream's xAI executor
//! reads it: the served model is named where an OpenAI-compatible provider
//! names it; a stream's time to first token is its first byte, and its
//! counts are those of its last `response.completed` or
//! `response.incomplete`, published when it ends, with none when it named
//! none; a compaction, streamed or not, is read whole as OpenAI JSON; an
//! image or video call is read whole for the model it names alone, with no
//! counts, as upstream's `executeImages` and `executeVideos` read it
//! (`ObserveResponseModel`); a speech call's answer, audio, isn't read: its
//! record names no response model and no counts, as upstream's
//! `executeSpeech` publishes it, and its time to first token is its first
//! byte.
//! Each message on an xAI WebSocket is read as an event, as upstream's
//! `XAIWebsocketsExecutor` reads it: its time to first token starts when
//! its request is sent, as a Codex WebSocket's does, and ends at its first
//! message; its counts are those of its `response.completed` or
//! `response.done`, published when it ends, with none when it named none.
//!
//! A record's `session_id` is the first of the session headers the client
//! sent (`X-Claude-Code-Session-Id`, `Session-Id`, `Session_id`,
//! `X-Session-Id`) that holds no control character and is no longer than
//! 256 bytes once trimmed. Nothing else is ever taken for one (policy).
//!
//! Deviations from upstream:
//! - The tap reads the traffic the executor reports instead of the
//!   executor publishing its own record, so a call's `requested_at` and
//!   latency start at its first attempt, not when the executor started
//!   building it. An executor call that failed before it sent anything
//!   is recorded from the credential the manager gave it, its model the
//!   one routed.
//! - A Codex stream's counts are kept at its terminal event and published
//!   when the call ends, with the latency of that event: open-ferry's
//!   executor turns a terminal failure into an error after it, which makes
//!   the record a failure.
//! - Gemini stream lines are read without upstream's
//!   `FilterSSEUsageMetadata`, which only drops usage from lines before
//!   the last.
//! - A Gemini Interactions stream is read line by line, where upstream
//!   reads each SSE frame's `data:` lines joined: an event whose JSON is
//!   split over several `data:` lines gives no counts. Its calls that the
//!   executor hands to the Gemini executor, for a client format it doesn't
//!   send natively or a credential that isn't `gemini-interactions`, are
//!   recorded as `gemini`'s.
//! - `reasoning_effort` is always empty: upstream reads it from the
//!   translated request, which the tap doesn't see.
//! - A failure's body is the error's text, scrubbed of every secret the
//!   attempts sent, the credential's own keys and tokens (also for a call
//!   that failed before it sent anything) and the client's key, each
//!   however short: the queue is served to the management API and can be
//!   written to disk, so it is scrubbed as a file is, not as what a client
//!   is given. Upstream writes the error's response body as it is.
//! - The answer's headers are masked as the request log masks them.
//! - The substitution warning is a `tracing` warning with the request's
//!   ID as a field.
//! - The models a record names, the one sent and the one the answer served,
//!   and the two the substitution warning quotes, are scrubbed of the
//!   secrets the attempts sent, the credential's own and the client's key,
//!   each however short, as a failure's body is: the served model is
//!   whatever the upstream said, and may echo a token it was sent, while the
//!   record is served to the management API and written to disk and the
//!   warning goes to main.log. The substituted-model check still reads the
//!   models as they are. Upstream records and logs both as they are.
//! - Meta's calls are read as Codex's, an Execute as the Codex Execute and a
//!   Stream as the Codex stream, and recorded as `MetaExecutor`'s. The tap
//!   keeps the counts of the first terminal event of either (a stream's
//!   `response.done` too, as for Codex), where upstream's Meta executor
//!   takes the latest `response.completed` or `response.incomplete` of a
//!   stream and the one its translation found in an Execute's.
//! - A failure whose error keeps the answer's usage
//!   ([`ExecError::keeps_usage`], set by the Meta and xAI executors when an
//!   answer's `apply_patch` call can't be carried over) keeps the counts
//!   the tap read of the whole answer, or of the stream as far as it was
//!   read. Upstream's executors keep what they had observed when the bridge
//!   failed, so where the bridge fails at an event before the terminal one
//!   of an answer read whole, upstream's record has no counts and this one
//!   has the terminal event's.
//! - A stream's reading stops at its terminal line, not at the blank line
//!   after it: an OpenAI-compatible stream at the `[DONE]` line, where
//!   upstream's scanner leaves its loop at the next line, and Claude's at
//!   the `message_stop` data line. Nothing else upstream reads between
//!   those lines carries counts.
//! - A call canceled before its executor answered, whether the executor was
//!   still dialing or waiting for the headers, is a failure with status 499
//!   and the body `context canceled` in every stream mode, as upstream's
//!   `TrackFailure` records the error its canceled send returns; upstream's
//!   text is the HTTP client's, which names the method and the URL.
//! - A Claude stream dropped before its `message_stop` is a failure with the
//!   counts it had read and status 499, for any credential. Upstream makes
//!   its cancellation error (`newClaudeOAuthCancellationError`) only for an
//!   OAuth credential, and records another credential's read cut off by the
//!   canceled context as the scanner's error.
//! - A Gemini or Vertex AI stream dropped once it was answered is a
//!   success, with the counts it had read. Upstream's is too when the client
//!   went away after the terminal chunk reached it (v8.0.20) or while the
//!   executor waited to hand a chunk on, but a failure when the cancel cut
//!   off a read before the terminal chunk; a dropped stream here can't tell
//!   those apart.
//! - The image generation tool's model is read from the request as it was
//!   sent, after the payload rules; upstream reads the body before its
//!   payload finalizer, so a rule that changes the tool's model changes
//!   the record's here. The tool's record is published when the call ends,
//!   right after the call's own, as long as that record is made from the
//!   terminal event; upstream publishes both at the event.
//! - A Codex WebSocket's time to first token starts when its request goes
//!   out on the open connection, as upstream's `StartResponseTTFT` does, and
//!   again when a send is tried on a new connection; the first start wins.
//!   The request is still announced before the dial, as upstream's request
//!   log has it, so the tap is told the send as a step of its own
//!   ([`Tap::request_sent`]).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use http::HeaderMap;
use open_ferry_translate::go;
use open_ferry_translate::thinking::base_model_name;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::Inner;
use super::accounting::{Detail, ensure_token_breakdown_for_provider};
use super::json::{self, Doc};
use super::observer::{ClientKey, EventCredential, UsageEvent};
use super::parse::{
    StreamUsageBuffer, parse_claude_usage, parse_codex_image_tool_usage, parse_codex_usage,
    parse_gemini_stream_usage, parse_gemini_usage, parse_interactions_stream_usage,
    parse_interactions_usage, parse_openai_usage,
};
use super::record_json::Record;
use super::response_model::{ResponseModel, is_model_substituted};
use super::ttft::{Ttft, is_responses_token_event};
use crate::auth::Auth;
use crate::exec::{ExecError, Format, Options, Request};
use crate::observe::redact::{Policy, Secrets};
use crate::observe::{
    AttemptKind, AttemptRequest, Outcome, RequestContext, SelectedAuth, Tap, mask,
};
use crate::session::normalize_explicit_id;

/// The most of an answer read whole that is kept to read its tokens; past
/// it, the answer is recorded without them.
pub(crate) const BODY_BOUND: usize = 32 << 20;

/// The longest stream line read; a longer one is skipped.
pub(crate) const LINE_BOUND: usize = 16 << 20;

/// The session headers a client may send, in the order they are read
/// (upstream's client request metadata).
const SESSION_HEADERS: [&str; 4] = [
    "x-claude-code-session-id",
    "session-id",
    "session_id",
    "x-session-id",
];

/// The image generation tool's model when the request's tool names none
/// (upstream's `codexDefaultImageToolModel`).
const DEFAULT_IMAGE_TOOL_MODEL: &str = "gpt-image-2";

/// The service tier of a request that names none (upstream's
/// `DefaultServiceTier`).
const DEFAULT_SERVICE_TIER: &str = "auto";

/// A call canceled before its answer came, as Go's context error says it.
const CANCELED_MESSAGE: &str = "context canceled";

/// The status of a canceled call (upstream's `HTTPStatusFromError` for
/// `context.Canceled`).
const CANCELED_STATUS: i64 = 499;

/// What the records of one client call share, read when the call is made.
struct Shared {
    /// The model as the client named it, trimmed.
    alias: String,
    /// The model routed, for a call that sent nothing.
    request_model: String,
    session_id: String,
    service_tier: String,
    generate: bool,
    stream: bool,
}

impl Shared {
    fn new(request: &Request, options: &Options) -> Self {
        let payload = &options.original_request;
        let doc = (contains(payload, br#""service_tier""#) || contains(payload, br#""generate""#))
            .then(|| Doc::scan(payload));
        let service_tier = doc
            .as_ref()
            .map(|doc| doc.get("service_tier").string().trim().to_owned())
            .filter(|tier| !tier.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVICE_TIER.to_owned());
        let generate = generate_enabled(
            doc.as_ref()
                .and_then(|doc| doc.get("generate").value().and_then(Value::as_bool)),
        );
        Self {
            alias: options.metadata.requested_model.trim().to_owned(),
            request_model: request.model.clone(),
            session_id: session_id(&options.headers),
            service_tier,
            generate,
            stream: options.stream,
        }
    }
}

/// Whether a call asked for generation, from the client's `generate` flag
/// if it sent one: only `false` turns it off (upstream's `GenerateEnabled`
/// and `GenerateFromContext`).
pub(crate) fn generate_enabled(generate: Option<bool>) -> bool {
    generate.unwrap_or(true)
}

/// The first session header the client sent that may be taken as it is.
fn session_id(headers: &HeaderMap) -> String {
    SESSION_HEADERS
        .iter()
        .flat_map(|name| headers.get_all(*name))
        .map(|value| normalize_explicit_id(&String::from_utf8_lossy(value.as_bytes())))
        .find(|id| !id.is_empty())
        .unwrap_or_default()
}

/// Whether `haystack` holds `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// How an executor call's answer is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Not recorded: a token count, a plain HTTP call, or a WebSocket that
    /// isn't Codex's or xAI's.
    Ignored,
    ClaudeExecute,
    ClaudeStream,
    GeminiExecute,
    GeminiStream,
    InteractionsExecute,
    InteractionsStream,
    OpenAiExecute,
    OpenAiStream,
    CodexCompact,
    CodexExecute,
    CodexStream,
    CodexWebsocket,
    /// A call to xAI's Images or video API.
    XaiMedia,
    /// A call to xAI's text-to-speech API, whose answer, audio, isn't read.
    XaiSpeech,
    XaiStream,
    XaiWebsocket,
}

impl Mode {
    fn of(provider: &str, kind: AttemptKind, format: &Format) -> Self {
        match (provider, kind) {
            (_, AttemptKind::CountTokens | AttemptKind::Http) => Self::Ignored,
            ("codex", AttemptKind::Websocket) => Self::CodexWebsocket,
            // A call to Codex's Image API answers as the OpenAI Images API
            // does and names no model.
            ("codex", AttemptKind::Execute) if format.as_str() == Format::OPENAI_IMAGE.as_str() => {
                Self::CodexCompact
            }
            ("codex", AttemptKind::Stream) if format.as_str() == Format::OPENAI_IMAGE.as_str() => {
                Self::OpenAiStream
            }
            ("codex", AttemptKind::Execute)
                if format.as_str() == Format::OPENAI_RESPONSE.as_str() =>
            {
                Self::CodexCompact
            }
            ("codex", AttemptKind::Execute) => Self::CodexExecute,
            ("codex", AttemptKind::Stream) => Self::CodexStream,
            // A call to xAI's text-to-speech API answers with audio.
            ("xai", AttemptKind::Execute) if format.as_str() == Format::OPENAI_SPEECH.as_str() => {
                Self::XaiSpeech
            }
            // A call to xAI's Images or video API names a model at most, no
            // counts.
            ("xai", AttemptKind::Execute)
                if format.as_str() == Format::OPENAI_IMAGE.as_str()
                    || format.as_str() == Format::OPENAI_VIDEO.as_str() =>
            {
                Self::XaiMedia
            }
            ("xai", AttemptKind::Execute)
                if format.as_str() == Format::OPENAI_RESPONSE.as_str() =>
            {
                Self::CodexCompact
            }
            // A compaction trigger reads its answer whole and names its model.
            ("xai", AttemptKind::Stream) if format.as_str() == Format::OPENAI_RESPONSE.as_str() => {
                Self::OpenAiExecute
            }
            ("xai", AttemptKind::Execute) => Self::CodexExecute,
            ("xai", AttemptKind::Stream) => Self::XaiStream,
            ("xai", AttemptKind::Websocket) => Self::XaiWebsocket,
            (_, AttemptKind::Websocket) => Self::Ignored,
            ("meta", AttemptKind::Execute) => Self::CodexExecute,
            ("meta", AttemptKind::Stream) => Self::CodexStream,
            // open-ferry's `claude-cli` answers in Claude's events.
            ("claude" | "claude-cli", AttemptKind::Execute) => Self::ClaudeExecute,
            ("claude" | "claude-cli", AttemptKind::Stream) => Self::ClaudeStream,
            ("gemini" | "vertex", AttemptKind::Execute) => Self::GeminiExecute,
            ("gemini" | "vertex", AttemptKind::Stream) => Self::GeminiStream,
            ("gemini-interactions", AttemptKind::Execute) => Self::InteractionsExecute,
            ("gemini-interactions", AttemptKind::Stream) => Self::InteractionsStream,
            (_, AttemptKind::Execute) => Self::OpenAiExecute,
            (_, AttemptKind::Stream) => Self::OpenAiStream,
        }
    }

    /// Whether the answer is read whole, when the call completes.
    fn reads_whole(self) -> bool {
        matches!(
            self,
            Self::ClaudeExecute
                | Self::GeminiExecute
                | Self::InteractionsExecute
                | Self::OpenAiExecute
                | Self::CodexCompact
                | Self::CodexExecute
                | Self::XaiMedia
                | Self::XaiSpeech
        )
    }
}

/// The name of the upstream executor type that serves `provider`, as
/// upstream's records name it (upstream's `ExecutorTypeName`).
fn executor_type(provider: &str, kind: Option<AttemptKind>) -> &'static str {
    match provider {
        "codex" if kind == Some(AttemptKind::Websocket) => "CodexWebsocketsExecutor",
        "xai" if kind == Some(AttemptKind::Websocket) => "XAIWebsocketsExecutor",
        "codex" => "CodexExecutor",
        "claude" => "ClaudeExecutor",
        // open-ferry's own; upstream has no such executor.
        "claude-cli" => "ClaudeCliExecutor",
        "meta" => "MetaExecutor",
        "gemini" | "gemini-interactions" => "GeminiExecutor",
        "vertex" => "GeminiVertexExecutor",
        "xai" => "XAIExecutor",
        _ => "OpenAICompatExecutor",
    }
}

/// What a record says of the credential.
#[derive(Default)]
struct Credential {
    id: String,
    index: String,
    /// The credential's label, for the usage observer.
    label: String,
    auth_type: &'static str,
    source: String,
    access_token_sha256: String,
}

impl Credential {
    fn of(auth: &Auth, client_key: &str) -> Self {
        Self {
            id: auth.id.clone(),
            index: auth.index.trim().to_owned(),
            label: auth.label.trim().to_owned(),
            auth_type: auth.auth_kind().map_or("", |kind| kind.as_str()),
            source: usage_source(auth, client_key),
            access_token_sha256: access_token_sha256(auth),
        }
    }
}

/// The account or key a record is counted under (upstream's
/// `resolveUsageSource`): a Vertex credential's project, the account the
/// credential names, its email or key, else the client's key.
pub(crate) fn usage_source(auth: &Auth, client_key: &str) -> String {
    fn trimmed(value: Option<&str>) -> Option<&str> {
        value.map(str::trim).filter(|value| !value.is_empty())
    }
    if go::equal_fold(auth.provider.trim(), "vertex")
        && let Some(project) = trimmed(auth.metadata_str("project_id"))
            .or_else(|| trimmed(auth.metadata_str("project")))
    {
        return project.to_owned();
    }
    [
        auth.account_info().map(|(_, value)| value),
        auth.metadata_str("email"),
        auth.attribute("api_key"),
        Some(client_key),
    ]
    .into_iter()
    .find_map(trimmed)
    .unwrap_or_default()
    .to_owned()
}

/// The SHA-256 of the credential's access token in hex, or empty when it
/// has none (upstream's `AccessTokenSHA256`).
pub(crate) fn access_token_sha256(auth: &Auth) -> String {
    let token_of = |object: &serde_json::Map<String, Value>| {
        ["access_token", "accessToken"].into_iter().find_map(|key| {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_owned)
        })
    };
    let token = token_of(&auth.metadata).or_else(|| {
        ["token", "Token"]
            .into_iter()
            .find_map(|key| match auth.metadata.get(key) {
                Some(Value::Object(object)) => token_of(object),
                _ => None,
            })
    });
    let Some(token) = token else {
        return String::new();
    };
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Go's `textproto.CanonicalMIMEHeaderKey`: each word's first letter in
/// upper case and the rest in lower case; a name with a byte that can't be
/// in a header name is left as it is.
pub(crate) fn canonical_header_key(name: &str) -> String {
    let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    if !name.bytes().all(token) {
        return name.to_owned();
    }
    let mut upper = true;
    name.chars()
        .map(|c| {
            let out = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            out
        })
        .collect()
}

/// `headers` as a record keeps them: canonical names in byte order, each
/// with its values, credentials masked.
fn record_headers(headers: &HeaderMap) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = headers
        .keys()
        .map(|name| {
            let canonical = canonical_header_key(name.as_str());
            let values = headers
                .get_all(name)
                .iter()
                .map(|value| {
                    mask::mask_header_value(&canonical, &String::from_utf8_lossy(value.as_bytes()))
                })
                .collect();
            (canonical, values)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Stream lines, split as they come in chunks.
#[derive(Default)]
struct Lines {
    partial: Vec<u8>,
    /// The line being read is past [`LINE_BOUND`] and is skipped.
    skipping: bool,
}

impl Lines {
    /// Gives `each` every line `chunk` ends, without its `\r\n` or `\n`.
    fn feed(&mut self, chunk: &[u8], mut each: impl FnMut(&[u8])) {
        let mut rest = chunk;
        while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
            let head = rest.get(..newline).unwrap_or_default();
            rest = rest.get(newline + 1..).unwrap_or_default();
            if self.skipping {
                self.skipping = false;
                self.partial.clear();
                continue;
            }
            if self.partial.is_empty() {
                if head.len() <= LINE_BOUND {
                    each(trim_cr(head));
                }
            } else if self.partial.len() + head.len() <= LINE_BOUND {
                self.partial.extend_from_slice(head);
                each(trim_cr(&self.partial));
                self.partial.clear();
            } else {
                self.partial.clear();
            }
        }
        if rest.is_empty() || self.skipping {
            return;
        }
        if self.partial.len() + rest.len() > LINE_BOUND {
            self.partial = Vec::new();
            self.skipping = true;
        } else {
            self.partial.extend_from_slice(rest);
        }
    }

    /// Gives `each` the last line, which had no line end.
    fn flush(&mut self, mut each: impl FnMut(&[u8])) {
        let partial = std::mem::take(&mut self.partial);
        if !self.skipping && !partial.is_empty() {
            each(trim_cr(&partial));
        }
        self.skipping = false;
    }
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Lines of an answer read whole.
fn lines_of(body: &[u8]) -> impl Iterator<Item = &[u8]> {
    body.split(|&b| b == b'\n').map(trim_cr)
}

/// A Codex event's terminal success, if `payload` is one of `types`.
fn is_terminal(payload: &[u8], types: &[&str]) -> bool {
    if !types.iter().any(|t| contains(payload, t.as_bytes())) {
        return false;
    }
    let event = Doc::scan(payload).get("type").string().into_owned();
    types.contains(&event.as_str())
}

/// Whether a stream line is the `data:` line of Claude's `message_stop`
/// event, which ends the reply (upstream's `observeClaudeStreamLine`).
fn is_message_stop(line: &[u8]) -> bool {
    let Some(data) = json::trim_space(line).strip_prefix(b"data:") else {
        return false;
    };
    let data = json::trim_space(data);
    json::valid(data) && Doc::scan(data).get("type").string() == "message_stop"
}

/// Whether a stream line is `[DONE]`.
fn is_done(line: &[u8]) -> bool {
    let line = json::trim_space(line);
    let data = line.strip_prefix(b"data:").map_or(line, json::trim_space);
    data == b"[DONE]"
}

/// Why a call failed.
#[derive(Clone, Default)]
struct Failure {
    status: i64,
    body: String,
    /// Whether the answer's counts still count ([`ExecError::keeps_usage`]).
    keeps_usage: bool,
}

impl Failure {
    fn of(error: &ExecError) -> Self {
        Self {
            status: i64::from(error.http_status()),
            body: error.to_string().trim().to_owned(),
            keeps_usage: error.keeps_usage,
        }
    }

    fn canceled() -> Self {
        Self {
            status: CANCELED_STATUS,
            body: CANCELED_MESSAGE.to_owned(),
            keeps_usage: false,
        }
    }
}

/// What a call publishes.
struct Publication {
    detail: Detail,
    failure: Option<Failure>,
    latency: Duration,
}

/// A Codex terminal success, kept until the call ends.
struct Held {
    /// The counts, or none for an answer that named none.
    detail: Option<Detail>,
    /// The latency at the event, for a stream.
    latency: Option<Duration>,
    /// The image generation tool's counts, for a Codex call over HTTP whose
    /// event has them.
    image: Option<Detail>,
}

/// The model of the request's first `image_generation` tool, trimmed, or
/// [`DEFAULT_IMAGE_TOOL_MODEL`] when that tool names none or there is none
/// (upstream's `codexImageGenerationToolModel`).
fn image_generation_tool_model(body: &[u8]) -> String {
    let doc = Doc::parse(body);
    let tool = doc
        .get("tools")
        .array()
        .into_iter()
        .find(|tool| tool.get("type").string() == "image_generation");
    tool.map(|tool| tool.get("model").string().trim().to_owned())
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| DEFAULT_IMAGE_TOOL_MODEL.to_owned())
}

/// The executor call being read.
struct Call {
    mode: Mode,
    provider: String,
    model: String,
    executor_type: &'static str,
    credential: Credential,
    requested_at: DateTime<Utc>,
    started: Instant,
    secrets: Secrets,
    ttft: Ttft,
    response_model: ResponseModel,
    buffer: StreamUsageBuffer,
    lines: Lines,
    body: Vec<u8>,
    /// The answer outgrew [`BODY_BOUND`].
    body_overflow: bool,
    event_stream: bool,
    headers: Vec<(String, Vec<String>)>,
    seen_done: bool,
    held: Option<Held>,
    /// Some of the attempt's answer was read: a body chunk, or a message.
    answered: bool,
    /// A stream's terminal marker was read (`[DONE]` or Claude's
    /// `message_stop`): nothing after it counts. A Codex stream's first
    /// terminal event is held, and the rest don't replace it.
    ended: bool,
    /// The request of a Codex call over HTTP, whose terminal event's image
    /// generation tool counts are read; none for any other call.
    image_tool_request: Option<bytes::Bytes>,
    /// The image generation tool's counts the published record came with.
    image: Option<Detail>,
}

impl Call {
    fn new(
        mode: Mode,
        provider: String,
        model: String,
        executor_type: &'static str,
        credential: Credential,
        started: Instant,
    ) -> Self {
        Self {
            mode,
            provider,
            model,
            executor_type,
            credential,
            requested_at: Utc::now(),
            started,
            secrets: Secrets::new(),
            ttft: Ttft::default(),
            response_model: ResponseModel::default(),
            buffer: StreamUsageBuffer::default(),
            lines: Lines::default(),
            body: Vec::new(),
            body_overflow: false,
            event_stream: false,
            headers: Vec::new(),
            seen_done: false,
            held: None,
            answered: false,
            ended: false,
            image_tool_request: None,
            image: None,
        }
    }

    /// A call whose executor failed before it sent anything, made with
    /// `selected`.
    fn unattempted(selected: &SelectedAuth, model: &str, client_key: &str, now: Instant) -> Self {
        let provider = selected.provider().to_owned();
        let executor_type = executor_type(&provider, None);
        let elapsed = (Utc::now() - selected.selected_at)
            .to_std()
            .unwrap_or_default();
        let mut call = Self::new(
            Mode::Ignored,
            provider,
            base_model_name(model).to_owned(),
            executor_type,
            Credential::of(&selected.auth, client_key),
            now.checked_sub(elapsed).unwrap_or(now),
        );
        call.requested_at = selected.selected_at;
        call.secrets.add_auth(&selected.auth);
        call
    }

    /// Another attempt is sent: its answer starts afresh.
    fn attempt(&mut self, request: &AttemptRequest<'_>, client_key: &str, now: Instant) {
        self.mode = Mode::of(request.provider, request.kind, request.format);
        self.provider = request.provider.to_owned();
        self.model = request.model.to_owned();
        self.executor_type = executor_type(request.provider, Some(request.kind));
        self.credential = Credential::of(request.auth, client_key);
        self.secrets.extend(request.secrets);
        self.secrets.add_auth(request.auth);
        self.lines = Lines::default();
        self.body = Vec::new();
        self.body_overflow = false;
        self.event_stream = false;
        self.answered = false;
        self.ended = false;
        // Only Codex's own executor reports the image generation tool, and
        // not over its WebSocket (upstream's `publishCodexImageToolUsage`).
        self.image_tool_request = (request.provider == "codex"
            && matches!(self.mode, Mode::CodexExecute | Mode::CodexStream))
        .then(|| request.body.clone());
        self.image = None;
        // A Codex or xAI WebSocket's clock starts when its request is sent,
        // once connected (see `Self::request_sent`), not at the dial.
        if !matches!(self.mode, Mode::CodexWebsocket | Mode::XaiWebsocket) {
            self.ttft.start(now);
        }
    }

    /// The attempt's request is going out on a connection that is up.
    fn request_sent(&mut self, now: Instant) {
        self.ttft.start(now);
    }

    fn response_head(&mut self, headers: &HeaderMap) {
        self.headers = record_headers(headers);
        self.event_stream = headers
            .get_all(http::header::CONTENT_TYPE)
            .iter()
            .any(|value| contains(value.as_bytes(), b"text/event-stream"));
    }

    fn chunk(&mut self, chunk: &[u8], now: Instant) {
        if self.mode != Mode::Ignored {
            self.answered = true;
        }
        match self.mode {
            Mode::Ignored => {}
            // The audio is neither held nor read.
            Mode::XaiSpeech => self.ttft.mark_first_response_byte(now),
            mode if mode.reads_whole() => {
                self.ttft.mark_first_response_byte(now);
                if self.body_overflow {
                    return;
                }
                if self.body.len() + chunk.len() > BODY_BOUND {
                    self.body = Vec::new();
                    self.body_overflow = true;
                } else {
                    self.body.extend_from_slice(chunk);
                }
            }
            Mode::CodexWebsocket => self.codex_stream_payload(json::trim_space(chunk), now),
            Mode::XaiWebsocket => self.xai_websocket_payload(json::trim_space(chunk), now),
            _ if self.ended => {}
            mode => {
                if mode == Mode::CodexStream {
                    self.ttft.observe_token_event(false, now);
                } else {
                    self.ttft.mark_first_response_byte(now);
                }
                let mut lines = std::mem::take(&mut self.lines);
                lines.feed(chunk, |line| self.line(line, now));
                self.lines = lines;
            }
        }
    }

    /// Reads a line of a stream, or of an answer read whole.
    fn line(&mut self, line: &[u8], now: Instant) {
        if self.ended {
            return;
        }
        match self.mode {
            Mode::ClaudeExecute => {
                self.response_model.observe(line, &self.provider);
                self.buffer.observe_claude_stream(line);
            }
            Mode::ClaudeStream => {
                self.response_model.observe(line, &self.provider);
                self.buffer.observe_claude_stream(line);
                self.ended = is_message_stop(line);
            }
            Mode::GeminiStream => {
                self.response_model.observe(line, &self.provider);
                self.buffer.observe(parse_gemini_stream_usage(line));
            }
            Mode::InteractionsStream => {
                self.response_model.observe(line, &self.provider);
                self.buffer.observe(parse_interactions_stream_usage(line));
            }
            Mode::OpenAiStream => {
                self.response_model.observe(line, &self.provider);
                self.buffer.observe_openai_stream(line);
                if is_done(line) {
                    self.seen_done = true;
                    self.ended = true;
                }
            }
            Mode::CodexExecute => {
                if let Some(rest) = line.strip_prefix(b"data:") {
                    self.codex_execute_payload(json::trim_space(rest));
                }
            }
            Mode::CodexStream => {
                if let Some(rest) = line.strip_prefix(b"data:") {
                    self.codex_stream_payload(json::trim_space(rest), now);
                }
            }
            Mode::XaiStream => {
                if let Some(rest) = line.strip_prefix(b"data:") {
                    self.xai_stream_payload(json::trim_space(rest));
                }
            }
            _ => {}
        }
    }

    /// Reads an event of a Codex or xAI answer read whole.
    fn codex_execute_payload(&mut self, payload: &[u8]) {
        if self.held.is_some() {
            return;
        }
        self.response_model.observe(payload, &self.provider);
        if is_terminal(payload, &["response.completed", "response.incomplete"]) {
            self.held = Some(Held {
                detail: parse_codex_usage(payload),
                latency: None,
                image: self.image_tool_usage(payload),
            });
        }
    }

    /// The image generation tool's counts in a terminal event, for a call
    /// whose tool is reported.
    fn image_tool_usage(&self, payload: &[u8]) -> Option<Detail> {
        self.image_tool_request.as_ref()?;
        parse_codex_image_tool_usage(payload)
    }

    /// Reads an event of a Codex stream or WebSocket (upstream's
    /// `observeCodexTokenEvent` and its terminal handling).
    fn codex_stream_payload(&mut self, payload: &[u8], now: Instant) {
        if payload.is_empty() {
            return;
        }
        if !self.ttft.is_set() {
            self.ttft
                .observe_token_event(is_responses_token_event(payload), now);
        }
        self.response_model.observe(payload, "codex");
        if self.held.is_none()
            && is_terminal(
                payload,
                &["response.completed", "response.incomplete", "response.done"],
            )
        {
            self.held = Some(Held {
                detail: parse_codex_usage(payload),
                latency: Some(now.saturating_duration_since(self.started)),
                image: self.image_tool_usage(payload),
            });
        }
    }

    /// Reads an event of an xAI stream: its model, and the counts of each
    /// `response.completed` or `response.incomplete` (upstream's
    /// `XAIExecutor.ExecuteStream`).
    fn xai_stream_payload(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        self.response_model.observe(payload, &self.provider);
        if is_terminal(payload, &["response.completed", "response.incomplete"]) {
            self.buffer.observe(parse_codex_usage(payload));
        }
    }

    /// Reads a message of an xAI WebSocket: its first marks the first
    /// byte, and the counts of its `response.completed` or `response.done`
    /// are kept (upstream's `XAIWebsocketsExecutor.ExecuteStream`).
    fn xai_websocket_payload(&mut self, payload: &[u8], now: Instant) {
        if payload.is_empty() {
            return;
        }
        self.ttft.mark_first_response_byte(now);
        self.response_model.observe(payload, &self.provider);
        if is_terminal(payload, &["response.completed", "response.done"]) {
            self.buffer.observe(parse_codex_usage(payload));
        }
    }

    /// Takes the model the stream buffer read, unless one was read already
    /// (upstream's `StreamUsageBuffer.Publish`).
    fn adopt_buffer_model(&mut self) {
        if self.response_model.get().is_empty() && !self.buffer.response_model().is_empty() {
            let model = self.buffer.response_model().to_owned();
            self.response_model.set(&model);
        }
    }

    /// The stream buffer's counts, if it read any.
    fn buffered(&mut self) -> Option<Detail> {
        let detail = self.buffer.detail().cloned()?;
        self.adopt_buffer_model();
        Some(detail)
    }

    /// The counts a failure whose error keeps them publishes: those of the
    /// answer read whole, or those the stream had read; none when it read
    /// none.
    fn kept_usage(&mut self) -> Detail {
        let detail = match self.mode {
            Mode::CodexStream | Mode::CodexWebsocket => {
                self.held.take().and_then(|held| held.detail)
            }
            mode if mode.reads_whole() => {
                let detail = self.whole_answer();
                // A failed call has no image generation tool record.
                self.image = None;
                detail
            }
            _ => self.buffered(),
        };
        detail.unwrap_or_default()
    }

    /// The counts of Meta's answer that isn't a stream: a JSON
    /// `response.completed` or `response.incomplete` event, or a Responses
    /// object, read as the completed event it stands for (upstream's
    /// `metaAsCompletedEvent`), with no counts when it names none
    /// (`EnsurePublished`). None for anything else, and for another
    /// provider's.
    fn plain_completed_usage(&mut self, body: &[u8]) -> Option<Detail> {
        if self.provider != "meta" {
            return None;
        }
        let trimmed = json::trim_space(body);
        if !json::valid(trimmed) {
            return None;
        }
        let doc = Doc::parse(trimmed);
        let kind = doc.get("type").string();
        let event = if kind == "response.completed" || kind == "response.incomplete" {
            trimmed.to_vec()
        } else if doc.get("object").string() == "response" || doc.get("output").exists() {
            [
                br#"{"type":"response.completed","response":"#.as_slice(),
                trimmed,
                b"}",
            ]
            .concat()
        } else {
            return None;
        };
        self.response_model.observe(&event, &self.provider);
        Some(parse_codex_usage(&event).unwrap_or_default())
    }

    /// The counts of an answer read whole; none when it is not recorded.
    fn whole_answer(&mut self) -> Option<Detail> {
        let body = std::mem::take(&mut self.body);
        match self.mode {
            Mode::ClaudeExecute => {
                let trimmed = json::trim_space(&body);
                if self.event_stream
                    || trimmed.starts_with(b"event:")
                    || trimmed.starts_with(b"data:")
                {
                    let now = self.started;
                    for line in lines_of(&body) {
                        self.line(line, now);
                    }
                    self.buffered()
                } else {
                    self.response_model.observe(&body, &self.provider);
                    Some(parse_claude_usage(&body))
                }
            }
            Mode::GeminiExecute => {
                self.response_model.observe(&body, &self.provider);
                Some(parse_gemini_usage(&body))
            }
            Mode::InteractionsExecute => {
                self.response_model.observe(&body, &self.provider);
                Some(parse_interactions_usage(&body))
            }
            Mode::OpenAiExecute => {
                self.response_model.observe(&body, &self.provider);
                Some(parse_openai_usage(&body))
            }
            Mode::CodexCompact => Some(parse_openai_usage(&body)),
            Mode::XaiMedia => {
                self.response_model.observe(&body, &self.provider);
                Some(Detail::default())
            }
            Mode::XaiSpeech => Some(Detail::default()),
            Mode::CodexExecute => {
                let now = self.started;
                for line in lines_of(&body) {
                    self.line(line, now);
                }
                let Some(held) = self.held.take() else {
                    return self.plain_completed_usage(&body);
                };
                // The tool's counts publish the call's record first, empty
                // when the call's own are missing (upstream's
                // `EnsurePublished` in `publishCodexImageToolUsage`).
                let detail = held
                    .detail
                    .or_else(|| held.image.as_ref().map(|_| Detail::default()));
                self.image = held.image;
                detail
            }
            _ => None,
        }
    }

    /// What the call publishes for how it ended, if anything.
    fn conclude(
        &mut self,
        outcome: Outcome,
        error: Option<Failure>,
        now: Instant,
    ) -> Option<Publication> {
        let latency = now.saturating_duration_since(self.started);
        let success = |detail: Detail| Publication {
            detail,
            failure: None,
            latency,
        };
        let failure = |detail: Detail, failure: Failure| Publication {
            detail,
            failure: Some(failure),
            latency,
        };
        let error = error.unwrap_or_default();
        if !matches!(self.mode, Mode::Ignored | Mode::CodexWebsocket) && !self.mode.reads_whole() {
            let mut lines = std::mem::take(&mut self.lines);
            lines.flush(|line| self.line(line, now));
        }
        match (self.mode, outcome) {
            (Mode::Ignored, _) => None,
            // Nothing of the answer came, as when the client went away
            // while the executor was still connecting or waiting for the
            // answer: upstream's `TrackFailure` records the error its
            // canceled send returns, and a canceled read is a failure too.
            (_, Outcome::Canceled) if !self.answered => {
                Some(failure(Detail::default(), Failure::canceled()))
            }
            // The answer came and counts, though the call failed (v8.0.20's
            // `PublishFailure` with the usage observed).
            (_, Outcome::Failed) if error.keeps_usage => {
                let detail = self.kept_usage();
                Some(failure(detail, error))
            }
            (mode, Outcome::Failed) if mode.reads_whole() => {
                Some(failure(Detail::default(), error))
            }
            (mode, Outcome::Canceled) if mode.reads_whole() => {
                Some(failure(Detail::default(), Failure::canceled()))
            }
            (mode, Outcome::Completed) if mode.reads_whole() => self.whole_answer().map(success),
            (Mode::ClaudeStream, Outcome::Failed) => {
                self.adopt_buffer_model();
                Some(failure(self.buffer.raw_detail().clone(), error))
            }
            // A canceled stream is a failure that keeps what usage it read
            // (`StreamUsageBuffer.PublishFailure`), unless it had read its
            // `message_stop`, which completes it.
            (Mode::ClaudeStream, Outcome::Canceled) if !self.ended => {
                self.adopt_buffer_model();
                Some(failure(
                    self.buffer.raw_detail().clone(),
                    Failure::canceled(),
                ))
            }
            (Mode::ClaudeStream, _) => self.buffered().map(success),
            (
                Mode::GeminiStream | Mode::InteractionsStream | Mode::OpenAiStream,
                Outcome::Failed,
            ) => Some(failure(Detail::default(), error)),
            (Mode::GeminiStream | Mode::InteractionsStream, _)
            | (Mode::OpenAiStream, Outcome::Completed) => {
                Some(success(self.buffered().unwrap_or_default()))
            }
            (Mode::OpenAiStream, _) => match self.buffered() {
                Some(detail) => Some(success(detail)),
                None => self.seen_done.then(|| success(Detail::default())),
            },
            (_, Outcome::Failed) => Some(failure(Detail::default(), error)),
            (Mode::XaiStream, _) => self.buffered().map(success),
            (Mode::XaiWebsocket, _) => self.buffered().map(success),
            (_, outcome) => match self.held.take() {
                Some(held) => {
                    self.image = held.image;
                    Some(Publication {
                        detail: held.detail.unwrap_or_default(),
                        failure: None,
                        latency: held.latency.unwrap_or(latency),
                    })
                }
                None => (outcome == Outcome::Completed).then(|| success(Detail::default())),
            },
        }
    }
}

/// What the tap keeps between events.
#[derive(Default)]
struct State {
    call: Option<Call>,
    error: Option<Failure>,
}

/// The usage reporter of one client call: one record per executor call.
pub(super) struct UsageTap {
    inner: Arc<Inner>,
    context: Arc<RequestContext>,
    shared: Shared,
    state: Mutex<State>,
}

impl UsageTap {
    pub(super) fn new(
        inner: Arc<Inner>,
        context: Arc<RequestContext>,
        request: &Request,
        options: &Options,
    ) -> Self {
        Self {
            inner,
            context,
            shared: Shared::new(request, options),
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn client_key(&self) -> &str {
        self.context.client_key().unwrap_or_default()
    }

    /// `body` without the call's secrets and the client's key, every one
    /// however short.
    fn scrub(&self, body: String, secrets: &Secrets) -> String {
        let mut secrets = secrets.clone();
        secrets.add(self.client_key());
        secrets.text(body, Policy::Disk)
    }

    fn record(&self, call: &Call, publication: Publication) -> Record {
        let (failed, fail_status, fail_body) = match publication.failure {
            Some(failure) => (
                true,
                failure.status,
                self.scrub(failure.body, &call.secrets),
            ),
            None => (false, 0, String::new()),
        };
        let context = &self.context;
        Record {
            request_id: context.id.as_str().to_owned(),
            provider: call.provider.clone(),
            executor_type: call.executor_type.to_owned(),
            model: self.scrub(call.model.clone(), &call.secrets),
            alias: self.shared.alias.clone(),
            source: call.credential.source.clone(),
            api_key: self.client_key().to_owned(),
            session_id: self.shared.session_id.clone(),
            auth_index: call.credential.index.clone(),
            access_token_sha256: call.credential.access_token_sha256.clone(),
            auth_type: call.credential.auth_type.to_owned(),
            service_tier: self.shared.service_tier.clone(),
            response_model: self.scrub(call.response_model.get().to_owned(), &call.secrets),
            generate: self.shared.generate,
            stream: self.shared.stream,
            requested_at: Some(call.requested_at),
            latency: publication.latency,
            ttft: call.ttft.duration(),
            failed,
            fail_status,
            fail_body,
            detail: publication.detail,
            response_headers: call.headers.clone(),
            endpoint: context.endpoint.clone(),
            client_ip: context.client_ip.clone(),
            resolved_client_ip: context.resolved_client_ip.clone(),
            forwarded_for: context.forwarded_for.clone(),
            user_agent: context.user_agent.clone(),
            ..Record::default()
        }
    }

    /// Publishes the record to the queue and the observer, and warns of a
    /// substituted model (upstream's `publishAttemptRecord`), then the image
    /// generation tool's record.
    fn publish(&self, call: &Call, publication: Publication) {
        let latency = publication.latency;
        if self.inner.queue.usage_statistics_enabled() {
            let record = self.record(call, publication);
            self.emit(call, &record);
        }
        self.warn_model_substitution(call);
        self.publish_image_tool(call, latency);
    }

    /// Sends `record`, the record of `call`, to the observer, if there is
    /// one, and to the queue, if it is on.
    fn emit(&self, call: &Call, record: &Record) {
        if self.inner.observer.present() {
            self.inner.observer.send(self.event(call, record));
        }
        if self.inner.queue.enabled() {
            self.inner.queue.enqueue(record.encode().into());
        }
    }

    /// Publishes the image generation tool's record, if the call's record
    /// came with the tool's counts and they aren't all zero (upstream's
    /// `PublishAdditionalModel` and `buildAdditionalModelRecord`).
    fn publish_image_tool(&self, call: &Call, latency: Duration) {
        let (Some(detail), Some(request)) = (&call.image, &call.image_tool_request) else {
            return;
        };
        if !self.inner.queue.usage_statistics_enabled() {
            return;
        }
        let detail =
            ensure_token_breakdown_for_provider(detail.clone(), &call.provider, call.executor_type);
        if !detail.has_tokens() {
            return;
        }
        let model = image_generation_tool_model(request);
        let mut record = self.record(
            call,
            Publication {
                detail,
                failure: None,
                latency,
            },
        );
        // The served model is the call's, never the tool's, unless the
        // tool's model is the one sent.
        if model != call.model {
            record.response_model = String::new();
        }
        record.model = self.scrub(model, &call.secrets);
        self.emit(call, &record);
    }

    /// The observer's event for `record`, the record of `call`, with the
    /// blanks filled in as [`Record::encode`] fills them.
    fn event(&self, call: &Call, record: &Record) -> UsageEvent {
        fn or(value: &str, fallback: &str) -> String {
            match value.trim() {
                "" => fallback.to_owned(),
                trimmed => trimmed.to_owned(),
            }
        }
        let model = or(&record.model, "unknown");
        let detail = ensure_token_breakdown_for_provider(
            record.detail.clone(),
            &record.provider,
            &record.executor_type,
        );
        let status = match (record.failed, record.fail_status) {
            (false, _) => 200,
            (true, status) if status <= 0 => 500,
            (true, status) => status,
        };
        let credential = &call.credential;
        UsageEvent {
            requested_at: record.requested_at.unwrap_or_else(Utc::now),
            request_id: record.request_id.trim().to_owned(),
            endpoint: record.endpoint.clone(),
            provider: or(&record.provider, "unknown"),
            alias: or(&record.alias, &model),
            model,
            credential: (!credential.id.is_empty()).then(|| EventCredential {
                id: credential.id.clone(),
                auth_index: credential.index.clone(),
                label: credential.label.clone(),
                auth_type: credential.auth_type.to_owned(),
            }),
            client_key: ClientKey::new(record.api_key.clone()),
            stream: record.stream,
            failed: record.failed,
            status,
            latency: record.latency,
            ttft: (record.stream && !record.ttft.is_zero()).then_some(record.ttft),
            tokens: detail.token_breakdown,
            total_tokens: detail.total_tokens,
        }
    }

    /// Warns that the answer named another model than the one sent, once
    /// per credential and model pair in the window (upstream's
    /// `warnModelSubstitution`).
    fn warn_model_substitution(&self, call: &Call) {
        let served = call.response_model.get();
        if served.is_empty() || !is_model_substituted(&call.model, served) {
            return;
        }
        let provider = match call.provider.trim() {
            "" => "unknown",
            provider => provider,
        };
        let index = match call.credential.index.as_str() {
            "" => "nil",
            index => index,
        };
        // The served model is whatever the upstream said, and the requested
        // one what the client sent; either may quote a secret, and main.log
        // is a file, so both are scrubbed as a file is.
        let served = self.scrub(served.to_owned(), &call.secrets);
        let requested = self.scrub(call.model.clone(), &call.secrets);
        tracing::warn!(
            request_id = %self.context.id.as_str(),
            "{provider} executor: upstream served model {} for requested model {} (auth_index={index})",
            go::quote(&served),
            go::quote(&requested),
        );
    }
}

impl Tap for UsageTap {
    fn attempt_request(&self, request: &AttemptRequest<'_>) {
        let now = (self.inner.clock)();
        let client_key = self.client_key();
        let mut state = self.lock();
        let call = state.call.get_or_insert_with(|| {
            Call::new(
                Mode::Ignored,
                String::new(),
                String::new(),
                "",
                Credential::default(),
                now,
            )
        });
        call.attempt(request, client_key, now);
    }

    fn request_sent(&self) {
        let now = (self.inner.clock)();
        if let Some(call) = self.lock().call.as_mut() {
            call.request_sent(now);
        }
    }

    fn response_head(&self, _status: u16, headers: &HeaderMap) {
        if let Some(call) = self.lock().call.as_mut() {
            call.response_head(headers);
        }
    }

    fn chunk(&self, chunk: &bytes::Bytes) {
        let now = (self.inner.clock)();
        if let Some(call) = self.lock().call.as_mut() {
            call.chunk(chunk, now);
        }
    }

    fn error(&self, error: &ExecError) {
        self.lock().error = Some(Failure::of(error));
    }

    fn finish(&self, outcome: Outcome) {
        let now = (self.inner.clock)();
        let (call, error) = {
            let mut state = self.lock();
            (state.call.take(), state.error.take())
        };
        let concluded = match call {
            Some(mut call) => call
                .conclude(outcome, error, now)
                .map(|publication| (call, publication)),
            None => self.context.selected().map(|selected| {
                let call = Call::unattempted(
                    &selected,
                    &self.shared.request_model,
                    self.client_key(),
                    now,
                );
                let latency = now.saturating_duration_since(call.started);
                let failure = match outcome {
                    Outcome::Completed => None,
                    Outcome::Failed => Some(error.unwrap_or_default()),
                    Outcome::Canceled => Some(Failure::canceled()),
                };
                let publication = Publication {
                    detail: Detail::default(),
                    failure,
                    latency,
                };
                (call, publication)
            }),
        };
        if let Some((call, publication)) = concluded {
            self.publish(&call, publication);
        }
    }
}
