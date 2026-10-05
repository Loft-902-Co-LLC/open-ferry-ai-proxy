// Ported from CLIProxyAPI internal/translator/gemini/interactions/interactions_gemini_common.go
// (ConvertGeminiResponseToInteractionsStream, convertGeminiResponseToInteractionsNonStreamDirect
// and the helpers they call) and interactions_gemini_response.go
// (ConvertGeminiResponseToInteractions, ConvertGeminiResponseToInteractionsNonStream,
// ConvertInteractionsResponseToGemini, ConvertInteractionsResponseToGeminiNonStream and the
// helpers they call) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini response → Gemini Interactions response, and Gemini Interactions
//! response → Gemini response.
//!
//! A Gemini stream becomes Interactions events: `interaction.created` and
//! `interaction.status_update` first, then a step for each run of text,
//! thought or signature and for each function call or result, and
//! `interaction.completed` once the candidate has finished and usage has
//! come, or at `[DONE]`.
//!
//! An Interactions stream becomes Gemini chunks: one for each text, thought,
//! signature or arguments delta, one with `finishReason: STOP` and the usage
//! at the end, and a Gemini error for a failed interaction.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use super::request::{each, gemini_part_to_steps, parts_of, text_part, thought_signature};
use crate::common::gemini::{
    set_gemini_function_response_raw, set_gemini_function_response_result,
};
use crate::common::gemini_response::create_time;
use crate::common::interactions_usage::interactions_usage;
use crate::common::sse::{push_event, push_frame};
use crate::gemini_schema;
use crate::go;
use crate::json::{bool_of, exact, int_of, object, path, set_path, str_of};

/// The current Unix time in nanoseconds, which upstream puts in the IDs it
/// makes up.
fn unix_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

/// gjson's `ParseBytes` on a chunk or body: the first JSON object or array
/// in it, after leading white space. Anything else, or JSON that doesn't
/// parse, reads as having no fields.
fn parse_root(bytes: &[u8]) -> Value {
    let start = bytes
        .iter()
        .position(|&byte| byte > b' ')
        .unwrap_or(bytes.len());
    let rest = bytes.get(start..).unwrap_or_default();
    if !matches!(rest.first(), Some(b'{' | b'[')) {
        return Value::Null;
    }
    exact::first(rest)
        .and_then(Result::ok)
        .unwrap_or(Value::Null)
}

/// `firstNonEmptyInteractionString`: the first value that isn't blank, as it
/// is.
fn first_non_blank<S: AsRef<str>>(values: impl IntoIterator<Item = S>) -> String {
    values
        .into_iter()
        .find(|value| !value.as_ref().trim().is_empty())
        .map(|value| value.as_ref().to_owned())
        .unwrap_or_default()
}

/// The Gemini usage of a response: `usageMetadata`, or else
/// `usage_metadata`.
fn gemini_usage(root: &Value) -> Option<&Value> {
    root.get("usageMetadata")
        .or_else(|| root.get("usage_metadata"))
}

/// `firstInteractionsGeminiUsage(...).Int()`: the first count `usage` has.
fn usage_count(usage: &Value, keys: [&str; 2]) -> Option<i64> {
    keys.iter().find_map(|key| usage.get(key)).map(int_of)
}

/// Streams a Gemini response as Gemini Interactions events
/// (`ConvertGeminiResponseToInteractions`, upstream's `StreamState`).
pub struct GeminiToInteractionsStream {
    model: String,
    id: String,
    step_id: String,
    started: bool,
    finished: bool,
    completed: bool,
    done: bool,
    step_open: bool,
    step_type: &'static str,
    step_index: i64,
    next_step_index: i64,
}

impl GeminiToInteractionsStream {
    /// A stream for `model`, under an interaction ID made from the time.
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            id: format!("interaction_{}", unix_nanos()),
            step_id: String::new(),
            started: false,
            finished: false,
            completed: false,
            done: false,
            step_open: false,
            step_type: "",
            step_index: 0,
            next_step_index: 0,
        }
    }

    /// The events for one Gemini chunk, each a whole SSE frame.
    pub fn translate(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        if go::trim_space(chunk) == b"[DONE]" {
            if !self.completed {
                self.stop_step(&mut out);
                self.complete(&mut out, None);
            }
            if !self.done {
                let mut frame = String::new();
                push_frame(&mut frame, "done", "[DONE]");
                out.push(frame);
                self.done = true;
            }
            return out;
        }
        let root = parse_root(chunk);
        if !self.started {
            let created = object([
                (
                    "interaction",
                    object([
                        ("id", Value::String(self.id.clone())),
                        ("status", Value::String("in_progress".to_owned())),
                        ("object", Value::String("interaction".to_owned())),
                        ("model", Value::String(self.model.clone())),
                    ]),
                ),
                (
                    "event_type",
                    Value::String("interaction.created".to_owned()),
                ),
            ]);
            push(&mut out, "interaction.created", &created);
            let status = object([
                ("interaction_id", Value::String(self.id.clone())),
                ("status", Value::String("in_progress".to_owned())),
                (
                    "event_type",
                    Value::String("interaction.status_update".to_owned()),
                ),
            ]);
            push(&mut out, "interaction.status_update", &status);
            self.started = true;
        }
        for part in each(gemini_schema::get(&root, "candidates.0.content.parts")) {
            self.push_part(&mut out, part);
        }
        let has_finish = gemini_schema::get(&root, "candidates.0.finishReason").is_some();
        if has_finish && !self.finished {
            self.stop_step(&mut out);
            self.finished = true;
        }
        if has_stream_usage(&root) && self.finished && !self.completed {
            self.complete(&mut out, Some(&root));
        }
        out
    }

    /// `appendGeminiPartToInteractionsStream`.
    fn push_part(&mut self, out: &mut Vec<String>, part: &Value) {
        let text = part.get("text").map(|text| str_of(Some(text)));
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            let delta = if part.get("thought").is_some_and(bool_of) {
                self.ensure_step(out, "thought", None);
                object([
                    (
                        "content",
                        object([
                            ("text", Value::String(text.into_owned())),
                            ("type", Value::String("text".to_owned())),
                        ]),
                    ),
                    ("type", Value::String("thought_summary".to_owned())),
                ])
            } else {
                self.ensure_step(out, "model_output", None);
                object([
                    ("text", Value::String(text.into_owned())),
                    ("type", Value::String("text".to_owned())),
                ])
            };
            self.push_delta(out, delta);
            self.push_signature(out, part);
            return;
        }
        if let Some(call) = part.get("functionCall") {
            self.push_signature(out, part);
            self.ensure_step(out, "function_call", Some(call));
            let arguments = call
                .get("args")
                .map_or_else(|| "{}".to_owned(), Value::to_string);
            self.push_delta(
                out,
                object([
                    ("arguments", Value::String(arguments)),
                    ("type", Value::String("arguments_delta".to_owned())),
                ]),
            );
            self.stop_step(out);
            return;
        }
        if let Some(response) = part.get("functionResponse") {
            self.ensure_step(out, "function_result", Some(response));
            let result = response
                .get("response")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            self.push_delta(
                out,
                object([
                    ("type", Value::String("function_result".to_owned())),
                    (
                        "name",
                        Value::String(str_of(response.get("name")).into_owned()),
                    ),
                    ("result", result),
                ]),
            );
            self.stop_step(out);
            return;
        }
        self.push_signature(out, part);
    }

    /// `appendInteractionsThoughtSignature`: a signature delta, in a thought
    /// step, if the part is signed.
    fn push_signature(&mut self, out: &mut Vec<String>, part: &Value) {
        let signature = thought_signature(part);
        if signature.is_empty() {
            return;
        }
        self.ensure_step(out, "thought", None);
        self.push_delta(
            out,
            object([
                ("signature", Value::String(signature)),
                ("type", Value::String("thought_signature".to_owned())),
            ]),
        );
    }

    /// A `step.delta` event in the open step.
    fn push_delta(&self, out: &mut Vec<String>, delta: Value) {
        let event = object([
            ("index", Value::from(self.step_index)),
            ("delta", delta),
            ("event_type", Value::String("step.delta".to_owned())),
        ]);
        push(out, "step.delta", &event);
    }

    /// `ensureInteractionsStep`: keeps an open step of `kind`, or closes the
    /// open step and starts one.
    fn ensure_step(&mut self, out: &mut Vec<String>, kind: &'static str, part: Option<&Value>) {
        if self.step_open && self.step_type == kind {
            return;
        }
        self.stop_step(out);
        self.step_id = format!("step_{}", unix_nanos());
        self.step_index = self.next_step_index;
        self.next_step_index += 1;
        self.step_type = kind;
        self.step_open = true;
        let mut step = object([("type", Value::String(kind.to_owned()))]);
        if kind == "function_call" {
            let part = part.unwrap_or(&Value::Null);
            let mut id = function_part_id(part);
            if id.is_empty() {
                id.clone_from(&self.step_id);
            }
            set_path(&mut step, "id", Value::String(id));
            set_path(
                &mut step,
                "name",
                Value::String(str_of(part.get("name")).into_owned()),
            );
            set_path(&mut step, "arguments", Value::Object(Map::new()));
        }
        let event = object([
            ("index", Value::from(self.step_index)),
            ("step", step),
            ("event_type", Value::String("step.start".to_owned())),
        ]);
        push(out, "step.start", &event);
    }

    /// `appendInteractionsStepStop`: closes the open step, if any.
    fn stop_step(&mut self, out: &mut Vec<String>) {
        if !self.step_open {
            return;
        }
        let event = object([
            ("index", Value::from(self.step_index)),
            ("event_type", Value::String("step.stop".to_owned())),
        ]);
        push(out, "step.stop", &event);
        self.step_open = false;
        self.step_type = "";
    }

    /// `appendInteractionsCompleted`, with the usage of `root` if given.
    fn complete(&mut self, out: &mut Vec<String>, root: Option<&Value>) {
        let now = Value::String(create_time(unix_nanos().div_euclid(1_000_000_000)));
        let mut usage = Value::Object(Map::new());
        if let Some(gemini) = root.and_then(gemini_usage) {
            stream_usage(&mut usage, gemini);
        }
        let completed = object([
            (
                "interaction",
                object([
                    ("id", Value::String(self.id.clone())),
                    ("status", Value::String("completed".to_owned())),
                    ("usage", usage),
                    ("created", now.clone()),
                    ("updated", now),
                    ("service_tier", Value::String("standard".to_owned())),
                    ("object", Value::String("interaction".to_owned())),
                    ("model", Value::String(self.model.clone())),
                ]),
            ),
            (
                "event_type",
                Value::String("interaction.completed".to_owned()),
            ),
        ]);
        push(out, "interaction.completed", &completed);
        self.completed = true;
    }
}

/// Appends an SSE frame holding `data`.
fn push(out: &mut Vec<String>, event: &str, data: &Value) {
    let mut frame = String::new();
    push_event(&mut frame, event, data);
    out.push(frame);
}

/// `interactionsFunctionPartID`: a function call's `id`, or else `call_id`.
fn function_part_id(part: &Value) -> String {
    part.get("id")
        .or_else(|| part.get("call_id"))
        .map(|id| str_of(Some(id)).into_owned())
        .unwrap_or_default()
}

/// The Gemini usage counts, under either spelling, that end a stream.
const STREAM_USAGE_COUNTS: [&str; 10] = [
    "promptTokenCount",
    "candidatesTokenCount",
    "totalTokenCount",
    "thoughtsTokenCount",
    "cachedContentTokenCount",
    "prompt_token_count",
    "candidates_token_count",
    "total_token_count",
    "thoughts_token_count",
    "cached_content_token_count",
];

/// `hasInteractionsGeminiStreamUsage`: whether the chunk's usage holds any
/// token count, rather than only, say, traffic details.
fn has_stream_usage(root: &Value) -> bool {
    gemini_usage(root).is_some_and(|usage| {
        STREAM_USAGE_COUNTS
            .iter()
            .any(|key| usage.get(key).is_some())
    })
}

/// `setInteractionsStreamUsageFromGemini`: the `interaction.completed`
/// event's usage.
fn stream_usage(out: &mut Value, usage: &Value) {
    let input = usage_count(usage, ["promptTokenCount", "prompt_token_count"]).unwrap_or(0);
    let output =
        usage_count(usage, ["candidatesTokenCount", "candidates_token_count"]).unwrap_or(0);
    let total = usage_count(usage, ["totalTokenCount", "total_token_count"]).unwrap_or(0);
    let thoughts = usage_count(usage, ["thoughtsTokenCount", "thoughts_token_count"]).unwrap_or(0);
    let mut cached = usage.get("cachedContentTokenCount").map_or(0, int_of);
    if cached == 0 {
        cached = usage.get("cached_content_token_count").map_or(0, int_of);
    }
    for (key, value) in [
        ("total_tokens", Value::from(total)),
        ("total_input_tokens", Value::from(input)),
        (
            "input_tokens_by_modality",
            Value::Array(vec![object([
                ("modality", Value::String("text".to_owned())),
                ("tokens", Value::from(input)),
            ])]),
        ),
        ("total_cached_tokens", Value::from(cached)),
        ("total_output_tokens", Value::from(output)),
        ("total_tool_use_tokens", Value::from(0)),
        ("total_thought_tokens", Value::from(thoughts)),
    ] {
        set_path(out, key, value);
    }
}

/// `ConvertGeminiResponseToInteractionsNonStream`: a whole Gemini response
/// as a Gemini Interactions response for `model`.
pub fn convert_gemini_response_to_interactions_non_stream(model: &str, body: &[u8]) -> Value {
    let root = parse_root(body);
    let mut id = str_of(root.get("responseId")).into_owned();
    if id.is_empty() {
        id = format!("interaction_{}", unix_nanos());
    }
    let mut out = object([
        ("id", Value::String(id)),
        ("object", Value::String("interaction".to_owned())),
        ("status", Value::String("completed".to_owned())),
        ("model", Value::String(model.to_owned())),
        ("steps", Value::Array(Vec::new())),
    ]);
    let steps: Vec<Value> = each(gemini_schema::get(&root, "candidates.0.content.parts"))
        .flat_map(gemini_part_to_steps)
        .collect();
    if !steps.is_empty() {
        set_path(&mut out, "steps", Value::Array(steps));
    }
    if let Some(usage) = gemini_usage(&root) {
        let mut counts = vec![
            (
                "input_tokens",
                usage_count(usage, ["promptTokenCount", "prompt_token_count"]).unwrap_or(0),
            ),
            (
                "output_tokens",
                usage_count(usage, ["candidatesTokenCount", "candidates_token_count"]).unwrap_or(0),
            ),
        ];
        if let Some(reasoning) = usage_count(usage, ["thoughtsTokenCount", "thoughts_token_count"])
        {
            counts.push(("reasoning_tokens", reasoning));
        }
        counts.push((
            "total_tokens",
            usage_count(usage, ["totalTokenCount", "total_token_count"]).unwrap_or(0),
        ));
        if let Some(cached) = usage_count(
            usage,
            ["cachedContentTokenCount", "cached_content_token_count"],
        ) {
            counts.push(("cached_tokens", cached));
        }
        for (key, count) in counts {
            set_path(&mut out, &format!("usage.{key}"), Value::from(count));
        }
    }
    out
}

/// What a Gemini chunk made from an Interactions response says about it.
#[derive(Default)]
struct Interaction {
    id: String,
    model: String,
    service_tier: String,
}

impl Interaction {
    /// `buildInteractionsGeminiChunk`: a Gemini chunk holding `parts` (an
    /// empty text part if there are none and `include_empty_part`).
    fn chunk(
        &self,
        model_name: &str,
        mut parts: Vec<Value>,
        finish_reason: &str,
        usage: Option<&Value>,
        include_empty_part: bool,
    ) -> Value {
        if parts.is_empty() && include_empty_part {
            parts.push(text_part("", false));
        }
        let mut candidate = object([
            (
                "content",
                object([
                    ("parts", Value::Array(parts)),
                    ("role", Value::String("model".to_owned())),
                ]),
            ),
            ("index", Value::from(0)),
        ]);
        if !finish_reason.is_empty() {
            set_path(
                &mut candidate,
                "finishReason",
                Value::String(finish_reason.to_owned()),
            );
        }
        let mut out = object([("candidates", Value::Array(vec![candidate]))]);
        let model = first_non_blank([self.model.as_str(), model_name]);
        if !model.is_empty() {
            set_path(&mut out, "modelVersion", Value::String(model));
        }
        if !self.id.is_empty() {
            set_path(&mut out, "responseId", Value::String(self.id.clone()));
        }
        if !self.service_tier.is_empty() {
            set_path(
                &mut out,
                "usageMetadata.serviceTier",
                Value::String(self.service_tier.clone()),
            );
        }
        if let Some(usage) = usage {
            usage_metadata(&mut out, usage);
        }
        out
    }

    /// Takes the ID, model and service tier of an `interaction` it hasn't
    /// got yet.
    fn update(&mut self, interaction: Option<&Value>, model_name: &str, service_tier: bool) {
        let field = |key: &str| str_of(interaction.and_then(|interaction| interaction.get(key)));
        self.id = first_non_blank([self.id.as_str(), &field("id")]);
        self.model = first_non_blank([self.model.as_str(), &field("model"), model_name]);
        if service_tier {
            self.service_tier =
                first_non_blank([self.service_tier.as_str(), &field("service_tier")]);
        }
    }
}

/// `setGeminiUsageMetadataFromInteractionsUsage`.
fn usage_metadata(out: &mut Value, usage: &Value) {
    let count = |keys: &[&str]| keys.iter().find_map(|key| usage.get(key)).map(int_of);
    let input = count(&["input_tokens", "total_input_tokens"]);
    let output = count(&["output_tokens", "total_output_tokens"]);
    if let Some(input) = input {
        set_path(out, "usageMetadata.promptTokenCount", Value::from(input));
        set_path(
            out,
            "usageMetadata.promptTokensDetails",
            Value::Array(vec![object([
                ("modality", Value::String("TEXT".to_owned())),
                ("tokenCount", Value::from(input)),
            ])]),
        );
    }
    if let Some(output) = output {
        set_path(
            out,
            "usageMetadata.candidatesTokenCount",
            Value::from(output),
        );
    }
    let total = count(&["total_tokens"]).or_else(|| {
        (input.is_some() || output.is_some())
            .then(|| input.unwrap_or(0).wrapping_add(output.unwrap_or(0)))
    });
    if let Some(total) = total {
        set_path(out, "usageMetadata.totalTokenCount", Value::from(total));
    }
    if let Some(thoughts) = count(&["reasoning_tokens", "total_thought_tokens"]) {
        set_path(
            out,
            "usageMetadata.thoughtsTokenCount",
            Value::from(thoughts),
        );
    }
    if let Some(cached) = count(&["cached_tokens", "total_cached_tokens"]) {
        set_path(
            out,
            "usageMetadata.cachedContentTokenCount",
            Value::from(cached),
        );
    }
}

/// Streams a Gemini Interactions response as Gemini chunks
/// (`ConvertInteractionsResponseToGemini`).
pub struct InteractionsToGeminiStream {
    model_name: String,
    interaction: Interaction,
    step_names: HashMap<i64, String>,
    step_ids: HashMap<i64, String>,
    step_signatures: HashMap<i64, String>,
}

impl InteractionsToGeminiStream {
    /// A stream for `model`.
    pub fn new(model: &str) -> Self {
        Self {
            model_name: model.to_owned(),
            interaction: Interaction {
                model: model.to_owned(),
                ..Interaction::default()
            },
            step_names: HashMap::new(),
            step_ids: HashMap::new(),
            step_signatures: HashMap::new(),
        }
    }

    /// The Gemini chunk for one Interactions event, if it makes one. The
    /// event may come as JSON or as SSE `data:` lines.
    pub fn translate(&mut self, chunk: &[u8]) -> Option<Value> {
        let root = parse_root(&sse_payload(chunk)?);
        let model_name = self.model_name.as_str();
        match str_of(root.get("event_type")).as_ref() {
            "interaction.created" => {
                self.interaction
                    .update(root.get("interaction"), model_name, false);
                None
            }
            "step.start" => {
                let index = root.get("index").map_or(0, int_of);
                let step = |key: &str| str_of(path(&root, &format!("step.{key}")));
                self.step_names.insert(index, step("name").into_owned());
                self.step_ids
                    .insert(index, first_non_blank([step("call_id"), step("id")]));
                self.step_signatures.insert(
                    index,
                    first_non_blank([
                        step("signature"),
                        step("thoughtSignature"),
                        step("thought_signature"),
                    ]),
                );
                None
            }
            "step.delta" => self.delta(&root),
            "interaction.completed" | "finish" => {
                self.interaction
                    .update(root.get("interaction"), model_name, true);
                Some(self.interaction.chunk(
                    model_name,
                    Vec::new(),
                    "STOP",
                    interactions_usage(&root),
                    true,
                ))
            }
            "response.failed" | "interaction.failed" => Some(error_chunk(&root)),
            _ => None,
        }
    }

    /// `interactionsStepDeltaToGeminiChunk`.
    fn delta(&mut self, root: &Value) -> Option<Value> {
        let index = root.get("index").map_or(0, int_of);
        let delta = root.get("delta").unwrap_or(&Value::Null);
        let field = |at: &str| str_of(path(delta, at));
        let part = match field("type").as_ref() {
            "arguments_delta" => {
                let name = first_non_blank([
                    self.step_names.get(&index).map_or("", String::as_str),
                    &str_of(path(root, "step.name")),
                ]);
                let mut call = object([
                    ("name", Value::String(name)),
                    ("args", Value::Object(Map::new())),
                ]);
                if let Some(id) = self.step_ids.get(&index).filter(|id| !id.is_empty()) {
                    set_path(&mut call, "id", Value::String(id.clone()));
                }
                let arguments = field("arguments");
                if let Some(arguments) = raw_json(arguments.trim()) {
                    set_path(&mut call, "args", arguments);
                }
                let mut part = object([("functionCall", call)]);
                if let Some(signature) = self
                    .step_signatures
                    .get(&index)
                    .filter(|signature| !signature.is_empty())
                {
                    set_path(
                        &mut part,
                        "thoughtSignature",
                        Value::String(signature.clone()),
                    );
                }
                part
            }
            "text" => {
                let text = first_non_blank([field("text"), field("content.text")]);
                if text.is_empty() {
                    return None;
                }
                text_part(&text, false)
            }
            "thought_summary" => {
                let text = first_non_blank([field("content.text"), field("text")]);
                if text.is_empty() {
                    return None;
                }
                text_part(&text, true)
            }
            "thought_signature" => {
                let signature = first_non_blank([
                    field("signature"),
                    field("thought_signature"),
                    field("thoughtSignature"),
                ]);
                if signature.is_empty() {
                    return None;
                }
                self.step_signatures.insert(index, signature.clone());
                let mut part = text_part("", true);
                set_path(&mut part, "thoughtSignature", Value::String(signature));
                part
            }
            _ => return None,
        };
        Some(
            self.interaction
                .chunk(&self.model_name, vec![part], "", None, false),
        )
    }
}

/// `interactionsGeminiSSEPayload`: the JSON of an event given as JSON, or as
/// SSE lines (the `data:` lines' payloads, joined by newlines). `None` for
/// an empty chunk or `[DONE]`.
fn sse_payload(chunk: &[u8]) -> Option<Vec<u8>> {
    let trimmed = go::trim_space(chunk);
    if trimmed.is_empty() || trimmed == b"[DONE]" {
        return None;
    }
    if trimmed.starts_with(b"{") {
        return Some(trimmed.to_vec());
    }
    let mut payload = Vec::new();
    for line in trimmed.split(|&byte| byte == b'\n') {
        let Some(data) = go::trim_space(line).strip_prefix(b"data:") else {
            continue;
        };
        let data = go::trim_space(data);
        if data.is_empty() || data == b"[DONE]" {
            continue;
        }
        if !payload.is_empty() {
            payload.push(b'\n');
        }
        payload.extend_from_slice(data);
    }
    (!payload.is_empty()).then_some(payload)
}

/// JSON text upstream copies as it is, if gjson finds it valid.
fn raw_json(text: &str) -> Option<Value> {
    if text.is_empty() || !go::gjson_valid(text.as_bytes()) {
        return None;
    }
    exact::from_str(text).ok()
}

/// A Gemini error for a failed interaction.
fn error_chunk(root: &Value) -> Value {
    let error = root
        .get("error")
        .or_else(|| path(root, "interaction.error"));
    let field = |key: &str| str_of(error.and_then(|error| error.get(key)));
    let mut message = field("message").into_owned();
    if message.is_empty() {
        message = "upstream error occurred".to_owned();
    }
    let code = first_non_blank([field("code"), str_of(root.get("code")), field("status")]);
    let (code, status) = map_interactions_error_to_gemini(&code);
    object([(
        "error",
        object([
            ("code", Value::from(code)),
            ("message", Value::String(message)),
            ("status", Value::String(status.to_owned())),
        ]),
    )])
}

/// `mapInteractionsErrorToGemini`: an Interactions error code or status as
/// an HTTP status and a Gemini status.
fn map_interactions_error_to_gemini(code: &str) -> (i64, &'static str) {
    let code = code.trim();
    match go::to_lower(code).as_str() {
        "400" | "invalid_argument" => (400, "INVALID_ARGUMENT"),
        "401" | "unauthenticated" => (401, "UNAUTHENTICATED"),
        "403" | "permission_denied" => (403, "PERMISSION_DENIED"),
        "404" | "not_found" => (404, "NOT_FOUND"),
        "429" | "resource_exhausted" | "rate_limit_exceeded" => (429, "RESOURCE_EXHAUSTED"),
        "499" | "canceled" | "cancelled" => (499, "CANCELLED"),
        "503" | "unavailable" => (503, "UNAVAILABLE"),
        "504" | "deadline_exceeded" => (504, "DEADLINE_EXCEEDED"),
        "500" | "internal" => (500, "INTERNAL"),
        _ => match code.parse::<i64>() {
            Ok(n) if (500..600).contains(&n) => (n, "INTERNAL"),
            Ok(n) if (400..500).contains(&n) => (n, "INVALID_ARGUMENT"),
            _ => (500, "INTERNAL"),
        },
    }
}

/// `ConvertInteractionsResponseToGeminiNonStream`: a whole Gemini
/// Interactions response as one Gemini response for `model`.
pub fn convert_interactions_response_to_gemini_non_stream(model: &str, body: &[u8]) -> Value {
    let root = parse_root(body);
    let interaction = root.get("interaction").unwrap_or(&root);
    let field = |key: &str| {
        [
            str_of(interaction.get(key)).into_owned(),
            str_of(root.get(key)).into_owned(),
        ]
    };
    let [interaction_id, root_id] = field("id");
    let [interaction_model, root_model] = field("model");
    let [interaction_tier, root_tier] = field("service_tier");
    let meta = Interaction {
        id: first_non_blank([
            interaction_id,
            root_id,
            format!("response_{}", unix_nanos()),
        ]),
        model: first_non_blank([interaction_model, root_model, model.to_owned()]),
        service_tier: first_non_blank([interaction_tier, root_tier]),
    };
    let steps = interaction.get("steps").or_else(|| root.get("steps"));
    let parts: Vec<Value> = each(steps).flat_map(step_parts).collect();
    meta.chunk(model, parts, "STOP", interactions_usage(&root), true)
}

/// `interactionsStepToGeminiParts`.
fn step_parts(step: &Value) -> Vec<Value> {
    match str_of(step.get("type")).as_ref() {
        "function_call" => vec![function_call_step_part(step)],
        "function_result" => vec![function_result_step_part(step)],
        "thought" => parts_of(step.get("content"), true),
        _ => parts_of(step.get("content"), false),
    }
}

/// The first of `step`'s fields that it has.
fn first_field<'v>(step: &'v Value, keys: [&str; 2]) -> Option<&'v Value> {
    keys.iter().find_map(|key| step.get(key))
}

/// `interactionsFunctionCallStepToGeminiPart`.
fn function_call_step_part(step: &Value) -> Value {
    let field = |key: &str| str_of(step.get(key));
    let mut call = object([
        ("name", Value::String(field("name").into_owned())),
        ("args", Value::Object(Map::new())),
    ]);
    let id = first_non_blank([field("call_id"), field("id")]);
    if !id.is_empty() {
        set_path(&mut call, "id", Value::String(id));
    }
    let mut part = object([("functionCall", call)]);
    let signature = first_non_blank([
        field("signature"),
        field("thoughtSignature"),
        field("thought_signature"),
    ]);
    if !signature.is_empty() {
        set_path(&mut part, "thoughtSignature", Value::String(signature));
    }
    // setInteractionsGeminiRawObject.
    let args = match first_field(step, ["arguments", "args"]) {
        None => Value::Object(Map::new()),
        Some(Value::String(text)) => {
            raw_json(text.trim()).unwrap_or_else(|| Value::String(text.clone()))
        }
        Some(args) => args.clone(),
    };
    set_path(&mut part, "functionCall.args", args);
    part
}

/// `interactionsFunctionResponseStepToGeminiPart`.
fn function_result_step_part(step: &Value) -> Value {
    let field = |key: &str| str_of(step.get(key));
    let mut response = object([
        ("name", Value::String(field("name").into_owned())),
        ("response", Value::Object(Map::new())),
    ]);
    let id = first_non_blank([field("call_id"), field("id")]);
    if !id.is_empty() {
        set_path(&mut response, "id", Value::String(id));
    }
    let mut part = object([("functionResponse", response)]);
    // setInteractionsGeminiFunctionResponse.
    let at = "functionResponse.response";
    match first_field(step, ["result", "response"]) {
        None => {}
        Some(Value::String(text))
            if !text.trim().is_empty() && go::gjson_valid(text.trim().as_bytes()) =>
        {
            set_gemini_function_response_raw(&mut part, at, text);
        }
        Some(result) => set_gemini_function_response_result(&mut part, at, Some(result.clone())),
    }
    part
}

#[cfg(test)]
mod tests;
