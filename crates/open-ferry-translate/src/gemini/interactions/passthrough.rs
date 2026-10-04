// Ported from CLIProxyAPI internal/translator/gemini/interactions/interactions_gemini_response.go
// (ConvertInteractionsRequestToInteractions, ConvertInteractionsResponsePassthrough,
// ConvertInteractionsResponsePassthroughNonStream) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Gemini Interactions → Gemini Interactions: requests and responses pass
//! through as they are.

use serde_json::Value;

/// `ConvertInteractionsRequestToInteractions`: the request as it is.
pub fn convert_interactions_request_to_interactions(
    _model: &str,
    body: Value,
    _stream: bool,
) -> Value {
    body
}

/// `ConvertInteractionsResponsePassthrough`: a stream chunk as it is, or
/// nothing for an empty one.
pub fn convert_interactions_response_passthrough(chunk: &[u8]) -> Option<&[u8]> {
    (!chunk.is_empty()).then_some(chunk)
}

/// `ConvertInteractionsResponsePassthroughNonStream`: a whole response as it
/// is.
pub fn convert_interactions_response_passthrough_non_stream(body: &[u8]) -> &[u8] {
    body
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Not upstream's: upstream has no test of the passthrough.
    #[test]
    fn passes_through_as_is() {
        let body = json!({ "model": "m", "input": "hi", "stream": true });
        assert_eq!(
            convert_interactions_request_to_interactions("other", body.clone(), false),
            body
        );
        assert_eq!(
            convert_interactions_response_passthrough(b" data: x\n"),
            Some(&b" data: x\n"[..])
        );
        assert_eq!(convert_interactions_response_passthrough(b""), None);
        assert_eq!(
            convert_interactions_response_passthrough_non_stream(b"not json"),
            b"not json"
        );
    }
}
