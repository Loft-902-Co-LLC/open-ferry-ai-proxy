// Ported from CLIProxyAPI internal/translator/gemini/openai/chat-completions/gemini_openai_response.go
// (ConvertGeminiResponseToOpenAI and ConvertGeminiResponseToOpenAINonStream)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini response → OpenAI Chat Completions response.
//!
//! A streamed Gemini chunk gives one Chat Completions chunk for each
//! candidate, or one holding only the usage if there are no candidates.
//! Thought text becomes `reasoning_content`, function calls become tool
//! calls, and inline data becomes `images`, as `data:` URLs. A candidate's
//! finish reason is sent once a chunk carries usage; a candidate that called
//! a tool finishes with `tool_calls`. Tool names are mapped back from their
//! sanitized form ([`restore_sanitized_tool_name`]) using the tools the client
//! declared, which upstream reads from `tools[].name`, so Chat Completions
//! tools, named in `tools[].function.name`, keep their sanitized names.
//!
//! Tool call IDs are the tool's name, the time in nanoseconds and a counter.
//!
//! Deviations from upstream:
//! - A chunk or response that isn't valid JSON is read as having no fields.
//! - A function call's `args` are written into `arguments` as compact JSON,
//!   where upstream copies Gemini's JSON text.
//! - A token count or candidate index too large for `i64`, such as `1e30`,
//!   saturates. Go's result depends on the CPU; amd64 gives the minimum
//!   `i64`.
//! - When a key is repeated, the last one counts; gjson reads the first.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::common::gemini::{
    SanitizedToolNames, restore_sanitized_tool_name, sanitized_tool_name_map,
};
use crate::go;
use crate::json::{bool_of, int_of, object, path, str_of};

/// Makes tool call IDs unique within the process, with the time.
static FUNCTION_CALL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Translates a Gemini response stream into Chat Completions chunks. Make one
/// per response.
pub struct GeminiToOpenAIStream {
    sanitized_names: Option<SanitizedToolNames>,
    /// The last `createTime` read, in Unix seconds; every chunk carries it.
    unix_timestamp: i64,
    /// For each candidate, how many tool calls it has made.
    function_index: HashMap<i64, i64>,
    /// The candidates that have made a tool call.
    saw_tool_call: HashSet<i64>,
    /// Each candidate's last finish reason, upper-cased.
    finish_reason: HashMap<i64, String>,
}

impl GeminiToOpenAIStream {
    /// A translator for the response to `original_request`, the client's
    /// request, whose tools name the functions.
    pub fn new(original_request: &Value) -> Self {
        Self {
            sanitized_names: sanitized_tool_name_map(original_request),
            unix_timestamp: 0,
            function_index: HashMap::new(),
            saw_tool_call: HashSet::new(),
            finish_reason: HashMap::new(),
        }
    }

    /// Translates one chunk, a Gemini response with or without its `data:`
    /// prefix, into the chunks to send. `[DONE]` gives none.
    pub fn translate(&mut self, chunk: &[u8]) -> Vec<Value> {
        let chunk = match chunk.strip_prefix(b"data:") {
            Some(data) => go::trim_space(data),
            None => chunk,
        };
        if chunk == b"[DONE]" {
            return Vec::new();
        }
        let response: Value = serde_json::from_slice(chunk).unwrap_or(Value::Null);

        let mut base = json!({
            "id": "",
            "object": "chat.completion.chunk",
            "created": 12345,
            "model": "model",
            "choices": [{
                "index": 0,
                "delta": {"role": null, "content": null, "reasoning_content": null, "tool_calls": null},
                "finish_reason": null,
                "native_finish_reason": null,
            }],
        });
        if let Some(model) = response.get("modelVersion") {
            base["model"] = Value::String(str_of(Some(model)).into_owned());
        }
        if let Some(created) = response.get("createTime")
            && let Some(created) = unix_seconds(&str_of(Some(created)))
        {
            self.unix_timestamp = created;
        }
        base["created"] = Value::from(self.unix_timestamp);
        if let Some(id) = response.get("responseId") {
            base["id"] = Value::String(str_of(Some(id)).into_owned());
        }
        let usage = response.get("usageMetadata");
        if let Some(usage) = usage {
            base["usage"] = usage_of(usage);
        }

        match response.get("candidates") {
            Some(Value::Array(candidates)) => candidates
                .iter()
                .map(|candidate| self.candidate_chunk(&base, candidate, usage.is_some()))
                .collect(),
            _ if usage.is_some() => vec![base],
            _ => Vec::new(),
        }
    }

    /// The chunk for one candidate: `base` with the candidate's index, parts
    /// and, once `has_usage`, its finish reason.
    fn candidate_chunk(&mut self, base: &Value, candidate: &Value, has_usage: bool) -> Value {
        let mut chunk = base.clone();
        let index = candidate.get("index").map_or(0, int_of);
        let choice = &mut chunk["choices"][0];
        choice["index"] = Value::from(index);
        if let Some(reason) = candidate.get("finishReason") {
            self.finish_reason
                .insert(index, go::to_upper(&str_of(Some(reason))));
        }

        let delta = &mut choice["delta"];
        if let Some(Value::Array(parts)) = path(candidate, "content.parts") {
            for part in parts {
                if let Some(text) = part_text(part) {
                    let field = if part.get("thought").is_some_and(bool_of) {
                        "reasoning_content"
                    } else {
                        "content"
                    };
                    delta["role"] = Value::from("assistant");
                    delta[field] = Value::String(str_of(Some(text)).into_owned());
                } else if let Some(function_call) = part.get("functionCall") {
                    self.saw_tool_call.insert(index);
                    let made = self.function_index.entry(index).or_default();
                    let mut call_index = *made;
                    *made += 1;
                    // A second call in the same chunk takes its place in the
                    // list instead.
                    match &mut delta["tool_calls"] {
                        Value::Array(calls) => call_index = calls.len() as i64,
                        calls => *calls = json!([]),
                    }
                    let name = restore_sanitized_tool_name(
                        self.sanitized_names.as_ref(),
                        &str_of(function_call.get("name")),
                    );
                    let mut call = object([
                        ("id", Value::String(function_call_id(&name))),
                        ("index", Value::from(call_index)),
                        ("type", Value::from("function")),
                        (
                            "function",
                            object([
                                ("name", Value::String(name)),
                                ("arguments", Value::from("")),
                            ]),
                        ),
                    ]);
                    if let Some(args) = function_call.get("args") {
                        call["function"]["arguments"] = Value::String(args.to_string());
                    }
                    delta["role"] = Value::from("assistant");
                    push(&mut delta["tool_calls"], call);
                } else if let Some(inline) = inline_data(part) {
                    let Some(url) = data_url(inline) else {
                        continue;
                    };
                    if !delta.get("images").is_some_and(Value::is_array) {
                        delta["images"] = json!([]);
                    }
                    let image = image_payload(url, delta["images"].as_array().map_or(0, Vec::len));
                    delta["role"] = Value::from("assistant");
                    push(&mut delta["images"], image);
                }
            }
        }

        let reason = self.finish_reason.get(&index).map_or("", String::as_str);
        if !reason.is_empty() && has_usage {
            let finish = if self.saw_tool_call.contains(&index) {
                "tool_calls"
            } else if reason == "MAX_TOKENS" {
                "max_tokens"
            } else {
                "stop"
            };
            choice["finish_reason"] = Value::from(finish);
            choice["native_finish_reason"] = Value::String(go::to_lower(reason));
        }
        chunk
    }
}

/// Converts a whole Gemini response into a Chat Completions response, one
/// choice per candidate. `original_request` is the client's request, whose
/// tools name the functions.
pub fn convert_gemini_response_to_openai_non_stream(
    original_request: &Value,
    response: &Value,
) -> Value {
    let names = sanitized_tool_name_map(original_request);
    let mut out = json!({
        "id": "",
        "object": "chat.completion",
        "created": 123456,
        "model": "model",
        "choices": [],
    });
    if let Some(model) = response.get("modelVersion") {
        out["model"] = Value::String(str_of(Some(model)).into_owned());
    }
    let created = response
        .get("createTime")
        .and_then(|created| unix_seconds(&str_of(Some(created))));
    out["created"] = Value::from(created.unwrap_or(0));
    if let Some(id) = response.get("responseId") {
        out["id"] = Value::String(str_of(Some(id)).into_owned());
    }
    if let Some(usage) = response.get("usageMetadata") {
        out["usage"] = usage_of(usage);
    }
    if let Some(Value::Array(candidates)) = response.get("candidates") {
        out["choices"] = candidates
            .iter()
            .map(|candidate| choice(candidate, names.as_ref()))
            .collect();
    }
    out
}

/// One candidate as a choice: its text and thought text each joined, its
/// tool calls and its images.
fn choice(candidate: &Value, names: Option<&SanitizedToolNames>) -> Value {
    let mut choice = json!({
        "index": 0,
        "message": {"role": "assistant", "content": null, "reasoning_content": null, "tool_calls": null},
        "finish_reason": null,
        "native_finish_reason": null,
    });
    choice["index"] = Value::from(candidate.get("index").map_or(0, int_of));
    if let Some(reason) = candidate.get("finishReason") {
        let reason = go::to_lower(&str_of(Some(reason)));
        choice["finish_reason"] = Value::String(reason.clone());
        choice["native_finish_reason"] = Value::String(reason);
    }

    let mut has_function_call = false;
    if let Some(Value::Array(parts)) = path(candidate, "content.parts") {
        let (mut text, mut reasoning) = (None::<String>, None::<String>);
        let mut tool_calls = Vec::new();
        let mut images = Vec::new();
        for part in parts {
            if let Some(part_text) = part_text(part) {
                let joined = if part.get("thought").is_some_and(bool_of) {
                    &mut reasoning
                } else {
                    &mut text
                };
                joined
                    .get_or_insert_default()
                    .push_str(&str_of(Some(part_text)));
            } else if let Some(function_call) = part.get("functionCall") {
                has_function_call = true;
                let name = restore_sanitized_tool_name(names, &str_of(function_call.get("name")));
                let mut call = object([
                    ("id", Value::String(function_call_id(&name))),
                    ("type", Value::from("function")),
                    (
                        "function",
                        object([
                            ("name", Value::String(name)),
                            ("arguments", Value::from("")),
                        ]),
                    ),
                ]);
                if let Some(args) = function_call.get("args") {
                    call["function"]["arguments"] = Value::String(args.to_string());
                }
                tool_calls.push(call);
            } else if let Some(inline) = inline_data(part)
                && let Some(url) = data_url(inline)
            {
                images.push(image_payload(url, images.len()));
            }
        }

        let message = &mut choice["message"];
        if let Some(text) = text {
            message["content"] = Value::String(text);
        }
        if let Some(reasoning) = reasoning {
            message["reasoning_content"] = Value::String(reasoning);
        }
        if !tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(tool_calls);
        }
        if !images.is_empty() {
            message["images"] = Value::Array(images);
        }
    }

    if has_function_call {
        choice["finish_reason"] = Value::from("tool_calls");
        choice["native_finish_reason"] = Value::from("tool_calls");
    }
    choice
}

/// A part's text, or the transcript a speech-to-text model sends in
/// `audioTranscription` instead.
fn part_text(part: &Value) -> Option<&Value> {
    part.get("text")
        .or_else(|| path(part, "audioTranscription.text"))
}

fn inline_data(part: &Value) -> Option<&Value> {
    part.get("inlineData").or_else(|| part.get("inline_data"))
}

/// Inline data as a `data:` URL, PNG unless it says otherwise. `None` if it
/// has no data.
fn data_url(inline: &Value) -> Option<String> {
    let data = str_of(inline.get("data"));
    if data.is_empty() {
        return None;
    }
    let mut mime_type = str_of(inline.get("mimeType"));
    if mime_type.is_empty() {
        mime_type = str_of(inline.get("mime_type"));
    }
    if mime_type.is_empty() {
        mime_type = "image/png".into();
    }
    Some(format!("data:{mime_type};base64,{data}"))
}

fn image_payload(url: String, index: usize) -> Value {
    object([
        ("type", Value::from("image_url")),
        ("image_url", object([("url", Value::String(url))])),
        ("index", Value::from(index)),
    ])
}

fn push(list: &mut Value, item: Value) {
    if let Value::Array(items) = list {
        items.push(item);
    }
}

/// The Chat Completions usage for Gemini's `usageMetadata`. Completion
/// tokens include thinking tokens.
fn usage_of(usage: &Value) -> Value {
    let count = |key: &str| usage.get(key).map_or(0, int_of);
    let thoughts = count("thoughtsTokenCount");
    let mut out = Map::new();
    out.insert(
        "completion_tokens".to_owned(),
        Value::from(count("candidatesTokenCount").wrapping_add(thoughts)),
    );
    if let Some(total) = usage.get("totalTokenCount") {
        out.insert("total_tokens".to_owned(), Value::from(int_of(total)));
    }
    out.insert(
        "prompt_tokens".to_owned(),
        Value::from(count("promptTokenCount")),
    );
    if thoughts > 0 {
        out.insert(
            "completion_tokens_details".to_owned(),
            json!({"reasoning_tokens": thoughts}),
        );
    }
    let cached = count("cachedContentTokenCount");
    if cached > 0 {
        out.insert(
            "prompt_tokens_details".to_owned(),
            json!({"cached_tokens": cached}),
        );
    }
    Value::Object(out)
}

/// A tool call ID: the tool's name, the time in nanoseconds and a counter.
fn function_call_id(name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_nanos()).unwrap_or(i64::MAX)
        });
    let count = FUNCTION_CALL_ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{name}-{nanos}-{count}")
}

/// Go's `time.Parse(time.RFC3339Nano, text)` as Unix seconds: the time in
/// the layout `2006-01-02T15:04:05.999999999Z07:00`, where the fraction is
/// optional and may follow a comma, the hour may be one digit, and an offset
/// may be up to 24 hours and 60 minutes.
fn unix_seconds(text: &str) -> Option<i64> {
    let mut cursor = Cursor(text.as_bytes());
    let year = cursor.year()?;
    cursor.literal(b'-')?;
    let month = cursor.number(true)?;
    cursor.literal(b'-')?;
    let day = cursor.number(true)?;
    cursor.literal(b'T')?;
    let hour = cursor.number(false)?;
    cursor.literal(b':')?;
    let minute = cursor.number(true)?;
    cursor.literal(b':')?;
    let second = cursor.number(true)?;
    cursor.fraction();
    let offset = cursor.zone_offset()?;
    let valid = cursor.0.is_empty()
        && (1..=12).contains(&month)
        && (1..=days_in_month(year, month)).contains(&day)
        && hour < 24
        && minute < 60
        && second < 60;
    valid.then(|| {
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset
    })
}

/// The text still to parse.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn digit(&self, at: usize) -> Option<i64> {
        self.0
            .get(at)
            .filter(|b| b.is_ascii_digit())
            .map(|b| i64::from(b - b'0'))
    }

    fn advance(&mut self, by: usize) {
        self.0 = self.0.get(by..).unwrap_or_default();
    }

    /// Four digits.
    fn year(&mut self) -> Option<i64> {
        let year = (0..4).try_fold(0, |year, at| Some(year * 10 + self.digit(at)?))?;
        self.advance(4);
        Some(year)
    }

    /// Go's `getnum`: two digits, or one when `fixed` is false and only one
    /// is there.
    fn number(&mut self, fixed: bool) -> Option<i64> {
        let first = self.digit(0)?;
        match self.digit(1) {
            Some(second) => {
                self.advance(2);
                Some(first * 10 + second)
            }
            None if !fixed => {
                self.advance(1);
                Some(first)
            }
            None => None,
        }
    }

    fn literal(&mut self, expected: u8) -> Option<()> {
        (self.0.first() == Some(&expected)).then(|| self.advance(1))
    }

    /// Skips fractional seconds: `.` or `,` and one or more digits.
    fn fraction(&mut self) {
        if matches!(self.0.first(), Some(b'.' | b',')) && self.digit(1).is_some() {
            let digits = self.0[1..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            self.advance(1 + digits);
        }
    }

    /// Go's `Z07:00`: `Z`, or a sign and two-digit hours and minutes, as
    /// seconds east of UTC.
    fn zone_offset(&mut self) -> Option<i64> {
        if self.literal(b'Z').is_some() {
            return Some(0);
        }
        let zone = self.0.get(..6)?;
        if zone[3] != b':' {
            return None;
        }
        let hours = Cursor(&zone[1..3]).number(true)?;
        let minutes = Cursor(&zone[4..6]).number(true)?;
        if hours > 24 || minutes > 60 {
            return None;
        }
        let offset = (hours * 60 + minutes) * 60;
        let offset = match zone[0] {
            b'+' => offset,
            b'-' => -offset,
            _ => return None,
        };
        self.advance(6);
        Some(offset)
    }
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
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
