// Local-router acceptance cases for upstream f73e66ac.
// These real-router cases assert exact correlation and recovery. Core/provider
// tests cover the handler-dispatch and writer-lock epoch gaps and failed writes.

use super::*;

const MODEL: &str = "round1-invariant-model";

fn create(event_id: &str) -> String {
    json!({"type":"response.create", "model":MODEL, "input":[], "event_id":event_id}).to_string()
}

async fn expect_error(ws: &mut Client, code: &str, event_id: &str) {
    let event = recv(ws).await;
    assert_eq!(event["type"], "error", "{ROUND1_INVARIANT}: {event}");
    assert_eq!(event["error"]["code"], code, "{ROUND1_INVARIANT}: {event}");
    assert_eq!(
        event["error"]["event_id"].as_str().unwrap_or_default(),
        event_id,
        "{ROUND1_INVARIANT}: {event}"
    );
    assert_eq!(
        event["status"],
        if code == "previous_response_not_found" {
            409
        } else {
            502
        },
        "{ROUND1_INVARIANT}: {event}"
    );
}

async fn expect_completion(ws: &mut Client, id: &str) {
    let created = recv(ws).await;
    assert_eq!(
        created["type"], "response.created",
        "{ROUND1_INVARIANT}: {created}"
    );
    assert_eq!(
        created["response"]["id"], id,
        "{ROUND1_INVARIANT}: {created}"
    );
    let completed = recv(ws).await;
    assert_eq!(
        completed["type"], "response.completed",
        "{ROUND1_INVARIANT}: {completed}"
    );
    assert_eq!(
        completed["response"]["id"], id,
        "{ROUND1_INVARIANT}: {completed}"
    );
}

// A second credential makes an unwanted retry or affinity switch observable.
async fn unused_sibling() -> Upstream {
    Upstream::start(|mut peer| async move {
        if peer.read().await.is_some() {
            peer.write(r#"{"type":"response.created","response":{"id":"wrong-credential"}}"#)
                .await;
            peer.write(
                r#"{"type":"response.completed","response":{"id":"wrong-credential","output":[]}}"#,
            )
            .await;
        }
    })
    .await
}

async fn active_loss(duplex: bool, accepted_output: bool) {
    let count = Arc::new(AtomicUsize::new(0));
    let mut upstream = Upstream::start(move |mut peer| {
        let index = count.fetch_add(1, Ordering::SeqCst);
        async move {
            peer.read().await.unwrap();
            if index == 0 {
                if accepted_output {
                    peer.write(r#"{"type":"response.created","response":{"id":"r1"}}"#)
                        .await;
                    peer.write(r#"{"type":"response.output_text.delta","delta":"accepted-once"}"#)
                        .await;
                }
                return; // Drop TCP without a close frame: outcome unknown, not a business rejection.
            }
            peer.write(r#"{"type":"response.created","response":{"id":"recovered"}}"#)
                .await;
            peer.write(
                r#"{"type":"response.completed","response":{"id":"recovered","output":[]}}"#,
            )
            .await;
            peer.read().await;
        }
    })
    .await;
    let sibling = unused_sibling().await;
    let url = retry_proxy(&steering(duplex), &upstream.url, &sibling.url).await;
    let mut ws = connect(&url, &[]).await;
    send(&mut ws, &create("c1")).await;
    if accepted_output {
        assert_eq!(
            recv(&mut ws).await["type"],
            "response.created",
            "{ROUND1_INVARIANT}"
        );
        let delta = recv(&mut ws).await;
        assert_eq!(
            delta["type"], "response.output_text.delta",
            "{ROUND1_INVARIANT}: {delta}"
        );
        assert_eq!(delta["delta"], "accepted-once", "{ROUND1_INVARIANT}");
    }
    expect_error(&mut ws, "upstream_websocket_lost", "c1").await;
    assert_eq!(upstream.connections(), 1, "{ROUND1_INVARIANT}");
    assert_eq!(upstream.frames(), 1, "{ROUND1_INVARIANT}");
    assert_eq!(sibling.connections(), 0, "{ROUND1_INVARIANT}");
    // Refusing this continuation is not another loss, and must not fence c3.
    send(
        &mut ws,
        &json!({"type":"response.create", "model":MODEL, "input":[],
        "previous_response_id":"r1", "event_id":"c2"})
        .to_string(),
    )
    .await;
    send(&mut ws, &create("c3")).await;
    expect_error(&mut ws, "previous_response_not_found", "c2").await;
    expect_completion(&mut ws, "recovered").await;
    assert_eq!(upstream.connections(), 2, "{ROUND1_INVARIANT}");
    assert_eq!(upstream.frames(), 2, "{ROUND1_INVARIANT}");
    assert_eq!(sibling.connections(), 0, "{ROUND1_INVARIANT}");
    ws.close(None).await.unwrap();
    upstream.finished().await;
    upstream.finished().await;
}

#[tokio::test]
async fn round1_loss_before_created_standard() {
    active_loss(false, false).await;
}

#[tokio::test]
async fn round1_loss_before_created_duplex() {
    active_loss(true, false).await;
}

#[tokio::test]
async fn round1_loss_after_accepted_output_standard() {
    active_loss(false, true).await;
}

#[tokio::test]
async fn round1_loss_after_accepted_output_duplex() {
    active_loss(true, true).await;
}

async fn idle_loss(duplex: bool) {
    let mut upstream = Upstream::start(|mut peer| async move {
        peer.read().await.unwrap();
        peer.write(r#"{"type":"response.created","response":{"id":"r1"}}"#)
            .await;
        peer.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
            .await;
        // Every connection drops after its completed turn. Idle EOF must have no loss frame.
    })
    .await;
    let sibling = unused_sibling().await;
    let url = retry_proxy(&steering(duplex), &upstream.url, &sibling.url).await;
    let mut ws = connect(&url, &[]).await;
    send(&mut ws, &create("c1")).await;
    expect_completion(&mut ws, "r1").await;
    upstream.finished().await;
    send(
        &mut ws,
        &json!({"type":"response.create", "model":MODEL, "input":[],
        "previous_response_id":"r1", "event_id":"c2"})
        .to_string(),
    )
    .await;
    expect_error(&mut ws, "previous_response_not_found", "c2").await;
    send(&mut ws, &create("c3")).await;
    expect_completion(&mut ws, "r1").await;
    assert_eq!(upstream.connections(), 2, "{ROUND1_INVARIANT}");
    assert_eq!(upstream.frames(), 2, "{ROUND1_INVARIANT}");
    assert_eq!(sibling.connections(), 0, "{ROUND1_INVARIANT}");
    let _ = ws.close(None).await;
    upstream.finished().await;
}

#[tokio::test]
async fn round1_loss_idle_eof_standard() {
    idle_loss(false).await;
}

#[tokio::test]
async fn round1_loss_idle_eof_duplex() {
    idle_loss(true).await;
}

async fn operation_loss(append: bool, accepted_steer: bool, anonymous: bool) {
    let count = Arc::new(AtomicUsize::new(0));
    let mut upstream = Upstream::start(move |mut peer| {
        let index = count.fetch_add(1, Ordering::SeqCst);
        async move {
            peer.read().await.unwrap();
            if index == 0 {
                peer.write(r#"{"type":"response.created","response":{"id":"r1"}}"#).await;
                let operation = peer.read().await.unwrap();
                assert_eq!(at(&operation, "/event_id"), if anonymous { "" } else { "op1" });
                if accepted_steer {
                    peer.write(r#"{"type":"response.steer.accepted","steer":{"id":"st1","previous_response_id":"r1"}}"#).await;
                }
                return;
            }
            peer.write(r#"{"type":"response.created","response":{"id":"recovered"}}"#).await;
            peer.write(r#"{"type":"response.completed","response":{"id":"recovered","output":[]}}"#).await;
            peer.read().await;
        }
    }).await;
    let sibling = unused_sibling().await;
    let url = retry_proxy(&steering(true), &upstream.url, &sibling.url).await;
    let mut ws = connect(&url, &[]).await;
    send(&mut ws, &create("c1")).await;
    assert_eq!(
        recv(&mut ws).await["type"],
        "response.created",
        "{ROUND1_INVARIANT}"
    );
    let kind = if append {
        "response.append"
    } else {
        "response.steer"
    };
    send(
        &mut ws,
        &json!({"type":kind, "previous_response_id":"r1",
        "input":[], "event_id":if anonymous { "" } else { "op1" }})
        .to_string(),
    )
    .await;
    if accepted_steer {
        assert_eq!(
            recv(&mut ws).await["type"],
            "response.steer.accepted",
            "{ROUND1_INVARIANT}"
        );
    }
    expect_error(&mut ws, "upstream_websocket_lost", "c1").await;
    if !anonymous {
        expect_error(&mut ws, "upstream_websocket_lost", "op1").await;
    }
    send(&mut ws, &create("c3")).await;
    expect_completion(&mut ws, "recovered").await; // Any duplicate loss frame fails here.
    assert_eq!(upstream.connections(), 2, "{ROUND1_INVARIANT}");
    assert_eq!(upstream.frames(), 3, "{ROUND1_INVARIANT}");
    assert_eq!(sibling.connections(), 0, "{ROUND1_INVARIANT}");
    ws.close(None).await.unwrap();
    upstream.finished().await;
    upstream.finished().await;
}

#[tokio::test]
async fn round1_loss_unacknowledged_steer() {
    operation_loss(false, false, false).await;
}

#[tokio::test]
async fn round1_loss_accepted_steer() {
    operation_loss(false, true, false).await;
}

#[tokio::test]
async fn round1_loss_append() {
    operation_loss(true, false, false).await;
}

async fn queued_loss(continuation: bool) {
    let count = Arc::new(AtomicUsize::new(0));
    let mut upstream = Upstream::start(move |mut peer| {
        let index = count.fetch_add(1, Ordering::SeqCst);
        async move {
            peer.read().await.unwrap();
            if index == 0 {
                peer.write(r#"{"type":"response.created","response":{"id":"r1"}}"#)
                    .await;
                // The interrupt bypasses the bounded business queue. Seeing it proves
                // the reader has already queued c2, without a sleep or plugin hook.
                let barrier = peer.read().await.unwrap();
                assert_eq!(at(&barrier, "/type"), "response.interrupt");
                return;
            }
            peer.write(r#"{"type":"response.created","response":{"id":"recovered"}}"#)
                .await;
            peer.write(
                r#"{"type":"response.completed","response":{"id":"recovered","output":[]}}"#,
            )
            .await;
            peer.read().await;
        }
    })
    .await;
    let sibling = unused_sibling().await;
    let url = retry_proxy(&steering(false), &upstream.url, &sibling.url).await;
    let mut ws = connect(&url, &[]).await;
    send(&mut ws, &create("c1")).await;
    assert_eq!(
        recv(&mut ws).await["type"],
        "response.created",
        "{ROUND1_INVARIANT}"
    );
    let mut queued = json!({"type":"response.create", "model":MODEL, "input":[], "event_id":"c2"});
    if continuation {
        queued["previous_response_id"] = Value::from("r1");
    }
    send(&mut ws, &queued.to_string()).await;
    send(
        &mut ws,
        r#"{"type":"response.interrupt","response_id":"r1"}"#,
    )
    .await;
    expect_error(&mut ws, "upstream_websocket_lost", "c1").await;
    let code = if continuation {
        "previous_response_not_found"
    } else {
        "upstream_websocket_not_sent"
    };
    expect_error(&mut ws, code, "c2").await;
    assert_eq!(upstream.connections(), 1, "{ROUND1_INVARIANT}");
    assert_eq!(upstream.frames(), 2, "{ROUND1_INVARIANT}"); // c1 and the interrupt, never c2.
    send(&mut ws, &create("c3")).await;
    expect_completion(&mut ws, "recovered").await;
    assert_eq!(upstream.connections(), 2, "{ROUND1_INVARIANT}");
    assert_eq!(upstream.frames(), 3, "{ROUND1_INVARIANT}");
    assert_eq!(sibling.connections(), 0, "{ROUND1_INVARIANT}");
    ws.close(None).await.unwrap();
    upstream.finished().await;
    upstream.finished().await;
}

#[tokio::test]
async fn round1_loss_queued_full_create_not_sent() {
    queued_loss(false).await;
}

#[tokio::test]
async fn round1_loss_queued_continuation_requires_full_context() {
    queued_loss(true).await;
}

async fn between_turns_loss(with_id: bool) {
    let count = Arc::new(AtomicUsize::new(0));
    let mut upstream = Upstream::start(move |mut peer| {
        let index = count.fetch_add(1, Ordering::SeqCst);
        async move {
            peer.read().await.unwrap();
            if index == 0 {
                peer.write(r#"{"type":"response.created","response":{"id":"r1"}}"#)
                    .await;
                peer.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
                    .await;
                peer.read().await.unwrap(); // c2 is sent, but never gets response.created.
                return;
            }
            peer.write(r#"{"type":"response.created","response":{"id":"recovered"}}"#)
                .await;
            peer.write(
                r#"{"type":"response.completed","response":{"id":"recovered","output":[]}}"#,
            )
            .await;
            peer.read().await;
        }
    })
    .await;
    let sibling = unused_sibling().await;
    let url = retry_proxy(&steering(true), &upstream.url, &sibling.url).await;
    let mut ws = connect(&url, &[]).await;
    send(&mut ws, &create("c1")).await;
    expect_completion(&mut ws, "r1").await;
    let second = if with_id {
        create("c2")
    } else {
        json!({"type":"response.create", "model":MODEL, "input":[]}).to_string()
    };
    send(&mut ws, &second).await;
    expect_error(
        &mut ws,
        "upstream_websocket_lost",
        if with_id { "c2" } else { "" },
    )
    .await;
    send(&mut ws, &create("c3")).await;
    expect_completion(&mut ws, "recovered").await; // No unscoped duplicate or inherited c1 error.
    assert_eq!(upstream.connections(), 2, "{ROUND1_INVARIANT}");
    assert_eq!(upstream.frames(), 3, "{ROUND1_INVARIANT}");
    assert_eq!(sibling.connections(), 0, "{ROUND1_INVARIANT}");
    ws.close(None).await.unwrap();
    upstream.finished().await;
    upstream.finished().await;
}

#[tokio::test]
async fn round1_loss_between_turns_correlated_once() {
    between_turns_loss(true).await;
}

#[tokio::test]
async fn round1_loss_between_turns_does_not_inherit_event_id() {
    between_turns_loss(false).await;
}

#[tokio::test]
async fn round1_loss_accepted_steer_owned_through_successor_terminal() {
    for terminal in ["", "response.completed", "response.failed", "error"] {
        let completed = !terminal.is_empty();
        let mut upstream = Upstream::start(move |mut peer| async move {
            peer.read().await.unwrap();
            peer.write(r#"{"type":"response.created","response":{"id":"r1"}}"#).await;
            peer.read().await.unwrap();
            peer.write(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#).await;
            peer.write(r#"{"type":"response.incomplete","response":{"id":"r1","output":[]}}"#).await;
            peer.write(r#"{"type":"response.created","response":{"id":"r2","previous_response_id":"r1"}}"#).await;
            if completed {
                peer.write(&json!({"type":terminal,"status":500,"response":{"id":"r2","output":[],"error":{"type":"server_error","message":"failed"}},"error":{"type":"server_error","message":"failed"}}).to_string()).await;
            }
        }).await;
        let sibling = unused_sibling().await;
        let url = retry_proxy(&steering(true), &upstream.url, &sibling.url).await;
        let mut ws = connect(&url, &[]).await;
        send(&mut ws, &create("c1")).await;
        assert_eq!(recv(&mut ws).await["type"], "response.created");
        send(
            &mut ws,
            r#"{"type":"response.steer","previous_response_id":"r1","event_id":"op1","input":[]}"#,
        )
        .await;
        for kind in [
            "response.steer.accepted",
            "response.incomplete",
            "response.created",
        ] {
            assert_eq!(recv(&mut ws).await["type"], kind);
        }
        if completed {
            assert_eq!(recv(&mut ws).await["type"], terminal);
        } else {
            expect_error(&mut ws, "upstream_websocket_lost", "c1").await;
            expect_error(&mut ws, "upstream_websocket_lost", "op1").await;
        }
        upstream.finished().await;
        send(
            &mut ws,
            r#"{"type":"response.create","previous_response_id":"r2","event_id":"c2","input":[]}"#,
        )
        .await;
        expect_error(&mut ws, "previous_response_not_found", "c2").await;
        assert_eq!(upstream.connections(), 1, "{ROUND1_INVARIANT}");
        assert_eq!(sibling.connections(), 0, "{ROUND1_INVARIANT}");
        ws.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn round1_loss_anonymous_operations_do_not_duplicate_turn_error() {
    operation_loss(true, false, true).await;
}
