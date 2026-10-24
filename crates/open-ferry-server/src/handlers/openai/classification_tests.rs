use super::*;
use http_body_util::BodyExt;

/// Includes payloads from the writer and bootstrap tests, plus malformed and
/// duplicate-key cases whose permissive lookup behavior must survive.
pub(crate) fn corpus() -> Vec<Vec<u8>> {
    let mut corpus: Vec<Vec<u8>> = [
        "",
        "data:",
        "data: ",
        "data: [DONE]",
        "data:{}",
        "data: {}\r\n\r\n",
        r#"{"choices":[{"delta":{"content":"Partial"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0},{"finish_reason":"length"}]}"#,
        r#"{"choices":{"finish_reason":"stop"}}"#,
        r#"{"choices":[{"finish_reason":false}]}"#,
        r#"{"choices":[{"finish_reason":""}]}"#,
        r#"{"choices":[{"finish_reason":null}]}"#,
        r#"{"choices":[],"error":{"message":"ignored"}}"#,
        r#"{"choices":null,"error":{"message":"ignored"}}"#,
        r#"{"choices":[{"finish_reason":"stop"}],"error":{"message":"ignored"}}"#,
        r#"{"error":"string"}"#,
        r#"{"error":null}"#,
        r#"{"error":{}}"#,
        r#"{"error":{"message":"context","code":"context_length_exceeded"}}"#,
        r#"{"error":{"message":false}}"#,
        r#"{"error":{"message":null}}"#,
        r#"{"error":{"message":""}}"#,
        r#"{"error":{},"error":"ignored"}"#,
        r#"{"error":"first","error":{"message":"ignored"}}"#,
        r#"{"\u0063hoices":[{"finish_reason":"stop"}]}"#,
        r#"{"choices":[{"finish_reason":"stop","finish_reason":null}]}"#,
        r#"{"choices":[],"choices":[{"finish_reason":"stop"}]}"#,
        r#"{"choices":[{"finish_reason":"stop"}],broken"#,
        r#"{"error":{},"truncated":{ "#,
        r#"{"choices":[{"finish_reason":"stop"}]"#,
        r#"{"error":{"message":"truncated"}"#,
        r#"{"type":"message_stop"}"#,
        r#"{"type":"error","error":{}}"#,
        r#"{"type":"ping"}"#,
        "data: {\"choices\":\ndata: [{\"finish_reason\":\"stop\"}]}\n\n",
        "data: {broken\r\n",
        "{\"choices\":[",
        "{\"error\":",
    ]
    .into_iter()
    .map(|s| s.as_bytes().to_vec())
    .collect();
    let fixtures = corpus.clone();
    for raw in fixtures {
        for prefix in [&b"data:"[..], b"data: ", b" \r\ndata: \t"] {
            let mut event = prefix.to_vec();
            event.extend_from_slice(&raw);
            event.extend_from_slice(b"\r\n");
            corpus.push(event);
        }
    }
    let mut large = br#"{"choices":[{"delta":{"content":""#.to_vec();
    large.resize(large.len() + (2 << 20), b'x');
    large.extend_from_slice(br#""},"finish_reason":null}]}"#);
    corpus.push(large);
    // Deterministic malformed payloads exercise the raw scanner's edge cases.
    let tokens = [
        "{",
        "}",
        "[",
        "]",
        "\"choices\":",
        "\"error\":",
        "\"finish_reason\":",
        "null",
        "false",
        "\"stop\"",
        ",",
        ":",
        " ",
        "\n",
        "\"\\u0065rror\":",
    ];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..2000 {
        let mut raw = Vec::new();
        for _ in 0..20 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            raw.extend_from_slice(tokens[seed as usize % tokens.len()].as_bytes());
        }
        corpus.push(raw);
    }
    corpus
}

#[test]
fn shared_classification_matches_original_lookups() {
    for raw in corpus() {
        let got = classify_chunk(&raw);
        let want = chunk_terminal_error(&raw);
        assert_eq!(
            got.terminal_error.as_ref().map(|e| (e.status, &e.text)),
            want.as_ref().map(|e| (e.status, &e.text)),
            "payload length {}",
            raw.len()
        );
        assert_eq!(
            got.finish_reason,
            chunk_has_finish_reason(&raw),
            "payload length {}",
            raw.len()
        );
    }
}

#[derive(Default)]
struct ReferenceWriter(ChatWriter);

impl StreamWriter for ReferenceWriter {
    fn write_chunk(&mut self, chunk: Bytes, out: &mut BytesMut) {
        if let Some(error) = chunk_terminal_error(&chunk) {
            self.0.terminal_chunk_error = Some(error);
            self.0.saw_finish_reason = true;
        } else if !self.0.saw_finish_reason && chunk_has_finish_reason(&chunk) {
            self.0.saw_finish_reason = true;
        }
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(&chunk);
        out.extend_from_slice(b"\n\n");
    }
    fn chunk_error(&mut self) -> Option<ErrorMessage> {
        self.0.chunk_error()
    }
    fn write_terminal_error(&mut self, error: &ErrorMessage, out: &mut BytesMut) {
        self.0.write_terminal_error(error, out);
    }
    fn close_error(&mut self) -> Option<ErrorMessage> {
        self.0.close_error()
    }
    fn write_done(&mut self, out: &mut BytesMut) {
        self.0.write_done(out);
    }
}

#[tokio::test]
async fn shared_classification_preserves_complete_stream_bytes() {
    let partial = Bytes::from_static(
        br#"{"choices":[{"delta":{"content":"Partial"},"finish_reason":null}]}"#,
    );
    for raw in corpus() {
        for failure in [false, true] {
            let items = || {
                let mut items = vec![Ok(partial.clone()), Ok(Bytes::copy_from_slice(&raw))];
                if failure {
                    items.push(Err(ErrorMessage::new(502, "upstream read failed")));
                }
                items.push(Ok(Bytes::from_static(
                    br#"{"choices":[{"finish_reason":"stop"}]}"#,
                )));
                stream::iter(items).boxed()
            };
            let got = forward(items(), ChatWriter::default(), None)
                .collect()
                .await
                .unwrap()
                .to_bytes();
            let want = forward(items(), ReferenceWriter::default(), None)
                .collect()
                .await
                .unwrap()
                .to_bytes();
            assert_eq!(got, want, "payload length {}, failure {failure}", raw.len());
        }
    }
}
