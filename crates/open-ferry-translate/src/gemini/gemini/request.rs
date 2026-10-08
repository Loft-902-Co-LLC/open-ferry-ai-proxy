// Ported from CLIProxyAPI internal/translator/gemini/gemini/gemini_gemini_request.go
// (ConvertGeminiRequestToGemini, backfillEmptyFunctionResponseNames,
// geminiFunctionResponseNamesNeedBackfill, backfillEmptyFunctionResponseNamesLegacy and
// nextGeminiRole) and internal/util/translator.go (RenameKey) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini request → Gemini request.
//!
//! The request is kept as it is, apart from what the Gemini API would
//! otherwise reject:
//! - `functionDeclarations` in a tool becomes `function_declarations`, and
//!   `parameters` in a declaration becomes `parametersJsonSchema`;
//! - a turn whose role is neither `user` nor `model` gets one: `user` if it
//!   holds a function response, else the role after the previous turn's
//!   (`user` first, then alternating);
//! - thought signatures are sanitized
//!   ([`sanitize_gemini_request_thought_signatures`]);
//! - `generationConfig.responseSchema` becomes `responseJsonSchema`;
//! - a function response without a name takes the name of the matching call
//!   in the model turn before it;
//! - the default safety settings are added when there are none.
//!
//! A request without `contents` only gets the safety settings.
//!
//! Upstream writes these changes with gjson paths built from indices and
//! object keys alike, so `contents` that isn't an array gets keys such as
//! `"0"`; [`crate::gemini_schema::set`] reproduces that.
//!
//! Deviations from upstream:
//! - Upstream logs at debug level when a turn has more unnamed function
//!   responses than the model turn before it has calls; this logs nothing.
//! - A function call name that is an object or array is copied into the
//!   response as compact JSON; upstream copies the client's raw text.

use serde_json::Value;

use crate::common::gemini::content_has_gemini_function_response;
use crate::gemini::common::attach_default_safety_settings;
use crate::gemini_schema::set;
use crate::json::{int_of, str_of};
use crate::signature::sanitize_gemini_request_thought_signatures;

/// Normalizes a Gemini request for the Gemini API. The model and the stream
/// flag aren't used: the model is in the URL.
pub fn convert_gemini_request_to_gemini(
    _model_name: &str,
    mut request: Value,
    _stream: bool,
) -> Value {
    if request.get("contents").is_none() {
        attach_default_safety_settings(&mut request, "safetySettings");
        return request;
    }
    rename_tool_fields(&mut request);
    fix_roles(&mut request);
    sanitize_gemini_request_thought_signatures(&mut request, "contents");
    if let Some(Value::Object(config)) = request.get_mut("generationConfig")
        && let Some(schema) = config.shift_remove("responseSchema")
    {
        config.insert("responseJsonSchema".to_owned(), schema);
    }
    backfill_empty_function_response_names(&mut request);
    attach_default_safety_settings(&mut request, "safetySettings");
    request
}

/// Renames `functionDeclarations` in each tool to `function_declarations`,
/// and `parameters` in each declaration to `parametersJsonSchema`. A renamed
/// field takes the place of one already under the new name, or goes last.
fn rename_tool_fields(request: &mut Value) {
    let Some(Value::Array(tools)) = request.get_mut("tools") else {
        return;
    };
    for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
        if let Some(declarations) = tool.shift_remove("functionDeclarations") {
            tool.insert("function_declarations".to_owned(), declarations);
        }
        let Some(Value::Array(declarations)) = tool.get_mut("function_declarations") else {
            continue;
        };
        for declaration in declarations.iter_mut().filter_map(Value::as_object_mut) {
            if let Some(parameters) = declaration.shift_remove("parameters") {
                declaration.insert("parametersJsonSchema".to_owned(), parameters);
            }
        }
    }
}

/// Gives every turn a valid role. Upstream edits array `contents` turn by
/// turn; anything else it walks as gjson does, and sets
/// `contents.<n>.role` for the nth element it visits.
fn fix_roles(request: &mut Value) {
    if let Some(Value::Array(contents)) = request.get_mut("contents") {
        let mut previous = "";
        for content in contents {
            let role = valid_role(content).unwrap_or_else(|| {
                let role = missing_role(content, previous);
                set(content, "role", Value::from(role));
                role
            });
            previous = role;
        }
        return;
    }
    let mut previous = "";
    let mut fixes = Vec::new();
    for (n, (_, content)) in elements(request.get("contents")).enumerate() {
        let role = valid_role(content).unwrap_or_else(|| {
            let role = missing_role(content, previous);
            fixes.push((n, role));
            role
        });
        previous = role;
    }
    for (n, role) in fixes {
        set(request, &format!("contents.{n}.role"), Value::from(role));
    }
}

/// The turn's role, if it is `user` or `model`.
fn valid_role(content: &Value) -> Option<&'static str> {
    match content.get("role").and_then(Value::as_str) {
        Some("user") => Some("user"),
        Some("model") => Some("model"),
        _ => None,
    }
}

/// The role for a turn without a valid one, after a turn with role
/// `previous` (`""` for the first turn).
fn missing_role(content: &Value, previous: &str) -> &'static str {
    if content_has_gemini_function_response(content) {
        "user"
    } else {
        next_gemini_role(previous)
    }
}

fn next_gemini_role(previous: &str) -> &'static str {
    if previous.is_empty() || previous == "model" {
        "user"
    } else {
        "model"
    }
}

/// Names each unnamed function response after the call in the preceding
/// model turn at the same position among the turn's responses. Gemini rejects
/// a function response with an empty name, which some clients send.
///
/// Only the first turn after a model turn is filled in. A response without a
/// matching call keeps its empty name.
fn backfill_empty_function_response_names(request: &mut Value) {
    let mut names = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for (content_index, content) in elements(request.get("contents")) {
        if str_of(content.get("role")) == "model" {
            pending = elements(content.get("parts"))
                .filter_map(|(_, part)| part.get("functionCall"))
                .map(|call| str_of(call.get("name")).into_owned())
                .collect();
            continue;
        }
        if pending.is_empty() {
            continue;
        }
        let responses = elements(content.get("parts"))
            .filter_map(|(part_index, part)| Some((part_index, part.get("functionResponse")?)));
        // Draining leaves nothing pending for the turns after this one.
        for ((part_index, response), call_name) in responses.zip(pending.drain(..)) {
            if str_of(response.get("name")).trim().is_empty() {
                names.push((
                    format!("contents.{content_index}.parts.{part_index}.functionResponse.name"),
                    call_name,
                ));
            }
        }
    }
    for (path, name) in names {
        set(request, &path, Value::String(name));
    }
}

/// gjson `ForEach`: an array's items, an object's values or a scalar itself,
/// each with the integer gjson's `key.Int()` gives for it. That is an array
/// item's index, an object key read as an integer (`0` unless it is one), or
/// `0` for a scalar. Nothing for `None`.
fn elements(value: Option<&Value>) -> Box<dyn Iterator<Item = (i64, &Value)> + '_> {
    match value {
        None => Box::new(std::iter::empty()),
        Some(Value::Array(items)) => Box::new((0..).zip(items)),
        Some(Value::Object(fields)) => Box::new(
            fields
                .iter()
                .map(|(key, value)| (int_of(&Value::String(key.clone())), value)),
        ),
        Some(scalar) => Box::new(std::iter::once((0, scalar))),
    }
}

#[cfg(test)]
mod tests;
