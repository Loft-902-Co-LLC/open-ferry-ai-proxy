// Ported from CLIProxyAPI internal/runtime/executor/helps/claude_input_tokens.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Fills in the input tokens of a Claude `message_start` translated from
//! another provider's stream, which doesn't know them yet, with an estimate
//! from the Claude request (`o200k_base` over its text).
//!
//! Deviations from upstream:
//! - Finding the `message_start` and patching it are two steps, so the
//!   estimate can run off the async runtime.
//! - The patched `message_start` is written by `serde_json`.
//! - Raw JSON (tool inputs and schemas, untyped blocks) is counted as
//!   `serde_json` writes it, compact, where upstream compacts the client's
//!   bytes; the two agree but for escapes, as each number keeps its text.

use open_ferry_core::exec::Format;
use open_ferry_translate::json::exact;
use serde_json::Value;

use crate::json::{get, int_of, set, str_of};

/// Per-stream state: whether the `message_start` still needs looking at
/// (`ClaudeInputTokenState`).
#[derive(Debug)]
pub(crate) struct State {
    handled: bool,
}

/// A `message_start` that needs its input tokens.
#[derive(Debug)]
pub(crate) struct Patch {
    chunk: usize,
    start: usize,
    end: usize,
    event: Value,
}

impl State {
    /// State for a stream from `upstream` answering a `source` request in
    /// `response`. Only a Claude request answered in Claude's format from
    /// another provider gets an estimate.
    pub(crate) fn new(source: &Format, upstream: &Format, response: &Format) -> Self {
        let enabled =
            *source == Format::CLAUDE && *upstream != Format::CLAUDE && *response == Format::CLAUDE;
        Self { handled: !enabled }
    }

    /// Looks for the stream's `message_start` in `chunks`. Once one is
    /// found, later chunks are left alone. Returns where to patch, when its
    /// input tokens are missing or zero.
    pub(crate) fn find(&mut self, chunks: &[Vec<u8>]) -> Option<Patch> {
        if self.handled {
            return None;
        }
        for (index, chunk) in chunks.iter().enumerate() {
            let Some((start, end, event)) = find_message_start(chunk) else {
                continue;
            };
            self.handled = true;
            let tokens = get(&event, "message.usage.input_tokens");
            if tokens.is_some() && int_of(tokens) != 0 {
                return None;
            }
            return Some(Patch {
                chunk: index,
                start,
                end,
                event,
            });
        }
        None
    }

    /// [`find`](Self::find), estimate and [`Patch::apply`] in one; for tests.
    #[cfg(test)]
    pub(crate) fn apply(&mut self, chunks: &mut [Vec<u8>], original: &[u8]) {
        if let Some(patch) = self.find(chunks) {
            patch.apply(chunks, estimate(original));
        }
    }
}

impl Patch {
    /// Writes the estimated input tokens into the `message_start`. An
    /// estimate that failed or came to zero leaves it alone.
    pub(crate) fn apply(self, chunks: &mut [Vec<u8>], estimate: Result<i64, String>) {
        let count = match estimate {
            Ok(count) => count,
            Err(error) => {
                tracing::warn!(error = %error, "failed to estimate Claude input tokens");
                return;
            }
        };
        if count == 0 {
            return;
        }
        let mut event = self.event;
        set(&mut event, "message.usage.input_tokens", Value::from(count));
        let Some(chunk) = chunks.get_mut(self.chunk) else {
            return;
        };
        let Some(tail) = chunk.get(self.end..).map(<[u8]>::to_vec) else {
            return;
        };
        chunk.truncate(self.start);
        chunk.extend_from_slice(event.to_string().as_bytes());
        chunk.extend_from_slice(&tail);
    }
}

/// The first `data:` line of `chunk` holding a `message_start`: where its
/// payload starts and ends, and the payload.
fn find_message_start(chunk: &[u8]) -> Option<(usize, usize, Value)> {
    let mut line_start = 0;
    while line_start < chunk.len() {
        let rest = chunk.get(line_start..)?;
        let line_end = rest
            .iter()
            .position(|&b| b == b'\n')
            .map_or(chunk.len(), |offset| line_start + offset);
        let mut content_end = line_end;
        if content_end > line_start && chunk.get(content_end - 1) == Some(&b'\r') {
            content_end -= 1;
        }
        let line = chunk.get(line_start..content_end)?;
        let indent = line
            .iter()
            .take_while(|&&b| b == b' ' || b == b'\t')
            .count();
        if line
            .get(indent..)
            .is_some_and(|rest| rest.starts_with(b"data:"))
        {
            let mut start = indent + 5;
            while matches!(line.get(start), Some(b' ' | b'\t')) {
                start += 1;
            }
            let mut end = line.len();
            while end > start && matches!(line.get(end - 1), Some(b' ' | b'\t')) {
                end -= 1;
            }
            let payload = line.get(start..end)?;
            if let Ok(event) = serde_json::from_slice::<Value>(payload)
                && str_of(get(&event, "type")) == "message_start"
            {
                return Some((line_start + start, line_start + end, event));
            }
        }
        line_start = line_end + 1;
    }
    None
}

/// Estimates a Claude request's input tokens with `o200k_base`
/// (`CountClaudeInputTokens`). The error never holds the request's text.
pub(crate) fn estimate(payload: &[u8]) -> Result<i64, String> {
    let segments = collect_segments(payload)?;
    if segments.is_empty() {
        return Ok(0);
    }
    let count = tiktoken_rs::o200k_base_singleton()
        .encode_ordinary(&segments.join("\n"))
        .len();
    Ok(i64::try_from(count).unwrap_or(i64::MAX))
}

/// The text of a Claude request that counts toward its input
/// (`collectClaudeInputTokenSegments`).
fn collect_segments(payload: &[u8]) -> Result<Vec<String>, String> {
    if open_ferry_translate::go::trim_space(payload).is_empty() {
        return Ok(Vec::new());
    }
    let root: Value = exact::from_slice(payload)
        .map_err(|_| "count Claude input tokens: invalid Claude request JSON".to_owned())?;
    let mut segments = Segments::default();
    segments.system(get(&root, "system"));
    if let Some(Value::Array(messages)) = get(&root, "messages") {
        for message in messages {
            segments.string_at(message, "role");
            if let Some(content) = get(message, "content") {
                segments.content(content);
            }
        }
    }
    if let Some(Value::Array(tools)) = get(&root, "tools") {
        for tool in tools {
            segments.string_at(tool, "type");
            segments.string_at(tool, "name");
            segments.string_at(tool, "description");
            segments.json(get(tool, "input_schema"));
        }
    }
    match get(&root, "tool_choice") {
        None => {}
        Some(Value::String(choice)) => segments.string(choice),
        Some(choice) => {
            segments.string_at(choice, "type");
            segments.string_at(choice, "name");
        }
    }
    Ok(segments.0)
}

/// Trimmed, non-empty text segments.
#[derive(Default)]
struct Segments(Vec<String>);

impl Segments {
    /// `appendClaudeTokenString`.
    fn string(&mut self, text: &str) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            self.0.push(trimmed.to_owned());
        }
    }

    fn string_at(&mut self, value: &Value, path: &str) {
        self.string(&str_of(get(value, path)));
    }

    /// A string as it is, anything else as compact JSON
    /// (`appendClaudeTokenJSON`).
    fn json(&mut self, value: Option<&Value>) {
        match value {
            None => {}
            Some(Value::String(text)) => self.string(text),
            Some(other) => self.string(&other.to_string()),
        }
    }

    /// `collectClaudeSystemTokenSegments`.
    fn system(&mut self, system: Option<&Value>) {
        match system {
            Some(Value::String(text)) => self.string(text),
            Some(Value::Array(parts)) => {
                for part in parts {
                    match part {
                        Value::String(text) => self.string(text),
                        part if str_of(get(part, "type")) == "text" => self.string_at(part, "text"),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// `collectClaudeContentTokenSegments`.
    fn content(&mut self, content: &Value) {
        let object = match content {
            Value::String(text) => return self.string(text),
            Value::Array(parts) => {
                for part in parts {
                    self.content(part);
                }
                return;
            }
            Value::Object(_) => content,
            _ => return,
        };
        let nested = |segments: &mut Self, key: &str| {
            if let Some(inner) = get(object, key) {
                segments.content(inner);
            }
        };
        match str_of(get(object, "type")).as_str() {
            "text" => self.string_at(object, "text"),
            "thinking" => self.string_at(object, "thinking"),
            "document" => {
                if str_of(get(object, "source.type")) == "text" {
                    for path in ["title", "context", "source.data", "source.content"] {
                        self.string_at(object, path);
                    }
                }
            }
            "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                self.string_at(object, "id");
                self.string_at(object, "name");
                self.json(get(object, "input"));
            }
            "tool_result"
            | "mcp_tool_result"
            | "web_search_tool_result"
            | "web_fetch_tool_result"
            | "code_execution_tool_result"
            | "bash_code_execution_tool_result"
            | "text_editor_code_execution_tool_result" => {
                self.string_at(object, "tool_use_id");
                self.string_at(object, "tool_call_id");
                nested(self, "content");
            }
            "web_search_result" | "search_result" => {
                if let Some(Value::String(source)) = get(object, "source") {
                    self.string(source);
                }
                for path in ["title", "url", "page_age"] {
                    self.string_at(object, path);
                }
                nested(self, "content");
            }
            "web_fetch_result" => {
                self.string_at(object, "url");
                self.string_at(object, "retrieved_at");
                nested(self, "content");
            }
            "code_execution_result"
            | "bash_code_execution_result"
            | "text_editor_code_execution_result" => {
                for path in ["stdout", "stderr", "return_code"] {
                    self.string_at(object, path);
                }
                nested(self, "content");
                nested(self, "output");
            }
            "tool_reference" => self.string_at(object, "tool_name"),
            "image" | "input_audio" | "audio" | "video" | "redacted_thinking" => {}
            "" => self.json(Some(object)),
            _ => self.string_at(object, "text"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::int_at;

    fn state() -> State {
        State::new(&Format::CLAUDE, &Format::OPENAI, &Format::CLAUDE)
    }

    /// `messageStartInputTokens`.
    fn message_start_input_tokens(chunks: &[Vec<u8>]) -> i64 {
        for chunk in chunks {
            for line in String::from_utf8_lossy(chunk).split('\n') {
                let Some(payload) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                let Ok(event) = serde_json::from_str::<Value>(payload.trim()) else {
                    continue;
                };
                if str_of(get(&event, "type")) == "message_start" {
                    return int_at(&event, "message.usage.input_tokens");
                }
            }
        }
        0
    }

    fn joined(chunks: &[Vec<u8>]) -> String {
        chunks
            .iter()
            .map(|chunk| String::from_utf8_lossy(chunk))
            .collect()
    }

    const HELLO: &str = r#"{"messages":[{"role":"user","content":"Hello."}]}"#;

    // TestCollectClaudeInputTokenSegments.
    #[test]
    fn collects_segments() {
        let payload = r#"{
            "model":"claude-test",
            "system":[
                {"type":"text","text":"Follow repository rules.","cache_control":{"type":"ephemeral"}},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"ignored-system-image"}}
            ],
            "messages":[
                {"role":"user","content":[
                    {"type":"text","text":"Review the implementation."},
                    {"type":"document","source":{"type":"text","data":"Reference document text."}},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"ignored-image"}}
                ]},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"Inspect the relevant files.","signature":"ignored-signature"},
                    {"type":"tool_use","id":"toolu_1","name":"read_file","input":{"path":"main.go"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_1","content":[
                        {"type":"text","text":"package main"},
                        {"type":"image","source":{"type":"base64","data":"ignored-tool-image"}}
                    ]}
                ]}
            ],
            "tools":[{
                "name":"read_file",
                "description":"Reads a repository file.",
                "input_schema":{"type":"object","properties":{"path":{"type":"string"}}},
                "cache_control":{"type":"ephemeral"}
            }],
            "tool_choice":{"type":"tool","name":"read_file"},
            "metadata":{"user_id":"ignored-metadata"},
            "max_tokens":4096,
            "stream":true
        }"#;
        let got = collect_segments(payload.as_bytes()).unwrap();
        assert_eq!(
            got,
            [
                "Follow repository rules.",
                "user",
                "Review the implementation.",
                "Reference document text.",
                "assistant",
                "Inspect the relevant files.",
                "toolu_1",
                "read_file",
                r#"{"path":"main.go"}"#,
                "user",
                "toolu_1",
                "package main",
                "read_file",
                "Reads a repository file.",
                r#"{"type":"object","properties":{"path":{"type":"string"}}}"#,
                "tool",
                "read_file",
            ]
        );
    }

    // TestCollectClaudeInputTokenSegmentsIncludesKnownToolResults.
    #[test]
    fn collects_known_tool_results() {
        let payload = r#"{
            "messages":[{"role":"user","content":[
                {"type":"web_search_tool_result","tool_use_id":"ws_tool_1","content":[
                    {"type":"web_search_result","source":"Search source","title":"Search result title","url":"https://search.example/result","page_age":"1 day","encrypted_content":"ignored-secret"}
                ]},
                {"type":"web_fetch_tool_result","tool_use_id":"fetch_tool_1","content":{
                    "type":"web_fetch_result","url":"https://docs.example/page","retrieved_at":"2026-07-22T00:00:00Z","content":{
                        "type":"document","title":"Fetched document","source":{"type":"text","data":"Fetched body"}
                    }
                }},
                {"type":"bash_code_execution_tool_result","tool_use_id":"bash_tool_1","content":{
                    "type":"bash_code_execution_result","stdout":"command output","stderr":"command error","return_code":1,
                    "content":[{"type":"text","text":"additional output"}]
                }},
                {"type":"tool_result","tool_use_id":"toolu_1","content":[
                    {"type":"tool_reference","tool_name":"proxy_mcp__nia__manage_resource"}
                ]}
            ]}]
        }"#;
        let segments = collect_segments(payload.as_bytes()).unwrap();
        let joined = format!("\n{}\n", segments.join("\n"));
        for want in [
            "ws_tool_1",
            "Search source",
            "Search result title",
            "https://search.example/result",
            "1 day",
            "fetch_tool_1",
            "https://docs.example/page",
            "2026-07-22T00:00:00Z",
            "Fetched document",
            "Fetched body",
            "bash_tool_1",
            "command output",
            "command error",
            "1",
            "additional output",
            "toolu_1",
            "proxy_mcp__nia__manage_resource",
        ] {
            assert!(
                joined.contains(&format!("\n{want}\n")),
                "missing {want:?}: {segments:?}"
            );
        }
        assert!(!joined.contains("ignored-secret"), "{segments:?}");
    }

    // TestCountClaudeInputTokensExcludesMultimediaAndControlFields.
    #[test]
    fn excludes_multimedia_and_control_fields() {
        let base = r#"{
            "system":"System text.",
            "messages":[{"role":"user","content":[{"type":"text","text":"User text."}]}],
            "tools":[{"name":"lookup","description":"Looks up data.","input_schema":{"type":"object"}}]
        }"#;
        let with_excluded = r#"{
            "model":"claude-test",
            "system":"System text.",
            "messages":[{"role":"user","content":[
                {"type":"text","text":"User text."},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"very-large-image-data"}},
                {"type":"input_audio","source":{"type":"base64","data":"very-large-audio-data"}},
                {"type":"video","source":{"type":"url","url":"https://example.com/video.mp4"}},
                {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"very-large-pdf-data"}}
            ]}],
            "tools":[{"name":"lookup","description":"Looks up data.","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}],
            "metadata":{"large_wrapper":"ignored"},
            "max_tokens":8192,
            "temperature":0.8,
            "top_p":0.9,
            "thinking":{"type":"enabled","budget_tokens":4096},
            "stream":true
        }"#;
        let base_count = estimate(base.as_bytes()).unwrap();
        assert!(base_count > 0);
        assert_eq!(estimate(with_excluded.as_bytes()).unwrap(), base_count);
    }

    // TestTranslateStreamWithClaudeInputTokensPatchesMessageStartOnce, on
    // translated chunks.
    #[test]
    fn patches_message_start_once() {
        let original =
            r#"{"system":"System text.","messages":[{"role":"user","content":"Hello."}]}"#;
        let mut state = State::new(&Format::CLAUDE, &Format::CODEX, &Format::CLAUDE);
        let combined = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n\
            event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0}\n\n";
        let mut chunks = vec![combined.as_bytes().to_vec()];
        state.apply(&mut chunks, original.as_bytes());
        assert!(
            message_start_input_tokens(&chunks) > 0,
            "{}",
            joined(&chunks)
        );
        assert!(state.handled);
        assert!(
            joined(&chunks).contains(r#""type":"content_block_start""#),
            "{}",
            joined(&chunks)
        );

        let mut second = vec![
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0}}}\n\n".to_vec(),
        ];
        state.apply(&mut second, original.as_bytes());
        assert_eq!(message_start_input_tokens(&second), 0);
    }

    // TestClaudeInputTokenStatePreservesCRLFAndNonTargetEvents.
    #[test]
    fn preserves_crlf_and_other_events() {
        let mut state = state();
        let chunk = "event: message_start\r\ndata:  {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}  \r\n\r\n\
            event: ping\r\ndata: {\"type\":\"ping\",\"value\":\"keep\"}\r\n\r\n";
        let mut chunks = vec![chunk.as_bytes().to_vec()];
        state.apply(&mut chunks, HELLO.as_bytes());
        let tokens = message_start_input_tokens(&chunks);
        assert!(tokens > 0);
        let want = format!(
            "event: message_start\r\ndata:  {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":{tokens},\"output_tokens\":0}}}}}}  \r\n\r\n\
             event: ping\r\ndata: {{\"type\":\"ping\",\"value\":\"keep\"}}\r\n\r\n"
        );
        assert_eq!(joined(&chunks), want);
    }

    // TestClaudeInputTokenStatePatchesMissingAndPreservesNonZero.
    #[test]
    fn patches_missing_and_preserves_non_zero() {
        let mut state = self::state();
        let mut chunks = vec![
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"output_tokens\":0}}}\n\n"
                .to_vec(),
        ];
        state.apply(&mut chunks, HELLO.as_bytes());
        assert!(message_start_input_tokens(&chunks) > 0);

        let mut state = self::state();
        let mut chunks = vec![
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":73}}}\n\n"
                .to_vec(),
        ];
        state.apply(&mut chunks, b"not valid json");
        assert_eq!(message_start_input_tokens(&chunks), 73);
        assert!(state.handled);
    }

    // TestClaudeInputTokenStateSkipsUnsupportedFlows.
    #[test]
    fn skips_unsupported_flows() {
        for (source, upstream, response) in [
            (Format::OPENAI, Format::GEMINI, Format::CLAUDE),
            (Format::CLAUDE, Format::CLAUDE, Format::CLAUDE),
            (Format::CLAUDE, Format::OPENAI, Format::OPENAI),
        ] {
            let mut state = State::new(&source, &upstream, &response);
            assert!(state.handled, "{source} {upstream} {response}");
            let mut chunks = vec![b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0}}}\n\n".to_vec()];
            state.apply(&mut chunks, HELLO.as_bytes());
            assert_eq!(
                message_start_input_tokens(&chunks),
                0,
                "{source} {upstream} {response}"
            );
        }
    }

    // TestClaudeInputTokenStateCountErrorKeepsZero, with the failure handed
    // to the patch instead of a failing codec.
    #[test]
    fn count_error_keeps_zero() {
        let mut state = state();
        let mut chunks = vec![
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0}}}\n\n"
                .to_vec(),
        ];
        let patch = state.find(&chunks).expect("message_start needs tokens");
        patch.apply(&mut chunks, Err("count failed".to_owned()));
        assert_eq!(message_start_input_tokens(&chunks), 0);
        assert!(state.handled);
    }

    // TestClaudeInputTokenStateInvalidJSONKeepsZeroWithoutLoggingRequest.
    // The warning logs the estimate's error, so the error is checked.
    #[test]
    fn invalid_json_keeps_zero_without_leaking_request() {
        const SENSITIVE: &str = r#"{"messages":["sensitive-original-request""#;
        let mut state = state();
        let mut chunks = vec![
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0}}}\n\n"
                .to_vec(),
        ];
        state.apply(&mut chunks, SENSITIVE.as_bytes());
        assert_eq!(message_start_input_tokens(&chunks), 0);
        assert!(state.handled);
        let error = estimate(SENSITIVE.as_bytes()).unwrap_err();
        assert!(error.contains("invalid Claude request JSON"), "{error}");
        assert!(!error.contains("sensitive-original-request"), "{error}");
        assert_eq!(estimate(b"  "), Ok(0));
    }

    // TestClaudeInputTokenizerConcurrentCount.
    #[test]
    fn concurrent_count() {
        let first: *const tiktoken_rs::CoreBPE = tiktoken_rs::o200k_base_singleton();
        assert!(std::ptr::eq(first, tiktoken_rs::o200k_base_singleton()));
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                std::thread::spawn(move || {
                    for iteration in 0..10 {
                        let payload = format!(
                            r#"{{"messages":[{{"role":"user","content":"worker {worker} iteration {iteration} 你好"}}]}}"#
                        );
                        let count = estimate(payload.as_bytes()).unwrap();
                        assert!(count > 0, "{count}");
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
