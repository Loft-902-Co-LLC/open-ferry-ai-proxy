//! Claude Code's output, one JSON object a line, read into what the client
//! gets: Claude's stream events, an error, and the account's rate-limit
//! windows as Anthropic's headers.
//!
//! - A `stream_event` of the main conversation (`parent_tool_use_id` null)
//!   carries one of Anthropic's stream events. Each is passed on, but
//!   `message_delta` and `message_stop`, which wait for the result, and an
//!   `error`, after which Claude Code retries or fails. A thinking block
//!   the client didn't ask for is left out, and the blocks after it
//!   renumbered, so the indexes stay contiguous. A second `message_start`
//!   means Claude Code began its answer again: the events so far are void,
//!   or, once some were sent, the call fails.
//! - An `assistant` message is kept, to build the answer from should no
//!   stream event come, and its `error` category noted.
//! - A `rate_limit_event` gives the account's windows, as Anthropic's
//!   `anthropic-ratelimit-unified-*` headers, which feed quota readings as
//!   the Claude provider's do.
//! - A `system` `api_retry` notes its category; for one retrying won't mend
//!   (a sign-in, billing, account, organization, model or rate-limit
//!   failure) Claude Code is stopped, as long as nothing was sent.
//! - The `result` ends the call: an error, as `api_error_status` or the
//!   category noted says, or the usage it gives, set on `message_delta`,
//!   and the held events.
//!
//! Everything else (`system` `init`, `status` and `thinking_tokens`, `user`
//! messages, lines that aren't JSON) is left alone.

use std::borrow::Cow;
use std::collections::HashMap;

use http::{HeaderMap, HeaderName, HeaderValue};
use open_ferry_core::exec::ExecError;
use open_ferry_core::observe::mask::mask_emails;
use serde_json::{Map, Value, json};

use crate::claude::ratelimit::{classify, error_headers};
use crate::json::str_at;

/// How much of a line that isn't JSON goes to the debug log.
const MAX_LOGGED_LINE: usize = 512;

/// How long a run of token characters must be to be masked.
const MIN_TOKEN_RUN: usize = 24;

/// The prefix of Anthropic's unified rate-limit headers.
const UNIFIED: &str = "anthropic-ratelimit-unified-";

/// The usage fields of a `message_delta`, and their names in `modelUsage`.
const USAGE_FIELDS: [(&str, &str); 4] = [
    ("input_tokens", "inputTokens"),
    ("output_tokens", "outputTokens"),
    ("cache_creation_input_tokens", "cacheCreationInputTokens"),
    ("cache_read_input_tokens", "cacheReadInputTokens"),
];

/// What a line comes to.
#[derive(Debug)]
pub(crate) enum Step {
    /// Nothing for the client.
    Nothing,
    /// Claude stream events for the client.
    Events(Vec<Value>),
    /// Claude Code began its answer again: the events so far are void, and
    /// these replace them.
    Restart(Vec<Value>),
    /// Claude Code is retrying a failure retrying won't mend: stop it, and
    /// fail with this.
    Stop(ExecError),
    /// The end: the last events, or the error.
    Done(Result<Vec<Value>, ExecError>),
}

/// The state of one run's output.
#[derive(Debug, Default)]
pub(crate) struct Events {
    /// Whether the client asked for thinking blocks.
    keep_thinking: bool,
    /// Whether events went to the client.
    sent: bool,
    /// Whether `message_start` came.
    started: bool,
    /// Each block's output index by Claude Code's, `None` for one left out.
    blocks: HashMap<u64, Option<u64>>,
    next_index: u64,
    /// The model `message_start` named.
    model: String,
    /// The `message_delta` and `message_stop` events, held for the result.
    held: Vec<Value>,
    /// The main conversation's `assistant` messages.
    assistants: Vec<Value>,
    /// The last error category Claude Code reported.
    category: String,
    /// The account's rate-limit windows, as headers.
    rate: HeaderMap,
}

impl Events {
    /// A run whose client asked for thinking blocks when `keep_thinking`.
    pub(crate) fn new(keep_thinking: bool) -> Self {
        Self {
            keep_thinking,
            ..Self::default()
        }
    }

    /// Notes that events went to the client: Claude Code may no longer
    /// begin again, nor be stopped early.
    pub(crate) fn mark_sent(&mut self) {
        self.sent = true;
    }

    /// The account's rate-limit windows, as Anthropic's headers, from the
    /// last `rate_limit_event`.
    pub(crate) fn rate_headers(&self) -> &HeaderMap {
        &self.rate
    }

    /// Reads one line of Claude Code's output.
    pub(crate) fn feed(&mut self, line: &[u8]) -> Step {
        let value: Value = match serde_json::from_slice(line) {
            Ok(value @ Value::Object(_)) => value,
            _ => {
                let text = String::from_utf8_lossy(line);
                if !text.trim().is_empty() {
                    tracing::debug!(
                        "claude-cli: a line that isn't a JSON object: {}",
                        mask(&text, MAX_LOGGED_LINE)
                    );
                }
                return Step::Nothing;
            }
        };
        let main = value.get("parent_tool_use_id").is_none_or(Value::is_null);
        match str_at(&value, "type").as_str() {
            "stream_event" if main => match value.get("event") {
                Some(event @ Value::Object(_)) => self.stream_event(event.clone()),
                _ => Step::Nothing,
            },
            "assistant" if main => {
                if let Some(category) = value.get("error").and_then(Value::as_str) {
                    self.category = category.to_owned();
                }
                if let Some(message @ Value::Object(_)) = value.get("message") {
                    self.assistants.push(message.clone());
                }
                Step::Nothing
            }
            "rate_limit_event" => {
                if let Some(info @ Value::Object(_)) = value.get("rate_limit_info") {
                    self.rate = rate_headers(info);
                }
                Step::Nothing
            }
            "system" if str_at(&value, "subtype") == "api_retry" => self.api_retry(&value),
            "result" => Step::Done(self.result(&value)),
            _ => Step::Nothing,
        }
    }

    fn stream_event(&mut self, mut event: Value) -> Step {
        let kind = str_at(&event, "type");
        if kind == "message_start" {
            let restart = self.started;
            if restart && self.sent {
                return Step::Done(Err(ExecError::upstream(
                    502,
                    "claude-cli: Claude Code began its answer again after part of it was sent",
                )));
            }
            self.started = true;
            self.blocks.clear();
            self.next_index = 0;
            self.held.clear();
            self.model = str_at(&event, "message.model");
            return if restart {
                tracing::debug!("claude-cli: Claude Code began its answer again");
                Step::Restart(vec![event])
            } else {
                Step::Events(vec![event])
            };
        }
        if !self.started {
            tracing::debug!("claude-cli: a {kind} event before message_start, left out");
            return Step::Nothing;
        }
        let index = event.get("index").and_then(Value::as_u64);
        match kind.as_str() {
            "content_block_start" => {
                let Some(index) = index else {
                    return Step::Events(vec![event]);
                };
                let block_type = str_at(&event, "content_block.type");
                if !self.keep_thinking
                    && matches!(block_type.as_str(), "thinking" | "redacted_thinking")
                {
                    self.blocks.insert(index, None);
                    return Step::Nothing;
                }
                let out = self.next_index;
                self.next_index += 1;
                self.blocks.insert(index, Some(out));
                event["index"] = Value::from(out);
                Step::Events(vec![event])
            }
            "content_block_delta" | "content_block_stop" => {
                match index.and_then(|index| self.blocks.get(&index).copied()) {
                    Some(None) => Step::Nothing,
                    Some(Some(out)) => {
                        event["index"] = Value::from(out);
                        Step::Events(vec![event])
                    }
                    None => Step::Events(vec![event]),
                }
            }
            "message_delta" | "message_stop" => {
                self.held.push(event);
                Step::Nothing
            }
            "error" => {
                tracing::debug!(
                    "claude-cli: an error event in Claude Code's stream: {}",
                    mask(&str_at(&event, "error.message"), MAX_LOGGED_LINE)
                );
                Step::Nothing
            }
            _ => Step::Events(vec![event]),
        }
    }

    fn api_retry(&mut self, value: &Value) -> Step {
        let category = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let error_status = status_of(value.get("error_status"));
        tracing::debug!(
            "claude-cli: Claude Code is retrying after {} (status {})",
            if category.is_empty() {
                "an error"
            } else {
                &category
            },
            error_status.map_or_else(|| "unknown".to_owned(), |status| status.to_string())
        );
        if !category.is_empty() {
            self.category.clone_from(&category);
        }
        if self.sent || !stops_early(&category) {
            return Step::Nothing;
        }
        let status = category_status(&category).or(error_status).unwrap_or(502);
        let message = format!(
            "claude-cli: Anthropic refused Claude Code's request ({category}); stopped it rather \
             than let it retry"
        );
        Step::Stop(cli_error(status, &message, &self.rate))
    }

    fn result(&mut self, value: &Value) -> Result<Vec<Value>, ExecError> {
        let subtype = str_at(value, "subtype");
        let is_error = value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || subtype.starts_with("error");
        // An API error Claude Code reports as its answer, with no stream.
        let failed = is_error || (!self.started && !self.category.is_empty());
        if failed {
            let status = status_of(value.get("api_error_status"))
                .or_else(|| category_status(&self.category))
                .unwrap_or(502);
            let message = format!(
                "claude-cli: Claude Code failed: {}",
                mask(&result_message(value, &subtype), 2048)
            );
            return Err(cli_error(status, &message, &self.rate));
        }
        let mut out = Vec::new();
        if !self.started {
            out = self.assistants_answer()?;
        }
        let usage = self.usage(value);
        let mut held = std::mem::take(&mut self.held);
        let stop = held
            .iter()
            .position(|event| str_at(event, "type") == "message_stop")
            .map(|at| held.remove(at));
        let mut delta = held
            .iter()
            .rposition(|event| str_at(event, "type") == "message_delta")
            .map(|at| held.remove(at))
            .unwrap_or_else(|| {
                json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": self.stop_reason(), "stop_sequence": null},
                    "usage": {},
                })
            });
        if !delta.get("usage").is_some_and(Value::is_object) {
            delta["usage"] = Value::Object(Map::new());
        }
        if let Some(target) = delta.get_mut("usage").and_then(Value::as_object_mut) {
            for (field, value) in usage {
                target.insert(field, value);
            }
        }
        out.extend(held);
        out.push(delta);
        out.push(stop.unwrap_or_else(|| json!({"type": "message_stop"})));
        Ok(out)
    }

    /// The usage of the answer: the result's `usage`, else the model's
    /// `modelUsage`. Claude Code's own calls to other models, which
    /// `modelUsage` also lists, are only logged.
    fn usage(&self, result: &Value) -> Vec<(String, Value)> {
        let totals = result.get("usage").filter(|usage| usage.is_object());
        let models = result.get("modelUsage").and_then(Value::as_object);
        let model = models.and_then(|models| models.get(&self.model));
        if let Some(models) = models {
            for (name, usage) in models.iter().filter(|(name, _)| **name != self.model) {
                tracing::debug!("claude-cli: Claude Code also used {name}: {usage}");
            }
        }
        USAGE_FIELDS
            .iter()
            .filter_map(|(field, camel)| {
                totals
                    .and_then(|usage| usage.get(*field))
                    .filter(|value| value.is_number())
                    .or_else(|| model.and_then(|usage| usage.get(*camel)))
                    .filter(|value| value.is_number())
                    .map(|value| ((*field).to_owned(), value.clone()))
            })
            .collect()
    }

    /// The stop reason of the last `assistant` message, else `end_turn`.
    fn stop_reason(&self) -> Value {
        self.assistants
            .iter()
            .rev()
            .filter_map(|message| message.get("stop_reason"))
            .find(|reason| reason.is_string())
            .cloned()
            .unwrap_or_else(|| Value::from("end_turn"))
    }

    /// The answer's events made from its `assistant` messages, for a run
    /// whose stream events didn't come: `message_start`, then each block
    /// as a start, a delta and a stop.
    fn assistants_answer(&mut self) -> Result<Vec<Value>, ExecError> {
        let Some(first) = self.assistants.first() else {
            return Err(ExecError::upstream(
                502,
                "claude-cli: Claude Code answered without a message",
            ));
        };
        let id = str_at(first, "id");
        let mut message = first.clone();
        message["content"] = Value::Array(Vec::new());
        message["stop_reason"] = Value::Null;
        message["stop_sequence"] = Value::Null;
        self.model = str_at(&message, "model");
        let blocks: Vec<Value> = self
            .assistants
            .iter()
            .filter(|message| str_at(message, "id") == id)
            .filter_map(|message| message.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|block| {
                self.keep_thinking
                    || !matches!(
                        str_at(block, "type").as_str(),
                        "thinking" | "redacted_thinking"
                    )
            })
            .cloned()
            .collect();
        self.started = true;
        let mut out = vec![json!({"type": "message_start", "message": message})];
        for (index, block) in blocks.into_iter().enumerate() {
            out.extend(block_events(index, block));
        }
        Ok(out)
    }
}

/// A whole block as the events that build it.
fn block_events(index: usize, block: Value) -> Vec<Value> {
    let start = |content_block: Value| json!({"type": "content_block_start", "index": index, "content_block": content_block});
    let delta =
        |delta: Value| json!({"type": "content_block_delta", "index": index, "delta": delta});
    let mut out = Vec::new();
    match str_at(&block, "type").as_str() {
        "text" => {
            let mut empty = block.clone();
            empty["text"] = Value::from("");
            out.push(start(empty));
            out.push(delta(
                json!({"type": "text_delta", "text": str_at(&block, "text")}),
            ));
        }
        "thinking" => {
            out.push(start(
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ));
            out.push(delta(
                json!({"type": "thinking_delta", "thinking": str_at(&block, "thinking")}),
            ));
            let signature = str_at(&block, "signature");
            if !signature.is_empty() {
                out.push(delta(
                    json!({"type": "signature_delta", "signature": signature}),
                ));
            }
        }
        _ => out.push(start(block)),
    }
    out.push(json!({"type": "content_block_stop", "index": index}));
    out
}

/// What a failed result says: its `result`, else its `errors`, else its
/// subtype.
fn result_message(value: &Value, subtype: &str) -> String {
    let result = str_at(value, "result");
    if !result.trim().is_empty() {
        return result.trim().to_owned();
    }
    let errors: Vec<String> = value
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|error| match error {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect();
    if !errors.is_empty() {
        return errors.join("; ");
    }
    if subtype.is_empty() {
        "an error without a message".to_owned()
    } else {
        subtype.to_owned()
    }
}

/// An HTTP error status, from a number or a numeric string.
fn status_of(value: Option<&Value>) -> Option<u16> {
    let status = match value? {
        Value::Number(number) => number.as_u64()?,
        Value::String(text) => text.trim().parse().ok()?,
        _ => return None,
    };
    u16::try_from(status)
        .ok()
        .filter(|status| (400..600).contains(status))
}

/// The status of one of Claude Code's error categories.
pub(crate) fn category_status(category: &str) -> Option<u16> {
    Some(match category {
        "authentication_failed" => 401,
        "oauth_org_not_allowed" | "account_on_hold" => 403,
        "billing_error" => 402,
        "rate_limit" => 429,
        "overloaded" => 529,
        "invalid_request" => 400,
        "model_not_found" => 404,
        "server_error" | "unknown" => 502,
        _ => return None,
    })
}

/// Whether an `api_retry` of `category` stops Claude Code: retrying it
/// won't help, and another credential may.
fn stops_early(category: &str) -> bool {
    matches!(
        category,
        "authentication_failed"
            | "oauth_org_not_allowed"
            | "account_on_hold"
            | "billing_error"
            | "rate_limit"
            | "model_not_found"
    )
}

/// The `type` of Anthropic's error body for `status`.
fn error_type(status: u16) -> &'static str {
    match status {
        400 | 413 => "invalid_request_error",
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

/// A failure as an error with `status` and Anthropic's error body, carrying
/// the account's rate-limit headers; a 429 is scoped and timed by them as
/// the Claude provider's is.
pub(crate) fn cli_error(status: u16, message: &str, rate: &HeaderMap) -> ExecError {
    let body = json!({
        "type": "error",
        "error": {"type": error_type(status), "message": message},
    })
    .to_string();
    if status == 429 {
        return classify(status, rate, body.as_bytes(), false);
    }
    let mut error = ExecError::upstream(status, body);
    error.headers = error_headers(rate);
    error
}

/// A `rate_limit_event`'s `rate_limit_info` as Anthropic's unified
/// rate-limit headers: `status`, `resetsAt`, `rateLimitType` (the
/// representative claim) and the overage fields, and each of
/// `unifiedWindows` (`five_hour` as `5h`, `seven_day` as `7d`, any other
/// by its own name) with its utilization, reset and status. The window
/// `rateLimitType` names takes the top-level status, utilization and reset
/// where it has none of its own. Numbers keep their JSON text.
pub(crate) fn rate_headers(info: &Value) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (field, header) in [
        ("status", "status"),
        ("resetsAt", "reset"),
        ("rateLimitType", "representative-claim"),
        ("overageStatus", "overage-status"),
        ("overageDisabledReason", "overage-disabled-reason"),
        ("overageResetsAt", "overage-reset"),
    ] {
        put(&mut headers, header, info.get(field));
    }
    if let Some(windows) = info.get("unifiedWindows").and_then(Value::as_object) {
        for (name, window) in windows {
            let Some(short) = window_name(name) else {
                continue;
            };
            for (field, header) in [
                ("utilization", "utilization"),
                ("resetsAt", "reset"),
                ("status", "status"),
            ] {
                put(
                    &mut headers,
                    &format!("{short}-{header}"),
                    window.get(field),
                );
            }
        }
    }
    if let Some(short) = info
        .get("rateLimitType")
        .and_then(Value::as_str)
        .and_then(window_name)
    {
        for (field, header) in [
            ("status", "status"),
            ("utilization", "utilization"),
            ("resetsAt", "reset"),
        ] {
            let name = format!("{UNIFIED}{short}-{header}");
            if !headers.contains_key(name.as_str()) {
                put(&mut headers, &format!("{short}-{header}"), info.get(field));
            }
        }
    }
    headers
}

/// A window's name in a header: `5h`, `7d`, or its own when it is safe.
fn window_name(name: &str) -> Option<Cow<'_, str>> {
    match name {
        "five_hour" => Some(Cow::Borrowed("5h")),
        "seven_day" => Some(Cow::Borrowed("7d")),
        _ if !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') =>
        {
            Some(Cow::Borrowed(name))
        }
        _ => None,
    }
}

/// Sets `anthropic-ratelimit-unified-<suffix>` to `value`'s text: a string
/// as it is, a number or a boolean as JSON writes it.
fn put(headers: &mut HeaderMap, suffix: &str, value: Option<&Value>) {
    let text = match value {
        Some(Value::String(text)) if !text.trim().is_empty() => text.trim().to_owned(),
        Some(value @ (Value::Number(_) | Value::Bool(_))) => value.to_string(),
        _ => return,
    };
    let (Ok(name), Ok(value)) = (
        HeaderName::try_from(format!("{UNIFIED}{suffix}")),
        HeaderValue::try_from(text),
    ) else {
        return;
    };
    headers.insert(name, value);
}

/// `text` for a log: emails and token-like runs masked, and cut to `max`
/// bytes.
pub(crate) fn mask(text: &str, max: usize) -> String {
    let masked = mask_tokens(&mask_emails(text));
    if masked.len() <= max {
        return masked;
    }
    let mut end = max;
    while !masked.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &masked[..end])
}

/// `text` with each run of token characters long enough to be a key or a
/// token, and holding both letters and digits, made `***`.
fn mask_tokens(text: &str) -> String {
    let is_token_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        let letters = run.chars().any(|c| c.is_ascii_alphabetic());
        let digits = run.chars().any(|c| c.is_ascii_digit());
        if run.len() >= MIN_TOKEN_RUN && letters && digits {
            out.push_str("***");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if is_token_char(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(value: Value) -> Vec<u8> {
        value.to_string().into_bytes()
    }

    fn stream(event: Value) -> Vec<u8> {
        line(json!({"type": "stream_event", "event": event, "parent_tool_use_id": null}))
    }

    fn events(step: Step) -> Vec<Value> {
        match step {
            Step::Events(events) | Step::Restart(events) | Step::Done(Ok(events)) => events,
            other => panic!("not events: {other:?}"),
        }
    }

    fn message_start() -> Value {
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-haiku-4-5", "content": [], "usage": {"input_tokens": 3, "output_tokens": 1}}})
    }

    fn success() -> Value {
        json!({
            "type": "result", "subtype": "success", "is_error": false, "result": "Hi",
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 590, "cache_read_input_tokens": 7, "service_tier": "standard"},
            "modelUsage": {"claude-haiku-4-5": {"inputTokens": 99, "outputTokens": 98, "cacheReadInputTokens": 1, "cacheCreationInputTokens": 2}},
        })
    }

    #[test]
    fn passes_the_stream_on_and_sets_the_usage() {
        let mut state = Events::new(false);
        assert!(matches!(
            state.feed(&line(
                json!({"type": "system", "subtype": "init", "model": "x"})
            )),
            Step::Nothing
        ));
        assert!(matches!(state.feed(b"not json"), Step::Nothing));
        assert_eq!(
            events(state.feed(&stream(message_start()))),
            [message_start()]
        );
        // A sub-agent's events aren't the answer's.
        assert!(matches!(
            state.feed(&line(json!({"type": "stream_event", "event": {"type": "ping"}, "parent_tool_use_id": "toolu_1"}))),
            Step::Nothing
        ));
        // Thinking not asked for is left out, and the text renumbered.
        for event in [
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
            json!({"type": "content_block_stop", "index": 0}),
        ] {
            assert!(matches!(state.feed(&stream(event)), Step::Nothing));
        }
        assert_eq!(
            events(state.feed(&stream(json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}})))),
            [json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})]
        );
        assert_eq!(
            events(state.feed(&stream(json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hi"}})))),
            [json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}})]
        );
        assert_eq!(
            events(state.feed(&stream(json!({"type": "content_block_stop", "index": 1})))),
            [json!({"type": "content_block_stop", "index": 0})]
        );
        // The end waits for the result.
        assert!(matches!(
            state.feed(&stream(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 5, "server_tool_use": {"web_search_requests": 0}}}))),
            Step::Nothing
        ));
        assert!(matches!(
            state.feed(&stream(json!({"type": "message_stop"}))),
            Step::Nothing
        ));
        assert!(matches!(
            state.feed(&line(json!({"type": "assistant", "message": {"id": "msg_1", "content": []}, "parent_tool_use_id": null}))),
            Step::Nothing
        ));
        assert_eq!(
            events(state.feed(&line(success()))),
            [
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 5, "server_tool_use": {"web_search_requests": 0}, "input_tokens": 10, "cache_creation_input_tokens": 590, "cache_read_input_tokens": 7}}),
                json!({"type": "message_stop"}),
            ]
        );
    }

    #[test]
    fn keeps_thinking_asked_for() {
        let mut state = Events::new(true);
        events(state.feed(&stream(message_start())));
        let start = json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}});
        assert_eq!(events(state.feed(&stream(start.clone()))), [start]);
    }

    #[test]
    fn takes_the_model_usage_when_the_result_has_none() {
        let mut state = Events::new(false);
        events(state.feed(&stream(message_start())));
        let mut result = success();
        result.as_object_mut().unwrap().remove("usage");
        let out = events(state.feed(&line(result)));
        assert_eq!(
            out[0]["usage"],
            json!({"input_tokens": 99, "output_tokens": 98, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 1})
        );
        assert_eq!(out[0]["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn begins_again_until_something_is_sent() {
        let mut state = Events::new(false);
        events(state.feed(&stream(message_start())));
        events(state.feed(&stream(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}))));
        assert!(matches!(
            state.feed(&stream(message_start())),
            Step::Restart(events) if events == [message_start()]
        ));
        state.mark_sent();
        let Step::Done(Err(error)) = state.feed(&stream(message_start())) else {
            panic!("a second start after sending must fail");
        };
        assert_eq!(error.status, 502);
    }

    #[test]
    fn builds_the_answer_from_assistant_messages() {
        let mut state = Events::new(false);
        for message in [
            json!({"id": "msg_9", "model": "claude-haiku-4-5", "role": "assistant", "type": "message", "content": [{"type": "thinking", "thinking": "t", "signature": "s"}], "usage": {"input_tokens": 1}}),
            json!({"id": "msg_9", "model": "claude-haiku-4-5", "content": [{"type": "text", "text": "Hello"}], "stop_reason": "end_turn"}),
        ] {
            state.feed(&line(
                json!({"type": "assistant", "message": message, "parent_tool_use_id": null}),
            ));
        }
        let out = events(state.feed(&line(success())));
        let kinds: Vec<String> = out.iter().map(|event| str_at(event, "type")).collect();
        assert_eq!(
            kinds,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(out[0]["message"]["id"], "msg_9");
        assert_eq!(out[0]["message"]["content"], json!([]));
        assert_eq!(out[2]["delta"]["text"], "Hello");
        assert_eq!(out[1]["index"], 0);
        assert_eq!(out[4]["usage"]["input_tokens"], 10);

        let mut empty = Events::new(false);
        let Step::Done(Err(error)) = empty.feed(&line(success())) else {
            panic!("no message must fail");
        };
        assert_eq!(error.status, 502);
    }

    #[test]
    fn maps_each_error() {
        for (category, status, kind) in [
            ("authentication_failed", 401, "authentication_error"),
            ("oauth_org_not_allowed", 403, "permission_error"),
            ("account_on_hold", 403, "permission_error"),
            ("billing_error", 402, "billing_error"),
            ("rate_limit", 429, "rate_limit_error"),
            ("overloaded", 529, "overloaded_error"),
            ("invalid_request", 400, "invalid_request_error"),
            ("model_not_found", 404, "not_found_error"),
            ("server_error", 502, "api_error"),
            ("unknown", 502, "api_error"),
        ] {
            // An assistant error, then a failed result without a status.
            let mut state = Events::new(false);
            state.feed(&line(json!({"type": "assistant", "error": category, "message": {"id": "m", "content": [{"type": "text", "text": "API Error"}]}, "parent_tool_use_id": null})));
            let Step::Done(Err(error)) = state.feed(&line(
                json!({"type": "result", "subtype": "success", "is_error": true, "result": "API Error: user@example.com sk-ant-oat01-abcdefghijklmnop0123456789"}),
            )) else {
                panic!("{category} must fail");
            };
            assert_eq!(error.status, status, "{category}");
            let body: Value = serde_json::from_str(&error.message).unwrap();
            assert_eq!(body["error"]["type"], kind, "{category}");
            let message = body["error"]["message"].as_str().unwrap();
            assert!(!message.contains("user@example.com"), "{message}");
            assert!(!message.contains("abcdefghijklmnop0123456789"), "{message}");
            assert!(message.starts_with("claude-cli: Claude Code failed: API Error:"));

            // An api_retry stops early only for what retrying won't mend.
            let mut state = Events::new(false);
            let step = state.feed(&line(json!({"type": "system", "subtype": "api_retry", "attempt": 1, "error_status": 500, "error": category})));
            let stops = matches!(
                category,
                "authentication_failed"
                    | "oauth_org_not_allowed"
                    | "account_on_hold"
                    | "billing_error"
                    | "rate_limit"
                    | "model_not_found"
            );
            match step {
                Step::Stop(error) => {
                    assert!(stops, "{category}");
                    assert_eq!(error.status, status, "{category}");
                }
                Step::Nothing => assert!(!stops, "{category}"),
                other => panic!("{other:?}"),
            }
            // Once something is sent, Claude Code retries.
            let mut sent = Events::new(false);
            sent.mark_sent();
            assert!(matches!(
                sent.feed(&line(
                    json!({"type": "system", "subtype": "api_retry", "error": category})
                )),
                Step::Nothing
            ));
        }

        // A result's own status wins, and a failed subtype fails.
        let mut state = Events::new(false);
        let Step::Done(Err(error)) = state.feed(&line(json!({"type": "result", "subtype": "error_during_execution", "is_error": false, "api_error_status": "503", "errors": ["boom", "bang"]}))) else {
            panic!();
        };
        assert_eq!(error.status, 503);
        assert!(error.message.contains("boom; bang"), "{}", error.message);
        let mut state = Events::new(false);
        let Step::Done(Err(error)) = state.feed(&line(
            json!({"type": "result", "subtype": "error_max_turns"}),
        )) else {
            panic!();
        };
        assert_eq!(error.status, 502);
        assert!(
            error.message.contains("error_max_turns"),
            "{}",
            error.message
        );
    }

    #[test]
    fn reads_rate_limit_windows_as_headers() {
        let mut state = Events::new(false);
        state.feed(&line(json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "rejected",
                "resetsAt": 1_900_000_000,
                "rateLimitType": "five_hour",
                "overageStatus": "rejected",
                "overageDisabledReason": "org_level_disabled",
                "isUsingOverage": false,
                "utilization": 1.0,
                "unifiedWindows": {
                    "five_hour": {"resetsAt": 1_900_000_000},
                    "seven_day": {"utilization": 0.25, "resetsAt": 1_900_500_000},
                    "seven_day_opus": {"utilization": 0.5},
                    "Bad Name": {"utilization": 0.5},
                },
                "somethingNew": {"x": 1},
            },
            "session_id": "s",
            "uuid": "u",
        })));
        let headers = state.rate_headers();
        let get = |name: &str| {
            headers
                .get(name)
                .map(|value| value.to_str().unwrap().to_owned())
        };
        assert_eq!(
            get("anthropic-ratelimit-unified-status").as_deref(),
            Some("rejected")
        );
        assert_eq!(
            get("anthropic-ratelimit-unified-reset").as_deref(),
            Some("1900000000")
        );
        assert_eq!(
            get("anthropic-ratelimit-unified-representative-claim").as_deref(),
            Some("five_hour")
        );
        assert_eq!(
            get("anthropic-ratelimit-unified-overage-status").as_deref(),
            Some("rejected")
        );
        assert_eq!(
            get("anthropic-ratelimit-unified-overage-disabled-reason").as_deref(),
            Some("org_level_disabled")
        );
        assert_eq!(
            get("anthropic-ratelimit-unified-5h-status").as_deref(),
            Some("rejected")
        );
        assert_eq!(
            get("anthropic-ratelimit-unified-5h-utilization").as_deref(),
            Some("1.0")
        );
        assert_eq!(
            get("anthropic-ratelimit-unified-5h-reset").as_deref(),
            Some("1900000000")
        );
        assert_eq!(
            get("anthropic-ratelimit-unified-7d-utilization").as_deref(),
            Some("0.25")
        );
        assert_eq!(
            get("anthropic-ratelimit-unified-7d-reset").as_deref(),
            Some("1900500000")
        );
        assert_eq!(get("anthropic-ratelimit-unified-7d-status"), None);
        assert_eq!(
            get("anthropic-ratelimit-unified-seven_day_opus-utilization").as_deref(),
            Some("0.5")
        );
        assert_eq!(headers.len(), 11);

        // A rejected five-hour window is the account's, and resets then.
        let error = cli_error(429, "limited", headers);
        assert!(error.credential_scoped);
        assert!(error.retry_after.is_some());
        assert_eq!(error.headers.len(), 11);

        // An event replaces the last.
        state.feed(&line(
            json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed"}}),
        ));
        assert_eq!(state.rate_headers().len(), 1);
    }

    #[test]
    fn masks_what_it_logs() {
        assert_eq!(
            mask(
                "token sk-ant-oat01-AbCdEfGhIjKlMnOpQrStUv012345 for a@example.com",
                200
            ),
            "token *** for a***@e***.com"
        );
        assert_eq!(
            mask("a short-word and 2.1.291", 200),
            "a short-word and 2.1.291"
        );
        assert_eq!(mask("ééé", 3), "é…");
    }
}
