//! The load generator: requests sent by a number of clients at once for a
//! while, or one after another, with each answer's latency.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::body::Format;
use crate::fake::MARKER;

/// One request, sent the same way every time.
pub struct Request {
    pub url: String,
    pub format: Format,
    pub key: String,
    pub body: Bytes,
}

/// Latency percentiles, nearest-rank.
#[derive(Clone, Copy)]
pub struct Percentiles {
    pub p50: Duration,
    pub p90: Duration,
    pub p99: Duration,
    pub max: Duration,
}

impl Percentiles {
    fn of(mut micros: Vec<u64>) -> Option<Self> {
        micros.sort_unstable();
        let at = |p: usize| {
            let rank = (p * micros.len()).div_ceil(100).max(1);
            micros
                .get(rank - 1)
                .copied()
                .map(Duration::from_micros)
                .unwrap_or_default()
        };
        let max = micros.last().copied().map(Duration::from_micros)?;
        Some(Self {
            p50: at(50),
            p90: at(90),
            p99: at(99),
            max,
        })
    }

    /// Percentiles of whole milliseconds, for tests.
    #[cfg(test)]
    pub fn of_millis(millis: &[u64]) -> Option<Self> {
        Self::of(millis.iter().map(|ms| ms * 1000).collect())
    }
}

/// What a run of requests gave.
pub struct Outcome {
    pub ok: usize,
    pub errors: usize,
    pub first_error: Option<String>,
    pub elapsed: Duration,
    /// Whether the run stopped before its time was up.
    pub stopped: bool,
    /// Time to the whole answer.
    pub total: Option<Percentiles>,
    /// Time to the answer's first body bytes.
    pub first_byte: Option<Percentiles>,
}

impl Outcome {
    pub fn per_second(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            self.ok as f64 / secs
        } else {
            0.0
        }
    }
}

#[derive(Default)]
struct Samples {
    total: Vec<u64>,
    first_byte: Vec<u64>,
    errors: usize,
    first_error: Option<String>,
}

impl Samples {
    fn record(&mut self, result: Result<(Duration, Duration), String>) {
        match result {
            Ok((first_byte, total)) => {
                self.first_byte.push(micros(first_byte));
                self.total.push(micros(total));
            }
            Err(err) => {
                self.errors += 1;
                self.first_error.get_or_insert(err);
            }
        }
    }

    fn merge(&mut self, other: Samples) {
        self.total.extend(other.total);
        self.first_byte.extend(other.first_byte);
        self.errors += other.errors;
        if self.first_error.is_none() {
            self.first_error = other.first_error;
        }
    }

    fn outcome(self, elapsed: Duration) -> Outcome {
        Outcome {
            ok: self.total.len(),
            errors: self.errors,
            first_error: self.first_error,
            elapsed,
            stopped: false,
            total: Percentiles::of(self.total),
            first_byte: Percentiles::of(self.first_byte),
        }
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// A client for the load: HTTP/1.1 on loopback, connections kept open, and
/// no proxy from the environment.
pub fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .tcp_nodelay(true)
        .timeout(Duration::from_secs(60))
        .build()
}

/// Sends `request` once, reading the whole answer. Returns the time to the
/// first body bytes and to the end, or why the answer isn't one the fake
/// upstream gave.
pub async fn send(
    client: &reqwest::Client,
    request: &Request,
) -> Result<(Duration, Duration), String> {
    let started = Instant::now();
    let builder = client
        .post(&request.url)
        .header("content-type", "application/json");
    let builder = match request.format {
        Format::Chat => builder.bearer_auth(&request.key),
        Format::Claude => builder
            .header("x-api-key", &request.key)
            .header("anthropic-version", "2023-06-01"),
    };
    let mut response = builder
        .body(request.body.clone())
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let status = response.status();
    let mut first_byte = None;
    let mut answer = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| err.to_string())? {
        first_byte.get_or_insert_with(|| started.elapsed());
        answer.extend_from_slice(&chunk);
    }
    let total = started.elapsed();
    if !status.is_success() {
        let text = String::from_utf8_lossy(&answer);
        let text: String = text.chars().take(300).collect();
        return Err(format!("HTTP {status}: {text}"));
    }
    if !answer
        .windows(MARKER.len())
        .any(|window| window == MARKER.as_bytes())
    {
        return Err("an answer without the fake upstream's text".to_owned());
    }
    Ok((first_byte.unwrap_or(total), total))
}

/// Says when a run has to stop before its time is up.
pub type Stop = Arc<dyn Fn() -> bool + Send + Sync>;

/// `concurrency` clients each send `request` again and again until
/// `duration` has passed, or `stop` says so, then finish the request they're
/// on.
pub async fn closed_loop(
    client: &reqwest::Client,
    request: Arc<Request>,
    concurrency: usize,
    duration: Duration,
    stop: Stop,
) -> Outcome {
    let started = Instant::now();
    let deadline = started + duration;
    let stopped = Arc::new(AtomicBool::new(false));
    let mut tasks = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let client = client.clone();
        let request = Arc::clone(&request);
        let (stop, stopped) = (Arc::clone(&stop), Arc::clone(&stopped));
        tasks.push(tokio::spawn(async move {
            let mut samples = Samples::default();
            while Instant::now() < deadline && !stopped.load(Ordering::Relaxed) {
                if stop() {
                    stopped.store(true, Ordering::Relaxed);
                    break;
                }
                samples.record(send(&client, &request).await);
            }
            samples
        }));
    }
    let mut samples = Samples::default();
    for task in tasks {
        match task.await {
            Ok(done) => samples.merge(done),
            Err(err) => samples.record(Err(err.to_string())),
        }
    }
    let mut outcome = samples.outcome(started.elapsed());
    outcome.stopped = stopped.load(Ordering::Relaxed);
    outcome
}

/// Sends `request` `count` times, one after another.
pub async fn sequential(client: &reqwest::Client, request: &Request, count: usize) -> Outcome {
    let started = Instant::now();
    let mut samples = Samples::default();
    for _ in 0..count {
        samples.record(send(client, request).await);
    }
    samples.outcome(started.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: nearest-rank percentiles.
    #[test]
    fn percentiles() {
        let p = Percentiles::of((1..=100).rev().collect()).unwrap();
        assert_eq!(p.p50, Duration::from_micros(50));
        assert_eq!(p.p90, Duration::from_micros(90));
        assert_eq!(p.p99, Duration::from_micros(99));
        assert_eq!(p.max, Duration::from_micros(100));
        let p = Percentiles::of(vec![7]).unwrap();
        assert_eq!(
            (p.p50, p.p99, p.max),
            (
                Duration::from_micros(7),
                Duration::from_micros(7),
                Duration::from_micros(7)
            )
        );
        assert!(Percentiles::of(Vec::new()).is_none());
    }

    // Not upstream's: the load generator against the fake upstream itself,
    // which answers it as it answers a proxy.
    #[tokio::test]
    async fn sends_and_checks_answers() {
        let fake = crate::fake::start(Duration::ZERO).await.unwrap();
        let client = client().unwrap();
        let request = Arc::new(Request {
            url: format!("http://{}/v1/chat/completions", fake.addr),
            format: Format::Chat,
            key: "k".into(),
            body: crate::body::short(Format::Chat, "m", true),
        });
        let never: Stop = Arc::new(|| false);
        let outcome = closed_loop(
            &client,
            Arc::clone(&request),
            2,
            Duration::from_millis(100),
            never,
        )
        .await;
        assert!(outcome.ok > 0);
        assert_eq!(outcome.errors, 0);
        assert!(!outcome.stopped);
        assert!(outcome.per_second() > 0.0);

        // A run told to stop stops early.
        let counts = Arc::clone(&fake.counts);
        let before = counts.answered();
        let after_three: Stop = Arc::new(move || counts.answered() >= before + 3);
        let outcome = closed_loop(
            &client,
            Arc::clone(&request),
            2,
            Duration::from_secs(30),
            after_three,
        )
        .await;
        assert!(outcome.stopped);
        assert!((3..=4).contains(&outcome.ok));
        assert!(outcome.elapsed < Duration::from_secs(30));

        let missing = Request {
            url: format!("http://{}/v1/other", fake.addr),
            format: Format::Claude,
            key: "k".into(),
            body: Bytes::from_static(b"{}"),
        };
        let outcome = sequential(&client, &missing, 2).await;
        assert_eq!((outcome.ok, outcome.errors), (0, 2));
        assert!(
            outcome
                .first_error
                .unwrap()
                .starts_with("HTTP 404 Not Found: {\"error\"")
        );
    }
}
