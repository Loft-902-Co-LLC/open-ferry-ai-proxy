//! What the `claude-cli` tests share: an entry that runs `fake-claude` in a
//! temporary directory, the calls, and the output Claude Code prints.

// Each test file uses some of these.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt as _;
use open_ferry_core::auth::Auth;
use open_ferry_core::auth::synthesizer::claude_cli::claude_cli_auth;
use open_ferry_core::auth::synthesizer::{StableIdGenerator, SynthesisContext};
use open_ferry_core::config::ClaudeCli;
use open_ferry_core::exec::{ExecError, Format, Options, Request, Response};
use open_ferry_core::executor::ProviderExecutor as _;
use open_ferry_providers::claude_cli::ClaudeCliExecutor;
use serde_json::{Value, json};

/// The stand-in for Claude Code.
pub const FAKE: &str = env!("CARGO_BIN_EXE_fake-claude");

/// The model the tests ask for.
pub const MODEL: &str = "claude-sonnet-5-5";

/// A temporary directory with `fake-claude`'s directory, which entries
/// made here use as their config directory, and the executor's work root.
pub struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    /// With `scenario` for `fake-claude`.
    pub fn new(scenario: &Value) -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(fixture.config_dir()).unwrap();
        std::fs::create_dir_all(fixture.work_root()).unwrap();
        fixture.set_scenario(scenario);
        fixture
    }

    /// Replaces the scenario.
    pub fn set_scenario(&self, scenario: &Value) {
        std::fs::write(
            self.config_dir().join("scenario.json"),
            scenario.to_string(),
        )
        .unwrap();
    }

    /// `fake-claude`'s directory: the entries' config directory.
    pub fn config_dir(&self) -> PathBuf {
        self.root.path().join("config")
    }

    /// Where the executor keeps the entries' files.
    pub fn work_root(&self) -> PathBuf {
        self.root.path().join("work")
    }

    /// An entry named `test` running `fake-claude` with this fixture's
    /// config directory.
    pub fn entry(&self) -> ClaudeCli {
        ClaudeCli {
            name: "test".into(),
            command: FAKE.into(),
            config_dir: self.config_dir().display().to_string(),
            ..ClaudeCli::default()
        }
    }

    /// The executor, with this fixture's work root.
    pub fn executor(&self) -> ClaudeCliExecutor {
        ClaudeCliExecutor::new().with_work_root(self.work_root())
    }

    /// Each run's record, by start time.
    pub fn records(&self) -> Vec<Value> {
        let mut records: Vec<Value> = std::fs::read_dir(self.config_dir())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|file| {
                let name = file.file_name().to_string_lossy().into_owned();
                name.starts_with("record-") && name.ends_with(".json")
            })
            .map(|file| serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap())
            .collect();
        records.sort_by_key(|record| record["start_ms"].as_u64());
        records
    }

    /// The `count` runs' records, once each has ended. A call answers once
    /// Claude Code has written its result, which may be before the process
    /// writes its end.
    pub async fn ended_records(&self, count: usize) -> Vec<Value> {
        for _ in 0..200 {
            let records = self.records();
            if records.len() == count && records.iter().all(|r| r["end_ms"].is_u64()) {
                return records;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the runs didn't end: {:?}", self.records());
    }

    /// The one run's record.
    pub fn record(&self) -> Value {
        let mut records = self.records();
        assert_eq!(records.len(), 1, "{records:?}");
        records.remove(0)
    }

    /// How many beats the hanging runs have made so far.
    pub fn heartbeats(&self) -> u64 {
        std::fs::read_dir(self.config_dir())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|file| file.file_name().to_string_lossy().starts_with("heartbeat-"))
            .map(|file| file.metadata().map_or(0, |meta| meta.len()))
            .sum()
    }

    /// Waits until a run hangs.
    pub async fn wait_hanging(&self) {
        for _ in 0..100 {
            if self.heartbeats() > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("no run hung");
    }

    /// Checks the runs were stopped: none ended by itself, and none beats
    /// any more, for long enough that a live one would have.
    pub async fn assert_stopped(&self) {
        let records = self.records();
        assert!(!records.is_empty(), "nothing ran");
        for record in &records {
            assert_eq!(record["end_ms"], Value::Null, "it ended by itself");
        }
        let mut last = self.heartbeats();
        let mut still = 0;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let now = self.heartbeats();
            still = if now == last { still + 1 } else { 0 };
            if still == 3 {
                return;
            }
            last = now;
        }
        panic!("fake-claude is still running");
    }
}

/// The credential for `entry`, as the config makes it.
pub fn auth(entry: &ClaudeCli) -> Arc<Auth> {
    let ctx = SynthesisContext::new("", chrono::Utc::now());
    Arc::new(claude_cli_auth(
        0,
        entry,
        &ctx,
        &mut StableIdGenerator::new(),
    ))
}

/// Options for a client speaking `format`.
pub fn options(format: Format, stream: bool) -> Options {
    let mut options = Options::new(format);
    options.stream = stream;
    options
}

/// A call to `entry` with `body`, from a client speaking `format`.
pub async fn execute(
    fixture: &Fixture,
    entry: &ClaudeCli,
    format: Format,
    body: Value,
) -> Result<Response, ExecError> {
    fixture
        .executor()
        .execute(auth(entry), request(body), options(format, false))
        .await
}

/// A streamed call to `entry` with `body`, from a client speaking
/// `format`: its headers' unified rate-limit fields and its chunks.
pub async fn stream(
    fixture: &Fixture,
    entry: &ClaudeCli,
    format: Format,
    body: Value,
) -> Result<(Vec<(String, String)>, Vec<Result<Bytes, ExecError>>), ExecError> {
    let response = fixture
        .executor()
        .execute_stream(auth(entry), request(body), options(format, true))
        .await?;
    let headers = response
        .headers
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap().to_owned()))
        .collect();
    let chunks = response.chunks.collect().await;
    Ok((headers, chunks))
}

/// The request for `body`, with the model it names, else [`MODEL`].
pub fn request(body: Value) -> Request {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(MODEL)
        .to_owned();
    Request {
        model,
        payload: Bytes::from(body.to_string()),
    }
}

/// The chunks' text, after checking none failed.
pub fn text(chunks: &[Result<Bytes, ExecError>]) -> String {
    chunks
        .iter()
        .map(|chunk| {
            let chunk = chunk.as_ref().unwrap_or_else(|error| panic!("{error:?}"));
            String::from_utf8(chunk.to_vec()).unwrap()
        })
        .collect()
}

/// The JSON of each `data:` line of an SSE stream, but `[DONE]`.
pub fn data(sse: &str) -> Vec<Value> {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap_or_else(|e| panic!("{e}: {data}")))
        .collect()
}

/// A Claude request saying `text`.
pub fn claude_body(text: &str) -> Value {
    json!({
        "model": MODEL,
        "max_tokens": 1024,
        "system": "Be brief.",
        "messages": [{"role": "user", "content": text}],
    })
}

/// A line of Claude Code's: a stream event of the main conversation.
pub fn event(event: Value) -> Value {
    json!({"type": "stream_event", "event": event, "parent_tool_use_id": null, "session_id": "s-1", "uuid": "u-1"})
}

/// Claude Code's `system` `init` line.
pub fn init() -> Value {
    json!({"type": "system", "subtype": "init", "model": MODEL, "tools": [], "session_id": "s-1"})
}

/// The stream events of an answer saying `parts`, with a thinking block
/// before it.
pub fn answer(parts: &[&str]) -> Vec<Value> {
    let mut lines = vec![
        event(
            json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": MODEL, "content": [], "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 3, "output_tokens": 1}}}),
        ),
        event(
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        ),
        event(
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Hmm."}}),
        ),
        event(
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
        ),
        event(json!({"type": "content_block_stop", "index": 0})),
        event(
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        ),
    ];
    for part in parts {
        lines.push(event(
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": part}}),
        ));
    }
    lines.push(event(json!({"type": "content_block_stop", "index": 1})));
    lines.push(event(
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 2}}),
    ));
    lines.push(event(json!({"type": "message_stop"})));
    lines
}

/// The `assistant` message Claude Code prints after the stream events.
pub fn assistant(text: &str) -> Value {
    json!({"type": "assistant", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": MODEL, "content": [{"type": "text", "text": text}], "stop_reason": null, "usage": {"input_tokens": 3, "output_tokens": 1}}, "parent_tool_use_id": null, "session_id": "s-1"})
}

/// A successful `result` with Anthropic's usage.
pub fn result(text: &str) -> Value {
    json!({
        "type": "result", "subtype": "success", "is_error": false, "duration_ms": 900,
        "num_turns": 1, "result": text, "session_id": "s-1", "total_cost_usd": 0.01,
        "usage": {"input_tokens": 12, "cache_creation_input_tokens": 590, "cache_read_input_tokens": 4, "output_tokens": 7},
        "modelUsage": {MODEL: {"inputTokens": 12, "outputTokens": 7, "cacheReadInputTokens": 4, "cacheCreationInputTokens": 590}},
    })
}

/// A `rate_limit_event` for a five-hour window at a quarter.
pub fn rate_limit() -> Value {
    json!({
        "type": "rate_limit_event",
        "rate_limit_info": {
            "status": "allowed", "resetsAt": 1_790_000_000, "rateLimitType": "five_hour",
            "overageStatus": "rejected", "overageDisabledReason": "org_level_disabled", "isUsingOverage": false,
            "unifiedWindows": {
                "five_hour": {"utilization": 0.25, "resetsAt": 1_790_000_000},
                "seven_day": {"utilization": 0.5, "resetsAt": 1_790_500_000, "status": "allowed"},
            },
        },
        "session_id": "s-1", "uuid": "u-2",
    })
}

/// A whole successful run saying `parts`.
pub fn success(parts: &[&str]) -> Value {
    let mut lines = vec![init(), rate_limit()];
    lines.extend(answer(parts));
    lines.push(assistant(&parts.concat()));
    lines.push(result(&parts.concat()));
    json!({"stdout": lines})
}

/// Whether `path` is gone, waiting a little for it to be removed.
pub async fn removed(path: &Path) -> bool {
    for _ in 0..50 {
        if !path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}
