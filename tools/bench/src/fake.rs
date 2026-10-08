//! The fake upstream: a server on 127.0.0.1 that answers like an
//! OpenAI-compatible provider (`POST /v1/chat/completions`) and like Claude
//! (`POST /v1/messages`), streamed or not, after a fixed delay.
//!
//! - **Its answer** is always [`ANSWER`], so the load generator can tell an
//!   answer that came through, translated or not, from an error the proxy
//!   made up.
//! - **It checks each request** is in its own format, as a proxy that
//!   translates must send it, and refuses one that isn't with a 400 in the
//!   provider's own error format, saying why.
//! - **It measures its own time** for each request, from the request read
//!   whole to the last of its answer handed over to be sent, so the report
//!   can say how much a proxy adds.
//! - **It counts the connections** a proxy opens to it: each one closed
//!   holds a port of the machine for a while, so a proxy that opens one per
//!   request can run the machine out of ports (see [`crate::ports`]).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::serve::ListenerExt;
use serde_json::{Map, Value, json};

/// The first word of every answer.
pub const MARKER: &str = "BENCH";

/// The words an answer streams after [`MARKER`], one per event.
const WORDS: [&str; 15] = [
    " the", " quick", " brown", " fox", " jumps", " over", " the", " lazy", " dog", " and", " the",
    " proxy", " passes", " it", " on.",
];

/// Every answer's whole text: [`MARKER`] and [`WORDS`].
pub const ANSWER: &str =
    "BENCH the quick brown fox jumps over the lazy dog and the proxy passes it on.";

/// Requests are read whole; the long conversation's are a few hundred
/// kilobytes, which axum's default limit of 2 MiB would allow too.
const BODY_LIMIT: usize = 16 * 1024 * 1024;

/// What the fake upstream has seen.
pub struct Counts {
    /// How long a closed connection holds its port, for
    /// [`Counts::recent_connections`].
    window: Duration,
    /// Requests answered at each path.
    pub chat: AtomicU64,
    pub messages: AtomicU64,
    /// Requests to any other path, which neither proxy should make.
    pub other: AtomicU64,
    /// Requests refused for not being in the provider's format.
    pub refused: AtomicU64,
    /// Microseconds spent in the delay, summed, and how many delays.
    delay_us: AtomicU64,
    delays: AtomicU64,
    /// The last other path asked for, to say what went wrong.
    pub last_other: Mutex<Option<String>>,
    /// Why the last refused request was refused.
    pub last_refused: Mutex<Option<String>>,
    /// Connections accepted.
    pub connections: AtomicU64,
    /// When each connection of the last `window` was accepted.
    recent: Mutex<VecDeque<Instant>>,
    /// The fake upstream's own time for each request answered since the
    /// last [`Counts::take_served`], in microseconds.
    served: Mutex<Vec<u64>>,
}

impl Counts {
    /// Counts that remember connections for `window`.
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            chat: AtomicU64::new(0),
            messages: AtomicU64::new(0),
            other: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            delay_us: AtomicU64::new(0),
            delays: AtomicU64::new(0),
            last_other: Mutex::new(None),
            last_refused: Mutex::new(None),
            connections: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::new()),
            served: Mutex::new(Vec::new()),
        }
    }

    pub fn answered(&self) -> u64 {
        self.chat.load(Ordering::Relaxed) + self.messages.load(Ordering::Relaxed)
    }

    /// The delay as served, on average, which can be longer than asked for
    /// when the machine is busy.
    pub fn mean_delay(&self) -> Option<Duration> {
        let delays = self.delays.load(Ordering::Relaxed);
        (delays > 0).then(|| Duration::from_micros(self.delay_us.load(Ordering::Relaxed) / delays))
    }

    fn accepted(&self) {
        self.connections.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut recent = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
        self.forget_old(&mut recent, now);
        recent.push_back(now);
    }

    /// Connections accepted within the last `window`.
    pub fn recent_connections(&self) -> usize {
        let mut recent = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
        self.forget_old(&mut recent, Instant::now());
        recent.len()
    }

    fn forget_old(&self, recent: &mut VecDeque<Instant>, now: Instant) {
        while recent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= self.window)
        {
            recent.pop_front();
        }
    }

    /// The fake upstream's own time for each request answered since the
    /// last call, in microseconds.
    pub fn take_served(&self) -> Vec<u64> {
        std::mem::take(&mut *self.served.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn refuse(&self, path: &str, problem: &str) {
        self.refused.fetch_add(1, Ordering::Relaxed);
        *self
            .last_refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(format!("{path}: {problem}"));
    }
}

struct Shared {
    delay: Duration,
    counts: Arc<Counts>,
}

pub struct FakeUpstream {
    pub addr: SocketAddr,
    pub counts: Arc<Counts>,
}

/// Starts the fake upstream on an ephemeral port of 127.0.0.1, remembering
/// connections for `window`. It runs until the program ends.
pub async fn start(delay: Duration, window: Duration) -> io::Result<FakeUpstream> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    let counts = Arc::new(Counts::new(window));
    let shared = Arc::new(Shared {
        delay,
        counts: Arc::clone(&counts),
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/messages", post(messages))
        .fallback(other)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(shared);
    let listener = {
        let counts = Arc::clone(&counts);
        listener.tap_io(move |tcp| accepted(tcp, &counts))
    };
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("fake upstream stopped: {err}");
        }
    });
    Ok(FakeUpstream { addr, counts })
}

/// Counts a connection the fake upstream accepted, and sends each write on
/// it at once. axum leaves Nagle's algorithm on, which holds back each of a
/// stream's events after the first until the proxy acknowledges that one;
/// Linux delays the acknowledgement for 40 ms when it has nothing to send,
/// so every streamed answer would take 40 ms more there, for both proxies
/// alike, and the time they add would be lost in it. Go turns Nagle's
/// algorithm off on every connection.
fn accepted(tcp: &mut tokio::net::TcpStream, counts: &Counts) {
    if let Err(err) = tcp.set_nodelay(true) {
        eprintln!("fake upstream: couldn't turn Nagle's algorithm off: {err}");
    }
    counts.accepted();
}

/// One request being served: records the fake upstream's own time for it
/// when [`Serving::done`].
struct Serving {
    started: Instant,
    counts: Arc<Counts>,
}

impl Serving {
    fn start(shared: &Shared) -> Self {
        Self {
            started: Instant::now(),
            counts: Arc::clone(&shared.counts),
        }
    }

    fn done(self) {
        let micros = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.counts
            .served
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(micros);
    }
}

/// What an answer depends on: whether to stream, and the model.
fn stream_and_model(request: &Value) -> (bool, String) {
    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("bench")
        .to_owned();
    (stream, model)
}

/// Waits the fixed delay. `std::thread::sleep` keeps to it within about a
/// millisecond on every platform, where a timer of the async runtime can
/// round it up to the system's clock tick (15.6 ms on Windows).
async fn wait(shared: &Shared) {
    let started = Instant::now();
    let delay = shared.delay;
    if !delay.is_zero() {
        let _ = tokio::task::spawn_blocking(move || std::thread::sleep(delay)).await;
    }
    let waited = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    shared.counts.delay_us.fetch_add(waited, Ordering::Relaxed);
    shared.counts.delays.fetch_add(1, Ordering::Relaxed);
}

/// The request as JSON, or why it isn't a request of the format `problem`
/// checks.
fn read(body: &[u8], problem: fn(&Map<String, Value>) -> Option<String>) -> Result<Value, String> {
    let request: Value =
        serde_json::from_slice(body).map_err(|err| format!("the body isn't JSON: {err}"))?;
    let Some(object) = request.as_object() else {
        return Err("the body isn't a JSON object".to_owned());
    };
    match problem(object) {
        Some(problem) => Err(problem),
        None => Ok(request),
    }
}

/// Why a request isn't an OpenAI Chat Completions request, if it isn't:
/// what the bench's requests could carry over from Claude Messages or
/// Responses, or lose, when a proxy translates them.
fn chat_problem(request: &Map<String, Value>) -> Option<String> {
    if !request.get("model").is_some_and(Value::is_string) {
        return Some("no model".to_owned());
    }
    for key in ["system", "input", "instructions", "max_output_tokens"] {
        if request.contains_key(key) {
            return Some(format!("a `{key}` field, which isn't Chat Completions'"));
        }
    }
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return Some("no messages".to_owned());
    };
    if messages.is_empty() {
        return Some("no messages".to_owned());
    }
    for (index, message) in messages.iter().enumerate() {
        let role = message["role"].as_str().unwrap_or_default();
        if !["system", "developer", "user", "assistant", "tool"].contains(&role) {
            return Some(format!("message {index} has the role {role:?}"));
        }
        if role == "tool" && !message["tool_call_id"].is_string() {
            return Some(format!("tool message {index} has no tool_call_id"));
        }
        match &message["content"] {
            Value::String(_) | Value::Null => {}
            Value::Array(parts) => {
                for part in parts {
                    let kind = part["type"].as_str().unwrap_or_default();
                    if !["text", "image_url", "input_audio", "file", "refusal"].contains(&kind) {
                        return Some(format!("message {index} has a part of type {kind:?}"));
                    }
                }
            }
            _ => return Some(format!("message {index}'s content isn't text or parts")),
        }
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            if call["type"] != "function"
                || !call["function"]["name"].is_string()
                || !call["function"]["arguments"].is_string()
            {
                return Some(format!(
                    "message {index} has a tool call that isn't a function's"
                ));
            }
        }
    }
    for tool in request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if tool["type"] != "function" || !tool["function"]["name"].is_string() {
            return Some("a tool that isn't a function".to_owned());
        }
    }
    None
}

/// Why a request isn't a Claude Messages request, if it isn't: what the
/// bench's requests could carry over from Chat Completions or Responses,
/// or lose, when a proxy translates them.
fn messages_problem(request: &Map<String, Value>) -> Option<String> {
    if !request.get("model").is_some_and(Value::is_string) {
        return Some("no model".to_owned());
    }
    if !request.get("max_tokens").is_some_and(Value::is_u64) {
        return Some("no max_tokens, which Claude requires".to_owned());
    }
    for key in [
        "input",
        "instructions",
        "max_output_tokens",
        "max_completion_tokens",
    ] {
        if request.contains_key(key) {
            return Some(format!("a `{key}` field, which isn't Claude Messages'"));
        }
    }
    match request.get("system") {
        None | Some(Value::String(_)) => {}
        Some(Value::Array(blocks)) if blocks.iter().all(|block| block["type"] == "text") => {}
        Some(_) => return Some("a system prompt that isn't text".to_owned()),
    }
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return Some("no messages".to_owned());
    };
    if messages.is_empty() {
        return Some("no messages".to_owned());
    }
    for (index, message) in messages.iter().enumerate() {
        let role = message["role"].as_str().unwrap_or_default();
        if role != "user" && role != "assistant" {
            return Some(format!("message {index} has the role {role:?}"));
        }
        if message.get("tool_calls").is_some() || message.get("tool_call_id").is_some() {
            return Some(format!("message {index} has Chat Completions' tool fields"));
        }
        match &message["content"] {
            Value::String(_) => {}
            Value::Array(blocks) => {
                for block in blocks {
                    let kind = block["type"].as_str().unwrap_or_default();
                    let known = [
                        "text",
                        "image",
                        "document",
                        "tool_use",
                        "tool_result",
                        "thinking",
                        "redacted_thinking",
                    ];
                    if !known.contains(&kind) {
                        return Some(format!("message {index} has a block of type {kind:?}"));
                    }
                }
            }
            _ => return Some(format!("message {index}'s content isn't text or blocks")),
        }
    }
    for tool in request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if !tool["name"].is_string() || !tool["input_schema"].is_object() {
            return Some("a tool without a name and an input_schema".to_owned());
        }
    }
    None
}

async fn chat(State(shared): State<Arc<Shared>>, body: Bytes) -> Response {
    let serving = Serving::start(&shared);
    let request = match read(&body, chat_problem) {
        Ok(request) => request,
        Err(problem) => {
            shared.counts.refuse("/v1/chat/completions", &problem);
            let error = json!({
                "error": {
                    "message": format!("not a Chat Completions request: {problem}"),
                    "type": "invalid_request_error",
                },
            });
            return (StatusCode::BAD_REQUEST, json_response(&error)).into_response();
        }
    };
    shared.counts.chat.fetch_add(1, Ordering::Relaxed);
    let (stream, model) = stream_and_model(&request);
    let prompt_tokens = body.len() / 4;
    wait(&shared).await;
    if !stream {
        let answer = json!({
            "id": "chatcmpl-bench",
            "object": "chat.completion",
            "created": 1_700_000_000,
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": ANSWER },
                "finish_reason": "stop",
            }],
            "usage": usage_openai(prompt_tokens),
        });
        let response = json_response(&answer);
        serving.done();
        return response;
    }
    let chunk = |delta: Value, finish: Value| {
        json!({
            "id": "chatcmpl-bench",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000,
            "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    };
    let mut events = vec![chunk(
        json!({ "role": "assistant", "content": "" }),
        Value::Null,
    )];
    events.push(chunk(json!({ "content": MARKER }), Value::Null));
    for word in WORDS {
        events.push(chunk(json!({ "content": word }), Value::Null));
    }
    events.push(chunk(json!({}), json!("stop")));
    events.push(json!({
        "id": "chatcmpl-bench",
        "object": "chat.completion.chunk",
        "created": 1_700_000_000,
        "model": model,
        "choices": [],
        "usage": usage_openai(prompt_tokens),
    }));
    let mut frames: Vec<String> = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    frames.push("data: [DONE]\n\n".to_owned());
    sse_response(frames, serving)
}

async fn messages(State(shared): State<Arc<Shared>>, body: Bytes) -> Response {
    let serving = Serving::start(&shared);
    let request = match read(&body, messages_problem) {
        Ok(request) => request,
        Err(problem) => {
            shared.counts.refuse("/v1/messages", &problem);
            let error = json!({
                "type": "error",
                "error": {
                    "type": "invalid_request_error",
                    "message": format!("not a Claude Messages request: {problem}"),
                },
            });
            return (StatusCode::BAD_REQUEST, json_response(&error)).into_response();
        }
    };
    shared.counts.messages.fetch_add(1, Ordering::Relaxed);
    let (stream, model) = stream_and_model(&request);
    let input_tokens = body.len() / 4;
    let output_tokens = WORDS.len() + 1;
    wait(&shared).await;
    if !stream {
        let answer = json!({
            "id": "msg_bench",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{ "type": "text", "text": ANSWER }],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
        });
        let response = json_response(&answer);
        serving.done();
        return response;
    }
    let mut events = vec![
        (
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_bench",
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": input_tokens, "output_tokens": 1 },
                },
            }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" },
            }),
        ),
    ];
    for text in std::iter::once(MARKER).chain(WORDS) {
        events.push((
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": text },
            }),
        ));
    }
    events.push((
        "content_block_stop",
        json!({ "type": "content_block_stop", "index": 0 }),
    ));
    events.push((
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": output_tokens },
        }),
    ));
    events.push(("message_stop", json!({ "type": "message_stop" })));
    let frames = events
        .iter()
        .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
        .collect();
    sse_response(frames, serving)
}

async fn other(State(shared): State<Arc<Shared>>, uri: Uri) -> Response {
    shared.counts.other.fetch_add(1, Ordering::Relaxed);
    *shared
        .counts
        .last_other
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(uri.path().to_owned());
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"error":{"message":"not served by the fake upstream","type":"not_found"}}"#,
    )
        .into_response()
}

fn usage_openai(prompt_tokens: usize) -> Value {
    let completion_tokens = WORDS.len() + 1;
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    })
}

fn json_response(answer: &Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        answer.to_string(),
    )
        .into_response()
}

/// Sends each frame as a body chunk of its own, as a provider flushes each
/// event, and records the time served once the last is handed over.
fn sse_response(frames: Vec<String>, serving: Serving) -> Response {
    let last = frames.len().saturating_sub(1);
    let mut serving = Some(serving);
    let frames = frames.into_iter().enumerate().map(move |(index, frame)| {
        if index == last
            && let Some(serving) = serving.take()
        {
            serving.done();
        }
        Ok::<_, Infallible>(frame)
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(futures_util::stream::iter(frames)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn post(url: String, body: Value) -> (u16, String) {
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    // Not upstream's: each answer the fake upstream gives, and its counts.
    #[tokio::test]
    async fn answers_each_format() {
        let fake = start(Duration::from_millis(1), Duration::from_secs(60))
            .await
            .unwrap();
        let base = format!("http://{}", fake.addr);
        let chat = json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] });
        let claude = json!({
            "model": "c",
            "max_tokens": 8,
            "messages": [{ "role": "user", "content": "hi" }],
        });

        let (status, text) = post(format!("{base}/v1/chat/completions"), chat.clone()).await;
        assert_eq!(status, 200);
        let answer: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(answer["model"], "m");
        assert_eq!(answer["choices"][0]["message"]["content"], ANSWER);
        assert_eq!(
            std::iter::once(MARKER).chain(WORDS).collect::<String>(),
            ANSWER
        );

        let mut streamed = chat.clone();
        streamed["stream"] = json!(true);
        let (status, text) = post(format!("{base}/v1/chat/completions"), streamed).await;
        assert_eq!(status, 200);
        assert!(text.starts_with("data: {\"id\":\"chatcmpl-bench\""));
        assert!(text.contains(r#""delta":{"content":"BENCH"}"#));
        assert!(text.ends_with("data: [DONE]\n\n"));

        let (status, text) = post(format!("{base}/v1/messages?beta=true"), claude.clone()).await;
        assert_eq!(status, 200);
        let answer: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(answer["content"][0]["text"], ANSWER);
        assert_eq!(answer["usage"]["output_tokens"], 16);

        let mut streamed = claude.clone();
        streamed["stream"] = json!(true);
        let (status, text) = post(format!("{base}/v1/messages"), streamed).await;
        assert_eq!(status, 200);
        assert!(text.starts_with("event: message_start\n"));
        assert!(text.contains(r#""delta":{"type":"text_delta","text":"BENCH"}"#));
        assert!(text.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));

        let (status, _) = post(format!("{base}/v1/models"), json!({})).await;
        assert_eq!(status, 404);

        assert_eq!(fake.counts.chat.load(Ordering::Relaxed), 2);
        assert_eq!(fake.counts.messages.load(Ordering::Relaxed), 2);
        assert_eq!(fake.counts.other.load(Ordering::Relaxed), 1);
        assert_eq!(
            fake.counts.last_other.lock().unwrap().as_deref(),
            Some("/v1/models")
        );
        assert!(fake.counts.mean_delay().unwrap() >= Duration::from_millis(1));
        // Its own time for each answer, at least the delay; then none.
        let served = fake.counts.take_served();
        assert_eq!(served.len(), 4);
        assert!(served.iter().all(|&micros| micros >= 1_000), "{served:?}");
        assert!(fake.counts.take_served().is_empty());
        // Each request came from a client of its own.
        assert_eq!(fake.counts.connections.load(Ordering::Relaxed), 5);
        assert_eq!(fake.counts.recent_connections(), 5);
    }

    // Not upstream's: each connection the fake upstream accepts is counted
    // and has Nagle's algorithm off, as a Go server's would.
    #[tokio::test]
    async fn sends_at_once() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut tcp, _) = listener.accept().await.unwrap();
        tcp.set_nodelay(false).unwrap();
        let counts = Counts::new(Duration::from_secs(60));
        accepted(&mut tcp, &counts);
        assert!(tcp.nodelay().unwrap());
        assert_eq!(counts.connections.load(Ordering::Relaxed), 1);
        assert_eq!(counts.recent_connections(), 1);
    }

    // Not upstream's: a request in another format is refused in the
    // provider's own error format, and counted, but not answered.
    #[tokio::test]
    async fn refuses_other_formats() {
        let fake = start(Duration::ZERO, Duration::from_secs(60))
            .await
            .unwrap();
        let base = format!("http://{}", fake.addr);
        let claude = json!({
            "model": "c",
            "max_tokens": 8,
            "system": "s",
            "messages": [{ "role": "user", "content": "hi" }],
        });
        let (status, text) = post(format!("{base}/v1/chat/completions"), claude).await;
        assert_eq!(status, 400);
        let error: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            error["error"]["message"],
            "not a Chat Completions request: a `system` field, which isn't Chat Completions'"
        );

        let chat = json!({
            "model": "m",
            "max_tokens": 8,
            "messages": [{ "role": "system", "content": "s" }, { "role": "user", "content": "hi" }],
        });
        let (status, text) = post(format!("{base}/v1/messages"), chat).await;
        assert_eq!(status, 400);
        let error: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(error["type"], "error");
        assert_eq!(
            error["error"]["message"],
            "not a Claude Messages request: message 0 has the role \"system\""
        );

        assert_eq!(fake.counts.refused.load(Ordering::Relaxed), 2);
        assert_eq!(fake.counts.answered(), 0);
        assert_eq!(
            fake.counts.last_refused.lock().unwrap().as_deref(),
            Some("/v1/messages: message 0 has the role \"system\"")
        );
        assert!(fake.counts.take_served().is_empty());
    }

    // Not upstream's: what each format check lets through and what it
    // refuses, with the bench's own requests as the ones it must pass.
    #[test]
    fn checks_formats() {
        use crate::body::{self, Format};
        let object = |body: Bytes| -> Map<String, Value> { serde_json::from_slice(&body).unwrap() };
        for stream in [false, true] {
            let chat = object(body::short(Format::Chat, "m", stream));
            assert_eq!(chat_problem(&chat), None);
            let chat = object(body::long(Format::Chat, "m", stream));
            assert_eq!(chat_problem(&chat), None);
            let claude = object(body::short(Format::Claude, "m", stream));
            assert_eq!(messages_problem(&claude), None);
            let claude = object(body::long(Format::Claude, "m", stream));
            assert_eq!(messages_problem(&claude), None);
            let responses = object(body::short(Format::Responses, "m", stream));
            assert!(chat_problem(&responses).is_some());
            assert!(messages_problem(&responses).is_some());
        }

        let claude = object(body::long(Format::Claude, "m", false));
        assert_eq!(
            chat_problem(&claude).as_deref(),
            Some("a `system` field, which isn't Chat Completions'")
        );
        let chat = object(body::long(Format::Chat, "m", false));
        assert_eq!(
            messages_problem(&chat).as_deref(),
            Some("message 0 has the role \"system\"")
        );

        let parse = |value: Value| -> Map<String, Value> { serde_json::from_value(value).unwrap() };
        let problems = [
            (
                chat_problem(&parse(json!({ "model": "m", "messages": [] }))),
                "no messages",
            ),
            (
                chat_problem(&parse(json!({
                    "model": "m",
                    "messages": [{ "role": "user", "content": [{ "type": "input_text", "text": "x" }] }],
                }))),
                "message 0 has a part of type \"input_text\"",
            ),
            (
                chat_problem(&parse(json!({
                    "model": "m",
                    "messages": [{ "role": "tool", "content": "x" }],
                }))),
                "tool message 0 has no tool_call_id",
            ),
            (
                chat_problem(&parse(json!({
                    "model": "m",
                    "messages": [{ "role": "user", "content": "x" }],
                    "tools": [{ "name": "t", "input_schema": {} }],
                }))),
                "a tool that isn't a function",
            ),
            (
                messages_problem(&parse(json!({
                    "model": "c",
                    "messages": [{ "role": "user", "content": "x" }],
                }))),
                "no max_tokens, which Claude requires",
            ),
            (
                messages_problem(&parse(json!({
                    "model": "c",
                    "max_tokens": 8,
                    "messages": [{ "role": "user", "content": [{ "type": "image_url" }] }],
                }))),
                "message 0 has a block of type \"image_url\"",
            ),
            (
                messages_problem(&parse(json!({
                    "model": "c",
                    "max_tokens": 8,
                    "messages": [{ "role": "assistant", "content": null, "tool_calls": [] }],
                }))),
                "message 0 has Chat Completions' tool fields",
            ),
            (
                messages_problem(&parse(json!({
                    "model": "c",
                    "max_tokens": 8,
                    "messages": [{ "role": "user", "content": "x" }],
                    "tools": [{ "type": "function", "function": { "name": "t" } }],
                }))),
                "a tool without a name and an input_schema",
            ),
        ];
        for (problem, expected) in problems {
            assert_eq!(problem.as_deref(), Some(expected));
        }
    }
}
