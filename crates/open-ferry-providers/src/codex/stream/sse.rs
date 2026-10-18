// Ported from CLIProxyAPI internal/runtime/executor/helps/codex_sse_scanner.go (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Assemble split JSON data fields without delaying complete events or comments.
//! Body taps still see the physical body. Diagnostics describe framing only;
//! syntax offsets use serde's line and column converted back to a byte offset.

use super::{LineError, LineReader, MAX_LINE};
use open_ferry_translate::go::{json_valid, trim_space};
use std::collections::VecDeque;

#[derive(Default)]
struct Nesting {
    depth: i64,
    string: bool,
    escaped: bool,
}
impl Nesting {
    fn feed(&mut self, bytes: &[u8]) -> bool {
        for &b in bytes {
            if self.string {
                if self.escaped {
                    self.escaped = false;
                } else if b == b'\\' {
                    self.escaped = true;
                } else if b == b'"' {
                    self.string = false;
                }
            } else {
                match b {
                    b'"' => self.string = true,
                    b'{' | b'[' => self.depth += 1,
                    b'}' | b']' => {
                        self.depth -= 1;
                        if self.depth <= 0 {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
        false
    }
}

pub(crate) struct SseReader {
    lines: LineReader,
    held: Vec<u8>,
    joined: Vec<u8>,
    nesting: Nesting,
    pub(super) held_lines: usize,
    pub(super) held_bytes: usize,
    pub(super) raw_lines: usize,
    pub(super) raw_bytes: usize,
    queue: VecDeque<(Vec<u8>, usize, usize)>,
    failure: Option<LineError>,
    max_bytes: usize,
    warned: bool,
}
impl SseReader {
    pub(crate) fn new(response: reqwest::Response) -> Self {
        Self {
            lines: LineReader::new(response),
            held: Vec::new(),
            joined: Vec::new(),
            nesting: Nesting::default(),
            held_lines: 0,
            held_bytes: 0,
            raw_lines: 0,
            raw_bytes: 0,
            queue: VecDeque::new(),
            failure: None,
            max_bytes: MAX_LINE,
            warned: false,
        }
    }
    pub(crate) fn report(&self, error: &dyn std::fmt::Display) {
        self.lines.report(error);
    }
    pub(crate) async fn next_line(&mut self) -> Option<Result<Vec<u8>, LineError>> {
        self.next_budgeted(|_, _| false).await
    }
    // An empty payload yields to bootstrap when its raw-line budget is exhausted.
    pub(super) async fn next_budgeted(
        &mut self,
        mut budget: impl FnMut(usize, usize) -> bool,
    ) -> Option<Result<Vec<u8>, LineError>> {
        loop {
            if let Some((line, count, size)) = self.queue.pop_front() {
                self.raw_lines = count;
                self.raw_bytes = size;
                return Some(Ok(line));
            }
            if let Some(error) = self.failure.take() {
                return Some(Err(error));
            }
            let line = match self.lines.next_line().await {
                Some(Ok(line)) => line,
                end => {
                    self.failure = end.and_then(Result::err);
                    if self.held_lines > 0 {
                        self.release();
                        continue;
                    }
                    return self.failure.take().map(Err);
                }
            };
            if !line.starts_with(b"data:") {
                if line.is_empty() && self.held_lines > 0 {
                    self.release();
                    self.queue.push_back((line, 1, 0));
                    continue;
                }
                self.raw_lines = 1;
                self.raw_bytes = line.len();
                return Some(Ok(line));
            }
            let value = trim_space(&line[5..]);
            if self.held_lines == 0
                && (value.is_empty()
                    || !matches!(value[0], b'{' | b'[')
                    || value == b"[DONE]"
                    || json_valid(value))
            {
                self.raw_lines = 1;
                self.raw_bytes = line.len();
                return Some(Ok(line));
            }
            if self.held_lines > 0 && self.held_bytes + line.len() > self.max_bytes {
                self.release();
                let size = line.len();
                self.queue.push_back((line, 1, size));
                continue;
            }
            let value = line[5..].strip_prefix(b" ").unwrap_or(&line[5..]);
            if self.held_lines > 0 {
                self.held.push(b'\n');
                self.joined.push(b'\n');
                self.nesting.feed(b"\n");
            }
            self.held.extend_from_slice(&line);
            self.joined.extend_from_slice(value);
            self.held_lines += 1;
            self.held_bytes += line.len();
            if self.nesting.feed(value) {
                self.release();
            } else if budget(self.held_lines, self.held_bytes) {
                return Some(Ok(Vec::new()));
            }
        }
    }
    fn release(&mut self) {
        let valid = json_valid(&self.joined);
        let line = if self.held_lines > 1 && valid {
            // Compact lexically so numeric literals and object order stay intact.
            let mut out = b"data: ".to_vec();
            let (mut string, mut escaped) = (false, false);
            for &b in &self.joined {
                if string || !matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                    out.push(b);
                }
                if string {
                    if escaped {
                        escaped = false;
                    } else if b == b'\\' {
                        escaped = true;
                    } else if b == b'"' {
                        string = false;
                    }
                } else if b == b'"' {
                    string = true;
                }
            }
            out
        } else {
            if !valid && !self.warned {
                self.warned = true;
                let data = if self.held_lines == 1 {
                    trim_space(&self.joined)
                } else {
                    &self.joined
                };
                let (lengths, valid_lines, offset) = diagnostic(&self.held, self.held_lines, data);
                tracing::warn!(
                    data_lines = self.held_lines,
                    data_bytes = data.len(),
                    line_bytes = lengths,
                    lines_valid = valid_lines,
                    json_error_offset = offset,
                    "codex executor: upstream SSE event data is not valid JSON"
                );
            }
            std::mem::take(&mut self.held)
        };
        self.queue
            .push_back((line, self.held_lines, self.held_bytes));
        self.held.clear();
        self.joined.clear();
        self.nesting = Nesting::default();
        self.held_lines = 0;
        self.held_bytes = 0;
    }
}

/// Bounded framing diagnostics, never the event's contents.
fn diagnostic(held: &[u8], count: usize, joined: &[u8]) -> (String, String, i64) {
    let mut lengths = Vec::new();
    let mut valid = Vec::new();
    for line in held.split(|&b| b == b'\n').take(8) {
        let value = trim_space(&line[5..]);
        lengths.push(value.len().to_string());
        valid.push(json_valid(value).to_string());
    }
    let suffix = if count > 8 {
        format!(",+{}", count - 8)
    } else {
        String::new()
    };
    let offset = serde_json::from_slice::<serde_json::Value>(joined)
        .err()
        .map_or(-1, |error| {
            let before: usize = joined
                .split_inclusive(|&b| b == b'\n')
                .take(error.line().saturating_sub(1))
                .map(<[u8]>::len)
                .sum();
            i64::try_from(before + error.column()).unwrap_or(-1)
        });
    (
        lengths.join(",") + &suffix,
        valid.join(",") + &suffix,
        offset,
    )
}

#[cfg(test)]
mod tests {
    //! Dropped: TestCodexSSEScannerReadsLineByLineWhileAssemblyIsOff needs
    //! SetAssembly, which has no upstream executor callers and isn't ported.

    use super::*;

    async fn scan_logged(input: &str, limit: usize) -> (Vec<String>, String, Vec<tracing::Level>) {
        let (logs, _capture) = crate::secret_echo::Logs::capture();
        let lines = scan(input, limit).await;
        (lines, logs.text(), logs.levels())
    }

    fn assert_warnings(logs: &str, levels: &[tracing::Level], count: usize) {
        let warnings: Vec<_> = logs
            .lines()
            .filter(|line| {
                line.contains("codex executor: upstream SSE event data is not valid JSON")
            })
            .collect();
        assert_eq!(logs.lines().count(), count, "unexpected entries: {logs}");
        assert_eq!(levels.len(), count, "unexpected levels: {levels:?}");
        assert!(
            levels.iter().all(|level| *level == tracing::Level::WARN),
            "{levels:?}: {logs}"
        );
        assert_eq!(warnings.len(), count, "{logs}");
        for warning in warnings {
            assert!(
                !warning.contains("SECRET_MARKER"),
                "payload leaked: {warning}"
            );
            assert!(
                warning.contains("data_lines=") && warning.contains("json_error_offset="),
                "{warning}"
            );
        }
    }
    async fn scan(input: &str, limit: usize) -> Vec<String> {
        let mut reader = SseReader::new(reqwest::Response::from(http::Response::new(
            input.to_owned(),
        )));
        reader.max_bytes = limit;
        let mut out = Vec::new();
        while let Some(line) = reader.next_line().await {
            out.push(String::from_utf8(line.unwrap()).unwrap());
        }
        out
    }
    // Ported from TestCodexSSEScannerAssemblesEventData.
    #[tokio::test]
    async fn assembles_event_data() {
        let cases: &[(&str, &str, &[&str], usize)] = &[
            (
                "one-line events pass through as read",
                "event: response.created\ndata: {\"type\":\"response.created\"}\n\n: keep-alive\n\ndata: {\"type\":\"x\",\"v\":\"a b\"}\n\n",
                &[
                    "event: response.created",
                    "data: {\"type\":\"response.created\"}",
                    "",
                    ": keep-alive",
                    "",
                    "data: {\"type\":\"x\",\"v\":\"a b\"}",
                    "",
                ],
                0,
            ),
            (
                "CRLF line endings",
                "event: a\r\ndata: {\"type\":\"a\"}\r\n\r\n",
                &["event: a", "data: {\"type\":\"a\"}", ""],
                0,
            ),
            (
                "data lines of one event are joined and compacted",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\ndata: \"response\":{\"id\":\"r\",\"output\":[]}}\n\n",
                &[
                    "event: response.completed",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}",
                    "",
                ],
                0,
            ),
            (
                "joined CRLF event",
                "data: {\"a\":\r\ndata: [1,\r\ndata: 2]}\r\n\r\n",
                &["data: {\"a\":[1,2]}", ""],
                0,
            ),
            (
                "comment and field lines inside an event come before its data",
                "event: e\ndata: {\"a\":\n: keep-alive\nid: 7\nretry: 3000\ndata: 1}\n\n",
                &[
                    "event: e",
                    ": keep-alive",
                    "id: 7",
                    "retry: 3000",
                    "data: {\"a\":1}",
                    "",
                ],
                0,
            ),
            (
                "unterminated one-line event at end of stream",
                "data: {\"type\":\"response.completed\"}",
                &["data: {\"type\":\"response.completed\"}"],
                0,
            ),
            (
                "unterminated multi-line event at end of stream",
                "data: {\"type\":\ndata: \"response.completed\"}\n",
                &["data: {\"type\":\"response.completed\"}"],
                0,
            ),
            (
                "complete events without blank separators stay separate",
                "data: {\"a\":1}\ndata: {\"b\":2}\n",
                &["data: {\"a\":1}", "data: {\"b\":2}"],
                0,
            ),
            (
                "split event followed by an event without a blank separator",
                "data: {\"a\":\ndata: 1}\ndata: {\"b\":2}\n",
                &["data: {\"a\":1}", "data: {\"b\":2}"],
                0,
            ),
            (
                "an invalid fragment absorbs the following data lines up to the blank line",
                "data: {\"a\":\"SECRET_MARKER\ndata: {\"b\":2}\n\n",
                &["data: {\"a\":\"SECRET_MARKER\ndata: {\"b\":2}", ""],
                1,
            ),
            (
                "only the first invalid event in a stream is described",
                "data: {\"a\":\n\ndata: {\"b\":\n\ndata: {\"c\":3}\n\n",
                &[
                    "data: {\"a\":",
                    "",
                    "data: {\"b\":",
                    "",
                    "data: {\"c\":3}",
                    "",
                ],
                1,
            ),
            (
                "a complete object on its own line inside an open array stays in the event",
                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[\ndata: {\"id\":\"m\",\"type\":\"message\"}\ndata: ]}}\n\n",
                &[
                    "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"id\":\"m\",\"type\":\"message\"}]}}",
                    "",
                ],
                0,
            ),
            (
                "unterminated incomplete event at end of stream is still delivered",
                "data: {\"type\":\"response.completed\",\"t\":\"SECRET_MARKER\",",
                &["data: {\"type\":\"response.completed\",\"t\":\"SECRET_MARKER\","],
                1,
            ),
            (
                "empty and non-JSON data lines take the stock path",
                "data:\n\ndata: \n\ndata: ping\n\n",
                &["data:", "", "data: ", "", "data: ping", ""],
                0,
            ),
            (
                "done sentinel",
                "data: [DONE]\n\n",
                &["data: [DONE]", ""],
                0,
            ),
            (
                "invalid one-line event passes through with one warning",
                "data: {\"type\":\"response.completed\",\"t\":\"SECRET_MARKER\",\n\n",
                &[
                    "data: {\"type\":\"response.completed\",\"t\":\"SECRET_MARKER\",",
                    "",
                ],
                1,
            ),
            (
                "event split inside a string stays invalid and keeps its data lines",
                "data: {\"t\":\"SECRET_MARKER\ndata: SECRET_MARKER\"}\n\n",
                &["data: {\"t\":\"SECRET_MARKER\ndata: SECRET_MARKER\"}", ""],
                1,
            ),
        ];
        for (name, input, want, warnings) in cases {
            let (lines, logs, levels) = scan_logged(input, 1 << 20).await;
            assert_eq!(lines, *want, "{name}");
            assert_warnings(&logs, &levels, *warnings);
        }
    }
    // Ported from TestCodexSSEScannerBoundsAnEventToTheLineLimit.
    #[tokio::test]
    async fn bounds_collected_events() {
        let a = format!("data: {{\"a\":\"{}", "x".repeat(40));
        let b = format!("data: {}", "y".repeat(40));
        let c = format!("data: {}\"}}", "z".repeat(40));
        let input = format!("{a}\n{b}\n{c}\n\ndata: {{\"b\":2}}\n\n");
        let (lines, logs, levels) = scan_logged(&input, 64).await;
        assert_warnings(&logs, &levels, 1);
        assert!(!logs.contains(&"x".repeat(40)), "payload leaked: {logs}");
        assert_eq!(
            lines,
            [
                a,
                b,
                c,
                String::new(),
                "data: {\"b\":2}".into(),
                String::new()
            ]
        );
    }
    // Ported from TestCodexSSEScannerAssemblesAnEventOfThousandsOfLinesQuickly
    // and TestCodexSSEScannerHoldsAnEventInMemoryThatFollowsItsBytes.
    #[tokio::test]
    async fn assembles_many_lines() {
        let input = format!("data: [\n{}data: ]\n\n", "data:\n".repeat(100_000));
        assert_eq!(scan(&input, 1 << 20).await, ["data: []", ""]);
        let input = format!("data: [{{}}\n{}data: ]\n\n", "data: ,{}\n".repeat(2000));
        let result = scan(&input, 1 << 20).await;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], format!("data: [{{}}{}]", ",{}".repeat(2000)));
    }
    // Ported from TestCodexSSEScannerDeliversNonDataLinesAsTheyAreRead.
    #[tokio::test]
    async fn non_data_lines_do_not_wait_for_event_assembly() {
        use futures_util::StreamExt as _;
        let body = futures_util::stream::iter([Ok::<_, std::io::Error>(
            bytes::Bytes::from_static(b"data: {\"a\":\n: keep-alive\nid: 7\n"),
        )])
        .chain(futures_util::stream::pending());
        let mut reader = SseReader::new(reqwest::Response::from(http::Response::new(
            reqwest::Body::wrap_stream(body),
        )));
        assert_eq!(reader.next_line().await.unwrap().unwrap(), b": keep-alive");
        assert_eq!(reader.next_line().await.unwrap().unwrap(), b"id: 7");
    }
    // Ported from TestCodexSSEScannerMatchesBufioScannerOnWellFormedStreams.
    #[tokio::test]
    async fn well_formed_lines_stay_unchanged() {
        let input = "event: e\ndata: {\"a\":1}\n\ndata: {\"b\":2}\ndata: {\"c\":3}\n: comment\n\nevent: done\ndata: [DONE]\n\n";
        for newline in ["\n", "\r\n"] {
            let input = input.replace('\n', newline);
            assert_eq!(
                scan(&input, 1 << 20).await,
                input.lines().collect::<Vec<_>>()
            );
        }
    }
    // Not upstream's: retain a small scanner-level regression alongside the
    // executor-level budget and clock tests in bootstrap_tests::sse.
    #[tokio::test]
    async fn budget_yield_preserves_assembly() {
        let input = "data: {\"a\":\ndata: 1}\n\n";
        let mut reader = SseReader::new(reqwest::Response::from(http::Response::new(
            input.to_owned(),
        )));
        assert!(
            reader
                .next_budgeted(|_, _| true)
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
        assert_eq!(reader.held_lines, 1);
        assert_eq!(
            reader.next_line().await.unwrap().unwrap(),
            b"data: {\"a\":1}"
        );
        assert_eq!(reader.raw_lines, 2);
        assert_eq!(reader.raw_bytes, 19);
    }
    // Ported from TestCodexSSEScannerDiagnosticDescribesFraming.
    #[tokio::test]
    async fn diagnostic_describes_framing() {
        let held = b"data: {\"type\":\"response.completed\",\"t\":\"abc\ndata: def\"}";
        let joined = b"{\"type\":\"response.completed\",\"t\":\"abc\ndef\"}";
        assert_eq!(
            diagnostic(held, 2, joined),
            ("37,5".into(), "false,false".into(), 38)
        );
        let input = std::str::from_utf8(held).unwrap().to_owned() + "\n\n";
        let (_, logs, levels) = scan_logged(&input, 1 << 20).await;
        assert_warnings(&logs, &levels, 1);
        assert!(
            logs.contains("data_lines=2 ") && logs.contains("data_bytes=43 "),
            "{logs}"
        );
        let many = b"data: {\n".repeat(10);
        let (lengths, valid, _) = diagnostic(&many[..many.len() - 1], 10, b"{");
        assert_eq!(lengths, "1,1,1,1,1,1,1,1,+2");
        assert_eq!(valid, "false,false,false,false,false,false,false,false,+2");
    }
    // Not upstream's: single-line diagnostics trim all surrounding whitespace,
    // as upstream's assemble does before calling diagnose.
    #[tokio::test]
    async fn diagnostic_trims_single_line_data() {
        let (_, logs, levels) = scan_logged("data:   {\"a\":", 1 << 20).await;
        assert_warnings(&logs, &levels, 1);
        assert!(logs.contains("data_bytes=5 "), "{logs}");
        assert!(logs.contains("json_error_offset=5"), "{logs}");
    }
}
