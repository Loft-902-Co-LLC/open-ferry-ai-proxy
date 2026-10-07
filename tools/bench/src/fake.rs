//! The fake upstream: a server on 127.0.0.1 that answers like an
//! OpenAI-compatible provider (`POST /v1/chat/completions`) and like Claude
//! (`POST /v1/messages`), streamed or not, after a fixed delay. Its answers
//! start with [`MARKER`], so the load generator can tell an answer that came
//! through from an error the proxy made up.

use std::convert::Infallible;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// The first word of every answer.
pub const MARKER: &str = "BENCH";

/// The words an answer streams after [`MARKER`], one per event.
const WORDS: [&str; 15] = [
    " the", " quick", " brown", " fox", " jumps", " over", " the", " lazy", " dog", " and", " the",
    " proxy", " passes", " it", " on.",
];

/// Requests are read whole; the long conversation's are a few hundred
/// kilobytes, which axum's default limit of 2 MiB would allow too.
const BODY_LIMIT: usize = 16 * 1024 * 1024;

/// What the fake upstream has seen.
#[derive(Default)]
pub struct Counts {
    pub chat: AtomicU64,
    pub messages: AtomicU64,
    /// Requests to any other path, which neither proxy should make.
    pub other: AtomicU64,
    /// Microseconds spent in the delay, summed, and how many delays.
    delay_us: AtomicU64,
    delays: AtomicU64,
    /// The last other path asked for, to say what went wrong.
    pub last_other: Mutex<Option<String>>,
}

impl Counts {
    pub fn answered(&self) -> u64 {
        self.chat.load(Ordering::Relaxed) + self.messages.load(Ordering::Relaxed)
    }

    /// The delay as served, on average, which can be longer than asked for
    /// when the machine is busy.
    pub fn mean_delay(&self) -> Option<Duration> {
        let delays = self.delays.load(Ordering::Relaxed);
        (delays > 0).then(|| Duration::from_micros(self.delay_us.load(Ordering::Relaxed) / delays))
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

/// Starts the fake upstream on an ephemeral port of 127.0.0.1. It runs until
/// the program ends.
pub async fn start(delay: Duration) -> io::Result<FakeUpstream> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    let counts = Arc::new(Counts::default());
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
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("fake upstream stopped: {err}");
        }
    });
    Ok(FakeUpstream { addr, counts })
}

/// Reads what an answer depends on: whether to stream, and the model.
fn read_request(body: &[u8]) -> (bool, String) {
    let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
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

async fn chat(State(shared): State<Arc<Shared>>, body: Bytes) -> Response {
    shared.counts.chat.fetch_add(1, Ordering::Relaxed);
    let (stream, model) = read_request(&body);
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
                "message": { "role": "assistant", "content": answer_text() },
                "finish_reason": "stop",
            }],
            "usage": usage_openai(prompt_tokens),
        });
        return json_response(&answer);
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
    sse_response(frames)
}

async fn messages(State(shared): State<Arc<Shared>>, body: Bytes) -> Response {
    shared.counts.messages.fetch_add(1, Ordering::Relaxed);
    let (stream, model) = read_request(&body);
    let input_tokens = body.len() / 4;
    let output_tokens = WORDS.len() + 1;
    wait(&shared).await;
    if !stream {
        let answer = json!({
            "id": "msg_bench",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{ "type": "text", "text": answer_text() }],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
        });
        return json_response(&answer);
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
    sse_response(frames)
}

async fn other(State(shared): State<Arc<Shared>>, uri: Uri) -> Response {
    shared.counts.other.fetch_add(1, Ordering::Relaxed);
    *shared.counts.last_other.lock().await = Some(uri.path().to_owned());
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"error":{"message":"not served by the fake upstream","type":"not_found"}}"#,
    )
        .into_response()
}

fn answer_text() -> String {
    std::iter::once(MARKER).chain(WORDS).collect()
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
/// event.
fn sse_response(frames: Vec<String>) -> Response {
    let body = Body::from_stream(futures_util::stream::iter(
        frames.into_iter().map(Ok::<_, Infallible>),
    ));
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
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
        let fake = start(Duration::from_millis(1)).await.unwrap();
        let base = format!("http://{}", fake.addr);

        let (status, text) = post(
            format!("{base}/v1/chat/completions"),
            json!({ "model": "m", "messages": [] }),
        )
        .await;
        assert_eq!(status, 200);
        let answer: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(answer["model"], "m");
        assert_eq!(
            answer["choices"][0]["message"]["content"],
            "BENCH the quick brown fox jumps over the lazy dog and the proxy passes it on."
        );

        let (status, text) = post(
            format!("{base}/v1/chat/completions"),
            json!({ "model": "m", "stream": true }),
        )
        .await;
        assert_eq!(status, 200);
        assert!(text.starts_with("data: {\"id\":\"chatcmpl-bench\""));
        assert!(text.contains(r#""delta":{"content":"BENCH"}"#));
        assert!(text.ends_with("data: [DONE]\n\n"));

        let (status, text) = post(
            format!("{base}/v1/messages?beta=true"),
            json!({ "model": "c", "messages": [] }),
        )
        .await;
        assert_eq!(status, 200);
        let answer: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(answer["content"][0]["text"].as_str().unwrap().len(), 77);
        assert_eq!(answer["usage"]["output_tokens"], 16);

        let (status, text) = post(
            format!("{base}/v1/messages"),
            json!({ "model": "c", "stream": true }),
        )
        .await;
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
            fake.counts.last_other.lock().await.as_deref(),
            Some("/v1/models")
        );
        assert!(fake.counts.mean_delay().unwrap() >= Duration::from_millis(1));
    }
}
