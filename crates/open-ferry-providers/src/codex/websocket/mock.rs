//! A Responses WebSocket server on an ephemeral 127.0.0.1 port, standing in
//! for Codex in tests, and an HTTP proxy that tunnels with `CONNECT`.
//!
//! The server asks its handler how to answer each connection (an
//! [`Answer`]): refuse the handshake, write raw bytes, or accept and run a
//! script with the [`Peer`]. It records each handshake, each text message
//! read, and each accepted connection that the client ended while a script
//! held it.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role};

/// How long a test waits for the server to see something.
const WAIT: Duration = Duration::from_secs(10);

/// A request head the server or proxy read.
#[derive(Clone, Debug)]
pub(in crate::codex) struct Handshake {
    pub(in crate::codex) method: String,
    /// The request target: a path, or `CONNECT`'s authority.
    pub(in crate::codex) path: String,
    pub(in crate::codex) headers: HeaderMap,
}

impl Handshake {
    pub(in crate::codex) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .map(|value| value.to_str().expect("a header that isn't text"))
    }
}

/// What the server saw.
#[derive(Clone, Debug, Default)]
pub(in crate::codex) struct Record {
    pub(in crate::codex) handshakes: Vec<Handshake>,
    /// The text messages read, on any connection.
    pub(in crate::codex) messages: Vec<String>,
    /// How many connections the client ended while [`Peer::hold`] held
    /// them.
    pub(in crate::codex) client_closed: usize,
}

/// What a script does with an accepted connection.
pub(in crate::codex) type Script =
    Arc<dyn Fn(Peer) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// How the server answers a connection.
pub(in crate::codex) enum Answer {
    /// Refuses the handshake with `status`, `headers` and `body`.
    Refuse {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    },
    /// Writes these bytes, then closes.
    Raw(Vec<u8>),
    /// Accepts, then runs the script; the connection is dropped (without a
    /// close frame, as gorilla's `Close` drops it) when the script ends.
    Accept(Script),
}

impl Answer {
    pub(in crate::codex) fn refuse(status: u16, body: &str) -> Self {
        Self::Refuse {
            status,
            headers: Vec::new(),
            body: body.to_owned(),
        }
    }

    pub(in crate::codex) fn accept<F, Fut>(script: F) -> Self
    where
        F: Fn(Peer) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::Accept(Arc::new(move |peer| Box::pin(script(peer))))
    }
}

/// The server's end of an accepted connection.
pub(in crate::codex) struct Peer {
    ws: WebSocketStream<TcpStream>,
    handshake: Handshake,
    record: Arc<watch::Sender<Record>>,
}

impl Peer {
    /// The handshake that opened the connection.
    pub(in crate::codex) fn handshake(&self) -> &Handshake {
        &self.handshake
    }

    /// The next text message, recorded; `None` once the connection ends.
    pub(in crate::codex) async fn recv(&mut self) -> Option<String> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    let text = text.as_str().to_owned();
                    let recorded = text.clone();
                    self.record
                        .send_modify(|record| record.messages.push(recorded));
                    return Some(text);
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
                Some(Ok(_)) => {}
            }
        }
    }

    /// Sends a text message; a failure is ignored, as the client may have
    /// gone.
    pub(in crate::codex) async fn send(&mut self, text: &str) {
        let _ = self.ws.send(Message::text(text)).await;
    }

    /// Sends each of `frames`.
    pub(in crate::codex) async fn send_all(&mut self, frames: &[String]) {
        for frame in frames {
            self.send(frame).await;
        }
    }

    /// Sends a close frame with `code` and `reason`.
    pub(in crate::codex) async fn close(&mut self, code: u16, reason: &str) {
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: reason.into(),
        };
        let _ = self.ws.send(Message::Close(Some(frame))).await;
    }

    /// Pings the client and reads until its pong; whether it came.
    pub(in crate::codex) async fn ping_pong(&mut self) -> bool {
        if self
            .ws
            .send(Message::Ping(b"mock".to_vec().into()))
            .await
            .is_err()
        {
            return false;
        }
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Pong(_))) => return true,
                Some(Ok(Message::Close(_)) | Err(_)) | None => return false,
                Some(Ok(_)) => {}
            }
        }
    }

    /// Reads until the client ends the connection, and records that.
    pub(in crate::codex) async fn hold(mut self) {
        while self.recv().await.is_some() {}
        self.record.send_modify(|record| record.client_closed += 1);
    }
}

/// The handler that picks each connection's answer, by its index.
type Handler = Arc<dyn Fn(usize) -> Answer + Send + Sync>;

/// A mock Codex Responses WebSocket.
pub(in crate::codex) struct Server {
    /// `http://127.0.0.1:<port>`, a credential's `base_url`.
    pub(in crate::codex) url: String,
    record: Arc<watch::Sender<Record>>,
}

impl Server {
    /// A server that answers its `n`th connection (from 0) with
    /// `handler(n)`.
    pub(in crate::codex) async fn start(
        handler: impl Fn(usize) -> Answer + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let record = Arc::new(watch::Sender::new(Record::default()));
        let handler: Handler = Arc::new(handler);
        let recorder = Arc::clone(&record);
        tokio::spawn(async move {
            let mut index = 0;
            while let Ok((tcp, _)) = listener.accept().await {
                let answer = handler(index);
                index += 1;
                tokio::spawn(serve(tcp, answer, Arc::clone(&recorder)));
            }
        });
        Self { url, record }
    }

    /// A server whose every connection reads one message, sends `frames`
    /// and drops the connection (upstream's `codexWebsocketServer`).
    pub(in crate::codex) async fn once(frames: &[&str]) -> Self {
        let frames = owned(frames);
        Self::start(move |_| {
            let frames = frames.clone();
            Answer::accept(move |mut peer| {
                let frames = frames.clone();
                async move {
                    if peer.recv().await.is_some() {
                        peer.send_all(&frames).await;
                    }
                }
            })
        })
        .await
    }

    /// A server whose every connection answers each message with `frames`
    /// until the client ends it.
    pub(in crate::codex) async fn turns(frames: &[&str]) -> Self {
        let frames = owned(frames);
        Self::start(move |_| {
            let frames = frames.clone();
            Answer::accept(move |mut peer| {
                let frames = frames.clone();
                async move {
                    while peer.recv().await.is_some() {
                        peer.send_all(&frames).await;
                    }
                    peer.record.send_modify(|record| record.client_closed += 1);
                }
            })
        })
        .await
    }

    /// A server that refuses every handshake with `status` and `body`.
    pub(in crate::codex) async fn refusing(status: u16, body: &str) -> Self {
        let body = body.to_owned();
        Self::start(move |_| Answer::refuse(status, &body)).await
    }

    /// What the server saw so far.
    pub(in crate::codex) fn record(&self) -> Record {
        self.record.borrow().clone()
    }

    /// Waits until what the server saw satisfies `ready`, or fails the
    /// test after a while.
    pub(in crate::codex) async fn wait_for(
        &self,
        what: &str,
        ready: impl FnMut(&Record) -> bool,
    ) -> Record {
        let mut receiver = self.record.subscribe();
        match tokio::time::timeout(WAIT, receiver.wait_for(ready)).await {
            Ok(Ok(record)) => record.clone(),
            _ => panic!("timed out waiting for {what}: {:?}", self.record()),
        }
    }

    /// Waits until `count` connections were ended by the client.
    pub(in crate::codex) async fn wait_closed(&self, count: usize) -> Record {
        self.wait_for(&format!("{count} closed connections"), |record| {
            record.client_closed >= count
        })
        .await
    }
}

fn owned(frames: &[&str]) -> Vec<String> {
    frames.iter().map(|frame| (*frame).to_owned()).collect()
}

/// Answers one connection.
async fn serve(mut tcp: TcpStream, answer: Answer, record: Arc<watch::Sender<Record>>) {
    let Some((handshake, mut rest)) = read_head(&mut tcp).await else {
        return;
    };
    // Reads a request body, so that closing doesn't reset the connection.
    let length = handshake
        .header("content-length")
        .and_then(|length| length.parse::<usize>().ok())
        .unwrap_or_default();
    while rest.len() < length {
        let mut chunk = [0_u8; 4096];
        match tcp.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => rest.extend_from_slice(&chunk[..read]),
        }
    }
    let key = handshake
        .headers
        .get("sec-websocket-key")
        .map(|key| key.as_bytes().to_vec())
        .unwrap_or_default();
    record.send_modify(|record| record.handshakes.push(handshake.clone()));
    match answer {
        Answer::Refuse {
            status,
            headers,
            body,
        } => {
            let reason = StatusCode::from_u16(status)
                .ok()
                .and_then(|status| status.canonical_reason())
                .unwrap_or("Unknown");
            let mut out = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                out.push_str(&format!("{name}: {value}\r\n"));
            }
            out.push_str("\r\n");
            out.push_str(&body);
            let _ = tcp.write_all(out.as_bytes()).await;
            let _ = tcp.shutdown().await;
        }
        Answer::Raw(bytes) => {
            let _ = tcp.write_all(&bytes).await;
            let _ = tcp.shutdown().await;
        }
        Answer::Accept(script) => {
            let accept = derive_accept_key(&key);
            let out = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nX-Mock-Upstream: accepted\r\n\r\n"
            );
            if tcp.write_all(out.as_bytes()).await.is_err() {
                return;
            }
            let ws = WebSocketStream::from_partially_read(tcp, rest, Role::Server, None).await;
            script(Peer {
                ws,
                handshake,
                record,
            })
            .await;
        }
    }
}

/// Reads a request head, returning it and the bytes read past it.
async fn read_head(tcp: &mut TcpStream) -> Option<(Handshake, Vec<u8>)> {
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let read = tcp.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut request = httparse::Request::new(&mut headers);
        if let httparse::Status::Complete(len) = request.parse(&buf).ok()? {
            let mut map = HeaderMap::new();
            for header in request.headers.iter() {
                map.append(
                    HeaderName::from_bytes(header.name.as_bytes()).unwrap(),
                    HeaderValue::from_bytes(header.value).unwrap(),
                );
            }
            let handshake = Handshake {
                method: request.method.unwrap_or_default().to_owned(),
                path: request.path.unwrap_or_default().to_owned(),
                headers: map,
            };
            return Some((handshake, buf[len..].to_vec()));
        }
    }
}

/// An HTTP proxy on an ephemeral 127.0.0.1 port that tunnels `CONNECT`s.
pub(in crate::codex) struct Proxy {
    /// `http://127.0.0.1:<port>`.
    pub(in crate::codex) url: String,
    connects: Arc<Mutex<Vec<Handshake>>>,
}

impl Proxy {
    pub(in crate::codex) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connects = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&connects);
        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move {
                    let Some((connect, rest)) = read_head(&mut client).await else {
                        return;
                    };
                    let authority = connect.path.clone();
                    recorder
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(connect);
                    let Ok(mut upstream) = TcpStream::connect(&authority).await else {
                        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                        return;
                    };
                    if client
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await
                        .is_err()
                        || upstream.write_all(&rest).await.is_err()
                    {
                        return;
                    }
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
        });
        Self { url, connects }
    }

    /// The `CONNECT` requests the proxy read.
    pub(in crate::codex) fn connects(&self) -> Vec<Handshake> {
        self.connects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}
