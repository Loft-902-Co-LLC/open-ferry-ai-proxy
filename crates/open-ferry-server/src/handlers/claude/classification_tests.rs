use super::*;

#[test]
fn multi_event_streams_preserve_observation_and_bytes() {
    for newline in ["\n", "\r\n"] {
        let delta = format!(
            "event: content_block_delta{newline}data: {{\"type\":\"content_block_delta\",\"delta\":{{\"text\":\"hello\"}}}}{newline}{newline}"
        );
        let stop = format!("event: message_stop{newline}data: {{\"type\":\"message_stop\"}}");
        let error = format!(
            "event: error{newline}data: {{\"type\":\"error\",\"error\":{{\"message\":\"failed\"}}}}{newline}{newline}"
        );
        let ping = format!("event: ping{newline}data: {{\"type\":\"ping\"}}{newline}{newline}");
        for (input, saw_error, saw_stop) in [
            (format!("{delta}{stop}{newline}{newline}"), false, true),
            (format!("{delta}{stop}"), false, true),
            (format!("{delta}{stop}{newline}"), false, true),
            (format!("{delta}{error}{ping}"), true, false),
            (format!("{delta}{error}{delta}{stop}"), true, true),
        ] {
            for chunk_size in [1, 7, 37, input.len()] {
                let mut actual = ClaudeWriter::default();
                let mut reference = ReferenceWriter::default();
                let (mut got, mut want) = (BytesMut::new(), BytesMut::new());
                for chunk in input.as_bytes().chunks(chunk_size) {
                    actual.write_chunk(Bytes::copy_from_slice(chunk), &mut got);
                    reference.write_chunk(Bytes::copy_from_slice(chunk), &mut want);
                    assert_eq!(got, want);
                    assert_eq!(
                        (actual.saw_error_event, actual.saw_message_stop),
                        (reference.saw_error_event, reference.saw_message_stop),
                        "chunk size {chunk_size}"
                    );
                }
                assert!(actual.close_error().is_none());
                assert!(reference.close_error().is_none());
                assert_eq!(
                    (actual.saw_error_event, actual.saw_message_stop),
                    (saw_error, saw_stop)
                );
                actual.write_done(&mut got);
                reference.write_done(&mut want);
                assert_eq!(got, want);
                // A subsequent transport error must not duplicate an error event.
                if saw_error {
                    let failure = ErrorMessage::new(502, "transport failed");
                    actual.write_terminal_error(&failure, &mut got);
                    reference.write_terminal_error(&failure, &mut want);
                    assert_eq!(got, want);
                    assert_eq!(
                        String::from_utf8_lossy(&got)
                            .matches("event: error")
                            .count(),
                        1
                    );
                }
            }
        }
    }
}

#[test]
fn reused_buffers_preserve_claude_classification_and_stream_bytes() {
    let mut corpus = crate::handlers::openai::classification_tests::corpus();
    for event in [
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        "event: error\ndata: {\"type\":\"error\",\"error\":null}\r\n\r\n",
        "event: error\ndata: {\"error\":\"string\"}\n\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        "data: {\"type\":\ndata: \"message_stop\"}\n\n",
        "{\"type\":\"message\",\"stop_reason\":\"end_turn\"}\n",
        "event: error\ndata: {\"type\":\"message\",\"stop_reason\":\"end_turn\",\"error\":{}}\n\n",
    ] {
        corpus.push(event.as_bytes().to_vec());
    }
    for raw in corpus {
        let mut reference = ReferenceWriter::default();
        let mut actual = ClaudeWriter::default();
        let (mut want, mut got) = (BytesMut::new(), BytesMut::new());
        for chunk in raw.chunks(37) {
            actual.write_chunk(Bytes::copy_from_slice(chunk), &mut got);
            reference.write_chunk(Bytes::copy_from_slice(chunk), &mut want);
            assert_eq!(got, want);
        }
        let got_error = actual.close_error();
        let want_error = reference.close_error();
        assert_eq!(
            got_error.as_ref().map(|e| (e.status, &e.text)),
            want_error.as_ref().map(|e| (e.status, &e.text))
        );
        assert_eq!(
            (
                actual.saw_error_event,
                actual.saw_message_stop,
                actual.saw_bare_message
            ),
            (
                reference.saw_error_event,
                reference.saw_message_stop,
                reference.saw_bare_message
            )
        );
        match (got_error, want_error) {
            (Some(got_error), Some(want_error)) => {
                actual.write_terminal_error(&got_error, &mut got);
                reference.write_terminal_error(&want_error, &mut want);
            }
            _ => {
                actual.write_done(&mut got);
                reference.write_done(&mut want);
            }
        }
        assert_eq!(got, want);
    }
}

#[test]
fn reused_observation_buffers_release_large_events() {
    let mut writer = ClaudeWriter::default();
    let mut out = BytesMut::new();
    let event = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"text\":\"{}\"}}\n\n",
        "x".repeat(1 << 20)
    );
    writer.write_chunk(Bytes::from(event), &mut out);
    assert!(writer.line.capacity() <= OBSERVATION_REUSE_LIMIT);
    assert!(writer.data.capacity() <= OBSERVATION_REUSE_LIMIT);
    assert!(writer.event.capacity() <= OBSERVATION_REUSE_LIMIT);
}

struct ReferenceWriter {
    saw_error_event: bool,
    saw_message_stop: bool,
    saw_bare_message: bool,
    trailing_newlines: usize,
    line: Vec<u8>,
    line_overflow: bool,
    event: String,
    data: Vec<u8>,
    observation_overflow: bool,
}

impl Default for ReferenceWriter {
    fn default() -> Self {
        Self {
            saw_error_event: false,
            saw_message_stop: false,
            saw_bare_message: false,
            trailing_newlines: 2,
            line: Vec::new(),
            line_overflow: false,
            event: String::new(),
            data: Vec::new(),
            observation_overflow: false,
        }
    }
}

impl ReferenceWriter {
    fn observe_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        let line = open_ferry_translate::go::trim_space(&line);
        let line_overflow = std::mem::take(&mut self.line_overflow);
        if line.is_empty() && !line_overflow {
            self.flush_event();
            self.event.clear();
            self.observation_overflow = false;
            return;
        }
        if self.observation_overflow {
            // Only a blank line can resume observation after an oversized event.
            return;
        }
        if let Some(event) = line.strip_prefix(b"event:") {
            self.event = String::from_utf8_lossy(event).trim().to_owned();
        } else if let Some(data) = line.strip_prefix(b"data:") {
            let data = open_ferry_translate::go::trim_space(data);
            if self.data.len() + data.len() < MAX_LINE {
                self.data.extend_from_slice(data);
                self.data.push(b'\n');
            } else {
                self.overflow_event();
            }
        } else {
            self.observe_json(line, false);
        }
    }

    fn overflow_event(&mut self) {
        self.observation_overflow = true;
        self.data.clear();
        self.event.clear();
    }

    fn observe_json(&mut self, data: &[u8], sse: bool) {
        let Ok(root) = serde_json::from_slice::<Value>(data) else {
            return;
        };
        match root["type"].as_str().unwrap_or_default() {
            "message_stop" => self.saw_message_stop = true,
            "error" => self.saw_error_event = true,
            "message" => {
                if !sse && !gjson_string(root.get("stop_reason")).trim().is_empty() {
                    self.saw_message_stop = true;
                    self.saw_bare_message = true;
                }
            }
            _ if sse && self.event == "error" && root.get("error").is_some() => {
                self.saw_error_event = true
            }
            _ => {}
        }
    }

    fn flush_event(&mut self) {
        let data = std::mem::take(&mut self.data);
        if !self.observation_overflow && !data.is_empty() {
            self.observe_json(&data, true);
        }
    }

    fn flush_pending(&mut self) {
        if !self.line.is_empty() {
            self.observe_line();
        }
        self.flush_event();
    }

    fn boundary(&mut self, out: &mut BytesMut) {
        if !self.saw_bare_message {
            for _ in self.trailing_newlines..2 {
                out.extend_from_slice(b"\n");
            }
            self.trailing_newlines = 2;
        }
    }
}

impl StreamWriter for ReferenceWriter {
    fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut) {
        for &byte in &chunk {
            match byte {
                b'\r' => {}
                b'\n' => self.trailing_newlines = (self.trailing_newlines + 1).min(2),
                _ => self.trailing_newlines = 0,
            }
            if byte == b'\n' {
                self.observe_line();
            } else if !self.line_overflow {
                if self.line.len() < MAX_LINE {
                    self.line.push(byte);
                } else {
                    self.line_overflow = true;
                    self.overflow_event();
                }
            }
        }
        out.extend_from_slice(&chunk);
    }

    fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut) {
        self.flush_pending();
        self.boundary(out);
        if self.saw_error_event {
            return;
        }
        out.extend_from_slice(b"event: error\ndata: ");
        out.extend_from_slice(claude_error_json(error).as_bytes());
        out.extend_from_slice(b"\n\n");
    }

    fn close_error(&mut self) -> Option<ErrorMessage> {
        self.flush_pending();
        (!self.saw_error_event && !self.saw_message_stop)
            .then(|| ErrorMessage::new(502, "upstream stream closed before message_stop"))
    }

    fn write_done(&mut self, out: &mut BytesMut) {
        self.flush_pending();
        if self.saw_error_event || self.saw_message_stop {
            self.boundary(out);
        }
    }
}
