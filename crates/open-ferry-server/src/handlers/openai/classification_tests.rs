use super::*;
use http_body_util::BodyExt;

#[test]
fn malformed_terminal_error_message_uses_document_lookup() {
    let chunk = br#"{"error":{"x":a"b,"message":"}tail"}"#;
    let want = chunk_terminal_error(chunk).unwrap();
    assert_eq!((want.status, want.text.as_str()), (400, "}tail"));
    let got = classify_chunk(chunk, true).terminal_error.unwrap();
    assert_eq!((got.status, got.text), (want.status, want.text));
}

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
        r#"{"error":{"x":a"b,"message":"}tail"}"#,
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
    corpus.extend(random_corpus());
    corpus
}

/// Stratified contexts make the random fragments reach the fields we classify,
/// while also exercising unrestricted malformed documents.
fn random_corpus() -> Vec<Vec<u8>> {
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
        fn pick<'a>(&mut self, values: &[&'a [u8]]) -> &'a [u8] {
            values[self.below(values.len())]
        }
    }
    let tokens: &[&[u8]] = &[
        b"{",
        b"}",
        b"[",
        b"]",
        b"\"choices\":",
        b"\"error\":",
        b"\"message\":",
        b"\"error\"",
        b"\"message\"",
        b"\"finish_reason\":",
        b"null",
        b"false",
        b"\"stop\"",
        b",",
        b":",
        b" ",
        b"\n",
        b"\"\\u0065rror\":",
        b"a",
        b"b",
        b"x",
        b"a\"b",
        b"\"",
        b"{\"error\":{",
        b"{\"message\":",
        b"\x0b",
        b"\xc2\xa0",
        b"\xff",
    ];
    let errors: &[&[u8]] = &[
        br#"{"message":"failure"}"#,
        br#"{"message":null}"#,
        br#"{"message":"","error":{"message":"nested"}}"#,
        br#"{"inner":{"error":{"message":"nested"}},"message":"outer"}"#,
        br#"{"x":a"b,"message":"}tail"}"#,
        b"{\"message\":\"\xff\"}",
        b"{}",
        b"null",
        br#""string""#,
        b"x",
    ];
    let choices: &[&[u8]] = &[
        br#"[{"finish_reason":null}]"#,
        br#"[{"finish_reason":""}]"#,
        br#"[{"finish_reason":"stop"}]"#,
        br#"[{"finish_reason":false}]"#,
        br#"{"finish_reason":"length"}"#,
        b"[]",
        b"null",
        br#"[{"delta":{"content":"text"}}]"#,
    ];
    let messages: &[&[u8]] = &[
        br#""}tail""#,
        br#""failure""#,
        br#""""#,
        b"null",
        b"false",
        br#"{"error":{"message":"nested"}}"#,
        b"a\"b",
        b"\"",
        b"\xff",
    ];
    let nested: &[&[u8]] = &[
        b"{}",
        br#"{"error":{"message":"nested"}}"#,
        br#"{"x":a"b}"#,
        b"{\"x\":\x0bnull}",
        b"{\"x\":\xc2\xa0false}",
        b"{\"x\":\xff}",
    ];
    let atoms: &[&[u8]] = &[b"a\"b", b"a", b"b", b"\"", b"\x0b", b"\xc2\xa0", b"\xff"];
    let prefixes: &[&[u8]] = &[
        b"",
        b"data:",
        b"data: ",
        b"\r\n data:\t",
        b"\x0bdata:",
        b"\xc2\xa0data: ",
    ];
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut corpus = Vec::new();
    for index in 0..10_000 {
        let mut fragment = Vec::new();
        for _ in 0..1 + rng.below(20) {
            fragment.extend_from_slice(rng.pick(tokens));
        }
        let mut raw = Vec::new();
        match index % 8 {
            0 => raw.extend_from_slice(&fragment),
            1 => {
                raw.extend_from_slice(b"{\"error\":");
                raw.extend_from_slice(rng.pick(errors));
                raw.push(b'}');
            }
            2 => {
                raw.extend_from_slice(b"{\"choices\":");
                raw.extend_from_slice(rng.pick(choices));
                raw.push(b'}');
            }
            3 => {
                raw.extend_from_slice(b"{\"error\":");
                raw.extend_from_slice(rng.pick(errors));
                raw.extend_from_slice(b",\"choices\":");
                raw.extend_from_slice(rng.pick(choices));
                raw.push(b'}');
            }
            4 => {
                raw.extend_from_slice(b"{\"choices\":");
                raw.extend_from_slice(rng.pick(choices));
                raw.extend_from_slice(b",\"error\":");
                raw.extend_from_slice(rng.pick(errors));
                raw.extend_from_slice(b",\"choices\":null}");
            }
            5 => {
                raw.extend_from_slice(b"{\"error\":{\"x\":");
                raw.extend_from_slice(rng.pick(atoms));
                raw.extend_from_slice(b",\"message\":");
                raw.extend_from_slice(rng.pick(messages));
                raw.extend_from_slice(b"}}");
            }
            6 => {
                raw.extend_from_slice(b"{\"error\":{\"nested\":");
                raw.extend_from_slice(rng.pick(nested));
                raw.extend_from_slice(b",\"message\":");
                raw.extend_from_slice(rng.pick(messages));
                raw.extend_from_slice(b"}}");
            }
            _ => {
                raw.extend_from_slice(b"{\"padding\":");
                raw.extend_from_slice(rng.pick(atoms));
                raw.extend_from_slice(b",\"choices\":");
                raw.extend_from_slice(rng.pick(choices));
                raw.extend_from_slice(b",\"error\":");
                raw.extend_from_slice(rng.pick(errors));
                raw.push(b'}');
                raw.extend_from_slice(&fragment);
            }
        }
        if rng.below(8) == 0 {
            raw.truncate(rng.below(raw.len() + 1));
        }
        let mut event = rng.pick(prefixes).to_vec();
        event.extend_from_slice(&raw);
        event.extend_from_slice(rng.pick(&[b"", b"\r\n", b",broken", b" garbage"]));
        corpus.push(event);
    }
    corpus
}

#[test]
fn randomized_corpus_reaches_classification_fields() {
    let samples = random_corpus();
    let (
        mut choices,
        mut error,
        mut error_object,
        mut error_message,
        mut finish_reason,
        mut terminal_error,
    ) = (0, 0, 0, 0, 0, 0);
    for event in &samples {
        let payload = go::trim_space(event);
        let payload = payload
            .strip_prefix(b"data:")
            .map(go::trim_space)
            .unwrap_or(payload);
        let found_choices = json::get(payload, "choices");
        let found_error = json::get(payload, "error");
        choices += usize::from(found_choices.is_some());
        error += usize::from(found_error.is_some());
        error_object += usize::from(found_error.is_some_and(|value| value.is_object()));
        error_message += usize::from(json::get(payload, "error.message").is_some());
        finish_reason += usize::from(found_choices.is_some_and(|value| {
            value
                .array()
                .iter()
                .any(|choice| choice.get("finish_reason").is_some())
        }));
        terminal_error += usize::from(chunk_terminal_error(event).is_some());
    }
    println!(
        "random coverage: samples={} choices={choices} error={error} error_object={error_object} error_message={error_message} finish_reason={finish_reason} terminal_error={terminal_error}",
        samples.len()
    );
    assert!(choices >= 2000 && error >= 3000 && error_object >= 1500);
    assert!(error_message >= 1000 && finish_reason >= 1000);
}

#[test]
fn shared_classification_matches_original_lookups() {
    for raw in corpus() {
        let got = classify_chunk(&raw, true);
        let want = chunk_terminal_error(&raw);
        let finished = classify_chunk(&raw, false);
        assert_eq!(
            finished
                .terminal_error
                .as_ref()
                .map(|e| (e.status, &e.text)),
            want.as_ref().map(|e| (e.status, &e.text))
        );
        assert_eq!(finished.finish_reason, want.is_some());
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
