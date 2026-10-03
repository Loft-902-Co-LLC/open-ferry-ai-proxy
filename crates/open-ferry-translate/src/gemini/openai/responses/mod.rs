//! OpenAI Responses clients talking to a Gemini upstream.

mod replay_cache;
mod request;
mod response;
mod signature_carrier;
mod trailing_signature;
mod web_search;

pub use request::convert_openai_responses_request_to_gemini;
pub use response::{
    GeminiToOpenAIResponsesStream, convert_gemini_response_to_openai_responses_non_stream,
};

use serde_json::Value;

/// gjson `Get` for a dotted path, where a part that is a number indexes an
/// array.
fn at<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Array(items) => {
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            items.get(key.parse::<usize>().ok()?)
        }
        _ => value.get(key),
    })
}

/// gjson `Array()`: an array's elements, nothing for a missing value or
/// `null`, and any other value on its own.
fn array_of(value: Option<&Value>) -> &[Value] {
    match value {
        None | Some(Value::Null) => &[],
        Some(Value::Array(items)) => items,
        Some(other) => std::slice::from_ref(other),
    }
}

#[cfg(test)]
mod test_support;
