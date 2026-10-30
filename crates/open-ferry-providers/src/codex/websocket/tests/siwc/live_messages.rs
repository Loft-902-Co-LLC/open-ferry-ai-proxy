// SIWC live-credential regressions for steering and queued client messages.

use super::*;
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

type Chunks = futures_util::stream::BoxStream<'static, Result<Bytes, ExecError>>;

const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp-1","output":[]}}"#;
const STEER: &[u8] =
    br#"{"type":"response.steer","previous_response_id":"resp-1","input":"steer"}"#;
const ACCEPTED: &str =
    r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"resp-1"}}"#;

fn equal_token_auth(base: &str, siwc: bool) -> Auth {
    let mut auth = credential(base);
    auth.attributes.remove("api_key");
    auth.attributes.remove("header:authorization");
    auth.metadata.insert("access_token".into(), json!(TOKEN));
    auth.metadata.insert("use_siwc".into(), json!(siwc));
    assert_eq!(auth.access_token(), TOKEN);
    auth
}

fn steering_executor() -> CodexExecutor {
    let mut config = Config::default();
    config.codex.response_steering = true;
    executor_with(config)
}

fn options(live: &Arc<RwLock<Arc<Auth>>>) -> (mpsc::Sender<InputFrame>, Options) {
    let lookup = live.clone();
    let (tx, rx) = mpsc::channel(8);
    let mut opts = attempted_options("live-messages", false);
    opts.websocket_input = Some(
        WebsocketInput::new(rx)
            .with_auth_check(Arc::new(move |_| Some(lookup.read().unwrap().clone()))),
    );
    (tx, opts)
}

async fn event(chunks: &mut Chunks, expected: &str) {
    let bytes = within("live message event", chunks.next())
        .await
        .unwrap()
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["type"], expected);
}

async fn rejected(chunks: &mut Chunks, clear: bool) {
    let error = within("live message rejected", chunks.next())
        .await
        .unwrap()
        .unwrap_err();
    if clear {
        assert_eq!(error.status, 401);
        assert!(
            !error.request_scoped,
            "missing token must classify as HTTP does"
        );
    } else {
        assert_eq!(error.message, ExecError::replay_required().message);
    }
    assert!(
        within("live message stream ended", chunks.next())
            .await
            .is_none()
    );
}

async fn steer_change(initial_siwc: bool, next_siwc: bool, token: &str, outer_only: bool) {
    let server = Server::start(|_| {
        Answer::accept(|mut peer| async move {
            if peer.recv().await.is_some() {
                peer.send(CREATED).await;
                if peer.recv().await.is_some() {
                    peer.send(ACCEPTED).await;
                }
            }
            peer.hold().await;
        })
    })
    .await;
    let executor = steering_executor();
    let auth = equal_token_auth(&format!("{}/v1", server.url), initial_siwc);
    let live = Arc::new(RwLock::new(Arc::new(auth.clone())));
    let (tx, opts) = options(&live);
    let mut chunks = executor
        .execute_stream(Arc::new(auth), request("gpt-5-codex", BODY), opts)
        .await
        .unwrap()
        .chunks;
    event(&mut chunks, "response.created").await;
    let mut next = (**live.read().unwrap()).clone();
    next.metadata.insert("use_siwc".into(), json!(next_siwc));
    if outer_only {
        next.metadata
            .insert("access_token".into(), json!("ordinary-refreshed"));
    } else {
        next.metadata["siwc"]["access_token"] = json!(token);
    }
    if initial_siwc != next_siwc {
        assert_eq!(
            next.access_token(),
            TOKEN,
            "isolate mode from token comparison"
        );
    }
    *live.write().unwrap() = Arc::new(next);
    tx.send(InputFrame::Payload(Bytes::from_static(STEER)))
        .await
        .unwrap();
    if outer_only {
        event(&mut chunks, "response.steer.accepted").await;
        assert_eq!(server.record().messages.len(), 2);
    } else {
        rejected(&mut chunks, token.is_empty()).await;
        assert_eq!(
            server.record().messages.len(),
            1,
            "changed credential sends no steer"
        );
    }
    assert_eq!(server.record().handshakes.len(), 1);
    drop(tx);
    drop(chunks);
    executor.close_execution_session("live-messages");
}

#[tokio::test]
async fn siwc_live_steer_cleared_token_is_not_request_scoped() {
    steer_change(true, true, "", false).await;
}

#[tokio::test]
async fn siwc_live_steer_equal_token_mode_switches_require_replay() {
    steer_change(true, false, TOKEN, false).await;
    steer_change(false, true, TOKEN, false).await;
}

#[tokio::test]
async fn siwc_live_steer_rotation_requires_replay() {
    steer_change(true, true, "rotated-live-steer-token", false).await;
}

#[tokio::test]
async fn siwc_live_steer_ordinary_outer_refresh_is_sent() {
    steer_change(false, false, TOKEN, true).await;
}

// The second create has reached Codex, but its response hasn't started. A steer
// for that pending parent must be rejected at input, not deferred indefinitely.
#[tokio::test]
async fn siwc_live_steer_checks_before_deferring_a_pending_parent() {
    let received = Arc::new(Notify::new());
    let notice = received.clone();
    let server = Server::start(move |_| {
        let notice = notice.clone();
        Answer::accept(move |mut peer| {
            let notice = notice.clone();
            async move {
                if peer.recv().await.is_some() {
                    peer.send(CREATED).await;
                    if peer.recv().await.is_some() {
                        notice.notify_one();
                    }
                }
                peer.hold().await;
            }
        })
    })
    .await;
    let executor = steering_executor();
    let auth = equal_token_auth(&format!("{}/v1", server.url), true);
    let live = Arc::new(RwLock::new(Arc::new(auth.clone())));
    let (tx, opts) = options(&live);
    let mut chunks = executor
        .execute_stream(Arc::new(auth), request("gpt-5-codex", BODY), opts)
        .await
        .unwrap()
        .chunks;
    event(&mut chunks, "response.created").await;
    tx.send(InputFrame::Payload(Bytes::from_static(
        br#"{"type":"response.create","input":"second"}"#,
    )))
    .await
    .unwrap();
    let reading = tokio::spawn(async move {
        let next = chunks.next().await;
        (next, chunks)
    });
    within("pending create reached upstream", received.notified()).await;
    let mut next = (**live.read().unwrap()).clone();
    next.metadata["siwc"]["access_token"] = json!("");
    *live.write().unwrap() = Arc::new(next);
    tx.send(InputFrame::Payload(Bytes::from_static(
        br#"{"type":"response.steer","previous_response_id":"pending-parent","input":"steer"}"#,
    )))
    .await
    .unwrap();
    let (result, mut chunks) = within("pending steer rejected before deferral", reading)
        .await
        .unwrap();
    let error = result.unwrap().unwrap_err();
    assert_eq!(error.status, 401);
    assert!(!error.request_scoped);
    assert!(chunks.next().await.is_none());
    assert_eq!(server.record().messages.len(), 2);
    executor.close_execution_session("live-messages");
}

// The lookup returns the preparation snapshot, then replaces the live record.
// This deterministically exercises the final check without a timed race or hook
// in production code. For steers, replacement follows the input lookup instead.
#[tokio::test]
async fn siwc_live_final_pre_send_rechecks_create_and_steer() {
    for steer in [false, true] {
        let server = Server::start(move |_| {
            Answer::accept(move |mut peer| async move {
                if peer.recv().await.is_some() {
                    peer.send(CREATED).await;
                    if !steer {
                        peer.send(COMPLETED).await;
                    }
                    if peer.recv().await.is_some() {
                        peer.send(if steer { ACCEPTED } else { CREATED }).await;
                    }
                }
                peer.hold().await;
            })
        })
        .await;
        let executor = steering_executor();
        let auth = equal_token_auth(&format!("{}/v1", server.url), true);
        let live = Arc::new(RwLock::new(Arc::new(auth.clone())));
        let lookup = live.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let checks = calls.clone();
        let (tx, rx) = mpsc::channel(4);
        let mut opts = attempted_options("live-messages", false);
        opts.websocket_input = Some(WebsocketInput::new(rx).with_auth_check(Arc::new(move |_| {
            let snapshot = lookup.read().unwrap().clone();
            if checks.fetch_add(1, Ordering::SeqCst) + 1 == if steer { 1 } else { 2 } {
                let mut next = (*snapshot).clone();
                next.metadata["siwc"]["access_token"] = json!("rotated-before-send");
                *lookup.write().unwrap() = Arc::new(next);
            }
            Some(snapshot)
        })));
        let mut chunks = executor
            .execute_stream(Arc::new(auth), request("gpt-5-codex", BODY), opts)
            .await
            .unwrap()
            .chunks;
        event(&mut chunks, "response.created").await;
        if !steer {
            event(&mut chunks, "response.completed").await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let frame = if steer {
            STEER
        } else {
            br#"{"type":"response.create","input":"second"}"#
        };
        tx.send(InputFrame::Payload(Bytes::from_static(frame)))
            .await
            .unwrap();
        rejected(&mut chunks, false).await;
        assert_eq!(
            server.record().messages.len(),
            1,
            "final check prevents sending stale work"
        );
        executor.close_execution_session("live-messages");
    }
}

#[tokio::test]
async fn siwc_live_queued_create_rechecks_after_rotation() {
    let release = Arc::new(Notify::new());
    let ready = release.clone();
    let server = Server::start(move |_| {
        let ready = ready.clone();
        Answer::accept(move |mut peer| {
            let ready = ready.clone();
            async move {
                if peer.recv().await.is_none() { return; }
                peer.send(CREATED).await;
                if peer.recv().await.is_none() { return; }
                peer.send(ACCEPTED).await;
                peer.send(r#"{"type":"response.incomplete","response":{"id":"resp-1","output":[],"incomplete_details":{"reason":"steered"}}}"#).await;
                peer.send(r#"{"type":"response.created","response":{"id":"auto-1","previous_response_id":"resp-1","output":[]}}"#).await;
                if peer.recv().await.is_none() { return; }
                peer.send(r#"{"type":"response.steer.failed","steer":{"id":"s2","previous_response_id":"auto-1"}}"#).await;
                ready.notified().await;
                peer.send(r#"{"type":"response.completed","response":{"id":"auto-1","output":[]}}"#).await;
                peer.hold().await;
            }
        })
    }).await;
    let executor = steering_executor();
    let auth = equal_token_auth(&format!("{}/v1", server.url), true);
    let live = Arc::new(RwLock::new(Arc::new(auth.clone())));
    let (tx, opts) = options(&live);
    let mut chunks = executor
        .execute_stream(Arc::new(auth), request("gpt-5-codex", BODY), opts)
        .await
        .unwrap()
        .chunks;
    event(&mut chunks, "response.created").await;
    tx.send(InputFrame::Payload(Bytes::from_static(STEER)))
        .await
        .unwrap();
    event(&mut chunks, "response.steer.accepted").await;
    event(&mut chunks, "response.incomplete").await;
    event(&mut chunks, "response.created").await;
    tx.send(InputFrame::Payload(Bytes::from_static(
        br#"{"type":"response.create","input":"queued","previous_response_id":"auto-1"}"#,
    )))
    .await
    .unwrap();
    tx.send(InputFrame::Payload(Bytes::from_static(
        br#"{"type":"response.steer","previous_response_id":"auto-1","input":"barrier"}"#,
    )))
    .await
    .unwrap();
    event(&mut chunks, "response.steer.failed").await;
    let mut next = (**live.read().unwrap()).clone();
    next.metadata["siwc"]["access_token"] = json!("rotated-while-queued");
    *live.write().unwrap() = Arc::new(next);
    release.notify_one();
    event(&mut chunks, "response.completed").await;
    rejected(&mut chunks, false).await;
    assert_eq!(
        server.record().messages.len(),
        3,
        "dequeued create is never sent"
    );
    executor.close_execution_session("live-messages");
}

#[tokio::test]
async fn r2_native_duplex_turns_ignore_persisted_mode_changes_and_nested_rotation() {
    for kind in ["response.create", "response.append", "response.steer"] {
        let server = Server::start(|_| {
            Answer::accept(|mut peer| async move {
                if peer.recv().await.is_none() {
                    return;
                }
                peer.send(CREATED).await;
                if peer.recv().await.is_some() {
                    peer.send(ACCEPTED).await;
                }
                peer.hold().await;
            })
        })
        .await;
        let executor = steering_executor();
        let auth = equal_token_auth(&format!("{}/v1", server.url), true);
        let live = Arc::new(RwLock::new(Arc::new(auth.clone())));
        let (tx, mut opts) = options(&live);
        opts.headers
            .insert("user-agent", HeaderValue::from_static("codex-rs/1"));
        let mut chunks = executor
            .execute_stream(Arc::new(auth), request("gpt-5-codex", BODY), opts)
            .await
            .unwrap()
            .chunks;
        event(&mut chunks, "response.created").await;
        let mut next = (**live.read().unwrap()).clone();
        next.metadata.insert("use_siwc".into(), json!(false));
        next.metadata["siwc"]["access_token"] = json!("rotated-unused-nested");
        *live.write().unwrap() = Arc::new(next);
        tx.send(InputFrame::Payload(Bytes::from(format!(
            r#"{{"type":"{kind}","previous_response_id":"resp-1","input":[]}}"#
        ))))
        .await
        .unwrap();
        event(&mut chunks, "response.steer.accepted").await;
        assert_eq!(server.record().messages.len(), 2);
        drop(tx);
        drop(chunks);
        executor.close_execution_session("live-messages");
    }
}
