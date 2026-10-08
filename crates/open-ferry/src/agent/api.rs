//! Calls to the running server's management and dashboard APIs, with the
//! TUI's management client, and what their answers mean.

use std::fmt;
use std::sync::Arc;

use axum::http::Method;
use open_ferry_tui::{ManagementClient, Reply};
use serde_json::Value;

use super::Failure;
use super::values::placed;

/// The error codes of answers whose message can quote the config or the
/// body sent: their text is only where the problem is.
const QUOTING: [&str; 4] = [
    "invalid_config",
    "invalid_yaml",
    "invalid_body",
    "invalid_json",
];

/// The running server's APIs, called with the management key.
#[derive(Clone)]
pub(crate) struct Remote {
    client: Arc<ManagementClient>,
    url: String,
}

impl fmt::Debug for Remote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Remote")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// A request body and its content type.
pub(crate) enum Body {
    Json(Value),
    Yaml(Vec<u8>),
}

impl Remote {
    /// The server at the root URL `url`, called with `key`.
    pub(crate) fn new(url: &str, key: &str) -> Self {
        Self {
            client: ManagementClient::new(url, key),
            url: url.to_owned(),
        }
    }

    /// The root URL it calls.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// Sends `method path` with `body`, and gives back the answer, whatever
    /// its status; a failure when the server couldn't be reached.
    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Body>,
    ) -> Result<Reply, Failure> {
        let body = body.map(|body| match body {
            Body::Json(value) => ("application/json", value.to_string().into_bytes()),
            Body::Yaml(data) => ("application/yaml", data),
        });
        self.client.send(method, path, body).await.map_err(|error| {
            Failure::new(
                "failed",
                format!("couldn't reach the server at {}: {error}", self.url),
            )
        })
    }

    /// Sends `method path` with `body`, and gives back the answer's JSON
    /// (`null` for an empty body) when its status is 2xx; else a failure
    /// that says what the server answered.
    pub(crate) async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<Body>,
    ) -> Result<Value, Failure> {
        let reply = self.send(method, path, body).await?;
        if !(200..300).contains(&reply.status) {
            return Err(answer_failure(reply.status, &reply.body));
        }
        if reply.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&reply.body).map_err(|_| {
            Failure::new(
                "failed",
                format!(
                    "the server's answer to {path} isn't JSON (status {})",
                    reply.status
                ),
            )
        })
    }
}

/// What an error answer's body says: its `message` and `error`, or the
/// body's text when it isn't JSON. For a config or a body that doesn't
/// load, the message is cut to where the problem is ([`placed`]), as it can
/// quote a value.
pub(crate) fn error_text(body: &[u8]) -> Option<String> {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let field = |name: &str| {
        parsed
            .as_ref()
            .and_then(|value| value.get(name))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    match (field("message"), field("error")) {
        (Some(message), Some(error)) if QUOTING.contains(&error.as_str()) => {
            Some(placed(&error, &message))
        }
        (Some(message), Some(error)) => Some(format!("{message} ({error})")),
        (Some(text), None) | (None, Some(text)) => Some(text),
        (None, None) if parsed.is_none() => {
            let text = String::from_utf8_lossy(body);
            let text = text.trim();
            (!text.is_empty()).then(|| text.chars().take(300).collect())
        }
        (None, None) => None,
    }
}

/// An error answer's `error` code, when its body is JSON that has one.
fn error_code(body: &[u8]) -> Option<String> {
    let parsed: Value = serde_json::from_slice(body).ok()?;
    parsed.get("error")?.as_str().map(str::to_owned)
}

/// The failure for an answer with `status` and `body` that isn't a 2xx.
pub(crate) fn answer_failure(status: u16, body: &[u8]) -> Failure {
    let what = error_text(body).unwrap_or_else(|| format!("status {status}"));
    match status {
        401 => Failure::new(
            "unauthorized",
            format!("the server refused the management key: {what}"),
        )
        .hint(
            "every refused key counts: after five in thirty minutes the server refuses this address for thirty minutes",
        ),
        403 => Failure::new("unauthorized", format!("the server refused: {what}")),
        404 => Failure::new("not_found", format!("not found: {what}")),
        409 if error_code(body).as_deref() == Some("config_changed") => {
            Failure::new("config_changed", format!("the server refused it: {what}"))
        }
        400 | 409 | 413 | 422 => Failure::new("refused", format!("the server refused it: {what}")),
        503 => Failure::new("unavailable", format!("the server can't do it now: {what}")),
        _ => Failure::new(
            "failed",
            format!("the server answered {status}: {what}"),
        ),
    }
}

/// `segments` as a URL path: each percent-encoded, joined with `/`.
pub(crate) fn path_segments(segments: &[String]) -> String {
    segments
        .iter()
        .map(|segment| encode(segment))
        .collect::<Vec<_>>()
        .join("/")
}

/// `text` percent-encoded for a path segment or a query value: all but
/// the unreserved characters.
pub(crate) fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: an error answer's text, and the code it maps to.
    #[test]
    fn reads_error_answers() {
        assert_eq!(
            error_text(br#"{"error":"invalid_request","message":"bad from"}"#).unwrap(),
            "bad from (invalid_request)"
        );
        assert_eq!(
            error_text(br#"{"error":"not_found"}"#).unwrap(),
            "not_found"
        );
        assert_eq!(error_text(b"plain words").unwrap(), "plain words");
        // Not upstream's: a config error's message is cut to where it is.
        assert_eq!(
            error_text(
                br#"{"error":"invalid_config","message":"yaml: unmarshal errors:\n  line 4: field sk-live-abc not found in type config.Routing (routing.strategy)"}"#
            )
            .unwrap(),
            "invalid_config (line 4, routing.strategy)"
        );
        assert_eq!(
            error_text(br#"{"error":"invalid_yaml","message":"sk-live-abc"}"#).unwrap(),
            "invalid_yaml"
        );
        assert_eq!(error_text(b""), None);
        assert_eq!(answer_failure(401, b"").error, "unauthorized");
        assert_eq!(answer_failure(404, b"").error, "not_found");
        assert_eq!(answer_failure(422, b"").error, "refused");
        assert_eq!(answer_failure(500, b"").error, "failed");
        assert_eq!(
            path_segments(&["a b".to_owned(), "c/d".to_owned(), "x-y.z".to_owned()]),
            "a%20b/c%2Fd/x-y.z"
        );
    }
}
