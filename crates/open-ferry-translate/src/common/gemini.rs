// Ported from CLIProxyAPI internal/translator/common/gemini.go (IsGeminiThoughtPart,
// MergeAdjacentGeminiContents, ContentHasGeminiFunctionResponse, ReorderGeminiUserParts,
// MergeAdjacentGeminiUserContents, ContainsJSONRef, SetGeminiFunctionResponseResult and
// SetGeminiFunctionResponseRaw), internal/util/util.go (SanitizeFunctionName) and
// internal/util/translator.go (SanitizedToolNameMap and RestoreSanitizedToolName)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Helpers for the `contents` of Gemini requests, shared by the translators
//! that build them, and for the function names Gemini accepts.
//!
//! Gemini wants user and model turns to alternate, and rejects a user turn
//! whose text comes after a function response. These helpers merge
//! consecutive user turns and move text ahead of function responses. Model
//! turns are never merged: that would shift part indices, which thought
//! signatures depend on.
//!
//! Gemini function names allow fewer characters than clients' tool names, so
//! requests carry [`sanitize_gemini_function_name`]d names and responses map
//! them back ([`sanitized_tool_name_map`]).
//!
//! Upstream's `SplitGeminiFunctionResponseTurns` and
//! `ContentHasGeminiFunctionCall` are not ported: only the Antigravity
//! translators call them.
//!
//! Deviations from upstream:
//! - Turns and parts are parsed JSON rather than raw bytes, so the empty turn
//!   upstream skips can't occur.
//! - Where a function result holding a `$ref` is stored as a string,
//!   [`set_gemini_function_response_result`] writes it as compact JSON;
//!   upstream copies the client's raw text. [`set_gemini_function_response_raw`]
//!   stores the text it is given, trimmed, as upstream does.
//! - [`set_gemini_function_response_raw`] stores `""` for text that isn't
//!   valid JSON. Upstream stores whatever gjson makes of it, which can leave
//!   the request invalid JSON.
//! - Upstream logs a warning when two tool names sanitize to the same name;
//!   [`sanitized_tool_name_map`] logs nothing.

use std::collections::HashMap;

use serde_json::Value;

use crate::json::{bool_of, set_path, str_of};

/// Gemini's limit on function name length, in bytes.
pub(crate) const GEMINI_FUNCTION_NAME_LIMIT: usize = 64;

/// Sanitized tool names, each with the name the client declared.
pub(crate) type SanitizedToolNames = HashMap<String, String>;

/// Reports whether a Gemini part holds the model's hidden thoughts.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "only the tests call it; the translators from Gemini clients keep their own `is_thought`"
    )
)]
pub(crate) fn is_gemini_thought_part(part: &Value) -> bool {
    part.get("thought").is_some_and(bool_of)
}

/// Merges consecutive user turns into one, moving their text ahead of their
/// function responses ([`reorder_gemini_user_parts`]). Turns without parts are
/// dropped, unless there is only one turn. Model turns are kept apart, so the
/// part indices thought signatures refer to don't move.
///
/// Claude requests can carry system messages mid-conversation, which become
/// user reminder turns; this joins them to the user turns around them.
pub(crate) fn merge_adjacent_gemini_contents(contents: Vec<Value>) -> Vec<Value> {
    merge_user_turns(contents, Merge::Reordering)
}

/// Merges consecutive user turns into one, like
/// [`merge_adjacent_gemini_contents`], but leaves turns holding a function
/// response apart, and keeps parts in their order.
pub(crate) fn merge_adjacent_gemini_user_contents(contents: Vec<Value>) -> Vec<Value> {
    merge_user_turns(contents, Merge::InOrder)
}

/// How [`merge_user_turns`] joins consecutive user turns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Merge {
    /// Joins them all, reordering the parts after each join
    /// ([`reorder_gemini_user_parts`]).
    Reordering,
    /// Joins those without a function response, keeping the parts' order.
    InOrder,
}

/// Joins each user turn onto the user turn before it, as `merge` says.
///
/// Upstream writes out and reorders all the joined turn's parts again for
/// each turn it adds. This keeps them aside until the run of user turns ends,
/// sorting only what each turn adds into place, for the same parts in time
/// linear in their number.
fn merge_user_turns(contents: Vec<Value>, merge: Merge) -> Vec<Value> {
    if contents.len() <= 1 {
        return contents;
    }
    let mut merged: Vec<Value> = Vec::with_capacity(contents.len());
    // The parts of the last turn in `merged` while it is a user turn, taken
    // out of it until no more join it.
    let mut run: Option<UserParts> = None;
    for mut content in contents {
        let parts = match content.get_mut("parts") {
            Some(Value::Array(parts)) if !parts.is_empty() => std::mem::take(parts),
            _ => continue,
        };
        let summary = PartsSummary::of(&parts);
        let is_user = str_of(content.get("role")) == "user";
        if is_user
            && let Some(last) = run.as_mut()
            && (merge == Merge::Reordering
                || !(last.summary().has_response || summary.has_response))
        {
            last.join(parts, summary, merge);
            continue;
        }
        if let Some(last) = run.take() {
            last.put_back(merged.last_mut());
        }
        if is_user {
            run = Some(UserParts::new(parts, summary));
        } else {
            content["parts"] = Value::Array(parts);
        }
        merged.push(content);
    }
    if let Some(last) = run {
        last.put_back(merged.last_mut());
    }
    merged
}

/// The parts of a user turn others join, as [`reorder_gemini_user_parts`]
/// leaves them after each join: text parts, then the others, then those that
/// haven't been reordered, each with its [`PartsSummary`].
struct UserParts {
    text: (Vec<Value>, PartsSummary),
    other: (Vec<Value>, PartsSummary),
    rest: (Vec<Value>, PartsSummary),
}

impl UserParts {
    fn new(parts: Vec<Value>, summary: PartsSummary) -> Self {
        Self {
            text: Default::default(),
            other: Default::default(),
            rest: (parts, summary),
        }
    }

    /// The summary of all the parts, in order.
    fn summary(&self) -> PartsSummary {
        self.text.1.then(self.other.1).then(self.rest.1)
    }

    /// Adds `parts`, reordering them all if `merge` says to and text then
    /// follows a function response. As the parts ahead of `rest` are already
    /// text, then not, only `rest` and `parts` need sorting into place.
    fn join(&mut self, parts: Vec<Value>, summary: PartsSummary, merge: Merge) {
        if merge == Merge::Reordering && self.summary().then(summary).text_after_response {
            let (rest, _) = std::mem::take(&mut self.rest);
            for part in rest.into_iter().chain(parts) {
                let group = if part.get("text").is_some() {
                    &mut self.text
                } else {
                    &mut self.other
                };
                group.1 = group.1.then(PartsSummary::of_part(&part));
                group.0.push(part);
            }
        } else {
            self.rest.0.extend(parts);
            self.rest.1 = self.rest.1.then(summary);
        }
    }

    /// Puts the parts back into their turn.
    fn put_back(self, turn: Option<&mut Value>) {
        let Self {
            text: (mut parts, _),
            other: (other, _),
            rest: (rest, _),
        } = self;
        parts.extend(other);
        parts.extend(rest);
        if let Some(turn) = turn {
            turn["parts"] = Value::Array(parts);
        }
    }
}

/// What reordering a run of parts depends on, which a longer run's can be
/// worked out from ([`PartsSummary::then`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PartsSummary {
    /// A function response is among them.
    has_response: bool,
    /// A text part other than a function response is among them.
    has_text: bool,
    /// Such a text part follows a function response.
    text_after_response: bool,
}

impl PartsSummary {
    fn of(parts: &[Value]) -> Self {
        parts.iter().fold(Self::default(), |summary, part| {
            summary.then(Self::of_part(part))
        })
    }

    fn of_part(part: &Value) -> Self {
        if is_function_response(part) {
            Self {
                has_response: true,
                ..Self::default()
            }
        } else {
            Self {
                has_text: part.get("text").is_some(),
                ..Self::default()
            }
        }
    }

    /// The summary of these parts followed by `next`'s.
    fn then(self, next: Self) -> Self {
        Self {
            has_response: self.has_response || next.has_response,
            has_text: self.has_text || next.has_text,
            text_after_response: self.text_after_response
                || next.text_after_response
                || (self.has_response && next.has_text),
        }
    }
}

/// Reports whether a Gemini turn holds a function response part
/// (`functionResponse` or `function_response`).
pub(crate) fn content_has_gemini_function_response(content: &Value) -> bool {
    match content.get("parts") {
        Some(Value::Array(parts)) => parts.iter().any(is_function_response),
        Some(Value::Object(parts)) => parts.values().any(is_function_response),
        _ => false,
    }
}

fn is_function_response(part: &Value) -> bool {
    part.get("functionResponse").is_some() || part.get("function_response").is_some()
}

/// Moves the text parts of a user turn ahead of its other parts, keeping each
/// group's order, when text follows a function response. Vertex AI rejects a
/// turn with text after a function response.
pub(crate) fn reorder_gemini_user_parts(parts: Vec<Value>) -> Vec<Value> {
    let mut seen_response = false;
    let text_follows_response = parts.iter().any(|part| {
        if is_function_response(part) {
            seen_response = true;
            false
        } else {
            seen_response && part.get("text").is_some()
        }
    });
    if !text_follows_response {
        return parts;
    }
    let (mut text, other): (Vec<Value>, Vec<Value>) = parts
        .into_iter()
        .partition(|part| part.get("text").is_some());
    text.extend(other);
    text
}

/// Reports whether `value` holds, at any depth, a `$ref` key with a string
/// value.
pub(crate) fn contains_json_ref(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields
            .iter()
            .any(|(key, child)| (key == "$ref" && child.is_string()) || contains_json_ref(child)),
        Value::Array(items) => items.iter().any(contains_json_ref),
        _ => false,
    }
}

/// Stores a function result at `path` of a Gemini part, or `""` if there is
/// none.
///
/// A result holding a `$ref` is stored as a JSON string instead: Gemini would
/// read the `$ref` as a reference to a media part and reject the request. It
/// goes under `result` when `path` ends in `response`, as Gemini wants
/// `response` to be an object.
pub(crate) fn set_gemini_function_response_result(
    part: &mut Value,
    path: &str,
    result: Option<Value>,
) {
    match result {
        None => {
            set_path(part, path, Value::String(String::new()));
        }
        Some(result) if contains_json_ref(&result) => {
            set_ref_result(part, path, result.to_string());
        }
        Some(result) => {
            set_path(part, path, result);
        }
    }
}

/// [`set_gemini_function_response_result`] for a result given as JSON text.
/// Blank text stores `""`.
pub(crate) fn set_gemini_function_response_raw(part: &mut Value, path: &str, raw: &str) {
    let trimmed = raw.trim();
    // Each number keeps its text, as upstream copies gjson's `Raw`.
    match crate::json::exact::from_str(trimmed) {
        Ok(result) if contains_json_ref(&result) => set_ref_result(part, path, trimmed.to_owned()),
        Ok(result) => {
            set_path(part, path, result);
        }
        Err(_) => {
            set_path(part, path, Value::String(String::new()));
        }
    }
}

/// Stores the text of a result holding a `$ref` as a string.
fn set_ref_result(part: &mut Value, path: &str, text: String) {
    let target = if path.ends_with("response") {
        format!("{path}.result")
    } else {
        path.to_owned()
    };
    set_path(part, &target, Value::String(text));
}

/// `SanitizeFunctionName`: `name` made a valid Gemini function name. Each
/// character other than an ASCII letter, digit, `_`, `.`, `:` or `-` becomes
/// `_`; a name that doesn't start with a letter or `_` gets a leading `_`;
/// and the result is cut to 64 bytes. An empty name stays empty.
pub(crate) fn sanitize_gemini_function_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    let mut sanitized: String = name
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' | ':' | '-' => c,
            _ => '_',
        })
        .collect();
    // Every character is ASCII now, so any byte is a boundary.
    if !matches!(sanitized.as_bytes()[0], b'a'..=b'z' | b'A'..=b'Z' | b'_') {
        sanitized.truncate(GEMINI_FUNCTION_NAME_LIMIT - 1);
        sanitized.insert(0, '_');
    }
    sanitized.truncate(GEMINI_FUNCTION_NAME_LIMIT);
    sanitized
}

/// `SanitizedToolNameMap`: the tools a request declares whose names
/// [`sanitize_gemini_function_name`] changes, by sanitized name, each with
/// the name as declared (trimmed). The first of two names that sanitize alike
/// wins. `None` if there are none.
pub(crate) fn sanitized_tool_name_map(request: &Value) -> Option<SanitizedToolNames> {
    let Some(Value::Array(tools)) = request.get("tools") else {
        return None;
    };
    let mut names = SanitizedToolNames::new();
    for tool in tools {
        let name = str_of(tool.get("name"));
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let sanitized = sanitize_gemini_function_name(name);
        if sanitized != name {
            names.entry(sanitized).or_insert_with(|| name.to_owned());
        }
    }
    (!names.is_empty()).then_some(names)
}

/// `RestoreSanitizedToolName`: the declared name a sanitized function name
/// stands for, or the name itself.
pub(crate) fn restore_sanitized_tool_name(
    names: Option<&SanitizedToolNames>,
    name: &str,
) -> String {
    names
        .filter(|_| !name.is_empty())
        .and_then(|names| names.get(name))
        .map_or_else(|| name.to_owned(), Clone::clone)
}

#[cfg(test)]
mod tests;
