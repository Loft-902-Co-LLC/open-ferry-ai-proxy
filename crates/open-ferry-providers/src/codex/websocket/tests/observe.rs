//! What the taps of a call are told as a WebSocket call connects and sends:
//! the request is announced before the dial, and told to be going out only
//! once the connection is up, so the usage statistics' time to first token
//! leaves the dial out. None of these is upstream's: upstream starts the
//! timer at the same place (`StartResponseTTFT`, after the handshake and
//! before the send) but has no test of it.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use http::Method;
use open_ferry_core::exec::Options;
use open_ferry_core::executor::ProviderExecutor;
use open_ferry_core::observe::{AttemptRequest, Observation, RequestContext, Tap};
use tokio::sync::watch;

use super::super::mock::{Answer, Server};
use super::super::request::websocket_url;
use super::super::session::{Conn, Target};
use super::{COMPLETED, DELTA, HELLO, auth, collect, executor, within, ws_options};

/// A tap that writes down each step it is told, with how many handshakes and
/// messages the server had seen at that moment.
struct Steps {
    server: Arc<Server>,
    steps: Mutex<Vec<String>>,
}

impl Steps {
    fn new(server: &Arc<Server>) -> Arc<Self> {
        Arc::new(Self {
            server: Arc::clone(server),
            steps: Mutex::new(Vec::new()),
        })
    }

    fn note(&self, step: &str) {
        let seen = self.server.record();
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(format!(
                "{step} handshakes={} messages={}",
                seen.handshakes.len(),
                seen.messages.len()
            ));
    }

    fn steps(&self) -> Vec<String> {
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Options for a call on a WebSocket in `session` that `self` sees.
    fn options(self: &Arc<Self>, session: &str) -> Options {
        let mut options = ws_options(session);
        let context = Arc::new(RequestContext::new(
            Method::POST,
            "/v1/responses".to_owned(),
        ));
        options.observation = Some(Arc::new(Observation::new(
            context,
            vec![Arc::clone(self) as Arc<dyn Tap>],
        )));
        options
    }
}

impl Tap for Steps {
    fn attempt_request(&self, _request: &AttemptRequest<'_>) {
        self.note("request");
    }

    fn request_sent(&self) {
        self.note("sent");
    }

    fn chunk(&self, _chunk: &Bytes) {
        self.note("chunk");
    }
}

// Not upstream's: the taps are told the request is going out only once the
// handshake is answered, and before anything is read from the server, so
// the time the dial takes isn't counted in the time to first token.
#[tokio::test]
async fn the_request_is_told_sent_once_the_connection_is_up() {
    let (gate, held) = watch::channel(false);
    let server = Arc::new(
        Server::start(move |_| {
            Answer::Held(
                held.clone(),
                Box::new(Answer::accept(|mut peer| async move {
                    if peer.recv().await.is_some() {
                        peer.send_all(&[DELTA.to_owned(), COMPLETED.to_owned()])
                            .await;
                    }
                })),
            )
        })
        .await,
    );
    let steps = Steps::new(&server);
    let executor = executor();
    let auth = auth(&server.url);
    let options = steps.options("");
    let call = tokio::spawn({
        let request = super::request("gpt-5-codex", HELLO);
        async move { super::super::execute_stream(&executor, &auth, request, options).await }
    });

    // The server read the handshake, and holds its answer: the dial is on.
    server
        .wait_for("the handshake", |seen| seen.handshakes.len() == 1)
        .await;
    assert_eq!(steps.steps(), ["request handshakes=0 messages=0"]);

    gate.send_replace(true);
    let response = within("the call", call).await.unwrap().unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        steps.steps(),
        [
            "request handshakes=0 messages=0",
            "sent handshakes=1 messages=0",
            "chunk handshakes=1 messages=1",
            "chunk handshakes=1 messages=1",
        ]
    );
}

// Not upstream's: a send that fails on the session's connection and is
// tried again on a new one tells the taps again, once that connection is
// up, so the time to first token counts from the first send (its first
// start stays) and the taps are told no more than the sends made.
#[tokio::test]
async fn a_retried_send_is_told_again_once_its_connection_is_up() {
    let server = Arc::new(Server::once(&[COMPLETED]).await);
    let steps = Steps::new(&server);
    let executor = executor();
    let auth = auth(&server.url);
    let session = executor.websockets().get_or_create("stale").unwrap();
    let url = websocket_url(&format!("{}/responses", server.url)).unwrap();
    let stale = Conn::detached(Target::new(
        &auth.id,
        &url,
        &executor.proxy_for(&auth),
        "sk-test",
    ));
    session.set_conn(Arc::clone(&stale));

    let response = super::super::execute_stream(
        &executor,
        &auth,
        super::request("gpt-5-codex", HELLO),
        steps.options("stale"),
    )
    .await
    .unwrap();
    let (_, error) = collect(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        steps.steps(),
        [
            "request handshakes=0 messages=0",
            // The send on the stale connection, which is never dialed.
            "sent handshakes=0 messages=0",
            // The send on the new connection, after its handshake.
            "sent handshakes=1 messages=0",
            "chunk handshakes=1 messages=1",
        ]
    );
    executor.close_execution_session("stale");
}
