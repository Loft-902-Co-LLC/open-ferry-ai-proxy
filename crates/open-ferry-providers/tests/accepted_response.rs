//! A response that was accepted (success status) and then cut short must not
//! send the request again: the provider may already have done the work. These
//! drive the real manager with the real OpenAI-compatible executor against a
//! loopback-only peer. A reset before any response is the control, and does
//! start another round.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use open_ferry_core::auth::{Auth, Status};
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Dispatcher, ExecError, Format, Options, Request};
use open_ferry_core::manager::{CallResult, ErrorEvents, Manager, Settings};
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;
use open_ferry_providers::openai_compat::OpenAiCompatExecutor;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

const PROVIDER: &str = "local-compat";
const AUTH_ID: &str = "accepted-response";
const MODEL: &str = "local-model";

/// What the peer does with one accepted POST, by arrival order. The last entry
/// repeats, so a request sent again always finds a well-formed answer and the
/// test fails on the POST count rather than hanging.
#[derive(Clone, Copy, Debug)]
enum Reply {
    /// Success status, then a body that ends short of its `Content-Length`.
    ShortBody,
    /// Success status, then a body that ends short of its `Content-Length`
    /// without a single payload byte, so no partial line is ever buffered.
    EmptyBody,
    /// Close with a reset before any response.
    Reset,
    /// A complete, valid response.
    Valid,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Unary,
    Stream,
    /// An image generation stream, which goes through the shared
    /// `images::raw_stream`.
    ImageStream,
}

struct Peer {
    address: std::net::SocketAddr,
    posts: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Peer {
    async fn start(script: Vec<Reply>, kind: Kind) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let posts = Arc::new(AtomicUsize::new(0));
        let counted = posts.clone();
        let task = tokio::spawn(async move {
            // Dropping the set when the task is aborted ends every connection.
            let mut connections = JoinSet::new();
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let script = script.clone();
                let counted = counted.clone();
                connections.spawn(async move {
                    let _ = serve(socket, &script, &counted, kind).await;
                });
            }
        });
        Self {
            address,
            posts,
            task,
        }
    }

    /// Stop the peer's own tasks, bounded so a stuck one can't hang the test.
    async fn stop(self) {
        self.task.abort();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

/// Read one complete request (head and body) before answering, so a graceful
/// close is never turned into a reset by unread upload data. Only then is the
/// POST counted as accepted.
async fn serve(
    mut socket: TcpStream,
    script: &[Reply],
    posts: &AtomicUsize,
    kind: Kind,
) -> std::io::Result<()> {
    let mut raw = Vec::new();
    while !raw.ends_with(b"\r\n\r\n") {
        raw.push(socket.read_u8().await?);
    }
    let head = String::from_utf8_lossy(&raw).into_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap_or_default())
        })
        .unwrap_or_default();
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await?;
    assert!(head.starts_with("POST "), "unexpected request: {head}");

    let index = posts.fetch_add(1, Ordering::SeqCst);
    let reply = script[index.min(script.len() - 1)];
    match reply {
        Reply::ShortBody | Reply::EmptyBody => {
            let content_type = match kind {
                Kind::Unary => "application/json",
                Kind::Stream | Kind::ImageStream => "text/event-stream",
            };
            let partial = if matches!(reply, Reply::ShortBody) {
                "x"
            } else {
                ""
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: 100\r\n\r\n{partial}"
            );
            socket.write_all(response.as_bytes()).await?;
            socket.shutdown().await?;
        }
        Reply::Reset => {
            socket2::SockRef::from(&socket).set_linger(Some(Duration::ZERO))?;
            // Dropping the socket now sends a reset.
        }
        Reply::Valid => {
            let (content_type, payload) = match kind {
                Kind::Unary => (
                    "application/json",
                    json!({
                        "id": "chatcmpl-fixture",
                        "object": "chat.completion",
                        "created": 0,
                        "model": MODEL,
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop"
                        }]
                    })
                    .to_string(),
                ),
                Kind::Stream | Kind::ImageStream => (
                    "text/event-stream",
                    format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({
                            "id": "chatcmpl-fixture",
                            "object": "chat.completion.chunk",
                            "created": 0,
                            "model": MODEL,
                            "choices": [{
                                "index": 0,
                                "delta": {"role": "assistant", "content": "ok"},
                                "finish_reason": null
                            }]
                        })
                    ),
                ),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
                payload.len()
            );
            socket.write_all(response.as_bytes()).await?;
            socket.shutdown().await?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct Failures(Mutex<Vec<CallResult>>);

impl ErrorEvents for Failures {
    fn publish(&self, result: &CallResult, _: &Auth) {
        self.0.lock().unwrap().push(result.clone());
    }
}

struct Observed {
    /// POSTs the peer accepted (read in full).
    posts: usize,
    /// The error the caller saw, if any.
    failure: Option<ExecError>,
    /// The error codes the manager published.
    codes: Vec<String>,
    /// The credential after the call.
    auth: Arc<Auth>,
}

/// One manager call with `request_retry = 1` and a single active credential.
async fn call(script: Vec<Reply>, kind: Kind) -> Observed {
    tokio::time::timeout(Duration::from_secs(20), async {
        let peer = Peer::start(script, kind).await;

        let registry = Arc::new(ModelRegistry::new());
        registry.register_client(
            AUTH_ID,
            PROVIDER,
            &[ModelInfo {
                id: MODEL.into(),
                ..ModelInfo::default()
            }],
        );
        let settings = Settings {
            request_retry: 1,
            max_retry_interval: Duration::ZERO,
            ..Settings::default()
        };
        let manager = Manager::new(settings, registry, None);
        let events = Arc::new(Failures::default());
        manager.set_error_events(events.clone());
        let mut config = Config::default();
        config.proxy_url = "direct".into();
        manager.register_executor(Arc::new(OpenAiCompatExecutor::new(
            PROVIDER,
            Arc::new(config),
        )));
        let mut auth = Auth {
            id: AUTH_ID.into(),
            provider: PROVIDER.into(),
            status: Status::Active,
            proxy_url: "direct".into(),
            ..Auth::default()
        };
        auth.attributes
            .insert("base_url".into(), format!("http://{}", peer.address));
        auth.attributes
            .insert("api_key".into(), "local-fixture-key".into());
        manager.register(auth).unwrap();

        let image = matches!(kind, Kind::ImageStream);
        let payload = if image {
            json!({"model": MODEL, "prompt": "draw"})
        } else {
            json!({"model": MODEL, "messages": [{"role": "user", "content": "hello"}]})
        };
        let request = Request {
            model: MODEL.into(),
            payload: Bytes::from(payload.to_string()),
        };
        let mut options = Options::new(if image {
            Format::OPENAI_IMAGE
        } else {
            Format::OPENAI
        });
        let failure = match kind {
            Kind::Unary => manager
                .execute(&[PROVIDER.into()], request, options)
                .await
                .err(),
            Kind::Stream | Kind::ImageStream => {
                options.stream = true;
                match manager
                    .execute_stream(&[PROVIDER.into()], request, options)
                    .await
                {
                    Err(error) => Some(error),
                    Ok(mut response) => {
                        let mut failure = None;
                        while let Some(chunk) = response.chunks.next().await {
                            if let Err(error) = chunk {
                                failure = Some(error);
                                break;
                            }
                        }
                        failure
                    }
                }
            }
        };

        let posts = peer.posts.load(Ordering::SeqCst);
        peer.stop().await;
        let codes = events
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|result| result.error.as_ref().map(|error| error.code.clone()))
            .collect();
        Observed {
            posts,
            failure,
            codes,
            auth: manager.get(AUTH_ID).expect("credential stays registered"),
        }
    })
    .await
    .expect("local manager fixture timed out")
}

/// The count is asserted first, so a request sent twice fails on it even when
/// the second answer is valid and the call succeeds.
fn assert_not_replayed(observed: &Observed, what: &str) {
    assert_eq!(
        observed.posts, 1,
        "{what}: an accepted POST must not be sent again: {:?}",
        observed.failure
    );
    let failure = observed
        .failure
        .as_ref()
        .unwrap_or_else(|| panic!("{what}: the caller must see the error"));

    assert!(
        !observed.codes.is_empty(),
        "{what}: the manager must publish the failure"
    );
    for code in &observed.codes {
        assert_ne!(code, "transient_transport", "{what}: {failure:?}");
    }
    let auth = &observed.auth;
    assert!(
        !auth.unavailable && auth.next_retry_after.is_none(),
        "{what}: credential cooled down: {auth:?}"
    );
    for (model, state) in &auth.model_states {
        assert!(
            !state.unavailable && state.next_retry_after.is_none(),
            "{what}: {model} cooled down: {state:?}"
        );
    }
}

#[tokio::test]
async fn cut_off_success_body_is_not_sent_again() {
    let observed = call(vec![Reply::ShortBody, Reply::Valid], Kind::Unary).await;
    assert_not_replayed(&observed, "unary");
}

#[tokio::test]
async fn cut_off_stream_before_first_chunk_is_not_sent_again() {
    let observed = call(vec![Reply::ShortBody, Reply::Valid], Kind::Stream).await;
    assert_not_replayed(&observed, "stream");
}

#[tokio::test]
async fn cut_off_image_stream_with_no_payload_is_not_sent_again() {
    let observed = call(vec![Reply::EmptyBody, Reply::Valid], Kind::ImageStream).await;
    assert_not_replayed(&observed, "image stream, empty short body");
}

#[tokio::test]
async fn image_stream_reset_before_any_response_is_sent_again() {
    let observed = call(vec![Reply::Reset, Reply::Valid], Kind::ImageStream).await;
    assert_eq!(observed.posts, 2, "control: {:?}", observed.failure);
    assert!(
        observed.failure.is_none(),
        "control: {:?}",
        observed.failure
    );
}

#[tokio::test]
async fn reset_before_any_response_is_sent_again() {
    let observed = call(vec![Reply::Reset, Reply::Valid], Kind::Unary).await;
    assert_eq!(observed.posts, 2, "control: {:?}", observed.failure);
    assert!(
        observed.failure.is_none(),
        "control: {:?}",
        observed.failure
    );
}

#[tokio::test]
async fn stream_reset_before_any_response_is_sent_again() {
    let observed = call(vec![Reply::Reset, Reply::Valid], Kind::Stream).await;
    assert_eq!(observed.posts, 2, "control: {:?}", observed.failure);
    assert!(
        observed.failure.is_none(),
        "control: {:?}",
        observed.failure
    );
}
