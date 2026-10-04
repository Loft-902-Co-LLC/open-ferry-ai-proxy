// Ported from CLIProxyAPI internal/runtime/executor/meta_executor_execute.go
// (translateMetaCompleted, metaAsCompletedEvent) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Reading Meta's answer to a call that didn't ask for a stream.
//!
//! Meta is always asked for an event stream, whatever the client wants. A
//! call that wants one answer reads the stream through to its
//! `response.completed` (or `response.incomplete`) event and translates
//! that. A reply that isn't a stream, a plain Responses object, is taken as
//! the completed event it would have been in.
//!
//! Deviations from upstream:
//! - A line that isn't JSON is read as having no fields, where gjson reads
//!   what it can from it.
//! - Upstream also hands the completed event on to usage reporting; that is
//!   the call's taps' work here.

use open_ferry_core::exec::{Format, Request};
use open_ferry_translate::go::trim_space;
use open_ferry_translate::registry::{Registry, ResponseContext};
use serde_json::{Value, json};

use super::error::stream_event_error;
use super::request::Prepared;
use crate::codex::terminal::{APPLY_PATCH_ERROR_MESSAGE, OutputItems, StatusError};
use crate::json::{exists, str_at};

/// What the error for a stream that ended with no completed event says.
const DISCONNECTED_MESSAGE: &str =
    "meta stream error: stream disconnected before response.completed or response.incomplete";

fn parse(data: &[u8]) -> Value {
    serde_json::from_slice(data).unwrap_or(Value::Null)
}

/// The error for a response `apply_patch` couldn't be translated in.
fn apply_patch_failure() -> StatusError {
    StatusError::new(502, APPLY_PATCH_ERROR_MESSAGE)
}

/// The completed event a plain reply stands for (`metaAsCompletedEvent`): a
/// JSON `response.completed` or `response.incomplete` event as it is, and a
/// JSON object that is a `response` or has an `output`, wrapped in a
/// `response.completed` event. Nothing for anything else.
pub(super) fn as_completed_event(data: &[u8]) -> Option<Vec<u8>> {
    let trimmed = trim_space(data);
    let root: Value = serde_json::from_slice(trimmed).ok()?;
    let kind = str_at(&root, "type");
    if kind == "response.completed" || kind == "response.incomplete" {
        return Some(trimmed.to_vec());
    }
    if str_at(&root, "object") == "response" || exists(&root, "output") {
        let wrapped = json!({"type": "response.completed", "response": root});
        return Some(wrapped.to_string().into_bytes());
    }
    None
}

/// `event`, with its `output` filled in from the items the stream gave
/// (`patchCodexCompletedOutput`).
fn patched(items: &OutputItems, event: &[u8]) -> Vec<u8> {
    let mut value = parse(event);
    if items.patch(&mut value) {
        value.to_string().into_bytes()
    } else {
        event.to_vec()
    }
}

/// Translates the completed `event` for the client.
fn translate(
    request: &Request,
    prepared: &Prepared,
    event: Vec<u8>,
) -> Result<Vec<u8>, StatusError> {
    let context = ResponseContext {
        model: &request.model,
        original_request: &prepared.original,
        request: &prepared.body,
    };
    Registry::global()
        .translate_non_stream(&Format::CODEX, &prepared.response_format, &context, event)
        .filter(|out| !out.is_empty())
        .ok_or_else(apply_patch_failure)
}

/// Translates Meta's whole reply `data` to the client's format, with `secret`
/// (the credential's token) redacted from the errors it makes
/// (`translateMetaCompleted`).
///
/// The reply is read a line at a time for `data:` lines. An `error` or
/// `response.failed` event is the call's error; `output_item.done` events
/// are kept; the first completed event, with the kept items in its output,
/// is translated. Without one, a plain reply is translated as if it were
/// one. Otherwise the call fails, with a 408.
pub(super) fn translate_completed(
    request: &Request,
    prepared: &mut Prepared,
    secret: &str,
    data: &[u8],
) -> Result<Vec<u8>, StatusError> {
    let mut items = OutputItems::default();
    for line in data.split(|&byte| byte == b'\n') {
        let Some(rest) = line.strip_prefix(b"data:") else {
            continue;
        };
        let event_data = trim_space(rest);
        if let Some(error) = stream_event_error(&parse(event_data), event_data) {
            return Err(error.redacted(secret));
        }
        let (events, error) = prepared.apply_patch.transform(event_data);
        if error.is_some() {
            return Err(apply_patch_failure());
        }
        for event in events {
            match str_at(&parse(&event), "type").as_str() {
                "response.output_item.done" => items.collect(&parse(&event)),
                "response.completed" | "response.incomplete" => {
                    let completed = patched(&items, &event);
                    return translate(request, prepared, completed);
                }
                _ => {}
            }
        }
    }

    if let Some(event) = as_completed_event(data) {
        let completed = patched(&items, &event);
        let completed = prepared
            .apply_patch
            .bridge
            .transform_non_stream(&completed)
            .map_err(|_| apply_patch_failure())?;
        return translate(request, prepared, completed);
    }

    prepared
        .apply_patch
        .finish()
        .map_err(|_| apply_patch_failure())?;
    Err(StatusError::new(408, DISCONNECTED_MESSAGE))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use open_ferry_core::exec::Options;
    use serde_json::json;

    use super::*;
    use crate::meta::request::prepare;

    /// A call from a Codex client, whose answer needs no translation.
    fn prepared(payload: &Value) -> (Request, Prepared) {
        let request = Request {
            model: "muse-spark-1.3".to_owned(),
            payload: Bytes::from(payload.to_string()),
        };
        let options = Options::new(Format::CODEX);
        let prepared = prepare(None, None, &request, &options, true).unwrap();
        (request, prepared)
    }

    fn completed(secret: &str, data: &str) -> Result<Value, StatusError> {
        let (request, mut prepared) = prepared(&json!({"model": "muse-spark-1.3", "input": []}));
        translate_completed(&request, &mut prepared, secret, data.as_bytes())
            .map(|out| serde_json::from_slice(&out).unwrap())
    }

    #[test]
    fn a_plain_reply_is_taken_as_the_completed_event() {
        for (data, want) in [
            (
                r#"{"type":"response.completed","response":{"id":"r"}}"#,
                Some(r#"{"type":"response.completed","response":{"id":"r"}}"#),
            ),
            (
                "  {\"type\":\"response.incomplete\",\"response\":{}}\n",
                Some(r#"{"type":"response.incomplete","response":{}}"#),
            ),
            (
                r#"{"id":"r","object":"response","status":"completed"}"#,
                Some(
                    r#"{"type":"response.completed","response":{"id":"r","object":"response","status":"completed"}}"#,
                ),
            ),
            (
                r#"{"output":[]}"#,
                Some(r#"{"type":"response.completed","response":{"output":[]}}"#),
            ),
            (r#"{"type":"error"}"#, None),
            (r#"{"object":"list"}"#, None),
            (r#"[{"output":[]}]"#, None),
            ("not json", None),
            ("", None),
        ] {
            let got =
                as_completed_event(data.as_bytes()).map(|out| String::from_utf8(out).unwrap());
            assert_eq!(got.as_deref(), want, "{data}");
        }
    }

    #[test]
    fn the_first_completed_event_is_translated() {
        let data = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n",
            "\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
            "\"item\":{\"type\":\"message\",\"id\":\"m\"}}\n",
            "\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}\n",
            "\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"later\"}}\n",
        );
        let out = completed("", data).unwrap();
        assert_eq!(out["response"]["id"], "r");
        // The item the stream gave fills in the empty output.
        assert_eq!(out["response"]["output"][0]["id"], "m");
    }

    #[test]
    fn a_reply_that_is_not_a_stream_is_translated() {
        let out = completed(
            "",
            r#"{"id":"r","object":"response","output":[{"id":"m"}]}"#,
        )
        .unwrap();
        assert_eq!(out["type"], "response.completed");
        assert_eq!(out["response"]["output"][0]["id"], "m");
    }

    #[test]
    fn an_error_event_is_the_error_with_the_token_redacted() {
        let token = "dummy-token-1234567890";
        for (event, status) in [
            (
                r#"{"type":"error","error":{"code":429,"message":"slow"}}"#,
                429,
            ),
            (r#"{"type":"response.failed","error":{"code":"x"}}"#, 502),
        ] {
            let data = format!("data: {event}\n\ndata: {{\"type\":\"response.completed\"}}\n");
            let error = completed(token, &data).unwrap_err();
            assert_eq!(error.status, status);
            assert_eq!(error.message, event);
        }
        let event = format!(r#"{{"type":"error","error":{{"message":"bad {token}"}}}}"#);
        let error = completed(token, &format!("data: {event}\n")).unwrap_err();
        assert!(!error.message.contains(token), "{}", error.message);
        assert!(error.message.contains("bad "));
    }

    #[test]
    fn a_reply_with_no_completed_event_is_a_408() {
        for data in [
            "",
            "data: {\"type\":\"response.created\"}\n",
            "data: [DONE]\n",
            "not a stream",
            ": keepalive\n",
        ] {
            let error = completed("", data).unwrap_err();
            assert_eq!(error.status, 408, "{data}");
            assert_eq!(error.message, DISCONNECTED_MESSAGE);
            assert!(!error.request_scoped);
            assert!(!error.credential_scoped);
        }
    }
}
