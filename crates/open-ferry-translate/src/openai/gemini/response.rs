// Ported from CLIProxyAPI internal/translator/openai/gemini/openai_gemini_response.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! OpenAI Chat Completions responses → Gemini `generateContent` responses.
//!
//! Tool call arguments become a function call's `args` object. Arguments
//! that aren't a JSON object are read as leniently as upstream reads them:
//! each `"key": value` pair found between the first `{` and the last `}`.
//!
//! Deviations from upstream:
//! - A stream's function calls come out in the order of their tool call
//!   index. Upstream keeps them in a Go map, so their order changes from run
//!   to run.
//! - Where upstream copies JSON text it read into a string, we write the
//!   same JSON compactly: content that isn't a string, and reasoning given
//!   as an object whose `text` isn't a string.
//! - Reading arguments leniently, a number Go reads as infinite or NaN, such
//!   as `inf`, stays text; Go writes `+Inf` or `NaN`, which isn't JSON. So
//!   does a hexadecimal number such as `0x1p3`, which Go reads as a number.
//!   A key is set as it is, as upstream escapes it for sjson, except where
//!   sjson rejects the path: the key is empty or holds `|`, `#`, `@`, `*`
//!   or `?`. sjson drops a leading `:` from a path, and so do we.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::common::gemini_response::gemini_token_count_json;
use crate::go;
use crate::json::lenient::{self, Found};
use crate::json::{int_of, object, path, str_of};

/// Converts a token count into a Gemini `countTokens` response body.
pub fn gemini_token_count(count: i64) -> Value {
    gemini_token_count_json(count)
}

/// Translates a Chat Completions stream into Gemini response chunks, one line
/// at a time. Keep one per response: it gathers tool calls until the finish
/// reason comes.
#[derive(Default)]
pub struct OpenAIToGeminiStream {
    /// The tool calls so far, by their index.
    tool_calls: BTreeMap<i64, ToolCall>,
}

#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl OpenAIToGeminiStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Translates one line, with or without its `data:` prefix, into zero or
    /// more Gemini response chunks.
    pub fn translate_line(&mut self, line: &[u8]) -> Vec<Value> {
        if go::trim_space(line) == b"[DONE]" {
            return Vec::new();
        }
        let payload = match line.strip_prefix(b"data:") {
            Some(payload) => go::trim_space(payload),
            None => line,
        };
        let root: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
        let Some(Value::Array(choices)) = root.get("choices") else {
            return Vec::new();
        };

        if choices.is_empty() {
            let Some(usage) = root.get("usage") else {
                return Vec::new();
            };
            let mut template = object([
                ("candidates", Value::Array(Vec::new())),
                ("usageMetadata", Value::Object(Map::new())),
            ]);
            if let Some(model) = root.get("model") {
                template["model"] = str_of(Some(model)).into_owned().into();
            }
            set_usage(&mut template, usage);
            return vec![template];
        }

        let mut results = Vec::new();
        for choice in choices {
            let mut template = response_template();
            if let Some(model) = root.get("model") {
                template["model"] = str_of(Some(model)).into_owned().into();
            }
            // Upstream answers a first chunk's role on its own, but never
            // marks a chunk as the first, so that never happens.
            let delta = choice.get("delta");

            let mut outputs = Vec::new();
            if let Some(reasoning) = delta.and_then(|delta| delta.get("reasoning_content")) {
                for text in reasoning_texts(reasoning) {
                    if !text.is_empty() {
                        outputs.push(with_part(
                            &template,
                            object([("thought", true.into()), ("text", text.into())]),
                        ));
                    }
                }
            }
            if let Some(content) = delta.and_then(|delta| delta.get("content")) {
                let content = str_of(Some(content));
                if !content.is_empty() {
                    outputs.push(with_part(
                        &template,
                        object([("text", content.into_owned().into())]),
                    ));
                }
            }
            if !outputs.is_empty() {
                results.extend(outputs);
                continue;
            }

            if let Some(Value::Array(calls)) = delta.and_then(|delta| delta.get("tool_calls")) {
                for call in calls {
                    self.add_tool_call(call);
                }
                continue;
            }

            if let Some(Value::String(reason)) = choice.get("finish_reason")
                && !reason.is_empty()
            {
                let candidate = &mut template["candidates"][0];
                candidate["finishReason"] = finish_reason(reason).into();
                let calls = std::mem::take(&mut self.tool_calls);
                parts_mut(&mut template).extend(calls.into_values().map(|call| {
                    let mut function_call = Map::new();
                    if !call.id.is_empty() {
                        function_call.insert("id".into(), call.id.into());
                    }
                    function_call.insert("name".into(), call.name.into());
                    function_call.insert("args".into(), args_object(&call.arguments));
                    object([("functionCall", function_call.into())])
                }));
                results.push(template);
                continue;
            }

            if let Some(usage) = root.get("usage") {
                set_usage(&mut template, usage);
                results.push(template);
            }
        }
        results
    }

    /// One tool call delta: the first for its index starts the call, and
    /// later ones fill in its ID and name and add to its arguments.
    fn add_tool_call(&mut self, call: &Value) {
        let kind = str_of(call.get("type"));
        if !kind.is_empty() && kind != "function" {
            return;
        }
        let Some(function) = call.get("function") else {
            return;
        };
        let id = str_of(call.get("id")).into_owned();
        let name = str_of(function.get("name")).into_owned();
        let arguments = str_of(function.get("arguments"));
        let index = call.get("index").map_or(0, int_of);
        let entry = self.tool_calls.entry(index).or_insert_with(|| ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments: String::new(),
        });
        if !id.is_empty() {
            entry.id = id;
        }
        if !name.is_empty() {
            entry.name = name;
        }
        entry.arguments.push_str(&arguments);
    }
}

/// Converts a whole Chat Completions response into a Gemini response.
///
/// Like upstream, every choice writes its parts over the same list from the
/// start, so a later choice's fields go into the earlier one's parts.
pub fn convert_openai_response_to_gemini_non_stream(body: &[u8]) -> Value {
    let root: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let mut out = response_template();
    if let Some(model) = root.get("model") {
        out["model"] = str_of(Some(model)).into_owned().into();
    }

    let mut parts: Vec<Map<String, Value>> = Vec::new();
    if let Some(Value::Array(choices)) = root.get("choices") {
        for choice in choices {
            let choice_index = choice.get("index").map_or(0, int_of);
            let message = choice.get("message");
            let mut index = 0;

            if let Some(reasoning) = message.and_then(|message| message.get("reasoning_content")) {
                for text in reasoning_texts(reasoning) {
                    if !text.is_empty() {
                        let part = next_part(&mut parts, &mut index);
                        part.insert("thought".into(), true.into());
                        part.insert("text".into(), text.into());
                    }
                }
            }
            if let Some(content) = message.and_then(|message| message.get("content")) {
                let content = str_of(Some(content));
                if !content.is_empty() {
                    let part = next_part(&mut parts, &mut index);
                    part.insert("text".into(), content.into_owned().into());
                }
            }
            if let Some(Value::Array(calls)) = message.and_then(|message| message.get("tool_calls"))
            {
                for call in calls {
                    if str_of(call.get("type")) != "function" {
                        continue;
                    }
                    let function = call.get("function");
                    let name = str_of(function.and_then(|function| function.get("name")));
                    let arguments = str_of(function.and_then(|function| function.get("arguments")));
                    let id = str_of(call.get("id"));

                    let part = next_part(&mut parts, &mut index);
                    let function_call = part
                        .entry("functionCall")
                        .or_insert_with(|| Value::Object(Map::new()));
                    if !function_call.is_object() {
                        *function_call = Value::Object(Map::new());
                    }
                    if !id.is_empty() {
                        function_call["id"] = id.into_owned().into();
                    }
                    function_call["name"] = name.into_owned().into();
                    function_call["args"] = args_object(&arguments);
                }
            }

            if let Some(Value::String(reason)) = choice.get("finish_reason")
                && !reason.is_empty()
            {
                out["candidates"][0]["finishReason"] = finish_reason(reason).into();
            }
            out["candidates"][0]["index"] = choice_index.into();
        }
    }
    if !parts.is_empty() {
        *parts_mut(&mut out) = parts.into_iter().map(Value::Object).collect();
    }

    if let Some(usage) = root.get("usage") {
        set_usage(&mut out, usage);
    }
    out
}

/// `ensurePart`: the part at `index`, made empty if the list is shorter, and
/// moves `index` past it.
fn next_part<'p>(
    parts: &'p mut Vec<Map<String, Value>>,
    index: &mut usize,
) -> &'p mut Map<String, Value> {
    if parts.len() <= *index {
        parts.resize_with(*index + 1, Map::new);
    }
    *index += 1;
    &mut parts[*index - 1]
}

/// Upstream's template for a response or a chunk with a choice.
fn response_template() -> Value {
    object([(
        "candidates",
        Value::Array(vec![object([
            (
                "content",
                object([
                    ("parts", Value::Array(Vec::new())),
                    ("role", "model".into()),
                ]),
            ),
            ("index", 0.into()),
        ])]),
    )])
}

/// The first candidate's parts.
fn parts_mut(template: &mut Value) -> &mut Vec<Value> {
    match &mut template["candidates"][0]["content"]["parts"] {
        Value::Array(parts) => parts,
        _ => unreachable!("the template has a parts list"),
    }
}

/// `template` with `part` as its only part.
fn with_part(template: &Value, part: Value) -> Value {
    let mut chunk = template.clone();
    parts_mut(&mut chunk).push(part);
    chunk
}

/// `mapOpenAIFinishReasonToGemini`.
fn finish_reason(reason: &str) -> &'static str {
    match reason {
        "length" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        _ => "STOP",
    }
}

/// `extractReasoningTexts`: the texts in a `reasoning_content`, which is a
/// string, an object with a `text`, or an array of either.
fn reasoning_texts(node: &Value) -> Vec<String> {
    match node {
        Value::Array(items) => items.iter().flat_map(reasoning_texts).collect(),
        Value::String(text) => vec![text.clone()],
        // An object's text, if it has one. Without one, upstream takes its
        // raw text unless that starts with `{`, which an object's always does.
        Value::Object(fields) => fields
            .get("text")
            .map(|text| str_of(Some(text)).into_owned())
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// `setGeminiUsageMetadataFromOpenAIUsage`: Chat Completions or Responses
/// token counts as Gemini's. `usageMetadata` is made only when a count is
/// set.
fn set_usage(out: &mut Value, usage: &Value) {
    let count = |keys: &[&str]| keys.iter().find_map(|key| path(usage, key)).map(int_of);
    let prompt = count(&["prompt_tokens", "input_tokens"]);
    let completion = count(&["completion_tokens", "output_tokens"]);
    let total = count(&["total_tokens"]);
    let reasoning = count(&[
        "completion_tokens_details.reasoning_tokens",
        "output_tokens_details.reasoning_tokens",
    ])
    .unwrap_or(0);
    let cached = count(&[
        "prompt_tokens_details.cached_tokens",
        "input_tokens_details.cached_tokens",
    ])
    .unwrap_or(0);

    let mut fields = Vec::new();
    if let Some(prompt) = prompt {
        fields.push(("promptTokenCount", prompt));
    }
    if let Some(completion) = completion {
        fields.push(("candidatesTokenCount", completion));
    }
    if let Some(total) = total {
        fields.push(("totalTokenCount", total));
    } else if prompt.is_some() || completion.is_some() {
        let sum = prompt.unwrap_or(0).wrapping_add(completion.unwrap_or(0));
        fields.push(("totalTokenCount", sum));
    }
    if reasoning > 0 {
        fields.push(("thoughtsTokenCount", reasoning));
    }
    if cached > 0 {
        fields.push(("cachedContentTokenCount", cached));
    }
    if fields.is_empty() {
        return;
    }
    let metadata = &mut out["usageMetadata"];
    if !metadata.is_object() {
        *metadata = Value::Object(Map::new());
    }
    for (key, value) in fields {
        metadata[key] = value.into();
    }
}

/// `parseArgsToObjectRaw`: tool call arguments as an object. Arguments that
/// are a JSON object are taken as they are; others are read leniently, and
/// what can't be read at all is `{}`.
fn args_object(arguments: &str) -> Value {
    let trimmed = arguments.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return Value::Object(Map::new());
    }
    if trimmed.starts_with('{')
        && go::gjson_valid(trimmed.as_bytes())
        && let Ok(object @ Value::Object(_)) = serde_json::from_str(trimmed)
    {
        return object;
    }
    Value::Object(tolerant_object(trimmed))
}

/// The characters upstream skips around keys and values.
fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\n' | '\r' | '\t')
}

/// `tolerantParseJSONObjectRaw`: each `"key": value` pair between the first
/// `{` and the last `}`, as far as they can be read. A value is a string, an
/// object or array (kept as JSON if it is valid, else as text), or else
/// everything up to the next comma: `true`, `false`, `null`, a number, or
/// text.
fn tolerant_object(text: &str) -> Map<String, Value> {
    let mut result = Map::new();
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return result;
    };
    if start >= end {
        return result;
    }
    let chars: Vec<char> = text[start + 1..end].chars().collect();
    let n = chars.len();
    let mut i = 0;
    let skip_blanks = |i: &mut usize| {
        while *i < n && is_blank(chars[*i]) {
            *i += 1;
        }
    };

    while i < n {
        while i < n && (is_blank(chars[i]) || chars[i] == ',') {
            i += 1;
        }
        if i >= n {
            break;
        }
        if chars[i] != '"' {
            while i < n && chars[i] != ',' {
                i += 1;
            }
            continue;
        }
        let Some((key, next)) = string_token(&chars, i) else {
            break;
        };
        let key = token_text(&key);
        i = next;
        skip_blanks(&mut i);
        if i >= n || chars[i] != ':' {
            break;
        }
        i += 1;
        skip_blanks(&mut i);
        if i >= n {
            break;
        }

        match chars[i] {
            '"' => match string_token(&chars, i) {
                Some((token, next)) => {
                    set_key(&mut result, &key, token_text(&token).into());
                    i = next;
                }
                None => {
                    set_key(&mut result, &key, "".into());
                    i = n;
                }
            },
            '{' | '[' => match bracketed(&chars, i) {
                Some((segment, next)) => {
                    let value = go::gjson_valid(segment.as_bytes())
                        .then(|| serde_json::from_str(&segment).ok())
                        .flatten()
                        .unwrap_or(Value::String(segment));
                    set_key(&mut result, &key, value);
                    i = next;
                }
                None => i = n,
            },
            _ => {
                let mut j = i;
                while j < n && chars[j] != ',' {
                    j += 1;
                }
                let token: String = chars[i..j].iter().collect();
                set_key(&mut result, &key, scalar(token.trim()));
                i = j;
            }
        }

        skip_blanks(&mut i);
        if i < n && chars[i] == ',' {
            i += 1;
        }
    }
    result
}

/// `parseJSONStringRunes`: the string token starting at `start`, quotes
/// included, and where it ends; `None` if it doesn't end.
fn string_token(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut escaped = false;
    for (i, &c) in chars.iter().enumerate().skip(start + 1) {
        if c == '\\' && !escaped {
            escaped = true;
            continue;
        }
        if c == '"' && !escaped {
            return Some((chars[start..=i].iter().collect(), i + 1));
        }
        escaped = false;
    }
    None
}

/// `jsonStringTokenToRawString`: a string token's text, decoded as gjson
/// decodes a string.
fn token_text(token: &str) -> String {
    match lenient::get(&format!("{{\"k\":{token}}}"), "k") {
        Some(Found::String(text)) => text,
        _ => token
            .strip_prefix('"')
            .and_then(|token| token.strip_suffix('"'))
            .unwrap_or(token)
            .to_owned(),
    }
}

/// `captureBracketed`: the object or array starting at `start`, up to its
/// matching bracket, and where it ends; `None` if it doesn't end.
fn bracketed(chars: &[char], start: usize) -> Option<(String, usize)> {
    let open = chars[start];
    let close = if open == '{' { '}' } else { ']' };
    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (j, &c) in chars.iter().enumerate().skip(start) {
        if in_string {
            if c == '\\' && !escaped {
                escaped = true;
                continue;
            }
            if c == '"' && !escaped {
                in_string = false;
            } else {
                escaped = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some((chars[start..=j].iter().collect(), j + 1));
            }
        }
    }
    None
}

/// A bare value: `true`, `false`, `null`, a number as Go's `strconv` reads
/// one, or else the text itself.
fn scalar(token: &str) -> Value {
    match token {
        "true" => return true.into(),
        "false" => return false.into(),
        "null" => return Value::Null,
        _ => {}
    }
    if let Ok(int) = token.parse::<i64>() {
        return int.into();
    }
    // Go's ParseUint takes no sign; Rust's takes a `+`.
    if !token.starts_with('+')
        && let Ok(uint) = token.parse::<u64>()
    {
        return uint.into();
    }
    // Rust reads the decimal numbers Go's ParseFloat does, and both read a
    // number too large for an f64 as infinite, where Go reports an error.
    if let Ok(float) = token.parse::<f64>()
        && float.is_finite()
        && let Ok(number) = serde_json::from_str(&go::format_float(float))
    {
        return number;
    }
    token.into()
}

/// Sets `key` as sjson sets a path of one key, so a key sjson rejects isn't
/// set.
fn set_key(result: &mut Map<String, Value>, key: &str, value: Value) {
    if key.is_empty() {
        return;
    }
    let key = key.strip_prefix(':').unwrap_or(key);
    if key
        .bytes()
        .any(|b| matches!(b, b'|' | b'#' | b'@' | b'*' | b'?'))
    {
        return;
    }
    result.insert(key.to_owned(), value);
}

#[cfg(test)]
mod tests;
