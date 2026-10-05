//! The credential's secret kept out of what the client is given: xAI's
//! messages, successes and failures alike, its error events and a refused
//! handshake's body, and out of the log lines. None of these is
//! upstream's: upstream passes them on, and logs them, as they came.

use std::sync::Arc;

use super::{COMPLETED, Steps, auth_as, call, executor, holding, refused, ws_options};
use crate::codex::websocket::mock::Server;
use crate::redact::REDACTED;
use crate::secret_echo::{
    KEY_HEADER, Logs, NEW_KEY, OLD_KEY, assert_old_key_redacted, echoing_the_handshake,
};

/// The credential's secret, as xAI might quote it back.
const KEY: &str = "xai-review-fake-key-0123";

const HELLO: &str = super::HELLO;

/// Checks no chunk has the key, and some have it redacted.
fn assert_redacted(chunks: &[String]) {
    for chunk in chunks {
        assert!(!chunk.contains(KEY), "the key reached the client: {chunk}");
    }
    assert!(
        chunks.iter().any(|chunk| chunk.contains(REDACTED)),
        "{chunks:?}"
    );
}

// Not upstream's: a successful turn whose events quote the key reaches the
// client with it redacted, while the call's taps see each message as it
// came.
#[tokio::test]
async fn a_successful_turn_has_the_key_redacted() {
    let delta = format!(r#"{{"type":"response.output_text.delta","delta":"your key is {KEY}"}}"#);
    let completed = format!(
        r#"{{"type":"response.completed","response":{{"id":"resp-1","output":[{{"type":"message","id":"msg-1","role":"assistant","content":[{{"type":"output_text","text":"your key is {KEY}"}}]}}],"usage":{{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}}}}"#
    );
    let server = Arc::new(Server::once(&[delta.as_str(), completed.as_str()]).await);
    let steps = Steps::new(&server);
    let (chunks, error) = call(
        &executor(),
        &auth_as("xai-secret", KEY, &server.url),
        HELLO,
        steps.options("secret-success"),
    )
    .await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(chunks.len(), 2, "{chunks:?}");
    assert_redacted(&chunks);
    assert!(
        chunks[0].contains(&format!("your key is {REDACTED}")),
        "{chunks:?}"
    );
    assert_eq!(steps.chunks(), [delta, completed]);
}

// Not upstream's: a failed response quoting the key reaches the client
// with it redacted, and so does the error that ends the call.
#[tokio::test]
async fn a_failed_response_has_the_key_redacted() {
    let failed = format!(
        r#"{{"type":"response.failed","response":{{"id":"resp-1","status":"failed","error":{{"code":"invalid_api_key","message":"Incorrect API key provided: {KEY}"}}}}}}"#
    );
    let server = Server::once(&[failed.as_str()]).await;
    let (chunks, error) = call(
        &executor(),
        &auth_as("xai-secret", KEY, &server.url),
        HELLO,
        ws_options(""),
    )
    .await;
    assert_redacted(&chunks);
    assert!(
        chunks[0].contains(r#""type":"response.failed""#),
        "{chunks:?}"
    );
    let error = error.expect("the stream ended without an error");
    assert!(!error.message.contains(KEY), "{error:?}");
}

// Not upstream's: an error event quoting the key is the call's error, with
// the key redacted.
#[tokio::test]
async fn an_error_event_has_the_key_redacted() {
    let event = format!(
        r#"{{"type":"error","status":401,"error":{{"code":"invalid_api_key","message":"Incorrect API key provided: {KEY}"}}}}"#
    );
    let server = holding(&[event.as_str()]).await;
    let (chunks, error) = call(
        &executor(),
        &auth_as("xai-secret", KEY, &server.url),
        HELLO,
        ws_options(""),
    )
    .await;
    assert!(chunks.is_empty(), "{chunks:?}");
    let error = error.expect("the stream ended without an error");
    assert_eq!(error.status, 401);
    assert!(!error.message.contains(KEY), "{error:?}");
    assert!(error.message.contains(REDACTED), "{error:?}");
}

// Not upstream's: a refused handshake whose body quotes the key fails the
// call with the key redacted.
#[tokio::test]
async fn a_refused_handshake_has_the_key_redacted() {
    let server = Server::refusing(401, &format!("Incorrect API key provided: {KEY}")).await;
    let error = refused(
        &executor(),
        &auth_as("xai-secret", KEY, &server.url),
        HELLO,
        ws_options(""),
    )
    .await;
    assert_eq!(error.status, 401);
    assert_eq!(
        error.message,
        format!("Incorrect API key provided: {REDACTED}")
    );
}

// Not upstream's: a turn that keeps the session's connection, though the
// credential's custom key header changed since it opened (the target is
// the credential, URL and proxy, as upstream's), has the key the
// connection's handshake sent redacted from what xAI quotes back, as well
// as its own.
#[tokio::test]
async fn a_kept_connection_has_its_handshakes_secrets_redacted() {
    let server = echoing_the_handshake().await;
    let executor = executor();
    for (turn, key) in [("turn one", OLD_KEY), ("turn two", NEW_KEY)] {
        let mut auth = (*auth_as("xai-secret", KEY, &server.url)).clone();
        auth.attributes.insert(KEY_HEADER.into(), key.into());
        let (chunks, error) = call(
            &executor,
            &Arc::new(auth),
            HELLO,
            ws_options("kept-connection"),
        )
        .await;
        assert!(error.is_none(), "{turn}: {error:?}");
        assert_eq!(chunks.len(), 2, "{turn}: {chunks:?}");
        assert_old_key_redacted(turn, &chunks);
    }
    let handshakes = server.record().handshakes;
    assert_eq!(handshakes.len(), 1);
    assert_eq!(handshakes[0].header("x-upstream-key"), Some(OLD_KEY));
}

// Not upstream's: the call's log lines hide a key in the base URL's query,
// however short, from the URL they name.
#[tokio::test]
async fn the_log_lines_hide_a_short_url_key() {
    let (logs, _capturing) = Logs::capture();
    let server = Server::once(&[COMPLETED]).await;
    let base_url = format!("{}/v1?api_key=zq7x", server.url);
    let (_, error) = call(
        &executor(),
        &auth_as("xai-secret", KEY, &base_url),
        HELLO,
        ws_options("logged"),
    )
    .await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(server.record().handshakes.len(), 1);
    let logs = logs.text();
    assert!(logs.contains("upstream request sent"), "{logs}");
    assert!(logs.contains("upstream terminal response"), "{logs}");
    assert!(logs.contains(&format!("api_key={REDACTED}")), "{logs}");
    assert!(!logs.contains("zq7x"), "{logs}");
}
