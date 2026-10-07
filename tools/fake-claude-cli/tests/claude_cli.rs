//! The `claude-cli` executor end to end, with `fake-claude` standing in for
//! Claude Code: how it is run, what it is given, and how its output
//! becomes each client's answer.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use open_ferry_core::auth::QuotaState;
use open_ferry_core::config::ClaudeCli;
use open_ferry_core::exec::Format;
use open_ferry_core::executor::ProviderExecutor as _;
use open_ferry_providers::claude_cli::{
    AuthStatus, Entry, VersionCheck, auth_status, check_version, check_versions,
};
use serde_json::{Value, json};

use common::{
    FAKE, Fixture, MODEL, answer, assistant, auth, claude_body, data, execute, init, options,
    rate_limit, removed, request, result, stream, success, text,
};

/// What leads a transcript.
const TRANSCRIPT_NOTE: &str = "The conversation so far follows, one turn after another, each \
     after a line naming who spoke: [user] or [assistant]. Reply to the last user turn as the \
     assistant would, with the reply alone, without a label.";

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap()
}

/// The JSON of each chunk an OpenAI translator made: one value, or the
/// `data:` lines of an SSE event.
fn payloads(chunks: &[Result<bytes::Bytes, open_ferry_core::exec::ExecError>]) -> Vec<Value> {
    chunks
        .iter()
        .flat_map(|chunk| {
            let chunk = text(std::slice::from_ref(chunk));
            match serde_json::from_str::<Value>(&chunk) {
                Ok(value) => vec![value],
                Err(_) => data(&chunk),
            }
        })
        .collect()
}

/// The one line Claude Code was given.
fn stdin_line(record: &Value) -> Value {
    let stdin = record["stdin"].as_str().unwrap();
    let line = stdin.strip_suffix('\n').expect("a line ending");
    assert!(!line.contains('\n'), "{stdin}");
    serde_json::from_str(line).unwrap()
}

#[tokio::test]
async fn runs_claude_code_with_its_flags_in_both_prompt_modes() {
    for (mode, flag) in [
        ("", "--system-prompt-file"),
        ("replace", "--system-prompt-file"),
        ("append", "--append-system-prompt-file"),
    ] {
        let fixture = Fixture::new(&success(&["Hi"]));
        let entry = ClaudeCli {
            system_prompt: mode.into(),
            ..fixture.entry()
        };
        let mut body = claude_body("Hello");
        body["model"] = json!("claude-opus-4-8");
        body["max_tokens"] = json!(4096);
        body["thinking"] = json!({"type": "enabled", "budget_tokens": 2048});
        execute(&fixture, &entry, Format::CLAUDE, body)
            .await
            .unwrap();

        let record = fixture.record();
        let argv = strings(&record["argv"]);
        let prompt = record["prompt"]["path"].as_str().unwrap().to_owned();
        assert_eq!(
            argv,
            [
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--model",
                "claude-opus-4-8",
                "--tools",
                "",
                "--strict-mcp-config",
                "--safe-mode",
                "--disable-slash-commands",
                "--no-session-persistence",
                "--permission-prompts",
                "none",
                "--max-turns",
                "1",
                flag,
                &prompt,
            ],
            "{mode:?}"
        );
        // The client's system prompt went in the file, which is gone.
        assert_eq!(record["prompt"]["contents"], "Be brief.");
        assert!(removed(Path::new(&prompt)).await, "{prompt}");
        // It ran in an empty directory of the entry's own.
        let cwd = PathBuf::from(record["cwd"].as_str().unwrap());
        assert!(
            canonical(&cwd).starts_with(canonical(&fixture.work_root())),
            "{cwd:?}"
        );
        assert_eq!(std::fs::read_dir(&cwd).unwrap().count(), 0);
        // The entry's config directory, the output cap and the budget.
        assert_eq!(
            record["env"],
            json!({
                "CLAUDE_CONFIG_DIR": fixture.config_dir().display().to_string(),
                "CLAUDE_CODE_MAX_OUTPUT_TOKENS": "4096",
                "MAX_THINKING_TOKENS": "2048",
            })
        );
    }

    // An effort is passed as one.
    let fixture = Fixture::new(&success(&["Hi"]));
    let mut body = claude_body("Hello");
    body["thinking"] = json!({"type": "adaptive"});
    body["output_config"] = json!({"effort": "high"});
    execute(&fixture, &fixture.entry(), Format::CLAUDE, body)
        .await
        .unwrap();
    let record = fixture.record();
    let argv = strings(&record["argv"]);
    assert_eq!(argv[argv.len() - 2..], ["--effort", "high"]);
    assert_eq!(record["env"].get("MAX_THINKING_TOKENS"), None);
}

#[tokio::test]
async fn sends_one_composed_user_line_that_never_leads_with_a_slash() {
    let fixture = Fixture::new(&success(&["Hi"]));
    execute(
        &fixture,
        &fixture.entry(),
        Format::CLAUDE,
        claude_body("/help me"),
    )
    .await
    .unwrap();
    assert_eq!(
        stdin_line(&fixture.record()),
        json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": "\u{2060}/help me"}]},
            "parent_tool_use_id": null,
            "client_composed": true,
        })
    );

    // An OpenAI client's request is translated first.
    let fixture = Fixture::new(&success(&["Hi"]));
    let body = json!({
        "model": MODEL,
        "messages": [{"role": "system", "content": "Be brief."}, {"role": "user", "content": "Hello"}],
    });
    execute(&fixture, &fixture.entry(), Format::OPENAI, body)
        .await
        .unwrap();
    let record = fixture.record();
    assert_eq!(record["prompt"]["contents"], "Be brief.");
    assert_eq!(
        stdin_line(&record)["message"]["content"],
        json!([{"type": "text", "text": "Hello"}])
    );
}

#[tokio::test]
async fn sends_a_conversation_as_a_transcript_that_only_grows() {
    let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}});
    let mut messages = vec![
        json!({"role": "user", "content": [{"type": "text", "text": "What is this?"}, image]}),
        json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "A cat?", "signature": "s"}, {"type": "text", "text": "A cat."}]}),
        json!({"role": "user", "content": "And its name?"}),
    ];
    let fixture = Fixture::new(&success(&["Tom."]));
    let call =
        |messages: &[Value]| json!({"model": MODEL, "max_tokens": 100, "messages": messages});
    execute(&fixture, &fixture.entry(), Format::CLAUDE, call(&messages))
        .await
        .unwrap();
    let first = stdin_line(&fixture.record())["message"]["content"].clone();
    assert_eq!(
        first,
        json!([
            {"type": "text", "text": TRANSCRIPT_NOTE},
            {"type": "text", "text": "[user]"},
            {"type": "text", "text": "What is this?"},
            image,
            {"type": "text", "text": "[assistant]"},
            {"type": "text", "text": "A cat."},
            {"type": "text", "text": "[user]"},
            {"type": "text", "text": "And its name?"},
            {"type": "text", "text": "[end of conversation]"},
        ])
    );

    messages.push(json!({"role": "assistant", "content": "Tom."}));
    messages.push(json!({"role": "user", "content": "Thanks."}));
    execute(&fixture, &fixture.entry(), Format::CLAUDE, call(&messages))
        .await
        .unwrap();
    let records = fixture.records();
    assert_eq!(records.len(), 2);
    let second = stdin_line(&records[1])["message"]["content"].clone();
    let (first, second) = (first.as_array().unwrap(), second.as_array().unwrap());
    // All but the end mark comes again, as it was.
    assert_eq!(second[..first.len() - 1], first[..first.len() - 1]);
}

#[tokio::test]
async fn refuses_client_tools_without_running_claude_code() {
    let fixture = Fixture::new(&success(&["Hi"]));
    let mut with_tools = claude_body("Hello");
    with_tools["tools"] = json!([{"name": "get_weather", "input_schema": {"type": "object"}}]);
    let mut with_choice = claude_body("Hello");
    with_choice["tool_choice"] = json!({"type": "any"});
    let with_result = json!({
        "model": MODEL,
        "max_tokens": 100,
        "messages": [
            {"role": "user", "content": "Weather?"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "Sunny"}]},
        ],
    });
    for body in [with_tools, with_choice, with_result] {
        let error = execute(&fixture, &fixture.entry(), Format::CLAUDE, body)
            .await
            .unwrap_err();
        assert_eq!(error.status, 400);
        assert!(
            error
                .message
                .contains("client tools aren't supported by claude-cli yet"),
            "{}",
            error.message
        );
    }
    let openai = json!({
        "model": MODEL,
        "messages": [{"role": "user", "content": "Weather?"}],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}}],
    });
    let error = execute(&fixture, &fixture.entry(), Format::OPENAI, openai)
        .await
        .unwrap_err();
    assert_eq!(error.status, 400, "{}", error.message);
    assert!(fixture.records().is_empty());
}

#[tokio::test]
async fn streams_claude_events_to_a_claude_client() {
    let fixture = Fixture::new(&success(&["Hel", "lo"]));
    let (headers, chunks) = stream(
        &fixture,
        &fixture.entry(),
        Format::CLAUDE,
        claude_body("Hi"),
    )
    .await
    .unwrap();
    assert!(
        headers.contains(&(
            "anthropic-ratelimit-unified-5h-utilization".to_owned(),
            "0.25".to_owned()
        )),
        "{headers:?}"
    );
    let sse = text(&chunks);
    let events = data(&sse);
    let kinds: Vec<String> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        kinds,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    // The thinking block no one asked for is left out, and the text block
    // takes its index.
    assert_eq!(events[1]["index"], 0);
    assert_eq!(events[1]["content_block"]["type"], "text");
    assert_eq!(events[2]["delta"]["text"], "Hel");
    assert_eq!(events[4]["index"], 0);
    assert_eq!(
        events[5],
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 7, "input_tokens": 12, "cache_creation_input_tokens": 590, "cache_read_input_tokens": 4},
        })
    );
    assert!(sse.starts_with("event: message_start\ndata: "), "{sse}");

    // Thinking asked for is passed on.
    let fixture = Fixture::new(&success(&["Hel", "lo"]));
    let mut body = claude_body("Hi");
    body["thinking"] = json!({"type": "adaptive"});
    let (_, chunks) = stream(&fixture, &fixture.entry(), Format::CLAUDE, body)
        .await
        .unwrap();
    let events = data(&text(&chunks));
    assert_eq!(events[1]["content_block"]["type"], "thinking");
    assert_eq!(events[4]["index"], 0);
    assert_eq!(events[5]["index"], 1);
    assert_eq!(events[5]["content_block"]["type"], "text");
}

#[tokio::test]
async fn streams_to_openai_chat_and_responses_clients() {
    let fixture = Fixture::new(&success(&["Hel", "lo"]));
    let body =
        json!({"model": MODEL, "messages": [{"role": "user", "content": "Hi"}], "stream": true});
    let (_, chunks) = stream(&fixture, &fixture.entry(), Format::OPENAI, body)
        .await
        .unwrap();
    let chunks = payloads(&chunks);
    let content: String = chunks
        .iter()
        .filter_map(|chunk| chunk["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(content, "Hello");
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk["choices"][0]["finish_reason"] == "stop"),
        "{chunks:?}"
    );
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk["object"] == "chat.completion.chunk")
    );
    // Usage comes last, with the cache's tokens counted in the prompt's.
    let usage = &chunks.last().unwrap()["usage"];
    assert_eq!(usage["prompt_tokens"], 606, "{usage}");
    assert_eq!(usage["completion_tokens"], 7, "{usage}");

    let fixture = Fixture::new(&success(&["Hel", "lo"]));
    let body = json!({"model": MODEL, "input": "Hi", "stream": true});
    let (_, chunks) = stream(&fixture, &fixture.entry(), Format::OPENAI_RESPONSE, body)
        .await
        .unwrap();
    let events = payloads(&chunks);
    let deltas: String = events
        .iter()
        .filter(|event| event["type"] == "response.output_text.delta")
        .filter_map(|event| event["delta"].as_str())
        .collect();
    assert_eq!(deltas, "Hello");
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("response.completed");
    let usage = &completed["response"]["usage"];
    assert_eq!(usage["output_tokens"], 7, "{usage}");
    assert!(usage["input_tokens_details"].is_object(), "{usage}");
}

#[tokio::test]
async fn gathers_the_events_for_calls_that_arent_streamed() {
    let fixture = Fixture::new(&success(&["Hel", "lo"]));
    let response = execute(
        &fixture,
        &fixture.entry(),
        Format::CLAUDE,
        claude_body("Hi"),
    )
    .await
    .unwrap();
    let message: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(
        message,
        json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": MODEL,
            "content": [{"type": "text", "text": "Hello"}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 12, "output_tokens": 7, "cache_creation_input_tokens": 590, "cache_read_input_tokens": 4},
        })
    );

    let fixture = Fixture::new(&success(&["Hel", "lo"]));
    let body = json!({"model": MODEL, "messages": [{"role": "user", "content": "Hi"}]});
    let response = execute(&fixture, &fixture.entry(), Format::OPENAI, body)
        .await
        .unwrap();
    let completion: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(completion["object"], "chat.completion", "{completion}");
    assert_eq!(completion["choices"][0]["message"]["content"], "Hello");
    assert_eq!(completion["choices"][0]["finish_reason"], "stop");
    assert_eq!(completion["usage"]["completion_tokens"], 7, "{completion}");

    let fixture = Fixture::new(&success(&["Hel", "lo"]));
    let body = json!({"model": MODEL, "input": "Hi"});
    let response = execute(&fixture, &fixture.entry(), Format::OPENAI_RESPONSE, body)
        .await
        .unwrap();
    let answer: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(answer["object"], "response", "{answer}");
    assert_eq!(
        answer["output"][0]["content"][0]["text"], "Hello",
        "{answer}"
    );
    assert_eq!(answer["usage"]["output_tokens"], 7, "{answer}");
    assert!(
        answer["usage"]["input_tokens_details"].is_object(),
        "{answer}"
    );
}

#[tokio::test]
async fn takes_usage_from_the_result_else_the_model_usage() {
    // The result's usage stands over the stream's.
    let fixture = Fixture::new(&success(&["Hi"]));
    let response = execute(
        &fixture,
        &fixture.entry(),
        Format::CLAUDE,
        claude_body("Hi"),
    )
    .await
    .unwrap();
    let message: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(message["usage"]["cache_creation_input_tokens"], 590);
    assert_eq!(message["usage"]["output_tokens"], 7);

    // Without it, the model's line of modelUsage; other models are left
    // out.
    let mut lines = answer(&["Hi"]);
    let mut last = result("Hi");
    last.as_object_mut().unwrap().remove("usage");
    last["modelUsage"]["claude-haiku-4-5"] = json!({"inputTokens": 1000, "outputTokens": 1000});
    lines.push(last);
    let fixture = Fixture::new(&json!({"stdout": lines}));
    let response = execute(
        &fixture,
        &fixture.entry(),
        Format::CLAUDE,
        claude_body("Hi"),
    )
    .await
    .unwrap();
    let message: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(
        message["usage"],
        json!({"input_tokens": 12, "output_tokens": 7, "cache_creation_input_tokens": 590, "cache_read_input_tokens": 4})
    );

    // Without stream events, the answer is built from the assistant
    // message.
    let fixture = Fixture::new(&json!({"stdout": [init(), assistant("Hello"), result("Hello")]}));
    let response = execute(
        &fixture,
        &fixture.entry(),
        Format::CLAUDE,
        claude_body("Hi"),
    )
    .await
    .unwrap();
    let message: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(
        message["content"],
        json!([{"type": "text", "text": "Hello"}])
    );
    assert_eq!(message["usage"]["output_tokens"], 7);
}

#[tokio::test]
async fn rate_limit_events_feed_quota_readings() {
    let fixture = Fixture::new(&success(&["Hi"]));
    let response = execute(
        &fixture,
        &fixture.entry(),
        Format::CLAUDE,
        claude_body("Hi"),
    )
    .await
    .unwrap();
    let mut quota = QuotaState::default();
    assert!(quota.observe_response_headers_for_provider(
        "claude-cli",
        &response.headers,
        chrono::Utc::now()
    ));
    let signals: Vec<(&str, &str)> = quota
        .signals
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    assert_eq!(
        signals,
        [
            ("Anthropic-Ratelimit-Unified-5h-Reset", "1790000000"),
            ("Anthropic-Ratelimit-Unified-5h-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.25"),
            ("Anthropic-Ratelimit-Unified-7d-Reset", "1790500000"),
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.5"),
            (
                "Anthropic-Ratelimit-Unified-Overage-Disabled-Reason",
                "org_level_disabled"
            ),
            ("Anthropic-Ratelimit-Unified-Overage-Status", "rejected"),
            (
                "Anthropic-Ratelimit-Unified-Representative-Claim",
                "five_hour"
            ),
            ("Anthropic-Ratelimit-Unified-Reset", "1790000000"),
            ("Anthropic-Ratelimit-Unified-Status", "allowed"),
        ]
    );
}

/// The status a run ending as `lines` say gets.
async fn status_of(lines: Value, hang: bool) -> (u16, String, Fixture) {
    let fixture = Fixture::new(&json!({"stdout": lines, "hang": hang}));
    let started = Instant::now();
    let error = execute(
        &fixture,
        &fixture.entry(),
        Format::CLAUDE,
        claude_body("Hi"),
    )
    .await
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(20));
    (error.status, error.message, fixture)
}

#[tokio::test]
async fn maps_each_error_category_to_a_status() {
    for (category, status) in [
        ("authentication_failed", 401),
        ("oauth_org_not_allowed", 403),
        ("account_on_hold", 403),
        ("billing_error", 402),
        ("rate_limit", 429),
        ("overloaded", 529),
        ("invalid_request", 400),
        ("model_not_found", 404),
        ("server_error", 502),
        ("unknown", 502),
    ] {
        let mut failed = assistant("API Error: it failed");
        failed["error"] = json!(category);
        let lines = json!([
            init(),
            failed,
            {"type": "result", "subtype": "success", "is_error": true, "result": "API Error: it failed", "session_id": "s-1"},
        ]);
        let (got, message, _) = status_of(lines, false).await;
        assert_eq!(got, status, "{category}: {message}");
        assert!(message.contains("API Error: it failed"), "{message}");
    }
}

#[tokio::test]
async fn stops_claude_code_retrying_what_retrying_wont_mend() {
    for (category, status) in [
        ("authentication_failed", 401),
        ("oauth_org_not_allowed", 403),
        ("account_on_hold", 403),
        ("billing_error", 402),
        ("rate_limit", 429),
        ("model_not_found", 404),
    ] {
        let lines = json!([
            init(),
            {"type": "system", "subtype": "api_retry", "attempt": 1, "max_retries": 10, "retry_delay_ms": 500, "error_status": status, "error": category, "session_id": "s-1"},
        ]);
        let (got, message, fixture) = status_of(lines, true).await;
        assert_eq!(got, status, "{category}: {message}");
        fixture.assert_stopped().await;
    }

    // A rejected window makes a 429 the whole credential's, until it
    // resets.
    let mut rejected = rate_limit();
    rejected["rate_limit_info"]["status"] = json!("rejected");
    rejected["rate_limit_info"]["resetsAt"] = json!(chrono::Utc::now().timestamp() + 3600);
    let fixture = Fixture::new(&json!({"hang": true, "stdout": [
        init(),
        rejected,
        {"type": "system", "subtype": "api_retry", "attempt": 1, "error_status": 429, "error": "rate_limit"},
    ]}));
    let error = execute(
        &fixture,
        &fixture.entry(),
        Format::CLAUDE,
        claude_body("Hi"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, 429);
    assert!(error.credential_scoped);
    assert!(error.retry_after.is_some());
    assert_eq!(
        error.headers["anthropic-ratelimit-unified-status"],
        "rejected"
    );
    fixture.assert_stopped().await;

    // An overloaded API is left to Claude Code's retries, and the result
    // says how it ended.
    let lines = json!([
        init(),
        {"type": "system", "subtype": "api_retry", "attempt": 1, "error_status": 529, "error": "overloaded"},
        {"type": "result", "subtype": "success", "is_error": true, "api_error_status": 529, "result": "API Error: 529 Overloaded"},
    ]);
    let (status, _, _) = status_of(lines, false).await;
    assert_eq!(status, 529);
}

#[tokio::test]
async fn a_failed_result_takes_its_api_status_else_502() {
    let lines = json!([init(), {"type": "result", "subtype": "success", "is_error": true, "api_error_status": 413, "result": "Prompt is too long"}]);
    let (status, message, _) = status_of(lines, false).await;
    assert_eq!(status, 413);
    assert!(message.contains("Prompt is too long"), "{message}");

    let lines = json!([init(), {"type": "result", "subtype": "error_during_execution", "is_error": true, "errors": ["something broke"]}]);
    let (status, message, _) = status_of(lines, false).await;
    assert_eq!(status, 502);
    assert!(message.contains("something broke"), "{message}");
}

#[tokio::test]
async fn an_exit_without_a_result_says_how_to_check_the_sign_in() {
    let fixture = Fixture::new(&json!({
        "stdout": [init()],
        "stderr": "Invalid API key · Please run /login",
        "exit_code": 1,
    }));
    let error = execute(
        &fixture,
        &fixture.entry(),
        Format::CLAUDE,
        claude_body("Hi"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, 502);
    let hint = format!(
        "CLAUDE_CONFIG_DIR={} claude auth status",
        fixture.config_dir().display()
    );
    assert!(error.message.contains(&hint), "{}", error.message);
    assert!(
        error.message.contains("ended without an answer"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn a_failure_after_the_answer_began_ends_the_stream_with_it() {
    let mut lines = vec![init()];
    lines.extend(answer(&["Hel"]).into_iter().take(7));
    lines.push(json!({"type": "result", "subtype": "success", "is_error": true, "api_error_status": 529, "result": "Overloaded"}));
    let fixture = Fixture::new(&json!({"stdout": lines}));
    let (_, chunks) = stream(
        &fixture,
        &fixture.entry(),
        Format::CLAUDE,
        claude_body("Hi"),
    )
    .await
    .unwrap();
    let (last, sent) = chunks.split_last().unwrap();
    assert_eq!(last.as_ref().unwrap_err().status, 529);
    assert!(text(sent).contains("\"text\":\"Hel\""));
}

#[tokio::test]
async fn times_out_and_stops_claude_code() {
    let fixture = Fixture::new(&json!({"stdout": [init()], "hang": true}));
    let entry = ClaudeCli {
        timeout: "500ms".into(),
        ..fixture.entry()
    };
    let started = Instant::now();
    let error = execute(&fixture, &entry, Format::CLAUDE, claude_body("Hi"))
        .await
        .unwrap_err();
    assert_eq!(error.status, 504);
    assert!(started.elapsed() < Duration::from_secs(10));
    fixture.assert_stopped().await;
}

#[tokio::test]
async fn a_client_that_goes_away_stops_claude_code() {
    let mut lines = vec![init()];
    lines.extend(answer(&["Hel"]).into_iter().take(7));
    let fixture = Fixture::new(&json!({"stdout": lines, "hang": true}));
    let mut response = fixture
        .executor()
        .execute_stream(
            auth(&fixture.entry()),
            request(claude_body("Hi")),
            options(Format::CLAUDE, true),
        )
        .await
        .unwrap();
    let first = response.chunks.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("message_start"));
    fixture.wait_hanging().await;
    drop(response);
    fixture.assert_stopped().await;
}

#[tokio::test]
async fn runs_at_most_max_concurrency_at_once() {
    for (limit, overlap) in [(1, false), (2, true)] {
        let mut scenario = success(&["Hi"]);
        scenario["start_delay_ms"] = json!(400);
        let fixture = Fixture::new(&scenario);
        let entry = ClaudeCli {
            max_concurrency: limit,
            ..fixture.entry()
        };
        let (a, b) = tokio::join!(
            execute(&fixture, &entry, Format::CLAUDE, claude_body("One")),
            execute(&fixture, &entry, Format::CLAUDE, claude_body("Two")),
        );
        a.unwrap();
        b.unwrap();
        let records = fixture.ended_records(2).await;
        let first_end = records[0]["end_ms"].as_u64().unwrap();
        let second_start = records[1]["start_ms"].as_u64().unwrap();
        assert_eq!(
            second_start < first_end,
            overlap,
            "limit {limit}: {records:?}"
        );
    }
}

#[tokio::test]
async fn checks_the_version_of_claude_code() {
    let fixture = Fixture::new(&json!({}));
    let entry = Entry::from_config(&fixture.entry());
    assert_eq!(
        check_version(&entry).await,
        VersionCheck::Supported("2.1.291".into())
    );
    fixture.set_scenario(&json!({"version": "2.1.200 (Claude Code)"}));
    assert_eq!(
        check_version(&entry).await,
        VersionCheck::Outdated("2.1.200".into())
    );
    fixture.set_scenario(&json!({"version": "not a version"}));
    assert!(matches!(
        check_version(&entry).await,
        VersionCheck::Unknown(_)
    ));
    let missing = Entry::from_config(&ClaudeCli {
        command: format!("{FAKE}-missing"),
        ..fixture.entry()
    });
    assert!(matches!(
        check_version(&missing).await,
        VersionCheck::Failed(_)
    ));
}

// Not upstream's: `open-ferry check` checks each command once, skipping
// disabled entries.
#[tokio::test]
async fn checks_each_command_once() {
    let fixture = Fixture::new(&json!({"version": "2.1.200 (Claude Code)"}));
    let entries = [
        ClaudeCli {
            name: "one".into(),
            ..fixture.entry()
        },
        ClaudeCli {
            name: "two".into(),
            ..fixture.entry()
        },
        ClaudeCli {
            name: "gone".into(),
            command: format!("{FAKE}-missing"),
            ..fixture.entry()
        },
        ClaudeCli {
            name: "off".into(),
            command: format!("{FAKE}-off"),
            disabled: true,
            ..fixture.entry()
        },
    ];
    let checks = check_versions(&entries).await;
    assert_eq!(checks.len(), 2, "{checks:?}");
    assert_eq!(
        checks[0],
        (
            vec!["one".to_owned(), "two".to_owned()],
            VersionCheck::Outdated("2.1.200".into())
        )
    );
    assert_eq!(checks[1].0, ["gone"]);
    assert!(matches!(checks[1].1, VersionCheck::Failed(_)));
}

#[tokio::test]
async fn auth_status_keeps_only_two_fields() {
    let fixture = Fixture::new(&json!({}));
    let entry = Entry::from_config(&fixture.entry());
    let status = auth_status(&entry, &fixture.work_root()).await.unwrap();
    assert_eq!(
        status,
        AuthStatus {
            logged_in: true,
            auth_method: "claude.ai".into()
        }
    );
    assert_eq!(
        serde_json::to_value(&status).unwrap(),
        json!({"loggedIn": true, "authMethod": "claude.ai"})
    );
    // It runs in the entry's config directory, signed in or not.
    fixture.set_scenario(
        &json!({"auth_status": r#"{"loggedIn":false,"authMethod":"none"}"#, "auth_exit_code": 1}),
    );
    assert!(
        !auth_status(&entry, &fixture.work_root())
            .await
            .unwrap()
            .logged_in
    );
}

#[tokio::test]
async fn refuses_to_count_tokens_and_keeps_the_credential_on_refresh() {
    let fixture = Fixture::new(&json!({}));
    let auth = auth(&fixture.entry());
    let executor = fixture.executor();
    let error = executor
        .count_tokens(
            auth.clone(),
            request(claude_body("Hi")),
            options(Format::CLAUDE, false),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status, 501);
    let refreshed = executor.refresh(auth.clone()).await.unwrap();
    assert_eq!(refreshed.id, auth.id);
    assert_eq!(refreshed.attributes, auth.attributes);
    assert!(fixture.records().is_empty());
}
