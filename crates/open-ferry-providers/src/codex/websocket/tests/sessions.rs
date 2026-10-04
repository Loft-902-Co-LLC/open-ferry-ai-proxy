//! Sessions and their connections: the store, reuse by target, and the
//! active call's channel.

use std::future::Ready;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use http::{HeaderMap, HeaderValue};
use open_ferry_core::exec::{ErrorKind, ExecError};
use open_ferry_core::executor::{CLOSE_ALL_EXECUTION_SESSIONS, ProviderExecutor};
use tokio::sync::mpsc;

use super::super::dial::{self, DialError, Dialed};
use super::super::mock::{Answer, Server};
use super::super::request;
use super::super::session::{Conn, Hold, Read, Store, Target};
use super::{executor, within};
use crate::codex::CodexExecutor;

const URL: &str = "ws://example.test/responses";

fn target(auth_id: &str, url: &str, proxy: &str) -> Target {
    Target::new(auth_id, url, proxy, "token")
}

/// A dial that counts its calls and fails.
fn counted_dial(dials: &AtomicUsize) -> impl FnOnce() -> Ready<Result<Dialed, DialError>> + '_ {
    move || {
        dials.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Err(DialError::Failed(ExecError::new(
            ErrorKind::Upstream,
            "not dialed in this test",
        ))))
    }
}

// TestCodexWebsocketsExecutor_CloseAllReleasesSessions. The store is the
// executor's own, where upstream's is shared by every executor.
#[tokio::test]
async fn close_all_releases_sessions() {
    let session_id = "test-session-store-survives-replace";
    let executor = executor();
    let first = executor.websockets().get_or_create(session_id).unwrap();
    let second = executor.websockets().get_or_create(session_id).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    let other = CodexExecutor::new("direct");
    let others = other.websockets().get_or_create(session_id).unwrap();
    assert!(!Arc::ptr_eq(&first, &others));

    executor.close_execution_session(CLOSE_ALL_EXECUTION_SESSIONS);
    assert_eq!(executor.websockets().len(), 0);
    assert_eq!(other.websockets().len(), 1);
    other.close_execution_session(session_id);
    assert_eq!(other.websockets().len(), 0);
    assert!(executor.websockets().get_or_create("  ").is_none());
}

// TestExistingWebsocketSessionConnRequiresMatchingHealthyConnection, through
// `ensure_conn`: another credential or URL, or a connection that failed
// (closed by its reader), dials anew.
#[tokio::test]
async fn reuse_needs_a_matching_healthy_connection() {
    let store = Store::new();
    let session = store.get_or_create("existing").unwrap();
    let dials = AtomicUsize::new(0);
    for (name, other) in [
        ("auth", target("auth-b", URL, "")),
        ("url", target("auth-a", "ws://other.test/responses", "")),
        ("token", Target::new("auth-a", URL, "", "another token")),
        ("disconnected", target("auth-a", URL, "")),
    ] {
        let conn = Conn::detached(target("auth-a", URL, ""));
        session.set_conn(Arc::clone(&conn));
        let (reused, headers) = session
            .ensure_conn(target("auth-a", URL, ""), counted_dial(&dials))
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&reused, &conn), "{name}");
        assert!(headers.is_none(), "{name}");

        if name == "disconnected" {
            conn.close();
        }
        let before = dials.load(Ordering::SeqCst);
        assert!(
            session
                .ensure_conn(other, counted_dial(&dials))
                .await
                .is_err()
        );
        assert_eq!(
            dials.load(Ordering::SeqCst),
            before + 1,
            "{name}: not dialed"
        );
        assert!(conn.is_closed(), "{name}");
        assert!(session.conn().is_none(), "{name}: still attached");
    }
}

// TestWebsocketSessionIsolatesReusableConnectionByProxy
#[tokio::test]
async fn connection_is_reused_only_through_the_same_proxy() {
    let store = Store::new();
    let session = store.get_or_create("proxy").unwrap();
    let url = "wss://upstream.example/v1";
    let proxy_a = "http://proxy-a.example:8081";
    let dials = AtomicUsize::new(0);

    let conn = Conn::detached(target("auth-1", url, proxy_a));
    session.set_conn(Arc::clone(&conn));
    let (reused, _) = session
        .ensure_conn(target("auth-1", url, proxy_a), counted_dial(&dials))
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&reused, &conn));
    assert_eq!(dials.load(Ordering::SeqCst), 0);

    for proxy in ["http://proxy-b.example:8082", ""] {
        let conn = Conn::detached(target("auth-1", url, proxy_a));
        session.set_conn(Arc::clone(&conn));
        assert!(
            session
                .ensure_conn(target("auth-1", url, proxy), counted_dial(&dials))
                .await
                .is_err()
        );
        assert!(
            conn.is_closed(),
            "{proxy:?}: the proxied connection is open"
        );
        assert!(session.conn().is_none(), "{proxy:?}: still attached");
    }
    assert_eq!(dials.load(Ordering::SeqCst), 2);
}

// TestCodexWebsocketSessionActiveChannelBelongsToConnection
#[test]
fn active_channel_belongs_to_its_connection() {
    let session = Store::new().ephemeral();
    let old = Conn::detached(target("auth", URL, ""));
    let new = Conn::detached(target("auth", URL, ""));

    let (old_token, _old_rx) = session.activate(old.id());
    assert!(
        session
            .active_for(old.id())
            .is_some_and(|(token, ..)| token == old_token),
        "the old connection doesn't own its channel"
    );

    let (new_token, _new_rx) = session.activate(new.id());
    assert!(
        !session.clear_active(old.id(), old_token),
        "the old connection cleared the new channel"
    );
    assert!(session.active_for(old.id()).is_none());
    assert!(
        session
            .active_for(new.id())
            .is_some_and(|(token, ..)| token == new_token),
        "the new connection lost its channel"
    );
    assert!(session.clear_active(new.id(), new_token));

    let (closed_token, closed_rx) = session.activate(old.id());
    assert!(session.clear_active(old.id(), closed_token));
    drop(closed_rx);
    let (retry_token, _retry_rx) = session.activate(new.id());
    assert_ne!(retry_token, closed_token);
    let (_, tx, _) = session.active_for(new.id()).unwrap();
    assert!(
        tx.try_send(Read::new(new.id(), Ok(String::new()))).is_ok(),
        "the retry's channel isn't writable"
    );
}

// TestClearRetryActiveStateClearsOriginalConnection: moving a call to a new
// connection ends its activation on the first.
#[tokio::test]
async fn switching_clears_the_original_connection() {
    let store = Store::new();
    let session = store.get_or_create("retry-state").unwrap();
    let original = Conn::detached(target("auth", URL, ""));
    let replacement = Conn::detached(target("auth", URL, ""));
    session.set_conn(Arc::clone(&original));
    let guard = session.lock_requests().await;
    let mut hold = Hold::new(
        Arc::clone(&session),
        false,
        Some(guard),
        Arc::clone(&original),
    );
    assert!(session.active_for(original.id()).is_some());

    hold.switch(Arc::clone(&replacement));
    assert!(session.active_for(original.id()).is_none());
    assert!(session.active_for(replacement.id()).is_some());

    hold.release();
    assert!(session.active_for(replacement.id()).is_none());
    assert!(!session.is_locked());
    assert!(
        !original.is_closed(),
        "release closed a session's connection"
    );
}

// Not upstream's: a call dropped before it ends lets its connection go and
// the session's next call in; an ephemeral session's connection closes
// when its call ends.
#[tokio::test]
async fn dropped_or_ephemeral_call_closes_its_connection() {
    let store = Store::new();
    let session = store.get_or_create("dropped").unwrap();
    let conn = Conn::detached(target("auth", URL, ""));
    session.set_conn(Arc::clone(&conn));
    let guard = session.lock_requests().await;
    let hold = Hold::new(Arc::clone(&session), false, Some(guard), Arc::clone(&conn));
    assert!(session.is_locked());
    drop(hold);
    assert!(conn.is_closed());
    assert!(session.conn().is_none());
    assert!(!session.is_locked());
    assert!(session.active_for(conn.id()).is_none());

    let ephemeral = store.ephemeral();
    let conn = Conn::detached(target("auth", URL, ""));
    ephemeral.set_conn(Arc::clone(&conn));
    let mut hold = Hold::new(Arc::clone(&ephemeral), true, None, Arc::clone(&conn));
    hold.release();
    assert!(conn.is_closed());
    assert!(ephemeral.conn().is_none());
}

/// A server that sends the index of each connection the client ends.
async fn target_server() -> (Server, mpsc::UnboundedReceiver<usize>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let server = Server::start(move |n| {
        let tx = tx.clone();
        Answer::accept(move |mut peer| {
            let tx = tx.clone();
            async move {
                while peer.recv().await.is_some() {}
                let _ = tx.send(n);
            }
        })
    })
    .await;
    (server, rx)
}

/// The `X-Test-Auth` of the next connection the client ended on `server`.
async fn next_closed(server: &Server, closed: &mut mpsc::UnboundedReceiver<usize>) -> String {
    let n = within("a closed connection", closed.recv()).await.unwrap();
    server.record().handshakes[n]
        .header("x-test-auth")
        .unwrap_or_default()
        .to_owned()
}

// TestWebsocketExecutorsReconnectWhenSessionTargetChanges (Codex), without
// `UpstreamDisconnectChan`.
#[tokio::test]
async fn reconnects_when_the_session_target_changes() {
    let (server_a, mut closed_a) = target_server().await;
    let (server_b, mut closed_b) = target_server().await;
    let store = Store::new();
    let session_id = "target-switch-session";
    let session = store.get_or_create(session_id).unwrap();
    let url_a = request::websocket_url(&server_a.url).unwrap();
    let url_b = request::websocket_url(&server_b.url).unwrap();

    let connect = |auth: &'static str, url: &str| {
        let session = Arc::clone(&session);
        let url = url.to_owned();
        async move {
            let mut headers = HeaderMap::new();
            headers.insert("x-test-auth", HeaderValue::from_static(auth));
            session
                .ensure_conn(Target::new(auth, &url, "direct", ""), || {
                    dial::dial("direct", &url, &headers)
                })
                .await
                .unwrap()
                .0
        }
    };

    let conn_a = connect("auth-a", &url_a).await;
    let reused = connect("auth-a", &url_a).await;
    assert!(
        Arc::ptr_eq(&reused, &conn_a),
        "a matching target didn't reuse the connection"
    );

    let conn_url_b = connect("auth-a", &url_b).await;
    assert!(
        !Arc::ptr_eq(&conn_url_b, &conn_a),
        "a URL change reused the connection"
    );
    assert_eq!(next_closed(&server_a, &mut closed_a).await, "auth-a");

    let conn_auth_b = connect("auth-b", &url_b).await;
    assert!(
        !Arc::ptr_eq(&conn_auth_b, &conn_url_b),
        "a credential change reused the connection"
    );
    assert_eq!(next_closed(&server_b, &mut closed_b).await, "auth-a");
    assert!(
        session
            .conn()
            .is_some_and(|conn| Arc::ptr_eq(&conn, &conn_auth_b))
    );

    store.close(session_id);
    assert_eq!(next_closed(&server_b, &mut closed_b).await, "auth-b");
    assert_eq!(server_a.record().handshakes.len(), 1);
    assert_eq!(server_b.record().handshakes.len(), 2);
}
