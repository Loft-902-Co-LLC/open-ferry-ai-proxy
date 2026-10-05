// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_handlers.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Legacy OpenAI Completions (`/v1/completions`) over Chat Completions.
//!
//! Upstream serves the legacy endpoint by turning its request into a Chat
//! Completions request whose one user message is the prompt, then turning the
//! Chat Completions response, or each chunk of a streamed one, back into a
//! `text_completion`. These are those three conversions.
//!
//! Deviations from upstream:
//! - A response or chunk that isn't valid JSON is read as having no fields, so
//!   such a chunk is skipped. gjson reads what it can from malformed JSON,
//!   such as an object with more text after it.
//! - A non-string prompt, model, ID, text or finish reason is written as
//!   compact JSON, where upstream uses its JSON text.
//! - A float parameter (`temperature`, `top_p`, `frequency_penalty` or
//!   `presence_penalty`) that isn't a finite number, such as `1e400` or the
//!   string `"NaN"`, is left out. Upstream writes it as `+Inf` or `NaN`,
//!   which isn't JSON.
//! - A number too large for `f64` in a choice's `logprobs`, such as `1e400`,
//!   is kept as written. Go can't marshal it, so upstream writes `choices`
//!   with no value, which isn't JSON.
//! - A number too large for `i64` in `max_tokens`, `top_logprobs`, `created`
//!   or a choice's `index`, such as `1e30`, saturates. Go's result depends on
//!   the CPU; amd64 gives the minimum `i64`.
//! - When a key is repeated, the last one counts; gjson reads the first.
//! - `stop` and `usage` are copied as JSON values, so their spacing isn't
//!   kept. Strings are written with serde_json's escaping, where Go's encoder
//!   also escapes `<`, `>` and `&` in the choices.

use serde_json::{Map, Value};

use crate::json::{bool_of, float_of, go_marshaled, int_of, object, path, str_of};

/// What upstream asks for when the request has no prompt, or an empty one.
const DEFAULT_PROMPT: &str = "Complete this:";

/// Converts a Completions request into a Chat Completions request, with the
/// prompt as the one user message. The sampling parameters both APIs share
/// are copied over, as upstream reads them; anything else is dropped.
pub fn convert_completions_request_to_chat_completions(request: &Value) -> Value {
    let mut prompt = str_of(request.get("prompt"));
    if prompt.is_empty() {
        prompt = DEFAULT_PROMPT.into();
    }
    let message = object([("role", "user".into()), ("content", prompt.into())]);
    let mut out = Map::new();
    out.insert("model".into(), str_of(request.get("model")).into());
    out.insert("messages".into(), Value::Array(vec![message]));

    // In upstream's order, each only if the client sent it.
    if let Some(max_tokens) = request.get("max_tokens") {
        out.insert("max_tokens".into(), int_of(max_tokens).into());
    }
    for key in [
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
    ] {
        if let Some(value) = request.get(key).and_then(float_of) {
            out.insert(key.into(), value);
        }
    }
    if let Some(stop) = request.get("stop") {
        out.insert("stop".into(), stop.clone());
    }
    if let Some(stream) = request.get("stream") {
        out.insert("stream".into(), bool_of(stream).into());
    }
    if let Some(logprobs) = request.get("logprobs") {
        out.insert("logprobs".into(), bool_of(logprobs).into());
    }
    if let Some(top_logprobs) = request.get("top_logprobs") {
        out.insert("top_logprobs".into(), int_of(top_logprobs).into());
    }
    if let Some(echo) = request.get("echo") {
        out.insert("echo".into(), bool_of(echo).into());
    }
    Value::Object(out)
}

/// Converts a Chat Completions response into a Completions response. Each
/// choice's message content becomes its `text`.
pub fn convert_chat_completions_response_to_completions(body: &[u8]) -> Value {
    let response = parse(body);
    let mut out = completion(&response);
    out.insert(
        "choices".into(),
        choices(&response).iter().map(response_choice).collect(),
    );
    if let Some(usage) = response.get("usage") {
        out.insert("usage".into(), usage.clone());
    }
    Value::Object(out)
}

/// Converts one Chat Completions stream chunk, the JSON of a `data:` line,
/// into a Completions chunk. `None` means the chunk should be skipped: it has
/// no text, no finish reason and no usage, like a chunk that only carries
/// the assistant's role.
pub fn convert_chat_completions_stream_chunk_to_completions(chunk: &[u8]) -> Option<Value> {
    let chunk = parse(chunk);
    let choices = choices(&chunk);
    let usage = chunk.get("usage");
    if usage.is_none() && !choices.iter().any(carries_content) {
        return None;
    }
    let mut out = completion(&chunk);
    out.insert("choices".into(), choices.iter().map(chunk_choice).collect());
    if let Some(usage) = usage {
        out.insert("usage".into(), usage.clone());
    }
    Some(Value::Object(out))
}

/// A response or chunk as JSON. Anything else has no fields, as gjson finds
/// none in most text that isn't JSON.
fn parse(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

/// The choices, if `choices` is an array.
fn choices(response: &Value) -> &[Value] {
    response
        .get("choices")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Upstream's output template with the response's ID, creation time and model
/// filled in. `choices` is empty.
fn completion(response: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("id".into(), str_of(response.get("id")).into());
    out.insert("object".into(), "text_completion".into());
    out.insert(
        "created".into(),
        response.get("created").map_or(0, int_of).into(),
    );
    out.insert("model".into(), str_of(response.get("model")).into());
    out.insert("choices".into(), Value::Array(Vec::new()));
    out
}

/// Whether a chunk's choice has text or a finish reason, which is what makes
/// upstream send the chunk on. A finish reason of `null`, or the string
/// `"null"`, doesn't count.
fn carries_content(choice: &Value) -> bool {
    path(choice, "delta.content").is_some_and(|content| !str_of(Some(content)).is_empty())
        || choice
            .get("finish_reason")
            .is_some_and(|reason| !matches!(&*str_of(Some(reason)), "" | "null"))
}

// Upstream builds each choice as a Go map, which `json.Marshal` writes with
// its keys sorted, so these insert `finish_reason`, `index`, `logprobs` and
// `text` in that order.

/// A response's choice. `text` is the message's content, or for a response
/// put together from chunks, the delta's.
fn response_choice(choice: &Value) -> Value {
    let mut out = Map::new();
    if let Some(reason) = choice.get("finish_reason") {
        out.insert("finish_reason".into(), str_of(Some(reason)).into());
    }
    out.insert("index".into(), index(choice));
    if let Some(logprobs) = choice.get("logprobs") {
        out.insert("logprobs".into(), go_marshaled(logprobs));
    }
    // The delta only counts when there is no message at all.
    let content = match choice.get("message") {
        Some(message) => message.get("content"),
        None => path(choice, "delta.content"),
    };
    if let Some(content) = content {
        out.insert("text".into(), str_of(Some(content)).into());
    }
    Value::Object(out)
}

/// A chunk's choice. It always has a `text`, empty when the delta has none,
/// and leaves out a finish reason that is the string `"null"` but keeps a
/// JSON `null` one, as `""`.
fn chunk_choice(choice: &Value) -> Value {
    let mut out = Map::new();
    if let Some(reason) = choice.get("finish_reason") {
        let reason = str_of(Some(reason));
        if reason != "null" {
            out.insert("finish_reason".into(), reason.into());
        }
    }
    out.insert("index".into(), index(choice));
    if let Some(logprobs) = choice.get("logprobs") {
        out.insert("logprobs".into(), go_marshaled(logprobs));
    }
    let text = str_of(path(choice, "delta.content"));
    out.insert("text".into(), text.into());
    Value::Object(out)
}

fn index(choice: &Value) -> Value {
    choice.get("index").map_or(0, int_of).into()
}

#[cfg(test)]
mod tests;
