// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/gemini_openai-responses_response.go
// (ConvertGeminiResponseToOpenAIResponses, ConvertGeminiResponseToOpenAIResponsesNonStream,
// FinalizeToolInput, geminiResponsesUsage, geminiResponsesTerminalState,
// geminiRecordFunctionEvidence, geminiPendingIdentityError, determineWebSearchStreamMode,
// hasEffectiveGoogleSearchTool, isUpstreamGeminiRequest, pickRequestJSON, unwrapRequestRoot,
// unwrapGeminiResponseRoot) and internal/util/translator.go (SanitizedToolNameMap,
// RestoreSanitizedToolName) (v8.0.15, MIT), and Go's time.Parse with the RFC 3339
// layout.
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini responses → OpenAI Responses events.
//!
//! [`GeminiToOpenAIResponsesStream`] turns each Gemini stream chunk into the
//! Responses events it implies, and
//! [`convert_gemini_response_to_openai_responses_non_stream`] turns a whole
//! Gemini response into one Responses object. Thought parts become reasoning
//! items, visible text an assistant message, and function calls function or
//! custom tool calls, named as the request declared them. Search grounding
//! becomes a `web_search_call` and `url_citation` annotations on the text.
//!
//! Gemini signs model parts. A signature on a thought rides in its reasoning
//! item's `encrypted_content`; one on text or a call that has no reasoning
//! item of its own is sent as a carrier (see
//! [`signature_carrier`](super::signature_carrier)), a reasoning item with no
//! summary placed next to the item it belongs to. While streaming, a
//! signature that trails a message's text is kept in the replay cache with
//! the message instead (see [`trailing_signature`](super::trailing_signature)).
//!
//! A call to the client's `apply_patch` custom tool has its patch text
//! decoded from the arguments. Arguments that aren't one valid input string,
//! repeated snapshots of a call that disagree, a call whose identity can't be
//! settled, and a stream that ends early all end the response with
//! `response.failed`; a whole response then gives nothing.
//!
//! A stream finishes only when a finish reason has come and a chunk with
//! usage or `[DONE]` arrives; a `MAX_TOKENS` finish ends it with
//! `response.incomplete`, and the message open then, as `incomplete`; usage is
//! kept as five cumulative counts, each replaced when a chunk has it, and
//! always written with all five; a whole response takes its `status` and
//! `incomplete_details` from the finish reason; usage may come as
//! `cpaUsageMetadata`, also inside a `response` wrapper; and after a finish
//! reason, [`finalize_tool_input`](GeminiToOpenAIResponsesStream::finalize_tool_input)
//! fails the stream only for an unresolved call identity.
//!
//! Deviations from upstream:
//! - A chunk or response that serde_json can't read is read as nothing: a
//!   chunk gives no events, and a response gives one with no output. That is
//!   one that isn't valid JSON, from which gjson reads what it can, or isn't
//!   UTF-8, or holds an unpaired surrogate escape such as `\ud800`, which gjson
//!   reads as U+FFFD, or is nested more than 128 levels deep.
//! - If the request declares `apply_patch`, such a chunk instead ends the
//!   stream with `response.failed`, and such a response gives none, with the
//!   tool input error, since it could carry part of a patch. An empty chunk
//!   or body and `[DONE]` don't count. This is stricter than upstream, which
//!   fails only when it finds an `apply_patch` call in invalid JSON, and
//!   reads a line that isn't JSON at all as nothing.
//! - Since what is read is always valid JSON, upstream's check that an
//!   `apply_patch` call came in valid JSON always passes.
//! - A non-string value read as text is written as compact JSON, where
//!   upstream uses its JSON text. Where a key appears twice in an object, the
//!   last one counts; gjson reads the first. A function call's `args` are
//!   kept as written, as upstream keeps them, unless a repeated key makes
//!   the text say something other than the value read; then they are
//!   written as compact JSON.
//! - Strings are written with serde_json's escaping. Upstream writes `<`, `>`,
//!   `&`, U+2028 and U+2029 in some fields as `\u003c` and so on; the JSON
//!   values are the same.
//! - A token count too large for `i64` saturates. Go's result depends on the
//!   CPU; amd64 gives the minimum `i64`.
//! - The request's tool declarations, the fields the final event repeats
//!   from it, and the query a search falls back to are read once, when the
//!   stream is created.
//! - A `temperature` or `top_p` that reads as infinite or NaN is written as
//!   `null`. Upstream writes `+Inf` or `NaN`, which isn't JSON.
//! - The whole-response conversion returns `None` where upstream returns
//!   `nil`.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::at;
use super::request::equal_fold;
use super::signature_carrier::{ANY, FUNCTION, NEXT, PREVIOUS, STANDALONE, TEXT, encode};
use super::trailing_signature::cache_text_signatures;
use super::web_search::{
    PartMapping, allows_web_search_tool_choice, build_url_citations_for_messages,
    build_web_search_call_item, extract_grounding_metadata, extract_web_search_query,
    grounding_queries, grounding_sources, has_valid_web_grounding, has_web_search_tool,
    merge_citation_annotations, merge_grounding_metadata, model_supports_web_search,
};
use crate::apply_patch::input::{CallState, InputError, failure};
use crate::common::gemini::{
    SanitizedToolNames, restore_sanitized_tool_name, sanitized_tool_name_map,
};
use crate::common::request_model_name;
use crate::common::responses::{echo_fields, pick_request, set_tool_call_identity};
use crate::common::sse::push_event;
use crate::go;
use crate::json::{bool_of, int_of, object, path, raw, str_of};
use crate::responses_tools::{
    ToolIdentity, responses_tool_reverse_identity_map, unwrap_responses_custom_tool_input,
};
use crate::signature::GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR;

/// The status of a finished message item or response.
const COMPLETED: &str = "completed";

/// `geminiResponsesUsage`: the token counts so far. Gemini sends each count
/// as a running total, not an increment.
#[derive(Default)]
struct Usage {
    present: bool,
    prompt: i64,
    candidates: i64,
    thoughts: i64,
    total: i64,
    cached: i64,
}

impl Usage {
    /// `Merge`: takes each count `root`'s `usageMetadata`, else its
    /// `cpaUsageMetadata`, has. Returns whether `root` has either.
    fn merge(&mut self, root: &Value) -> bool {
        let Some(metadata) = root
            .get("usageMetadata")
            .or_else(|| root.get("cpaUsageMetadata"))
        else {
            return false;
        };
        self.present = true;
        for (key, count) in [
            ("promptTokenCount", &mut self.prompt),
            ("candidatesTokenCount", &mut self.candidates),
            ("thoughtsTokenCount", &mut self.thoughts),
            ("totalTokenCount", &mut self.total),
            ("cachedContentTokenCount", &mut self.cached),
        ] {
            if let Some(value) = metadata.get(key) {
                *count = int_of(value);
            }
        }
        true
    }

    /// `JSON`: the Responses `usage` object.
    fn to_json(&self) -> Value {
        json!({
            "input_tokens": self.prompt,
            "input_tokens_details": {"cached_tokens": self.cached},
            "output_tokens": self.candidates.wrapping_add(self.thoughts),
            "output_tokens_details": {"reasoning_tokens": self.thoughts},
            "total_tokens": self.total,
        })
    }
}

/// `geminiResponsesTerminalState`: the final event, response status and
/// `incomplete_details` a finish reason gives.
fn terminal_state(finish_reason: &str) -> (&'static str, &'static str, Option<Value>) {
    if equal_fold(finish_reason.trim(), "MAX_TOKENS") {
        (
            "response.incomplete",
            "incomplete",
            Some(json!({"reason": "max_output_tokens"})),
        )
    } else {
        ("response.completed", COMPLETED, None)
    }
}

/// Translates a Gemini stream into Responses events, one chunk at a time.
/// Keep one per response.
pub struct GeminiToOpenAIResponsesStream {
    /// The model given to [`new`](Self::new), which keys the text signatures
    /// kept in the replay cache.
    model: String,
    /// The model `response.created` names: the request's, or `model`.
    response_model: String,
    identities: HashMap<String, ToolIdentity>,
    sanitized_names: Option<SanitizedToolNames>,
    /// The request fields the final event repeats, or `None` without a
    /// request.
    echo: Option<Vec<(&'static str, Value)>>,
    /// The query the request asks to search for, or `None` without a request.
    request_query: Option<String>,
    /// Whether text waits for the search to finish, so the `web_search_call`
    /// comes first.
    web_search_mode: bool,
    error: Option<ToolInputError>,
    seq: i64,
    response_id: String,
    /// When the response was created, in Unix seconds.
    created_at: i64,
    started: bool,
    completed: bool,
    /// The first finish reason Gemini sent, or `STOP` for `[DONE]` without
    /// one; `""` before then.
    finish_reason: String,
    usage: Usage,
    /// The status a message item gets when it is finished: `incomplete` for
    /// one the final event finishes after `MAX_TOKENS`.
    message_status: &'static str,
    msg_opened: bool,
    msg_closed: bool,
    msg_index: i64,
    msg_id: String,
    msg_text: String,
    reasoning_opened: bool,
    reasoning_closed: bool,
    reasoning_index: i64,
    reasoning_id: String,
    /// The open reasoning item's signature, as Gemini sent it.
    reasoning_signature: String,
    /// Where the signature belongs, if not to the reasoning item itself.
    reasoning_direction: &'static str,
    reasoning_target: &'static str,
    reasoning_text: String,
    /// Reasoning text that came before the item was opened.
    reasoning_pending_deltas: Vec<String>,
    /// A signature waiting to see what it belongs to.
    pending_signature: String,
    detached: BTreeMap<i64, DetachedItem>,
    completed_messages: BTreeMap<i64, MessageItem>,
    completed_reasoning: BTreeMap<i64, ReasoningItem>,
    seen_signatures: HashSet<String>,
    /// What the last item was: [`TEXT`] (thought text included), [`FUNCTION`]
    /// or `""`.
    last_kind: &'static str,
    /// The signatures that trailed each message's text, by message ID.
    hidden_text_signatures: HashMap<String, Vec<String>>,
    next_index: i64,
    /// Tool calls, by output index.
    calls: BTreeMap<i64, Call>,
    evidence: EvidenceStore,
    web_search_opened: bool,
    web_search_done: bool,
    web_search_index: i64,
    web_search_id: String,
    web_search_query: String,
    web_search_queries: Vec<String>,
    web_search_sources: Vec<Value>,
    /// Text held back until the search finishes.
    buffered_deltas: Vec<String>,
    /// The same text by Gemini part index.
    buffered_parts: Vec<(i64, String)>,
    /// The grounding metadata so far, merged.
    grounding: Option<Value>,
    part_mappings: Vec<PartMapping>,
    logical_part_index: i64,
    part_kind: &'static str,
    seen_first_part: bool,
    text_run: bool,
    /// The open message's length in characters.
    msg_rune_offset: i64,
    /// How many annotations have been sent for each message.
    emitted_annotations: HashMap<i64, usize>,
}

/// `geminiDetachedReasoningItem`: a carrier sent on its own.
struct DetachedItem {
    id: String,
    carrier: String,
}

/// `geminiCompletedMessageItem`
struct MessageItem {
    id: String,
    text: String,
    status: &'static str,
    annotations: Vec<Value>,
}

/// `geminiCompletedReasoningItem`
struct ReasoningItem {
    id: String,
    encrypted_content: String,
    text: String,
}

/// What upstream keeps about one tool call in its `Func*` maps.
struct Call {
    call_id: String,
    name: String,
    namespace: String,
    custom: bool,
    args: String,
    input: String,
}

/// Why a stream with an `apply_patch` call failed. Upstream keeps it as an
/// `error` for the caller.
#[derive(Clone, Debug)]
enum ToolInputError {
    /// The arguments, or a snapshot of them, aren't one valid input string.
    Arguments(InputError),
    /// Snapshots of one call disagree about which call, or what input, it is.
    Conflict(&'static str),
    /// A call without a name was never resolved.
    Unresolved,
    /// The stream ended before a finish reason.
    Unterminated,
    /// A chunk or response couldn't be read. Upstream fails so only when it
    /// finds an `apply_patch` call in invalid JSON.
    Unreadable,
}

impl fmt::Display for ToolInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(error) => fmt::Display::fmt(error, f),
            Self::Conflict(message) => f.write_str(message),
            Self::Unresolved => f.write_str("unresolved Gemini apply_patch call identity"),
            Self::Unterminated => {
                f.write_str("upstream apply_patch stream ended before protocol completion")
            }
            Self::Unreadable => f.write_str("invalid Gemini apply_patch response JSON"),
        }
    }
}

impl Error for ToolInputError {}

/// `geminiFunctionCallEvidence`: what the snapshots of one call have said.
#[derive(Default)]
struct CallEvidence {
    part_index: Option<i64>,
    apply_patch: bool,
    raw_name: String,
    upstream_id: String,
    /// The decoded `apply_patch` input of the first complete snapshot.
    input: Option<String>,
    error: Option<ToolInputError>,
    patch_call: Option<CallState>,
}

/// Upstream's `FunctionEvidence` map, whose keys share evidence.
#[derive(Default)]
struct EvidenceStore {
    by_key: HashMap<String, usize>,
    items: Vec<CallEvidence>,
}

impl EvidenceStore {
    /// `geminiRecordFunctionEvidence`: the evidence for `call`, with what it
    /// says recorded. Full snapshots are evidence, never prefixes. Stable IDs
    /// and explicit part indexes identify repeated snapshots without merging
    /// distinct unkeyed calls. `Err` if it names two `apply_patch` calls at
    /// once; nothing is recorded then.
    fn record(
        &mut self,
        identities: &HashMap<String, ToolIdentity>,
        call: &Value,
        args: &str,
        part_index: i64,
    ) -> Result<usize, ToolInputError> {
        const CONFLICTING_INDEXES: &str = "conflicting apply_patch call indexes";
        let name = str_of(call.get("name"));
        let id = str_of(call.get("id"));
        let mut keys = Vec::new();
        if part_index >= 0 {
            keys.push(format!("part:{part_index}"));
        }
        if !id.is_empty() {
            keys.push(format!("id:{id}"));
        }
        // Never guess which later named call owns an unkeyed nameless
        // snapshot.
        if keys.is_empty() && name.is_empty() {
            keys.push(format!("unknown:{}", self.by_key.len()));
        }
        let is_patch = identities
            .get(&*name)
            .is_some_and(|identity| identity.apply_patch);
        let mut patch_related = is_patch;
        let mut found = None;
        let mut conflict = false;
        for key in &keys {
            if let Some(&prior) = self.by_key.get(key) {
                patch_related |= self.items[prior].apply_patch;
                match found {
                    None => found = Some(prior),
                    Some(index) if index != prior => conflict = true,
                    Some(_) => {}
                }
            }
        }
        let index = match found {
            Some(index) => index,
            None => {
                self.items.push(CallEvidence::default());
                self.items.len() - 1
            }
        };
        if conflict {
            // Reject before rebinding either established call's keys.
            if patch_related {
                return Err(ToolInputError::Conflict(CONFLICTING_INDEXES));
            }
            // Ordinary calls keep the first match.
            self.items[index].error = Some(ToolInputError::Conflict(CONFLICTING_INDEXES));
        }
        for key in keys {
            self.by_key.insert(key, index);
        }
        let evidence = &mut self.items[index];
        let record = |evidence: &mut CallEvidence, error: ToolInputError| {
            evidence.error.get_or_insert(error);
        };
        if part_index >= 0 {
            match evidence.part_index {
                Some(previous) if previous != part_index => record(
                    evidence,
                    ToolInputError::Conflict("conflicting apply_patch part index"),
                ),
                _ => evidence.part_index = Some(part_index),
            }
        }
        if is_patch {
            evidence.apply_patch = true;
        }
        if !name.is_empty() {
            if !evidence.raw_name.is_empty() && evidence.raw_name != name {
                record(
                    evidence,
                    ToolInputError::Conflict("conflicting apply_patch call name"),
                );
            } else {
                evidence.raw_name = name.into_owned();
            }
        }
        if !id.is_empty() {
            if !evidence.upstream_id.is_empty() && evidence.upstream_id != id {
                record(
                    evidence,
                    ToolInputError::Conflict("conflicting apply_patch call ID"),
                );
            } else {
                evidence.upstream_id = id.into_owned();
            }
        }
        match CallState::default().finish_arguments(args) {
            Err(error) => record(evidence, ToolInputError::Arguments(error)),
            Ok((_, input)) => {
                if evidence
                    .input
                    .as_ref()
                    .is_some_and(|previous| *previous != input)
                {
                    record(
                        evidence,
                        ToolInputError::Conflict("conflicting apply_patch complete snapshots"),
                    );
                }
                evidence.input = Some(input);
            }
        }
        Ok(index)
    }

    /// `geminiPendingIdentityError`: if the request declares `apply_patch`,
    /// the error for a call that never got a name.
    fn pending_identity_error(
        &self,
        identities: &HashMap<String, ToolIdentity>,
    ) -> Option<ToolInputError> {
        if !patch_enabled(identities) {
            return None;
        }
        self.items
            .iter()
            .find(|evidence| evidence.raw_name.is_empty())
            .map(|evidence| evidence.error.clone().unwrap_or(ToolInputError::Unresolved))
    }
}

impl GeminiToOpenAIResponsesStream {
    /// `model` is the model the request went to, named in `response.created`
    /// when neither request names one. `original_request` is the client's
    /// Responses request and `request` the translated one; the first that
    /// isn't `Null` supplies the tool declarations and the fields the final
    /// event repeats.
    pub fn new(model: &str, original_request: &Value, request: &Value) -> Self {
        let picked = pick_request(original_request, request);
        let response_model = request_model_name(original_request, request)
            .unwrap_or(model)
            .to_owned();
        let web_search_mode =
            web_search_stream_mode(model, &response_model, original_request, request);
        Self {
            model: model.to_owned(),
            identities: responses_tool_reverse_identity_map(picked.unwrap_or(&Value::Null)),
            sanitized_names: sanitized_tool_name_map(original_request),
            echo: picked.map(|request| echo_fields(unwrap_request_root(request), |_| None)),
            request_query: picked
                .map(|request| extract_web_search_query(unwrap_request_root(request))),
            web_search_mode,
            response_model,
            error: None,
            seq: 0,
            response_id: String::new(),
            created_at: 0,
            started: false,
            completed: false,
            finish_reason: String::new(),
            usage: Usage::default(),
            message_status: COMPLETED,
            msg_opened: false,
            msg_closed: false,
            msg_index: 0,
            msg_id: String::new(),
            msg_text: String::new(),
            reasoning_opened: false,
            reasoning_closed: false,
            reasoning_index: 0,
            reasoning_id: String::new(),
            reasoning_signature: String::new(),
            reasoning_direction: "",
            reasoning_target: "",
            reasoning_text: String::new(),
            reasoning_pending_deltas: Vec::new(),
            pending_signature: String::new(),
            detached: BTreeMap::new(),
            completed_messages: BTreeMap::new(),
            completed_reasoning: BTreeMap::new(),
            seen_signatures: HashSet::new(),
            last_kind: "",
            hidden_text_signatures: HashMap::new(),
            next_index: 0,
            calls: BTreeMap::new(),
            evidence: EvidenceStore::default(),
            web_search_opened: false,
            web_search_done: false,
            web_search_index: 0,
            web_search_id: String::new(),
            web_search_query: String::new(),
            web_search_queries: Vec::new(),
            web_search_sources: Vec::new(),
            buffered_deltas: Vec::new(),
            buffered_parts: Vec::new(),
            grounding: None,
            part_mappings: Vec::new(),
            logical_part_index: 0,
            part_kind: "",
            seen_first_part: false,
            text_run: false,
            msg_rune_offset: 0,
            emitted_annotations: HashMap::new(),
        }
    }

    /// `ConvertGeminiResponseToOpenAIResponses`: translates one chunk of the
    /// Gemini stream, a JSON line with or without `data:`, or `[DONE]`.
    /// Returns the SSE frames to send, `event:` and `data:` lines each, or
    /// `""`. The final event comes once a finish reason has come and a chunk
    /// with usage, or `[DONE]`, arrives. Nothing follows `response.completed`,
    /// `response.incomplete` or `response.failed`.
    pub fn translate_line(&mut self, line: &[u8]) -> String {
        let mut out = String::new();
        let data = go::trim_space(line.strip_prefix(b"data:").unwrap_or(line));
        if data.is_empty() || self.completed {
            return out;
        }
        let done = data == b"[DONE]";
        let text = if done {
            if !self.started {
                return out;
            }
            if self.finish_reason.is_empty() {
                self.finish_reason = "STOP".to_owned();
            }
            "{}"
        } else {
            match std::str::from_utf8(data) {
                Ok(text) => text,
                Err(_) => {
                    self.unreadable(&mut out);
                    return out;
                }
            }
        };
        let Ok(parsed) = serde_json::from_str::<Value>(text) else {
            self.unreadable(&mut out);
            return out;
        };
        let (root, wrapped) = unwrap_response_root(&parsed);
        let has_usage = self.usage.merge(root);
        self.message_status = COMPLETED;

        if !self.started {
            self.start(root, &mut out);
        }
        if let Some(metadata) = extract_grounding_metadata(root) {
            self.ground(metadata, &mut out);
        }
        if let Some(Value::Array(parts)) = at(root, "candidates.0.content.parts") {
            for (index, part) in parts.iter().enumerate() {
                let args = RawArgs {
                    text,
                    wrapped,
                    index,
                };
                if !self.part(index, part, args, &mut out) {
                    break;
                }
            }
        }
        if self.completed {
            return out;
        }
        // Keep the first finish reason, through a tail of usage or `[DONE]`.
        let reason = str_of(at(root, "candidates.0.finishReason"));
        if !reason.is_empty() && self.finish_reason.is_empty() {
            self.finish_reason = reason.into_owned();
        }
        if !self.finish_reason.is_empty() {
            self.finish(done || has_usage, &mut out);
        }
        out
    }

    /// `FinalizeToolInput`: call when the Gemini stream ends. If the request
    /// declares `apply_patch` and the stream ended before a finish reason, or
    /// with a call whose identity was never settled, returns
    /// `response.failed`, since a patch may be cut short. A stream that had a
    /// finish reason but never finished, waiting for usage, otherwise gives
    /// nothing.
    pub fn finalize_tool_input(&mut self) -> String {
        let mut out = String::new();
        if self.error.is_some() || self.completed {
            return out;
        }
        if !self.finish_reason.is_empty() {
            match self.evidence.pending_identity_error(&self.identities) {
                None => return out,
                Some(error) => self.error = Some(error),
            }
        }
        if !patch_enabled(&self.identities) {
            return out;
        }
        self.error.get_or_insert(ToolInputError::Unterminated);
        self.completed = true;
        self.seq += 1;
        push_event(
            &mut out,
            "response.failed",
            &failure(&self.response_id, self.seq),
        );
        out
    }

    /// `ToolInputError`: why the stream failed, if an `apply_patch` call did.
    pub fn tool_input_error(&self) -> Option<&(dyn Error + 'static)> {
        self.error
            .as_ref()
            .map(|error| error as &(dyn Error + 'static))
    }

    /// A chunk serde_json couldn't read: fails the stream if the request
    /// declares `apply_patch`, since the chunk could carry part of a patch.
    fn unreadable(&mut self, out: &mut String) {
        if patch_enabled(&self.identities) {
            self.fail(ToolInputError::Unreadable, out);
        }
    }

    fn fail(&mut self, error: ToolInputError, out: &mut String) {
        self.error = Some(error);
        self.completed = true;
        let seq = self.next_seq();
        push_event(out, "response.failed", &failure(&self.response_id, seq));
    }

    fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    /// The first chunk: `response.created` and `response.in_progress`.
    fn start(&mut self, root: &Value, out: &mut String) {
        let mut id = str_of(root.get("responseId")).into_owned();
        if id.is_empty() {
            id = new_response_id();
        }
        if !id.starts_with("resp_") {
            id = format!("resp_{id}");
        }
        self.response_id = id;
        if let Some(created) = root
            .get("createTime")
            .and_then(|time| parse_rfc3339(&str_of(Some(time))))
        {
            self.created_at = created;
        }
        if self.created_at == 0 {
            self.created_at = now();
        }

        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": "in_progress",
            "background": false,
            "error": null,
            "output": [],
        });
        if !self.response_model.is_empty() {
            response["model"] = self.response_model.clone().into();
        }
        let seq = self.next_seq();
        let created = json!({
            "type": "response.created",
            "sequence_number": seq,
            "response": response,
        });
        push_event(out, "response.created", &created);

        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": "in_progress",
            "output": [],
        });
        if !self.response_model.is_empty() {
            response["model"] = self.response_model.clone().into();
        }
        let seq = self.next_seq();
        let in_progress = json!({
            "type": "response.in_progress",
            "sequence_number": seq,
            "response": response,
        });
        push_event(out, "response.in_progress", &in_progress);

        self.started = true;
        self.next_index = 0;
    }

    /// A chunk's grounding metadata: merged with what came before, it can
    /// open the search item and add citations to finished messages.
    fn ground(&mut self, metadata: &Value, out: &mut String) {
        self.grounding = merge_grounding_metadata(self.grounding.as_ref(), Some(metadata));
        let merged = self.grounding.as_ref();
        let queries = grounding_queries(merged);
        let sources = grounding_sources(merged);
        let valid = has_valid_web_grounding(merged);
        if !queries.is_empty() {
            if self.web_search_query.is_empty() {
                self.web_search_query = queries[0].clone();
            }
            self.web_search_queries = queries;
        }
        if !sources.is_empty() {
            self.web_search_sources = sources;
        }
        if !self.web_search_opened && valid {
            self.open_web_search(out);
        }
        self.emit_late_citations(out);
    }

    /// The query the search item names: Gemini's first, else the request's.
    fn fill_web_search_query(&mut self) {
        if self.web_search_query.is_empty()
            && let Some(query) = self.web_search_queries.first()
        {
            self.web_search_query = query.clone();
        }
        if self.web_search_query.is_empty()
            && let Some(query) = &self.request_query
        {
            self.web_search_query = query.clone();
        }
    }

    fn web_search_item(&self) -> Value {
        build_web_search_call_item(
            &self.web_search_id,
            &self.web_search_query,
            &self.web_search_queries,
            &self.web_search_sources,
        )
    }

    fn open_web_search(&mut self, out: &mut String) {
        if self.web_search_opened {
            return;
        }
        self.finalize_reasoning(out);
        self.web_search_opened = true;
        self.web_search_index = self.next_index;
        self.next_index += 1;
        self.web_search_id = format!(
            "ws_{}",
            self.response_id
                .strip_prefix("resp_")
                .unwrap_or(&self.response_id)
        );
        self.fill_web_search_query();

        let seq = self.next_seq();
        let added = json!({
            "type": "response.output_item.added",
            "sequence_number": seq,
            "output_index": self.web_search_index,
            "item": {
                "id": self.web_search_id,
                "type": "web_search_call",
                "status": "in_progress",
                "action": {"type": "search", "query": self.web_search_query},
            },
        });
        push_event(out, "response.output_item.added", &added);
        let seq = self.next_seq();
        let searching = json!({
            "type": "response.web_search_call.searching",
            "sequence_number": seq,
            "output_index": self.web_search_index,
            "item_id": self.web_search_id,
        });
        push_event(out, "response.web_search_call.searching", &searching);
    }

    fn finalize_web_search(&mut self, out: &mut String) {
        if !self.web_search_opened || self.web_search_done {
            return;
        }
        self.fill_web_search_query();
        let seq = self.next_seq();
        let completed = json!({
            "type": "response.web_search_call.completed",
            "sequence_number": seq,
            "output_index": self.web_search_index,
            "item_id": self.web_search_id,
        });
        push_event(out, "response.web_search_call.completed", &completed);
        let seq = self.next_seq();
        let done = json!({
            "type": "response.output_item.done",
            "sequence_number": seq,
            "output_index": self.web_search_index,
            "item": self.web_search_item(),
        });
        push_event(out, "response.output_item.done", &done);
        self.web_search_done = true;
    }

    /// The reasoning item's `encrypted_content`: its signature, wrapped as a
    /// carrier if it belongs to a neighbour.
    fn reasoning_encrypted_content(&self) -> String {
        if self.reasoning_signature.is_empty() || self.reasoning_direction.is_empty() {
            return self.reasoning_signature.clone();
        }
        encode(
            &self.reasoning_signature,
            self.reasoning_direction,
            self.reasoning_target,
        )
    }

    fn open_reasoning(&mut self, out: &mut String) {
        if self.reasoning_opened
            || self.reasoning_closed
            || (self.reasoning_text.is_empty() && self.reasoning_signature.is_empty())
        {
            return;
        }
        self.finalize_web_search(out);
        self.reasoning_opened = true;
        self.reasoning_index = self.next_index;
        self.next_index += 1;
        self.reasoning_id = format!("rs_{}_{}", self.response_id, self.reasoning_index);
        let seq = self.next_seq();
        let added = json!({
            "type": "response.output_item.added",
            "sequence_number": seq,
            "output_index": self.reasoning_index,
            "item": {
                "id": self.reasoning_id,
                "type": "reasoning",
                "status": "in_progress",
                "encrypted_content": self.reasoning_encrypted_content(),
                "summary": [],
            },
        });
        push_event(out, "response.output_item.added", &added);
        let seq = self.next_seq();
        let part_added = json!({
            "type": "response.reasoning_summary_part.added",
            "sequence_number": seq,
            "item_id": self.reasoning_id,
            "output_index": self.reasoning_index,
            "summary_index": 0,
            "part": {"type": "summary_text", "text": ""},
        });
        push_event(out, "response.reasoning_summary_part.added", &part_added);
        for delta in std::mem::take(&mut self.reasoning_pending_deltas) {
            self.push_reasoning_delta(&delta, out);
        }
    }

    fn push_reasoning_delta(&mut self, delta: &str, out: &mut String) {
        let seq = self.next_seq();
        let event = json!({
            "type": "response.reasoning_summary_text.delta",
            "sequence_number": seq,
            "item_id": self.reasoning_id,
            "output_index": self.reasoning_index,
            "summary_index": 0,
            "delta": delta,
        });
        push_event(out, "response.reasoning_summary_text.delta", &event);
    }

    /// Ends the reasoning item, opening it first if it has anything, once.
    fn finalize_reasoning(&mut self, out: &mut String) {
        self.open_reasoning(out);
        if !self.reasoning_opened || self.reasoning_closed {
            return;
        }
        let full = self.reasoning_text.clone();
        let seq = self.next_seq();
        let text_done = json!({
            "type": "response.reasoning_summary_text.done",
            "sequence_number": seq,
            "item_id": self.reasoning_id,
            "output_index": self.reasoning_index,
            "summary_index": 0,
            "text": full,
        });
        push_event(out, "response.reasoning_summary_text.done", &text_done);
        let seq = self.next_seq();
        let part_done = json!({
            "type": "response.reasoning_summary_part.done",
            "sequence_number": seq,
            "item_id": self.reasoning_id,
            "output_index": self.reasoning_index,
            "summary_index": 0,
            "part": {"type": "summary_text", "text": full},
        });
        push_event(out, "response.reasoning_summary_part.done", &part_done);
        let encrypted_content = self.reasoning_encrypted_content();
        let seq = self.next_seq();
        let item_done = json!({
            "type": "response.output_item.done",
            "sequence_number": seq,
            "output_index": self.reasoning_index,
            "item": {
                "id": self.reasoning_id,
                "type": "reasoning",
                "encrypted_content": encrypted_content,
                "summary": [{"type": "summary_text", "text": full}],
            },
        });
        push_event(out, "response.output_item.done", &item_done);
        self.completed_reasoning.insert(
            self.reasoning_index,
            ReasoningItem {
                id: self.reasoning_id.clone(),
                encrypted_content,
                text: full,
            },
        );
        self.reasoning_closed = true;
    }

    fn reset_reasoning(&mut self) {
        self.reasoning_opened = false;
        self.reasoning_closed = false;
        self.reasoning_index = 0;
        self.reasoning_id.clear();
        self.reasoning_signature.clear();
        self.reasoning_direction = "";
        self.reasoning_target = "";
        self.reasoning_text.clear();
        self.reasoning_pending_deltas.clear();
    }

    fn reset_message(&mut self) {
        self.msg_opened = false;
        self.msg_closed = false;
        self.msg_text.clear();
        self.msg_rune_offset = 0;
    }

    fn open_message(&mut self, out: &mut String) {
        self.msg_opened = true;
        self.msg_index = self.next_index;
        self.next_index += 1;
        self.msg_id = format!("msg_{}_{}", self.response_id, self.msg_index);
        let seq = self.next_seq();
        let added = json!({
            "type": "response.output_item.added",
            "sequence_number": seq,
            "output_index": self.msg_index,
            "item": {
                "id": self.msg_id,
                "type": "message",
                "status": "in_progress",
                "content": [],
                "role": "assistant",
            },
        });
        push_event(out, "response.output_item.added", &added);
        let seq = self.next_seq();
        let part_added = json!({
            "type": "response.content_part.added",
            "sequence_number": seq,
            "item_id": self.msg_id,
            "output_index": self.msg_index,
            "content_index": 0,
            "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": ""},
        });
        push_event(out, "response.content_part.added", &part_added);
        self.msg_text.clear();
        self.msg_rune_offset = 0;
    }

    fn push_text_delta(&mut self, delta: &str, out: &mut String) {
        let seq = self.next_seq();
        let event = json!({
            "type": "response.output_text.delta",
            "sequence_number": seq,
            "item_id": self.msg_id,
            "output_index": self.msg_index,
            "content_index": 0,
            "delta": delta,
            "logprobs": [],
        });
        push_event(out, "response.output_text.delta", &event);
    }

    /// Records where a Gemini part's text sits in the open message.
    fn map_part(&mut self, part_index: i64, text: &str) {
        match self.part_mappings.last_mut() {
            Some(last) if last.part_index == part_index && last.message_index == self.msg_index => {
                last.text.push_str(text);
            }
            _ => self.part_mappings.push(PartMapping {
                part_index,
                message_index: self.msg_index,
                start_rune: self.msg_rune_offset,
                text: text.to_owned(),
            }),
        }
        self.msg_rune_offset += text.chars().count() as i64;
    }

    /// Sends the text held back for the search, now that it has finished.
    fn flush_buffered_text(&mut self, out: &mut String) {
        self.finalize_web_search(out);
        if self.buffered_deltas.is_empty() {
            return;
        }
        if self.msg_closed {
            self.reset_message();
        }
        if !self.msg_opened {
            self.open_message(out);
        }
        for delta in std::mem::take(&mut self.buffered_deltas) {
            self.msg_text.push_str(&delta);
            self.push_text_delta(&delta, out);
        }
        for (part_index, text) in std::mem::take(&mut self.buffered_parts) {
            self.map_part(part_index, &text);
        }
    }

    fn emit_new_annotations(
        &mut self,
        msg_index: i64,
        item_id: &str,
        annotations: &[Value],
        out: &mut String,
    ) {
        let emitted = self
            .emitted_annotations
            .get(&msg_index)
            .copied()
            .unwrap_or(0);
        for (annotation_index, annotation) in annotations.iter().enumerate().skip(emitted) {
            let seq = self.next_seq();
            let event = json!({
                "type": "response.output_text.annotation.added",
                "sequence_number": seq,
                "response_id": self.response_id,
                "item_id": item_id,
                "output_index": msg_index,
                "content_index": 0,
                "annotation_index": annotation_index,
                "annotation": annotation,
            });
            push_event(out, "response.output_text.annotation.added", &event);
        }
        if annotations.len() > emitted {
            self.emitted_annotations
                .insert(msg_index, annotations.len());
        }
    }

    /// Ends the assistant message, with any citations, once. Its item gets
    /// [`message_status`](Self::message_status).
    fn finalize_message(&mut self, out: &mut String) {
        self.finalize_web_search(out);
        if !self.buffered_deltas.is_empty() {
            self.flush_buffered_text(out);
        }
        if !self.msg_opened || self.msg_closed {
            return;
        }
        let full = self.msg_text.clone();
        let mut citations = Vec::new();
        if let Some(mut by_message) = build_url_citations_for_messages(
            self.grounding.as_ref(),
            &self.part_mappings,
            std::slice::from_ref(&full),
        ) {
            citations = by_message.remove(&self.msg_index).unwrap_or_default();
            if citations.is_empty()
                && self.completed_messages.is_empty()
                && let Some(first) = by_message.remove(&0)
            {
                citations = first;
            }
        }
        let msg_id = self.msg_id.clone();
        self.emit_new_annotations(self.msg_index, &msg_id, &citations, out);
        let seq = self.next_seq();
        let done = json!({
            "type": "response.output_text.done",
            "sequence_number": seq,
            "item_id": self.msg_id,
            "output_index": self.msg_index,
            "content_index": 0,
            "text": full,
            "logprobs": [],
        });
        push_event(out, "response.output_text.done", &done);
        let seq = self.next_seq();
        let part_done = json!({
            "type": "response.content_part.done",
            "sequence_number": seq,
            "item_id": self.msg_id,
            "output_index": self.msg_index,
            "content_index": 0,
            "part": output_text_part(&full, &citations),
        });
        push_event(out, "response.content_part.done", &part_done);
        let seq = self.next_seq();
        let item_done = json!({
            "type": "response.output_item.done",
            "sequence_number": seq,
            "output_index": self.msg_index,
            "item": message_item(&self.msg_id, self.message_status, &full, &citations),
        });
        push_event(out, "response.output_item.done", &item_done);
        self.completed_messages.insert(
            self.msg_index,
            MessageItem {
                id: self.msg_id.clone(),
                text: full,
                status: self.message_status,
                annotations: citations,
            },
        );
        self.msg_closed = true;
        self.msg_rune_offset = 0;
    }

    /// Citations for finished messages that grounding sent after them.
    fn emit_late_citations(&mut self, out: &mut String) {
        if self.grounding.is_none() || self.completed_messages.is_empty() {
            return;
        }
        let texts: Vec<String> = self
            .completed_messages
            .values()
            .map(|message| message.text.clone())
            .collect();
        let Some(by_message) =
            build_url_citations_for_messages(self.grounding.as_ref(), &self.part_mappings, &texts)
        else {
            return;
        };
        let single = self.completed_messages.len() == 1;
        let indexes: Vec<i64> = self.completed_messages.keys().copied().collect();
        for index in indexes {
            let mut late = by_message.get(&index).cloned().unwrap_or_default();
            if late.is_empty()
                && single
                && let Some(first) = by_message.get(&0)
            {
                late = first.clone();
            }
            let message = &self.completed_messages[&index];
            let id = message.id.clone();
            let annotations = merge_citation_annotations(message.annotations.clone(), late);
            self.emit_new_annotations(index, &id, &annotations, out);
            if !annotations.is_empty()
                && let Some(message) = self.completed_messages.get_mut(&index)
            {
                message.annotations = annotations;
            }
        }
    }

    /// Sends `signature` as a carrier of its own.
    fn emit_detached(
        &mut self,
        signature: &str,
        direction: &'static str,
        target: &'static str,
        out: &mut String,
    ) {
        let signature = signature.trim();
        if signature.is_empty() || self.seen_signatures.contains(signature) {
            return;
        }
        self.finalize_reasoning(out);
        self.finalize_message(out);
        let index = self.next_index;
        self.next_index += 1;
        let placement = if direction == PREVIOUS {
            "after"
        } else {
            "before"
        };
        let id = format!("rs_{}_detached_{placement}_{index}", self.response_id);
        let carrier = encode(signature, direction, target);
        let seq = self.next_seq();
        let added = json!({
            "type": "response.output_item.added",
            "sequence_number": seq,
            "output_index": index,
            "item": {
                "id": id,
                "type": "reasoning",
                "status": "in_progress",
                "encrypted_content": carrier,
                "summary": [],
            },
        });
        push_event(out, "response.output_item.added", &added);
        let seq = self.next_seq();
        let done = json!({
            "type": "response.output_item.done",
            "sequence_number": seq,
            "output_index": index,
            "item": {"id": id, "type": "reasoning", "encrypted_content": carrier, "summary": []},
        });
        push_event(out, "response.output_item.done", &done);
        self.detached.insert(index, DetachedItem { id, carrier });
        self.seen_signatures.insert(signature.to_owned());
    }

    /// A signature that came after the last item: kept with the message's
    /// text if that was a message, else sent as a carrier for it.
    fn emit_trailing(&mut self, signature: &str, out: &mut String) {
        match self.last_kind {
            TEXT => {
                let signature = signature.trim();
                if signature.is_empty() || self.seen_signatures.contains(signature) {
                    return;
                }
                self.finalize_reasoning(out);
                self.finalize_message(out);
                // The last kind also covers thought text. Never bind a later
                // thought's signature to a message from before that thought.
                if !self.msg_opened
                    || (self.reasoning_opened && self.reasoning_index > self.msg_index)
                {
                    self.emit_detached(signature, PREVIOUS, TEXT, out);
                    return;
                }
                // Keep failed writes in the list, so a later write can't put a
                // newer signature ahead of an earlier carrier.
                let signatures = self
                    .hidden_text_signatures
                    .entry(self.msg_id.clone())
                    .or_default();
                signatures.push(signature.to_owned());
                if cache_text_signatures(&self.model, &self.msg_id, &self.msg_text, signatures) {
                    self.seen_signatures.insert(signature.to_owned());
                    return;
                }
                // Keep replay working if the cache won't take it.
                self.emit_detached(signature, PREVIOUS, TEXT, out);
            }
            FUNCTION => self.emit_detached(signature, PREVIOUS, FUNCTION, out),
            _ => self.emit_detached(signature, STANDALONE, ANY, out),
        }
    }

    /// One part of a chunk; `chunk_index` is its place in the chunk. Returns
    /// `false` if the stream failed.
    fn part(
        &mut self,
        chunk_index: usize,
        part: &Value,
        args: RawArgs<'_>,
        out: &mut String,
    ) -> bool {
        let explicit_index = explicit_part_index(part);
        let signature = part_signature(part);
        let function_call = part.get("functionCall");
        let text_value = part.get("text");
        let text = str_of(text_value);
        let has_text = text_value.is_some();
        let is_thought = part.get("thought").is_some_and(bool_of);
        let kind = if is_thought {
            "thought"
        } else if function_call.is_some() {
            "function"
        } else if has_text {
            "text"
        } else {
            "unknown"
        };

        let current_index;
        if explicit_index >= 0 {
            current_index = explicit_index;
            self.logical_part_index = explicit_index;
            self.part_kind = kind;
            self.seen_first_part = true;
            self.text_run = kind == "text";
        } else if !self.seen_first_part {
            self.seen_first_part = true;
            self.logical_part_index = 0;
            self.part_kind = kind;
            current_index = 0;
            if kind == "text" {
                self.text_run = true;
            }
        } else {
            if chunk_index > 0 || kind != self.part_kind {
                self.logical_part_index = self.logical_part_index.wrapping_add(1);
                self.part_kind = kind;
                self.text_run = kind == "text";
            } else if kind == "function" {
                self.logical_part_index = self.logical_part_index.wrapping_add(1);
                self.part_kind = kind;
                self.text_run = false;
            } else if kind == "text" && !self.text_run {
                self.logical_part_index = self.logical_part_index.wrapping_add(1);
                self.part_kind = kind;
                self.text_run = true;
            }
            current_index = self.logical_part_index;
        }

        if function_call.is_some() && !self.pending_signature.is_empty() {
            let pending = std::mem::take(&mut self.pending_signature);
            if signature.is_empty() {
                self.emit_detached(&pending, NEXT, FUNCTION, out);
            } else {
                self.emit_trailing(&pending, out);
            }
        }
        let reasoning_active = (self.reasoning_opened && !self.reasoning_closed)
            || (!self.reasoning_opened
                && (!self.reasoning_text.is_empty() || !self.reasoning_signature.is_empty()));
        if !signature.is_empty() && !is_thought {
            if reasoning_active {
                if self.reasoning_signature.is_empty() || self.reasoning_signature == signature {
                    self.reasoning_signature = signature.clone();
                    (self.reasoning_direction, self.reasoning_target) = if function_call.is_some() {
                        (NEXT, FUNCTION)
                    } else if !text.is_empty() {
                        (NEXT, TEXT)
                    } else {
                        (STANDALONE, TEXT)
                    };
                    self.seen_signatures.insert(signature.clone());
                } else {
                    self.finalize_reasoning(out);
                    if function_call.is_some() {
                        self.emit_detached(&signature, NEXT, FUNCTION, out);
                    } else if !self.seen_signatures.contains(&signature) {
                        self.pending_signature = signature.clone();
                    }
                }
                if has_text && text.is_empty() && function_call.is_none() {
                    self.finalize_reasoning(out);
                    return true;
                }
            } else if function_call.is_some() {
                self.emit_detached(&signature, NEXT, FUNCTION, out);
            } else if !text.is_empty() {
                if !self.pending_signature.is_empty() && self.pending_signature != signature {
                    let pending = std::mem::take(&mut self.pending_signature);
                    self.emit_trailing(&pending, out);
                }
                if !self.seen_signatures.contains(&signature) {
                    self.pending_signature = signature.clone();
                }
            } else if has_text {
                let pending = std::mem::take(&mut self.pending_signature);
                if !pending.is_empty() && pending != signature {
                    self.emit_trailing(&pending, out);
                }
                if self.msg_opened || !self.calls.is_empty() || !self.buffered_deltas.is_empty() {
                    self.emit_trailing(&signature, out);
                } else if !self.seen_signatures.contains(&signature) {
                    self.pending_signature = signature.clone();
                }
                return true;
            }
        }

        if is_thought {
            self.thought(&signature, &text, out);
            return true;
        }

        if !text.is_empty() {
            self.visible_text(&signature, &text, current_index, out);
            return true;
        }

        if let Some(call) = function_call {
            return self.function_call(call, args, explicit_index, out);
        }
        true
    }

    fn thought(&mut self, signature: &str, text: &str, out: &mut String) {
        if !self.buffered_deltas.is_empty() {
            self.finalize_message(out);
        }
        if !self.pending_signature.is_empty() && self.msg_opened && !self.msg_closed {
            let pending = std::mem::take(&mut self.pending_signature);
            self.emit_trailing(&pending, out);
        }
        let mut incoming = String::new();
        if !signature.is_empty() && signature != GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR {
            let pending = std::mem::take(&mut self.pending_signature);
            if !pending.is_empty() && pending != signature {
                self.emit_detached(&pending, STANDALONE, ANY, out);
            }
            incoming = signature.to_owned();
        } else if !self.pending_signature.is_empty() {
            incoming = std::mem::take(&mut self.pending_signature);
        }
        if self.reasoning_opened
            && !self.reasoning_closed
            && !incoming.is_empty()
            && !self.reasoning_signature.is_empty()
            && incoming != self.reasoning_signature
        {
            self.finalize_reasoning(out);
            self.reset_reasoning();
        }
        if self.reasoning_closed {
            self.finalize_message(out);
            self.reset_reasoning();
        } else if !self.reasoning_opened
            && self.reasoning_text.is_empty()
            && self.msg_opened
            && !self.msg_closed
        {
            self.finalize_message(out);
        }
        if !incoming.is_empty() {
            self.reasoning_signature = incoming.clone();
            self.reasoning_direction = STANDALONE;
            self.reasoning_target = TEXT;
            self.seen_signatures.insert(incoming);
        }
        if !text.is_empty() {
            self.last_kind = TEXT;
            self.reasoning_text.push_str(text);
            if self.reasoning_opened {
                self.push_reasoning_delta(text, out);
            } else {
                self.reasoning_pending_deltas.push(text.to_owned());
            }
        }
        if !self.reasoning_opened && !self.reasoning_signature.is_empty() {
            self.open_reasoning(out);
        }
    }

    fn visible_text(&mut self, signature: &str, text: &str, part_index: i64, out: &mut String) {
        if signature.is_empty()
            && !self.pending_signature.is_empty()
            && ((self.msg_opened && !self.msg_closed) || !self.buffered_deltas.is_empty())
        {
            let pending = std::mem::take(&mut self.pending_signature);
            self.emit_trailing(&pending, out);
        }
        // Output items are sequential: finish reasoning before opening the
        // message. A signature that comes later is cached with the message.
        self.finalize_reasoning(out);
        if self.msg_closed {
            self.reset_message();
        }
        // Hold text back until the search finishes, so the search item has
        // every source and comes before the message.
        if self.web_search_mode && !self.web_search_done {
            self.last_kind = TEXT;
            self.buffered_deltas.push(text.to_owned());
            match self.buffered_parts.last_mut() {
                Some((last_index, last_text)) if *last_index == part_index => {
                    last_text.push_str(text);
                }
                _ => self.buffered_parts.push((part_index, text.to_owned())),
            }
            self.text_run = true;
            return;
        }
        if !self.msg_opened {
            self.open_message(out);
        }
        self.last_kind = TEXT;
        self.msg_text.push_str(text);
        self.map_part(part_index, text);
        self.push_text_delta(text, out);
        self.text_run = true;
    }

    fn function_call(
        &mut self,
        call: &Value,
        args: RawArgs<'_>,
        explicit_index: i64,
        out: &mut String,
    ) -> bool {
        // Responses streams need every other item done before a call starts.
        self.finalize_reasoning(out);
        self.finalize_web_search(out);
        if !self.buffered_deltas.is_empty() {
            self.flush_buffered_text(out);
        }
        self.finalize_message(out);
        self.last_kind = FUNCTION;

        let raw_args = args.read(call.get("args"));
        let evidence_index = match self.evidence.record(
            &self.identities,
            call,
            raw_args.as_deref().unwrap_or(""),
            explicit_index,
        ) {
            Ok(index) => index,
            Err(error) => {
                self.fail(error, out);
                return false;
            }
        };
        let evidence = &self.evidence.items[evidence_index];
        if evidence.apply_patch
            && let Some(error) = &evidence.error
        {
            let error = error.clone();
            self.fail(error, out);
            return false;
        }
        if evidence.raw_name.is_empty() {
            return true;
        }
        let raw_name = if evidence.apply_patch {
            evidence.raw_name.clone()
        } else {
            str_of(call.get("name")).into_owned()
        };
        let identity = self.identity(&raw_name);
        if evidence.apply_patch && evidence.patch_call.is_some() {
            // A repeated snapshot of a call already sent.
            let result = self.evidence.items[evidence_index]
                .patch_call
                .as_mut()
                .map(|patch| patch.finish_arguments(raw_args.as_deref().unwrap_or("")));
            if let Some(Err(error)) = result {
                self.fail(ToolInputError::Arguments(error), out);
                return false;
            }
            return true;
        }
        let upstream_id = evidence.upstream_id.clone();

        let index = self.next_index;
        self.next_index += 1;
        let mut call_id = if identity.apply_patch {
            upstream_id
        } else {
            String::new()
        };
        if call_id.is_empty() {
            call_id = new_stream_call_id();
        }
        let args_json = raw_args.map_or_else(|| "{}".to_owned(), Cow::into_owned);

        if identity.custom {
            let item_id = format!("ctc_{call_id}");
            let mut input = unwrap_responses_custom_tool_input(&args_json);
            let mut patch = None;
            if identity.apply_patch {
                let mut state = CallState::new(item_id.clone(), call_id.clone(), index);
                match state.finish_arguments(&args_json) {
                    Ok((_, decoded)) => input = decoded,
                    Err(error) => {
                        self.fail(ToolInputError::Arguments(error), out);
                        return false;
                    }
                }
                patch = Some(state);
            }

            let seq = self.next_seq();
            let mut added = json!({
                "type": "response.output_item.added",
                "sequence_number": seq,
                "output_index": index,
                "item": {
                    "id": item_id,
                    "type": "custom_tool_call",
                    "status": "in_progress",
                    "input": "",
                    "call_id": call_id,
                    "name": "",
                },
            });
            set_tool_call_identity(&mut added["item"], &identity.name, &identity.namespace);
            push_event(out, "response.output_item.added", &added);
            // Gemini sends complete arguments; this delta is not an early
            // preview.
            if let Some(state) = &patch
                && !input.is_empty()
            {
                let seq = self.next_seq();
                push_event(
                    out,
                    "response.custom_tool_call_input.delta",
                    &state.input_delta(&input, seq),
                );
            }
            let seq = self.next_seq();
            let input_done = match &patch {
                Some(state) => state.input_done(&input, seq),
                None => json!({
                    "type": "response.custom_tool_call_input.done",
                    "sequence_number": seq,
                    "item_id": item_id,
                    "output_index": index,
                    "input": input,
                }),
            };
            push_event(out, "response.custom_tool_call_input.done", &input_done);
            let seq = self.next_seq();
            let mut item_done = json!({
                "type": "response.output_item.done",
                "sequence_number": seq,
                "output_index": index,
                "item": {
                    "id": item_id,
                    "type": "custom_tool_call",
                    "status": "completed",
                    "input": input,
                    "call_id": call_id,
                    "name": "",
                },
            });
            set_tool_call_identity(&mut item_done["item"], &identity.name, &identity.namespace);
            push_event(out, "response.output_item.done", &item_done);
            if let Some(state) = patch {
                self.evidence.items[evidence_index].patch_call = Some(state);
            }
            self.calls.insert(
                index,
                Call {
                    call_id,
                    name: identity.name,
                    namespace: identity.namespace,
                    custom: true,
                    args: args_json,
                    input,
                },
            );
        } else {
            let item_id = format!("fc_{call_id}");
            let seq = self.next_seq();
            let mut added = json!({
                "type": "response.output_item.added",
                "sequence_number": seq,
                "output_index": index,
                "item": {
                    "id": item_id,
                    "type": "function_call",
                    "status": "in_progress",
                    "arguments": "",
                    "call_id": call_id,
                    "name": "",
                },
            });
            set_tool_call_identity(&mut added["item"], &identity.name, &identity.namespace);
            push_event(out, "response.output_item.added", &added);
            // Gemini sends the whole call at once.
            let seq = self.next_seq();
            let delta = json!({
                "type": "response.function_call_arguments.delta",
                "sequence_number": seq,
                "item_id": item_id,
                "output_index": index,
                "delta": args_json,
            });
            push_event(out, "response.function_call_arguments.delta", &delta);
            let seq = self.next_seq();
            let args_done = json!({
                "type": "response.function_call_arguments.done",
                "sequence_number": seq,
                "item_id": item_id,
                "output_index": index,
                "arguments": args_json,
            });
            push_event(out, "response.function_call_arguments.done", &args_done);
            let seq = self.next_seq();
            let mut item_done = json!({
                "type": "response.output_item.done",
                "sequence_number": seq,
                "output_index": index,
                "item": {
                    "id": item_id,
                    "type": "function_call",
                    "status": "completed",
                    "arguments": args_json,
                    "call_id": call_id,
                    "name": "",
                },
            });
            set_tool_call_identity(&mut item_done["item"], &identity.name, &identity.namespace);
            push_event(out, "response.output_item.done", &item_done);
            self.calls.insert(
                index,
                Call {
                    call_id,
                    name: identity.name,
                    namespace: identity.namespace,
                    custom: false,
                    args: args_json,
                    input: String::new(),
                },
            );
        }
        true
    }

    /// The tool a Gemini function name stands for.
    fn identity(&self, raw_name: &str) -> ToolIdentity {
        self.identities
            .get(raw_name)
            .cloned()
            .unwrap_or_else(|| ToolIdentity {
                name: restore_sanitized_tool_name(self.sanitized_names.as_ref(), raw_name),
                ..ToolIdentity::default()
            })
    }

    /// A chunk after the finish reason: fails the stream for a call whose
    /// identity was never settled, else, if `terminal` (the chunk had usage
    /// or was `[DONE]`), ends every open item, then sends
    /// `response.completed`, or `response.incomplete` after `MAX_TOKENS`.
    fn finish(&mut self, terminal: bool, out: &mut String) {
        if let Some(error) = self.evidence.pending_identity_error(&self.identities) {
            self.fail(error, out);
            return;
        }
        if !terminal {
            return;
        }
        let (event_type, status, incomplete_details) = terminal_state(&self.finish_reason);
        self.message_status = status;
        if !self.pending_signature.is_empty() {
            let pending = std::mem::take(&mut self.pending_signature);
            self.emit_trailing(&pending, out);
        }
        // The search first, with every source, so it comes before later
        // items; then reasoning, then the message. Upstream then closes any
        // call still open, but each call is closed as it arrives.
        self.finalize_web_search(out);
        self.finalize_reasoning(out);
        self.finalize_message(out);

        let seq = self.next_seq();
        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "background": false,
            "error": null,
        });
        if let Some(details) = incomplete_details {
            response["incomplete_details"] = details;
        }
        for (key, value) in self.echo.iter().flatten() {
            response[*key] = value.clone();
        }

        self.emit_late_citations(out);

        let mut outputs = Vec::new();
        for index in 0..self.next_index {
            if self.web_search_done && index == self.web_search_index {
                outputs.push(self.web_search_item());
            } else if let Some(reasoning) = self.completed_reasoning.get(&index) {
                outputs.push(json!({
                    "id": reasoning.id,
                    "type": "reasoning",
                    "encrypted_content": reasoning.encrypted_content,
                    "summary": [{"type": "summary_text", "text": reasoning.text}],
                }));
            } else if let Some(message) = self.completed_messages.get(&index) {
                outputs.push(message_item(
                    &message.id,
                    message.status,
                    &message.text,
                    &message.annotations,
                ));
            } else if let Some(detached) = self.detached.get(&index) {
                outputs.push(json!({
                    "id": detached.id,
                    "type": "reasoning",
                    "encrypted_content": detached.carrier,
                    "summary": [],
                }));
            } else if let Some(call) = self
                .calls
                .get(&index)
                .filter(|call| !call.call_id.is_empty())
            {
                let mut item = if call.custom {
                    json!({
                        "id": format!("ctc_{}", call.call_id),
                        "type": "custom_tool_call",
                        "status": "completed",
                        "input": call.input,
                        "call_id": call.call_id,
                        "name": "",
                    })
                } else {
                    json!({
                        "id": format!("fc_{}", call.call_id),
                        "type": "function_call",
                        "status": "completed",
                        "arguments": if call.args.is_empty() { "{}" } else { &call.args },
                        "call_id": call.call_id,
                        "name": "",
                    })
                };
                set_tool_call_identity(&mut item, &call.name, &call.namespace);
                outputs.push(item);
            }
        }
        if !outputs.is_empty() {
            response["output"] = Value::Array(outputs);
        }
        if self.web_search_done {
            response["tool_usage"] = json!({"web_search": {"num_requests": 1}});
        }
        if self.usage.present {
            response["usage"] = self.usage.to_json();
        }
        let completed = object([
            ("type", event_type.into()),
            ("sequence_number", seq.into()),
            ("response", response),
        ]);
        push_event(out, event_type, &completed);
        self.completed = true;
    }
}

/// Where in a chunk's text to find a part's `functionCall.args` as written.
#[derive(Clone, Copy)]
struct RawArgs<'t> {
    text: &'t str,
    /// Whether the chunk wraps the response in `response`.
    wrapped: bool,
    index: usize,
}

impl<'t> RawArgs<'t> {
    /// gjson `Raw` of `args`, the value read from this part: as written, or
    /// as compact JSON if the text holds something else.
    fn read(self, args: Option<&Value>) -> Option<Cow<'t, str>> {
        let args = args?;
        let written = self.find().filter(|written| {
            serde_json::from_str::<Value>(written).is_ok_and(|value| value == *args)
        });
        Some(match written {
            Some(written) => Cow::Borrowed(written),
            None => Cow::Owned(args.to_string()),
        })
    }

    fn find(self) -> Option<&'t str> {
        let root = if self.wrapped {
            raw::member(self.text, "response")?
        } else {
            self.text
        };
        let candidate = raw::element(raw::member(root, "candidates")?, 0)?;
        let parts = raw::member(raw::member(candidate, "content")?, "parts")?;
        let call = raw::member(raw::element(parts, self.index)?, "functionCall")?;
        raw::member(call, "args")
    }
}

/// A part's `partIndex`, else its `index`, else -1.
fn explicit_part_index(part: &Value) -> i64 {
    part.get("partIndex")
        .or_else(|| part.get("index"))
        .map_or(-1, int_of)
}

/// A part's trimmed `thoughtSignature`, else its `thought_signature`.
fn part_signature(part: &Value) -> String {
    let signature = str_of(part.get("thoughtSignature")).trim().to_owned();
    if !signature.is_empty() {
        return signature;
    }
    str_of(part.get("thought_signature")).trim().to_owned()
}

/// `ConvertGeminiResponseToOpenAIResponsesNonStream`: a whole Gemini
/// response as one Responses object. A response without a `responseId` gets
/// a new ID, and one without a `createTime` the current time. A `MAX_TOKENS`
/// finish makes it `incomplete`, and with it the message still open after
/// the last part. `None` if an `apply_patch` call failed, where upstream
/// returns `nil`.
pub fn convert_gemini_response_to_openai_responses_non_stream(
    original_request: &Value,
    request: &Value,
    response: &[u8],
) -> Option<Value> {
    non_stream(original_request, request, response).ok()
}

/// The kind of output item, and its place in that kind's list.
#[derive(Clone, Copy)]
enum Output {
    Detached(usize),
    Reasoning(usize),
    Message(usize),
    Function(usize),
}

/// What the whole-response conversion gathers from the parts.
#[derive(Default)]
struct Gathered {
    reasoning_text: String,
    reasoning_signature: String,
    reasoning_direction: &'static str,
    reasoning_target: &'static str,
    /// Text, signature, direction and target of each reasoning item.
    reasoning: Vec<(String, String, &'static str, &'static str)>,
    reasoning_signatures: HashSet<String>,
    /// Each call's item and the signature that came with it.
    functions: Vec<(Value, String)>,
    /// Each message's text and signatures.
    messages: Vec<(String, Vec<String>)>,
    /// Signature, direction and target of each carrier.
    detached: Vec<(String, &'static str, &'static str)>,
    order: Vec<Output>,
    message_text: String,
    message_signatures: Vec<String>,
    part_mappings: Vec<PartMapping>,
    rune_offset: i64,
}

impl Gathered {
    fn flush_reasoning(&mut self) {
        if self.reasoning_text.is_empty() && self.reasoning_signature.is_empty() {
            return;
        }
        self.order.push(Output::Reasoning(self.reasoning.len()));
        if !self.reasoning_signature.is_empty() {
            self.reasoning_signatures
                .insert(self.reasoning_signature.clone());
        }
        self.reasoning.push((
            std::mem::take(&mut self.reasoning_text),
            std::mem::take(&mut self.reasoning_signature),
            std::mem::take(&mut self.reasoning_direction),
            std::mem::take(&mut self.reasoning_target),
        ));
    }

    fn flush_message(&mut self) {
        if self.message_text.is_empty() {
            return;
        }
        self.order.push(Output::Message(self.messages.len()));
        self.messages.push((
            std::mem::take(&mut self.message_text),
            std::mem::take(&mut self.message_signatures),
        ));
        self.rune_offset = 0;
    }

    fn push_detached(&mut self, signature: String, direction: &'static str, target: &'static str) {
        self.order.push(Output::Detached(self.detached.len()));
        self.detached.push((signature, direction, target));
    }
}

/// [`convert_gemini_response_to_openai_responses_non_stream`], with the error
/// upstream keeps in its parameter.
fn non_stream(
    original_request: &Value,
    request: &Value,
    body: &[u8],
) -> Result<Value, ToolInputError> {
    let picked = pick_request(original_request, request);
    let sanitized_names = sanitized_tool_name_map(original_request);
    let identities = responses_tool_reverse_identity_map(picked.unwrap_or(&Value::Null));
    let body_text = std::str::from_utf8(body).ok();
    let parsed = match body_text.map(serde_json::from_str::<Value>) {
        Some(Ok(parsed)) => parsed,
        _ if !go::trim_space(body).is_empty() && patch_enabled(&identities) => {
            return Err(ToolInputError::Unreadable);
        }
        _ => Value::Null,
    };
    let body_text = body_text.unwrap_or_default();
    let (root, wrapped) = unwrap_response_root(&parsed);
    let (_, status, incomplete_details) =
        terminal_state(&str_of(at(root, "candidates.0.finishReason")));

    let mut id = str_of(root.get("responseId")).into_owned();
    if id.is_empty() {
        id = new_response_id();
    }
    if !id.starts_with("resp_") {
        id = format!("resp_{id}");
    }
    let rid = id.strip_prefix("resp_").unwrap_or(&id).to_owned();
    let mut created_at = now();
    if let Some(created) = root
        .get("createTime")
        .and_then(|time| parse_rfc3339(&str_of(Some(time))))
    {
        created_at = created;
    }
    let mut response = json!({
        "id": id,
        "object": "response",
        "created_at": created_at,
        "status": status,
        "background": false,
        "error": null,
        "incomplete_details": incomplete_details,
    });
    match picked {
        Some(request) => {
            let model_version = root.get("modelVersion");
            let echo = echo_fields(unwrap_request_root(request), |key| {
                if key == "model" { model_version } else { None }
            });
            for (key, value) in echo {
                response[key] = value;
            }
        }
        None => {
            if let Some(version) = root.get("modelVersion") {
                response["model"] = Value::String(str_of(Some(version)).into_owned());
            }
        }
    }

    let mut gathered = Gathered::default();
    let mut evidence = EvidenceStore::default();
    let mut error = None;
    let parts = match at(root, "candidates.0.content.parts") {
        Some(Value::Array(parts)) => parts.as_slice(),
        _ => &[],
    };
    for (key, part) in parts.iter().enumerate() {
        let explicit_index = explicit_part_index(part);
        let part_index = if part.get("partIndex").is_some() || part.get("index").is_some() {
            explicit_index
        } else {
            key as i64
        };
        let mut signature = part_signature(part);
        let g = &mut gathered;
        if part.get("thought").is_some_and(bool_of) {
            g.flush_message();
            g.rune_offset = 0;
            if !signature.is_empty()
                && !g.reasoning_signature.is_empty()
                && signature != g.reasoning_signature
            {
                g.flush_reasoning();
            }
            if let Some(text) = part.get("text") {
                g.reasoning_text.push_str(&str_of(Some(text)));
            }
            if !signature.is_empty() {
                g.reasoning_signature = signature;
                g.reasoning_direction = STANDALONE;
                g.reasoning_target = TEXT;
            }
            continue;
        }
        let text = str_of(part.get("text"));
        if !text.is_empty() {
            let mut message_signature = String::new();
            if !signature.is_empty() {
                if !g.reasoning_text.is_empty() && g.reasoning_signature.is_empty() {
                    g.reasoning_signature = signature;
                    g.reasoning_direction = NEXT;
                    g.reasoning_target = TEXT;
                } else {
                    message_signature = signature;
                }
            }
            g.flush_reasoning();
            if g.message_signatures
                .last()
                .is_some_and(|last| message_signature.is_empty() || *last != message_signature)
            {
                g.flush_message();
                g.rune_offset = 0;
            }
            g.part_mappings.push(PartMapping {
                part_index,
                message_index: g.messages.len() as i64,
                start_rune: g.rune_offset,
                text: text.clone().into_owned(),
            });
            g.rune_offset += text.chars().count() as i64;
            g.message_text.push_str(&text);
            if !message_signature.is_empty()
                && g.message_signatures.last() != Some(&message_signature)
            {
                g.message_signatures.push(message_signature);
            }
            continue;
        }
        if let Some(call) = part.get("functionCall") {
            if !g.reasoning_text.is_empty()
                && g.reasoning_signature.is_empty()
                && !signature.is_empty()
            {
                g.reasoning_signature = std::mem::take(&mut signature);
                g.reasoning_direction = NEXT;
                g.reasoning_target = FUNCTION;
            }
            g.flush_reasoning();
            g.flush_message();
            g.rune_offset = 0;

            let raw_args = RawArgs {
                text: body_text,
                wrapped,
                index: key,
            }
            .read(call.get("args"));
            let evidence_index = match evidence.record(
                &identities,
                call,
                raw_args.as_deref().unwrap_or(""),
                explicit_index,
            ) {
                Ok(index) => index,
                Err(failure) => {
                    error = Some(failure);
                    break;
                }
            };
            let call_evidence = &mut evidence.items[evidence_index];
            if call_evidence.apply_patch
                && let Some(failure) = &call_evidence.error
            {
                error = Some(failure.clone());
                break;
            }
            if call_evidence.raw_name.is_empty() {
                continue;
            }
            let raw_name = if call_evidence.apply_patch {
                call_evidence.raw_name.clone()
            } else {
                str_of(call.get("name")).into_owned()
            };
            let identity = identities
                .get(&raw_name)
                .cloned()
                .unwrap_or_else(|| ToolIdentity {
                    name: restore_sanitized_tool_name(sanitized_names.as_ref(), &raw_name),
                    ..ToolIdentity::default()
                });
            if identity.apply_patch
                && let Some(patch) = &mut call_evidence.patch_call
            {
                if let Err(failure) = patch.finish_arguments(raw_args.as_deref().unwrap_or("")) {
                    error = Some(ToolInputError::Arguments(failure));
                    break;
                }
                continue;
            }

            let args = raw_args.map(Cow::into_owned).unwrap_or_default();
            let mut call_id = new_call_id();
            if identity.apply_patch && !call_evidence.upstream_id.is_empty() {
                call_id = call_evidence.upstream_id.clone();
            }
            let mut item = if identity.custom {
                let mut input = unwrap_responses_custom_tool_input(&args);
                if identity.apply_patch {
                    let mut patch = CallState::default();
                    match patch.finish_arguments(&args) {
                        Ok((_, decoded)) => input = decoded,
                        Err(failure) => {
                            error = Some(ToolInputError::Arguments(failure));
                            break;
                        }
                    }
                    call_evidence.patch_call = Some(patch);
                }
                json!({
                    "id": format!("ctc_{call_id}"),
                    "type": "custom_tool_call",
                    "status": "completed",
                    "input": input,
                    "call_id": call_id,
                    "name": "",
                })
            } else {
                json!({
                    "id": format!("fc_{call_id}"),
                    "type": "function_call",
                    "status": "completed",
                    "arguments": args,
                    "call_id": call_id,
                    "name": "",
                })
            };
            set_tool_call_identity(&mut item, &identity.name, &identity.namespace);
            g.order.push(Output::Function(g.functions.len()));
            g.functions.push((item, signature));
            continue;
        }
        if !signature.is_empty() {
            if !g.reasoning_text.is_empty() {
                if g.reasoning_signature.is_empty() {
                    g.reasoning_signature = signature;
                    g.reasoning_direction = STANDALONE;
                    g.reasoning_target = TEXT;
                } else if g.reasoning_signature != signature {
                    g.flush_reasoning();
                    g.push_detached(signature, PREVIOUS, TEXT);
                }
            } else if !g.message_text.is_empty() {
                match g.message_signatures.last() {
                    None => g.message_signatures.push(signature),
                    Some(last) if *last != signature => {
                        g.flush_message();
                        g.rune_offset = 0;
                        g.push_detached(signature, PREVIOUS, TEXT);
                    }
                    Some(_) => {}
                }
            } else if !g.functions.is_empty() {
                g.push_detached(signature, PREVIOUS, FUNCTION);
            } else {
                g.push_detached(signature, NEXT, ANY);
            }
        }
    }

    if let Some(error) = error.or_else(|| evidence.pending_identity_error(&identities)) {
        return Err(error);
    }
    // The message still open at the end is the one a finish reason such as
    // `MAX_TOKENS` cut short.
    let active_message = (!gathered.message_text.is_empty()).then_some(gathered.messages.len());
    gathered.flush_reasoning();
    gathered.flush_message();

    // Web search, from the grounding metadata.
    let metadata = extract_grounding_metadata(root);
    let has_grounding = has_valid_web_grounding(metadata);
    let mut web_search = None;
    let mut citations = None;
    if has_grounding {
        let queries = grounding_queries(metadata);
        let mut query = queries.first().cloned().unwrap_or_default();
        if query.is_empty()
            && let Some(request) = picked
        {
            query = extract_web_search_query(unwrap_request_root(request));
        }
        let sources = grounding_sources(metadata);
        web_search = Some(build_web_search_call_item(
            &format!("ws_{rid}"),
            &query,
            &queries,
            &sources,
        ));
        let texts: Vec<String> = gathered
            .messages
            .iter()
            .map(|(text, _)| text.clone())
            .collect();
        citations = build_url_citations_for_messages(metadata, &gathered.part_mappings, &texts);
    }

    let mut outputs = Vec::new();
    let mut detached_index = 0;
    let mut seen_detached = HashSet::new();
    let mut push_detached =
        |outputs: &mut Vec<Value>, signature: &str, direction: &str, target: &str| {
            if signature.is_empty() || !seen_detached.insert(signature.to_owned()) {
                return;
            }
            let placement = if direction == PREVIOUS {
                "after"
            } else {
                "before"
            };
            outputs.push(json!({
                "id": format!("rs_{rid}_detached_{placement}_{detached_index}"),
                "type": "reasoning",
                "encrypted_content": encode(signature, direction, target),
                "summary": [],
            }));
            detached_index += 1;
        };
    let mut web_search_pending = web_search;
    for output in &gathered.order {
        match *output {
            Output::Detached(index) => {
                let (signature, direction, target) = &gathered.detached[index];
                if !gathered.reasoning_signatures.contains(signature) {
                    push_detached(&mut outputs, signature, direction, target);
                }
            }
            Output::Reasoning(index) => {
                let (text, signature, direction, target) = &gathered.reasoning[index];
                let id = if gathered.reasoning.len() > 1 {
                    format!("rs_{rid}_{index}")
                } else {
                    format!("rs_{rid}")
                };
                let encrypted_content = if !signature.is_empty() && !direction.is_empty() {
                    encode(signature, direction, target)
                } else {
                    signature.clone()
                };
                let mut item = json!({
                    "id": id,
                    "type": "reasoning",
                    "encrypted_content": encrypted_content,
                });
                if !text.is_empty() {
                    item["summary"] = json!([{"type": "summary_text", "text": text}]);
                }
                outputs.push(item);
            }
            Output::Message(index) => {
                if let Some(web_search) = web_search_pending.take() {
                    outputs.push(web_search);
                }
                let (text, signatures) = &gathered.messages[index];
                for signature in signatures {
                    if !gathered.reasoning_signatures.contains(signature) {
                        push_detached(&mut outputs, signature, NEXT, TEXT);
                    }
                }
                let annotations = citations
                    .as_ref()
                    .and_then(|citations| citations.get(&(index as i64)))
                    .map_or(&[][..], Vec::as_slice);
                let status = if active_message == Some(index) {
                    status
                } else {
                    COMPLETED
                };
                outputs.push(message_item(
                    &format!("msg_{rid}_{index}"),
                    status,
                    text,
                    annotations,
                ));
            }
            Output::Function(index) => {
                let (item, signature) = &gathered.functions[index];
                push_detached(&mut outputs, signature, NEXT, FUNCTION);
                outputs.push(item.clone());
            }
        }
    }
    if let Some(web_search) = web_search_pending {
        outputs.push(web_search);
    }
    if !outputs.is_empty() {
        response["output"] = Value::Array(outputs);
    }
    if has_grounding {
        response["tool_usage"] = json!({"web_search": {"num_requests": 1}});
    }
    let mut usage = Usage::default();
    if usage.merge(root) {
        response["usage"] = usage.to_json();
    }
    Ok(response)
}

/// Whether the request declares the custom `apply_patch` tool.
fn patch_enabled(identities: &HashMap<String, ToolIdentity>) -> bool {
    identities.values().any(|identity| identity.apply_patch)
}

/// `unwrapRequestRoot`: the request a wrapper holds in `request`, else the
/// request itself.
fn unwrap_request_root(root: &Value) -> &Value {
    match root.get("request") {
        Some(request)
            if ["model", "input", "instructions"]
                .iter()
                .any(|key| request.get(*key).is_some()) =>
        {
            request
        }
        _ => root,
    }
}

/// `unwrapGeminiResponseRoot`: the response a Vertex-style wrapper holds in
/// `response`, else the response itself, and whether it was wrapped.
fn unwrap_response_root(root: &Value) -> (&Value, bool) {
    match root.get("response") {
        Some(response)
            if [
                "candidates",
                "responseId",
                "usageMetadata",
                "cpaUsageMetadata",
            ]
            .iter()
            .any(|key| response.get(*key).is_some()) =>
        {
            (response, true)
        }
        _ => (root, false),
    }
}

/// An `output_text` content part.
fn output_text_part(text: &str, annotations: &[Value]) -> Value {
    json!({"type": "output_text", "annotations": annotations, "logprobs": [], "text": text})
}

/// A finished assistant message item with one `output_text` part.
fn message_item(id: &str, status: &str, text: &str, annotations: &[Value]) -> Value {
    json!({
        "id": id,
        "type": "message",
        "status": status,
        "content": [output_text_part(text, annotations)],
        "role": "assistant",
    })
}

/// `hasEffectiveGoogleSearchTool`: whether a Gemini request searches.
fn has_effective_google_search_tool(request: &Value) -> bool {
    if str_of(request.get("requestType")) == "web_search" {
        return true;
    }
    ["request.tools", "tools"].iter().any(|at| {
        matches!(path(request, at), Some(Value::Array(tools))
            if tools.iter().any(|tool| tool.get("googleSearch").is_some()))
    })
}

/// `isUpstreamGeminiRequest`: whether `request` is a Gemini request rather
/// than a Responses one.
fn is_upstream_gemini_request(request: &Value) -> bool {
    request.get("requestType").is_some()
        || ["contents", "request.contents"]
            .iter()
            .any(|at| path(request, at).is_some())
}

/// `determineWebSearchStreamMode`: whether text should wait for the search
/// to finish. `request_model` is the model the client named, or `model`.
fn web_search_stream_mode(
    model: &str,
    request_model: &str,
    original_request: &Value,
    request: &Value,
) -> bool {
    if !original_request.is_null()
        && !allows_web_search_tool_choice(unwrap_request_root(original_request))
    {
        return false;
    }
    if !request.is_null() {
        let root = unwrap_request_root(request);
        if root.get("tool_choice").is_some() && !allows_web_search_tool_choice(root) {
            return false;
        }
        if is_upstream_gemini_request(request) || has_effective_google_search_tool(request) {
            return has_effective_google_search_tool(request);
        }
    }
    pick_request(original_request, request).is_some_and(|picked| {
        let root = unwrap_request_root(picked);
        has_web_search_tool(root)
            && allows_web_search_tool_choice(root)
            && (model_supports_web_search(model) || model_supports_web_search(request_model))
    })
}

/// Process-wide counters for the IDs made up here, as upstream's
/// `responseIDCounter` and `funcCallIDCounter`.
static RESPONSE_IDS: AtomicU64 = AtomicU64::new(0);
static CALL_IDS: AtomicU64 = AtomicU64::new(0);

fn unix_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

/// The current Unix time in seconds.
fn now() -> i64 {
    unix_nanos() / 1_000_000_000
}

/// A response ID for a response Gemini didn't name.
fn new_response_id() -> String {
    let n = RESPONSE_IDS.fetch_add(1, Ordering::Relaxed) + 1;
    format!("resp_{:x}_{n}", unix_nanos())
}

/// A call ID for the stream, with the time in decimal as upstream writes it.
fn new_stream_call_id() -> String {
    let n = CALL_IDS.fetch_add(1, Ordering::Relaxed) + 1;
    format!("call_{}_{n}", unix_nanos())
}

/// A call ID for a whole response, with the time in hex as upstream writes
/// it.
fn new_call_id() -> String {
    let n = CALL_IDS.fetch_add(1, Ordering::Relaxed) + 1;
    format!("call_{:x}_{n}", unix_nanos())
}

/// Go's `time.Parse(time.RFC3339Nano, text)`, as Unix seconds: a date and
/// time such as `2024-01-02T15:04:05.123Z` or `2024-01-02T15:04:05+01:00`.
/// Like Go, it takes a one-digit hour, a comma before the fraction, any
/// number of fraction digits, and offsets up to 24 hours and 60 minutes.
fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let digit = |at: usize| bytes.get(at).is_some_and(u8::is_ascii_digit);
    let number = |from: usize, count: usize| -> Option<i64> {
        let digits = bytes.get(from..from + count)?;
        digits
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| digits.iter().fold(0, |n, d| n * 10 + i64::from(d - b'0')))
    };
    let literal = |at: usize, byte: u8| bytes.get(at) == Some(&byte);

    let year = number(0, 4)?;
    if !literal(4, b'-') {
        return None;
    }
    let month = number(5, 2)?;
    if !literal(7, b'-') {
        return None;
    }
    let day = number(8, 2)?;
    if !literal(10, b'T') {
        return None;
    }
    let mut i = if digit(12) { 13 } else { 12 };
    let hour = number(11, i - 11)?;
    if !literal(i, b':') {
        return None;
    }
    let minute = number(i + 1, 2)?;
    if !literal(i + 3, b':') {
        return None;
    }
    let second = number(i + 4, 2)?;
    i += 6;
    if matches!(bytes.get(i), Some(b'.' | b',')) && digit(i + 1) {
        i += 1;
        while digit(i) {
            i += 1;
        }
    }
    let offset = if literal(i, b'Z') {
        i += 1;
        0
    } else {
        if bytes.len() < i + 6 || !literal(i + 3, b':') {
            return None;
        }
        let hours = number(i + 1, 2)?;
        let minutes = number(i + 4, 2)?;
        if hours > 24 || minutes > 60 {
            return None;
        }
        let offset = (hours * 60 + minutes) * 60;
        let offset = match bytes[i] {
            b'+' => offset,
            b'-' => -offset,
            _ => return None,
        };
        i += 6;
        offset
    };
    if i != bytes.len() || !(1..=12).contains(&month) || hour >= 24 || minute >= 60 || second >= 60
    {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=days_in_month).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

/// Days from 1970-01-01 to a date in the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests;
