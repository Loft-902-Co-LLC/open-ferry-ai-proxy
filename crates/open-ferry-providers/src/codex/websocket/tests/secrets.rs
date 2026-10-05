//! The credential's secret kept out of a connection's failures, the call's
//! error and the logs, and out of Codex's messages, successes and failures
//! alike. None of these is upstream's: upstream passes close reasons,
//! connection errors and messages on, and logs them, as they came.

use std::sync::Arc;

use open_ferry_core::auth::Auth;
use open_ferry_core::executor::ProviderExecutor;

use super::super::mock::{Answer, Server};
use super::{
    HELLO, auth_with, collect, executor, options, refused, request, with_header, within, ws_options,
};
use crate::redact::REDACTED;
use crate::secret_echo::{
    KEY_HEADER, Logs, NEW_KEY, OLD_KEY, assert_old_key_redacted, echoing_the_handshake,
};

/// The credential's secret, as Codex or a proxy might quote it.
const TOKEN: &str = "sk-review-fake-token";

// Not upstream's: a close reason that quotes the token is redacted in the
// call's error and in the log of the disconnect.
#[tokio::test]
async fn a_close_reason_has_the_token_redacted() {
    let (logs, _capturing) = Logs::capture();
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            if peer.recv().await.is_some() {
                peer.close(1008, &format!("invalid token: {TOKEN}")).await;
                peer.hold().await;
            }
        })
    })
    .await;
    let response = executor()
        .execute_stream(
            Arc::new(auth_with(&server.url, &[("api_key", TOKEN)])),
            request("gpt-5-codex", HELLO),
            ws_options("closing-with-token"),
        )
        .await
        .unwrap();
    let (_, error) = collect(response).await;
    let error = error.expect("the stream ended without an error");
    assert_eq!(
        error.message,
        format!("websocket: close 1008 (policy violation): invalid token: {REDACTED}")
    );
    server.wait_closed(1).await;
    let logs = logs.text();
    assert!(
        logs.contains("codex websockets: upstream disconnected"),
        "{logs}"
    );
    assert!(logs.contains(REDACTED), "{logs}");
    assert!(!logs.contains(TOKEN), "{logs}");
}

// Not upstream's: a refused handshake whose body quotes the token fails the
// call with the token redacted, and logs nothing of it.
#[tokio::test]
async fn a_refused_handshake_has_the_token_redacted() {
    let (logs, _capturing) = Logs::capture();
    let server = Server::refusing(401, &format!(r#"{{"error":"invalid token {TOKEN}"}}"#)).await;
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(auth_with(&server.url, &[("api_key", TOKEN)])),
                request("gpt-5-codex", HELLO),
                ws_options(""),
            ),
        )
        .await,
    );
    assert!(error.message.contains(REDACTED), "{error:?}");
    assert!(!error.message.contains(TOKEN), "{error:?}");
    assert!(!logs.text().contains(TOKEN), "{}", logs.text());
}

// Not upstream's: a proxy that refuses the `CONNECT` with a reason quoting
// the token fails the call with the token redacted.
#[tokio::test]
async fn a_refused_connect_has_the_token_redacted() {
    let (logs, _capturing) = Logs::capture();
    let proxy = Server::start(|_| {
        Answer::Raw(format!("HTTP/1.1 407 invalid token {TOKEN}\r\n\r\n").into_bytes())
    })
    .await;
    let auth = Auth {
        proxy_url: proxy.url.clone(),
        ..auth_with("http://127.0.0.1:9", &[("api_key", TOKEN)])
    };
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(auth),
                request("gpt-5-codex", HELLO),
                ws_options(""),
            ),
        )
        .await,
    );
    assert_eq!(
        error.message,
        format!("codex websockets executor: proxy CONNECT failed: 407 invalid token {REDACTED}")
    );
    assert_eq!(proxy.record().handshakes.len(), 1);
    assert!(!logs.text().contains(TOKEN), "{}", logs.text());
}

/// The client's key, which the credential forwards upstream.
const FORWARDED_KEY: &str = "forwarded-key-0123456789";
/// The client's cookie, which the credential forwards upstream.
const COOKIE: &str = "cookie-secret-0123456789";
/// The proxy's password.
const PROXY_SECRET: &str = "proxy-secret-0123456789";

/// A credential for `base_url` forwarding the client's key and cookie.
fn forwarding(base_url: &str) -> Auth {
    auth_with(
        base_url,
        &[
            ("api_key", TOKEN),
            ("header:X-Upstream-Key", "$X-Client-Key"),
            ("header:Cookie", "$Cookie"),
        ],
    )
}

/// Options of a client on the WebSocket that sends its key and cookie.
fn client_options() -> open_ferry_core::exec::Options {
    with_header(
        with_header(ws_options(""), "x-client-key", FORWARDED_KEY),
        "cookie",
        &format!("sid={COOKIE}"),
    )
}

// Not upstream's: a refused handshake whose body quotes the headers the
// custom ones set, the forwarded cookie among them, fails the call with
// them redacted.
#[tokio::test]
async fn a_refused_handshake_has_every_secret_sent_redacted() {
    let server = Server::refusing(
        401,
        &format!(r#"{{"error":"invalid {FORWARDED_KEY} for {COOKIE} with {TOKEN}"}}"#),
    )
    .await;
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(forwarding(&server.url)),
                request("gpt-5-codex", HELLO),
                client_options(),
            ),
        )
        .await,
    );
    let handshake = server.record().handshakes[0].clone();
    assert_eq!(handshake.header("x-upstream-key"), Some(FORWARDED_KEY));
    assert_eq!(
        handshake.header("cookie"),
        Some(format!("sid={COOKIE}").as_str())
    );
    for secret in [FORWARDED_KEY, COOKIE, TOKEN] {
        assert!(!error.message.contains(secret), "{error:?}");
    }
    assert!(
        error.message.contains(&format!(
            "invalid {REDACTED} for {REDACTED} with {REDACTED}"
        )),
        "{error:?}"
    );
}

// Not upstream's: a proxy that refuses the `CONNECT` with a reason quoting
// its password and the `Proxy-Authorization` it was sent fails the call
// with both redacted.
#[tokio::test]
async fn a_refused_connect_has_the_proxys_password_redacted() {
    let credential = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("user:{PROXY_SECRET}"),
    );
    let reason = format!(
        "HTTP/1.1 407 bad {PROXY_SECRET} {credential}

"
    );
    let proxy = Server::start(move |_| Answer::Raw(reason.clone().into_bytes())).await;
    let auth = Auth {
        proxy_url: proxy
            .url
            .replacen("http://", &format!("http://user:{PROXY_SECRET}@"), 1),
        ..forwarding("http://127.0.0.1:9")
    };
    let error = refused(
        within(
            "the call",
            executor().execute_stream(
                Arc::new(auth),
                request("gpt-5-codex", HELLO),
                client_options(),
            ),
        )
        .await,
    );
    let handshake = proxy.record().handshakes[0].clone();
    assert_eq!(
        handshake.header("proxy-authorization"),
        Some(format!("Basic {credential}").as_str())
    );
    assert_eq!(
        error.message,
        format!("codex websockets executor: proxy CONNECT failed: 407 bad {REDACTED} {REDACTED}")
    );
}

/// Codex's messages for an answer in which the model says the token, the
/// client's key and its cookie: a delta and the completed response.
fn turn_saying_the_secrets() -> [String; 2] {
    let said = format!("token {TOKEN} key {FORWARDED_KEY} cookie {COOKIE}");
    [
        format!(
            r#"{{"type":"response.output_text.delta","item_id":"msg-1","output_index":0,"content_index":0,"delta":"{said}"}}"#
        ),
        format!(
            r#"{{"type":"response.completed","response":{{"id":"resp-1","status":"completed","output":[{{"type":"message","id":"msg-1","role":"assistant","content":[{{"type":"output_text","text":"{said}"}}]}}],"usage":{{"input_tokens":0,"output_tokens":0,"total_tokens":0}}}}}}"#
        ),
    ]
}

/// Checks `text` has none of the secrets the client sent, and has them
/// redacted.
fn assert_redacted(what: &str, text: &str) {
    for secret in [TOKEN, FORWARDED_KEY, COOKIE] {
        assert!(!text.contains(secret), "{what}: {secret} in {text}");
    }
    assert!(
        text.contains(&format!(
            "token {REDACTED} key {REDACTED} cookie {REDACTED}"
        )),
        "{what}: {text}"
    );
}

// Not upstream's: a streaming call's messages, in which the model says the
// token and the client's key and cookie that the credential forwards, reach
// the client with them redacted, while the call's taps read each message as
// it came.
#[tokio::test]
async fn a_successful_stream_has_every_secret_sent_redacted() {
    let frames = turn_saying_the_secrets();
    let server = Server::once(&[frames[0].as_str(), frames[1].as_str()]).await;
    let (observation, raw) = crate::secret_echo::Raw::observe();
    let options = open_ferry_core::exec::Options {
        observation: Some(observation),
        ..client_options()
    };
    let response = within(
        "the call",
        executor().execute_stream(
            Arc::new(forwarding(&server.url)),
            request("gpt-5-codex", HELLO),
            options,
        ),
    )
    .await
    .unwrap();
    let (chunks, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(chunks.len(), 2, "{chunks:?}");
    for chunk in &chunks {
        assert_redacted("a chunk", chunk);
    }
    assert!(raw.seen().contains(TOKEN), "{}", raw.seen());
}

// Not upstream's: the same for a call that isn't streamed, whose completed
// response is translated from the messages redacted.
#[tokio::test]
async fn a_successful_call_has_every_secret_sent_redacted() {
    let frames = turn_saying_the_secrets();
    let server = Server::once(&[frames[0].as_str(), frames[1].as_str()]).await;
    let (observation, raw) = crate::secret_echo::Raw::observe();
    let options = with_header(
        with_header(options("openai-response"), "x-client-key", FORWARDED_KEY),
        "cookie",
        &format!("sid={COOKIE}"),
    );
    let options = open_ferry_core::exec::Options {
        observation: Some(observation),
        ..options
    };
    let response = within(
        "the call",
        super::super::execute(
            &executor(),
            &forwarding(&server.url),
            &request("gpt-5-codex", HELLO),
            &options,
        ),
    )
    .await
    .unwrap();
    assert_redacted("the answer", &String::from_utf8_lossy(&response.payload));
    assert!(raw.seen().contains(TOKEN), "{}", raw.seen());
}

// Not upstream's: a `response.failed` and an error event that quote the
// token fail the call, streamed or not, with it redacted from the error.
#[tokio::test]
async fn failure_events_have_the_token_redacted() {
    let failed = format!(
        r#"{{"type":"response.failed","response":{{"id":"resp-1","status":"failed","error":{{"code":"invalid_api_key","message":"Incorrect API key provided: {TOKEN}"}}}}}}"#
    );
    let error_event = format!(
        r#"{{"type":"error","status":401,"error":{{"code":"invalid_api_key","message":"Incorrect API key provided: {TOKEN}"}}}}"#
    );
    for (name, frame) in [
        ("a failed response", failed),
        ("an error event", error_event),
    ] {
        let server = Server::once(&[frame.as_str()]).await;
        let auth = auth_with(&server.url, &[("api_key", TOKEN)]);
        let response = within(
            "the stream",
            executor().execute_stream(
                Arc::new(auth.clone()),
                request("gpt-5-codex", HELLO),
                ws_options(""),
            ),
        )
        .await
        .unwrap();
        let (chunks, error) = collect(response).await;
        for chunk in &chunks {
            assert!(!chunk.contains(TOKEN), "{name}: {chunk}");
        }
        let error = error.unwrap_or_else(|| panic!("{name}: the stream ended without an error"));
        assert!(!error.message.contains(TOKEN), "{name}: {error:?}");
        assert!(error.message.contains(REDACTED), "{name}: {error:?}");

        let server = Server::once(&[frame.as_str()]).await;
        let auth = auth_with(&server.url, &[("api_key", TOKEN)]);
        let error = refused(
            within(
                "the call",
                super::super::execute(
                    &executor(),
                    &auth,
                    &request("gpt-5-codex", HELLO),
                    &options("openai-response"),
                ),
            )
            .await,
        );
        assert!(!error.message.contains(TOKEN), "{name}: {error:?}");
        assert!(error.message.contains(REDACTED), "{name}: {error:?}");
    }
}

// Not upstream's: a turn that keeps the session's connection, though the
// credential's custom key header changed since it opened (the target is
// the credential, URL, proxy and token, as upstream's), has the key the
// connection's handshake sent redacted from what Codex quotes back, as
// well as its own.
#[tokio::test]
async fn a_kept_connection_has_its_handshakes_secrets_redacted() {
    let server = echoing_the_handshake().await;
    let executor = executor();
    for (turn, key) in [("turn one", OLD_KEY), ("turn two", NEW_KEY)] {
        let auth = auth_with(&server.url, &[("api_key", TOKEN), (KEY_HEADER, key)]);
        let response = within(
            turn,
            executor.execute_stream(
                Arc::new(auth),
                request("gpt-5-codex", HELLO),
                ws_options("kept-connection"),
            ),
        )
        .await
        .unwrap();
        let (chunks, error) = collect(response).await;
        assert!(error.is_none(), "{turn}: {error:?}");
        assert_eq!(chunks.len(), 2, "{turn}: {chunks:?}");
        assert_old_key_redacted(turn, &chunks);
    }
    let handshakes = server.record().handshakes;
    assert_eq!(handshakes.len(), 1);
    assert_eq!(handshakes[0].header("x-upstream-key"), Some(OLD_KEY));
}

// Not upstream's: the connection's log lines hide a key in the base URL's
// query, however short, from the URL they name.
#[tokio::test]
async fn the_connection_logs_hide_a_short_url_key() {
    let (logs, _capturing) = Logs::capture();
    let server = Server::once(&[super::COMPLETED]).await;
    let base_url = format!("{}/v1?api_key=zq7x", server.url);
    let response = within(
        "the call",
        executor().execute_stream(
            Arc::new(auth_with(&base_url, &[])),
            request("gpt-5-codex", HELLO),
            ws_options("logged"),
        ),
    )
    .await
    .unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(server.record().handshakes.len(), 1);
    let logs = logs.text();
    assert!(logs.contains("upstream connected"), "{logs}");
    assert!(logs.contains(&format!("api_key={REDACTED}")), "{logs}");
    assert!(!logs.contains("zq7x"), "{logs}");
}
