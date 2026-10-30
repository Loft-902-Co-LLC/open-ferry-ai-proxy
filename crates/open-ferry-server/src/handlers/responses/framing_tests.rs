// Ported from CLIProxyAPI openai_responses_framing_metadata_test.go and
// openai_responses_framing_recovery_test.go (v8.0.47, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use serde_json::{Value, json};

const COMPLETED: &str = "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
const EMPTY_METADATA: &str = "event: response.metadata\ndata:";
const DATA_FIRST: &str = "data: {\"response\":{\"id\":\"resp-1\",\"status\":\"completed\"}}";

fn run(codex: bool, chunks: &[&[u8]]) -> (String, Framer) {
    let mut framer = Framer::new(codex);
    let mut out = BytesMut::new();
    for chunk in chunks {
        framer.write_chunk(&mut out, chunk).unwrap();
    }
    framer.flush(&mut out);
    (String::from_utf8(out.to_vec()).unwrap(), framer)
}

fn assert_upstream_frames(codex: bool, chunks: &[&[u8]], framer: &Framer) {
    let mut oracle = upstream::Upstream::new(codex);
    for chunk in chunks {
        oracle.write_chunk(chunk);
    }
    oracle.flush();
    assert_eq!(framer.frames, oracle.frames);
    assert_eq!(framer.terminal_event, oracle.terminal_event);
}

fn public_completion(chunks: &[&str]) {
    let bytes: Vec<_> = chunks.iter().map(|chunk| chunk.as_bytes()).collect();
    let (got, framer) = run(false, &bytes);
    assert!(
        !got.contains("response.metadata") && !got.contains("private"),
        "{chunks:?}: {got}"
    );
    assert!(!got.contains("event: error"), "{chunks:?}: {got}");
    assert!(got.contains("response.completed"), "{chunks:?}: {got}");
    assert_eq!(
        framer.terminal_event, "response.completed",
        "{chunks:?}: {got}"
    );
}

#[test]
fn metadata_filter_checks_event_and_payload_even_when_malformed() {
    for private in [
        "response.metadata",
        "codex.response.metadata",
        "codex.rate_limits",
    ] {
        for frame in [
            format!("event: {private}\ndata: not-json\n\n"),
            format!("event: {private}\n\n"),
            format!("event: {private}\ndata:\n\n"),
            format!("data: {{\"type\":\"{private}\",\"metadata\":{{\"marker\":\"private\"}}\n\n"),
            format!("event: response.created\ndata: {{\"type\":\"{private}\"}}\n\n"),
            format!("event: {private}\ndata: {{\"type\":\"response.created\"}}\n\n"),
        ] {
            public_completion(&[&frame, COMPLETED]);
        }
    }
}

#[test]
fn fragmented_metadata_data_field_preserves_codex_and_filters_public() {
    let payload = r#"{"type":"response.metadata","metadata":{"marker":"private"}}"#;
    for ending in ["\n", "\r\n"] {
        let prefix = format!("event: response.metadata{ending}data:");
        let frame = format!("{prefix} {payload}{ending}{ending}");
        for split in [prefix.len(), prefix.len() + 1] {
            for codex in [false, true] {
                let (got, _) = run(
                    codex,
                    &[
                        &frame.as_bytes()[..split],
                        &frame.as_bytes()[split..],
                        COMPLETED.as_bytes(),
                    ],
                );
                assert_eq!(
                    got.contains(payload),
                    codex,
                    "split={split} codex={codex}: {got}"
                );
                assert!(got.contains(COMPLETED));
            }
        }
    }
}

#[test]
fn empty_metadata_before_fragmented_fields_and_data_only_completion() {
    for field in [
        "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
        "data: {\"type\":\"response.completed\"}\n\n",
        "id: event-42\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
        "retry: 1000\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
        "x-extra: value\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
    ] {
        let colon = field.find(':').unwrap();
        for split in 1..=colon + 1 {
            public_completion(&[EMPTY_METADATA, &field[..split], &field[split..]]);
        }
    }
    for prefix in [
        "\n",
        "\r\n",
        ": keepalive\n",
        "id: event-42\n",
        "retry: 1000\n",
        "x-extra: value\n",
    ] {
        public_completion(&[EMPTY_METADATA, &format!("{prefix}{COMPLETED}")]);
    }
}

#[test]
fn empty_metadata_before_data_then_fragmented_event() {
    let event = "event: response.completed\n\n";
    for split in 1..=6 {
        public_completion(&[EMPTY_METADATA, DATA_FIRST, &event[..split], &event[split..]]);
    }
    public_completion(&[EMPTY_METADATA, &format!("{DATA_FIRST}\n{event}")]);
    for field in [
        "id: event-42",
        "retry: 1000",
        ": keepalive",
        "x-extra: value",
    ] {
        for split in 1..=field.len() {
            public_completion(&[
                EMPTY_METADATA,
                DATA_FIRST,
                &field[..split],
                &field[split..],
                event,
            ]);
        }
    }
    for field in ["", "id: event-42\n", ": keepalive\n"] {
        for split in 1..6 {
            public_completion(&[
                EMPTY_METADATA,
                &format!("{DATA_FIRST}\n"),
                field,
                &event[..split],
                &event[split..],
            ]);
            public_completion(&[
                EMPTY_METADATA,
                &format!("{DATA_FIRST}\n{field}{}", &event[..split]),
                &event[split..],
            ]);
        }
    }
}

#[test]
fn preserves_empty_custom_event_until_its_delimiter() {
    let mut framer = Framer::new(false);
    let mut out = BytesMut::new();
    framer
        .write_chunk(&mut out, b"event: custom\ndata:")
        .unwrap();
    assert!(out.is_empty());
    framer.write_chunk(&mut out, b"\n\n").unwrap();
    assert_eq!(&out[..], b"event: custom\ndata:\n\n");
}

#[test]
fn field_prefixes_in_json_strings_are_not_sse_fields() {
    for marker in [
        "data: private",
        "event: private",
        "id: private",
        "retry: private",
        ": private",
        "escaped \" data: private",
        "escaped \\ event: private",
    ] {
        let payload = json!({"type":"response.metadata","metadata":{"marker":marker}}).to_string();
        let frame = format!("data: {payload}\n\n");
        // Try every split, including escaped quote/backslash boundaries.
        for split in 1..frame.len() {
            for codex in [false, true] {
                let (got, _) = run(
                    codex,
                    &[
                        &frame.as_bytes()[..split],
                        &frame.as_bytes()[split..],
                        COMPLETED.as_bytes(),
                    ],
                );
                assert_eq!(
                    got.contains(&payload),
                    codex,
                    "marker={marker:?} split={split} codex={codex}: {got}"
                );
                assert!(got.contains(COMPLETED));
            }
        }
    }
}

#[test]
fn data_first_events_without_delimiters_do_not_merge() {
    let chunks: &[&[u8]] = &[
        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}",
        b"event: response.output_text.delta",
        b"data: {\"type\":\"response.completed\",\"response\":{}}",
        b"event: response.completed",
    ];
    for codex in [false, true] {
        let (got, framer) = run(codex, chunks);
        assert_upstream_frames(codex, chunks, &framer);
        assert_eq!(framer.data_frames, 2, "{got}");
        assert_eq!(framer.terminal_event, "response.completed", "{got}");
        assert_eq!(got.matches("\n\n").count(), 2, "{got}");
    }
}

#[test]
fn custom_data_first_event_does_not_swallow_completion() {
    for codex in [false, true] {
        let (got, framer) = run(
            codex,
            &[
                b"data: {}\nevent: custom",
                b"data: {\"type\":\"response.completed\"}\n\n",
            ],
        );
        assert_eq!(
            got,
            "data: {}\nevent: custom\n\ndata: {\"type\":\"response.completed\"}\n\n"
        );
        assert_eq!(framer.terminal_event, "response.completed");
        assert_eq!(framer.data_frames, 2);
    }
}

#[test]
fn repeated_custom_data_first_events_are_emitted_at_each_chunk() {
    let chunk = b"data: {}\nevent: custom";
    let mut framer = Framer::new(false);
    let mut out = BytesMut::new();
    for n in 1..=2000 {
        framer.write_chunk(&mut out, chunk).unwrap();
        assert_eq!(framer.data_frames, n);
        assert_eq!(framer.held(), 0);
        assert!(framer.work() <= 3 * n as usize * chunk.len());
    }
    framer.flush(&mut out);
    assert_eq!(out.as_ref(), b"data: {}\nevent: custom\n\n".repeat(2000));
}

#[test]
fn data_first_fragmented_event_name_matches_upstream_early_emit() {
    for codex in [false, true] {
        let (got, framer) = run(
            codex,
            &[DATA_FIRST.as_bytes(), b"event: respon", b"se.completed\n\n"],
        );
        assert_eq!(
            got,
            format!("{DATA_FIRST}\nevent: respon\n\nse.completed\n\n")
        );
        assert_eq!(framer.terminal_event, "");
    }
}

#[test]
fn terminal_trailing_id_retry_split_matches_upstream_field_loss() {
    let first = b"event: response.completed\ndata: {\"type\":\"response.completed\"}";
    let tail = b"\nid: terminal\nretry: 1000\n\n";
    let wire = [first.as_slice(), tail].concat();
    for codex in [false, true] {
        let (whole, whole_framer) = run(codex, &[&wire]);
        let (split, split_framer) = run(codex, &[first, tail]);
        assert_eq!(whole.as_bytes(), wire);
        assert_eq!(split, format!("{}\n\n", String::from_utf8_lossy(first)));
        assert!(whole.contains("id: terminal\nretry: 1000"));
        assert!(!split.contains("id:") && !split.contains("retry:"));
        assert_upstream_frames(codex, &[&wire], &whole_framer);
        assert_upstream_frames(codex, &[first, tail], &split_framer);
    }
}

#[test]
fn data_first_empty_event_name_split_matches_upstream_no_data_frame() {
    let first = b"data: {\"response\":{\"id\":\"r\"}}\nevent:";
    let tail = b" response.completed\n\n";
    for codex in [false, true] {
        let (out, framer) = run(codex, &[first, tail]);
        assert_eq!(
            out,
            format!(
                "{}\n\n{}",
                String::from_utf8_lossy(first),
                String::from_utf8_lossy(tail)
            )
        );
        assert_eq!(framer.frames.len(), 2);
        assert!(data_payload(&framer.frames[1]).is_none());
        assert_upstream_frames(codex, &[first, tail], &framer);
    }
}

#[test]
fn malformed_public_json_string_does_not_become_an_sse_field() {
    let first = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"";
    for marker in [
        "data: text",
        "event: text",
        "id: text",
        "retry: text",
        ": text",
    ] {
        let last = format!("{marker}\", BAD}}\n\n");
        let expected = [first.as_slice(), last.as_bytes()].concat();
        for codex in [false, true] {
            let (got, _) = run(codex, &[first, last.as_bytes()]);
            assert_eq!(got.as_bytes(), expected, "marker={marker:?} codex={codex}");
        }
    }
}

fn recovery_fixtures() -> Vec<Vec<&'static str>> {
    vec![
        vec![
            r#"data: {"type""#,
            ":\"response.metadata\",\ndata: \"metadata\":{\"marker\":\"private\"}}\n\n",
        ],
        vec![
            r#"data: {"type""#,
            r#":"response.metadata","metadata""#,
            ":{\"marker\":\"private\"}}\n\n",
        ],
        vec![
            r#"data: {"type""#,
            ":\"response.metadata\",\n: keepalive\ndata: \"metadata\":{\"marker\":\"private\"}}\n\n",
        ],
        vec![
            "data",
            ": {\"type\":\"response.metadata\",\"metadata\":{\"marker\":\"private\"}}\n\n",
        ],
        vec![
            r#"data: {"type""#,
            ":\"response.metadata\",\n: keepalive\ndata: \"metadata\"",
            ":{\"marker\":\"private\"}}\n\n",
        ],
        vec![
            r#"data: {"type""#,
            ": keepalive",
            r#"data: :"response.metadata","metadata""#,
            ":{\"marker\":\"private\"}}\n\n",
        ],
        vec![
            r#"data: {"type":"response.metadata","metadata":{"marker""#,
            ":\"",
            "data: private\"}}\n\n",
        ],
        vec![
            r#"data: {"type""#,
            ":",
            "data: :\"response.metadata\",\"metadata\":{\"marker\":\"private\"}}\n\n",
        ],
    ]
}

#[test]
fn recovers_fragmented_json_without_removing_real_comments_or_string_data() {
    for (i, fixture) in recovery_fixtures().iter().enumerate() {
        for codex in [false, true] {
            let mut chunks: Vec<_> = fixture.iter().map(|chunk| chunk.as_bytes()).collect();
            chunks.push(COMPLETED.as_bytes());
            let (got, _) = run(codex, &chunks);
            if codex {
                let frame = got.split("\n\n").next().unwrap();
                let payload = data_payload(frame.as_bytes()).unwrap();
                let value: Value = serde_json::from_slice(&payload)
                    .unwrap_or_else(|error| panic!("fixture={i}: {error}: {got}"));
                assert_eq!(value["type"], "response.metadata", "fixture={i}: {got}");
                assert_eq!(
                    value["metadata"]["marker"],
                    if i == 6 { "data: private" } else { "private" }
                );
                if fixture.iter().any(|chunk| chunk.contains(": keepalive")) {
                    assert!(got.contains(": keepalive"), "fixture={i}: {got}");
                }
            } else {
                assert!(
                    !got.contains("response.metadata") && !got.contains("private"),
                    "fixture={i}: {got}"
                );
            }
            assert!(got.contains(COMPLETED), "fixture={i}: {got}");
        }
    }
}

#[test]
fn recovery_prefix_buffers_obey_the_event_limit() {
    for suffix in [false, true] {
        let mut framer = Framer::new(false).with_limit(64);
        let mut out = BytesMut::new();
        framer
            .write_chunk(&mut out, EMPTY_METADATA.as_bytes())
            .unwrap();
        framer
            .write_chunk(
                &mut out,
                if suffix {
                    b"data: {}"
                } else {
                    b"data: {\"a\":\""
                },
            )
            .unwrap();
        let mut error = None;
        for _ in 0..65 {
            if let Err(found) = framer.write_chunk(&mut out, b"x") {
                error = Some(found);
                break;
            }
            assert!(framer.held() <= 64);
        }
        assert_eq!(error.unwrap().status, 502);
        assert_eq!(framer.held(), 0);
        framer.flush(&mut out);
        assert!(out.is_empty());
    }
}

#[test]
fn recovered_terminal_output_is_rebuilt_exactly_once() {
    let item = "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"once\"}}\n\n";
    let first = "data: {\"type\"";
    let last = ":\"response.completed\",\"response\":{\"output\":[]}}\n\n";
    for codex in [false, true] {
        let (got, framer) = run(
            codex,
            &[
                item.as_bytes(),
                first.as_bytes(),
                last.as_bytes(),
                COMPLETED.as_bytes(),
                item.as_bytes(),
            ],
        );
        let payloads: Vec<Value> = got
            .split("\n\n")
            .filter_map(|frame| data_payload(frame.as_bytes()))
            .map(|data| serde_json::from_slice(&data).unwrap())
            .collect();
        let completed: Vec<_> = payloads
            .iter()
            .filter(|value| value["type"] == "response.completed")
            .collect();
        assert_eq!(completed.len(), 1, "{got}");
        assert_eq!(
            completed[0]["response"]["output"],
            json!([{"type":"message","id":"once"}])
        );
        assert_eq!(framer.terminal_event, "response.completed");
        assert_eq!(framer.data_frames, 2);
    }
}

const PARTITION_INVARIANT: &str = "Every partition matches the independent v8.0.47 oracle's recovered pre-repair frame bytes (including CR/LF, whitespace, field order and repeated fields) and terminal decision. No exceptions or normalization.";

fn partition_streams() -> Vec<Vec<u8>> {
    let mut streams: Vec<Vec<u8>> = vec![
        b"data: {}\n\n".to_vec(),
        b"event: custom\ndata:\n\n".to_vec(),
        b"id: 7\nretry: 1000\ndata: {}\n\n".to_vec(),
        b"data: {\"type\":\"response.metadata\"}\n\n".to_vec(),
        b"event: codex.test\ndata: {}\n\n".to_vec(),
        b"event: response.metadata\ndata: malformed\n\n".to_vec(),
        b"data: {\"a\":\ndata: 1}\n\n".to_vec(),
        COMPLETED.as_bytes().to_vec(),
        format!("data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"message\",\"id\":\"once\"}}}}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"output\":[]}}}}\n\n{COMPLETED}").into_bytes(),
        b"data: {\"type\":\"response.metadata\",\"metadata\":{\"marker\":\"data: private\"}}\r\n\r\n".to_vec(),
        b"data: {\"response\":{\"id\":\"r\"}}\nevent: response.completed\n\n".to_vec(),
        "event: response.output_text.delta\ndata: {\"delta\":\"雪🌉\"}\n\n".as_bytes().to_vec(),
        b": keepalive\nid: seven\nretry: 500\ndata: {}\r\n\r\n".to_vec(),
        b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"failed\"}}}\n\n".to_vec(),
        b"event: response.incomplete\ndata: {\"type\":\"response.incomplete\"}\n\n".to_vec(),
        b"data: {\"type\":\"response.metadata\",\"bad\":\n\n".to_vec(),
        b"event: response.completed\ndata: {\"type\":\"response.completed\"}\nid: terminal\nretry: 1000\n\n".to_vec(),
    ];
    streams.extend([
        b"event: a\ndata:\nfoo\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n".to_vec(),
        b"event: response.completed\ndata:1\n\n".to_vec(),
    ]);
    // Literal recovered-wire forms of recovery_fixtures(), not inputs generated
    // by the production framer during the differential test.
    for wire in [
        "data: {\"type\":\"response.metadata\",\ndata: \"metadata\":{\"marker\":\"private\"}}\n\n",
        "data: {\"type\":\"response.metadata\",\"metadata\":{\"marker\":\"private\"}}\n\n",
        ": keepalive\ndata: {\"type\":\"response.metadata\",\ndata: \"metadata\":{\"marker\":\"private\"}}\n\n",
        "data: {\"type\":\"response.metadata\",\"metadata\":{\"marker\":\"private\"}}\n\n",
        ": keepalive\ndata: {\"type\":\"response.metadata\",\ndata: \"metadata\":{\"marker\":\"private\"}}\n\n",
        ": keepalive\ndata: {\"type\"\ndata: :\"response.metadata\",\"metadata\":{\"marker\":\"private\"}}\n\n",
        "data: {\"type\":\"response.metadata\",\"metadata\":{\"marker\":\"data: private\"}}\n\n",
        "data: {\"type\"\n:\ndata: :\"response.metadata\",\"metadata\":{\"marker\":\"private\"}}\n\n",
    ] {
        if !streams.iter().any(|stream| stream == wire.as_bytes()) {
            streams.push(wire.as_bytes().to_vec());
        }
    }
    streams
}

#[derive(Debug, PartialEq, Eq)]
struct FrameFields {
    event: String,
    data: Option<Vec<u8>>,
    other: Vec<String>,
}

// Client-level fields for terminal repair assertions. Differential comparisons
// use the raw frame bytes and terminal decision instead.
fn frame_fields(frames: &[Vec<u8>]) -> Vec<FrameFields> {
    frames
        .iter()
        .map(|frame| {
            let mut fields = FrameFields {
                event: String::new(),
                data: None,
                other: Vec::new(),
            };
            for line in frame.split(|&byte| byte == b'\n') {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                let trimmed = go::trim_space(line);
                if let Some(event) = trimmed.strip_prefix(b"event:") {
                    fields.event = String::from_utf8_lossy(go::trim_space(event)).into_owned();
                } else if let Some(data) = trimmed.strip_prefix(b"data:") {
                    if let Some(payload) = &mut fields.data {
                        payload.push(b'\n');
                        payload.extend_from_slice(go::trim_space(data));
                    } else {
                        fields.data = Some(go::trim_space(data).to_vec());
                    }
                } else if !line.is_empty() {
                    fields
                        .other
                        .push(String::from_utf8_lossy(line).into_owned());
                }
            }
            fields
        })
        .collect()
}

fn for_each_partition(wire: &[u8], mut check: impl FnMut(&[&[u8]])) {
    check(&wire.chunks(1).collect::<Vec<_>>());
    let mut boundaries = std::collections::BTreeSet::from([0, wire.len()]);
    for (i, &byte) in wire.iter().enumerate() {
        if byte == b':' || i == 0 || wire[i - 1] == b'\n' {
            for cut in i.saturating_sub(1)..=(i + 1).min(wire.len()) {
                boundaries.insert(cut);
            }
        }
    }
    for a in 0..=wire.len() {
        check(&[&wire[..a], &wire[a..]]);
        if wire.len() <= 60 {
            for b in a..=wire.len() {
                check(&[&wire[..a], &wire[a..b], &wire[b..]]);
            }
        } else if boundaries.contains(&a) {
            for &b in boundaries.range(a..) {
                check(&[&wire[..a], &wire[a..b], &wire[b..]]);
            }
        }
    }
}

// Post-repair client assertions, separate from the pre-repair differential
// family. Ported fixtures: openai_responses_handlers_stream_test.go v8.0.47,
// TestResponsesSSEFramer{FlushesMultilineDataWithoutDelimiter,
// UsesPayloadErrorOverCompletedEvent,UsesErrorEventOverPayloadType}.
#[test]
fn terminal_repair_matches_upstream_precedence_fixtures() {
    let fixtures = [
        (
            "multiline-completed",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\ndata: \"response\":{\"id\":\"resp-1\",\"status\":\"completed\"}}",
            "response.completed",
            None,
        ),
        (
            "payload-failure-over-completed",
            "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\"}}\nevent: response.completed\n\n",
            "response.failed",
            None,
        ),
        (
            "error-event-over-payload-type",
            "event: error\ndata: {\"type\":\"provider.error\",\"message\":\"failed\"}\n\n",
            "error",
            Some("failed"),
        ),
        (
            "nested-response-error",
            "data: {\"response\":{\"error\":{\"message\":\"failed\"}}}\n\n",
            "error",
            Some("failed"),
        ),
    ];
    for (name, input, terminal, message) in fixtures {
        for codex in [false, true] {
            let chunks = [
                input.as_bytes(),
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"late\"}\n\n",
            ];
            let (out, framer) = run(codex, &chunks);
            assert_upstream_frames(codex, &chunks, &framer);
            let expected = if codex && terminal == "error" {
                "response.failed"
            } else {
                terminal
            };
            assert_eq!(
                framer.terminal_event, expected,
                "{name} codex={codex}: {out}"
            );
            assert_eq!(framer.data_frames, 1, "{name}: {out}");
            let events = frame_fields(&client_frames(&out));
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event, expected, "{name}: {out}");
            let payload: Value = serde_json::from_slice(events[0].data.as_ref().unwrap()).unwrap();
            assert_eq!(payload["type"], expected, "{name}: {out}");
            if let Some(message) = message {
                assert_eq!(
                    if codex {
                        &payload["response"]["error"]["message"]
                    } else {
                        &payload["error"]["message"]
                    },
                    message,
                    "{name}: {out}"
                );
            }
            if name == "multiline-completed" {
                assert_eq!(
                    payload["response"],
                    json!({"id":"resp-1","status":"completed"})
                );
                assert_eq!(
                    out.as_bytes(),
                    upstream::wire(&[input.as_bytes().to_vec()]).as_ref()
                );
            } else if name == "payload-failure-over-completed" {
                assert_eq!(payload["response"]["status"], "failed");
                assert!(!out.contains("event: response.completed"));
            }
            assert!(!out.contains("late"), "{name}: {out}");
        }
    }
}

// Client-level output reconstruction from upstream's
// TestForwardResponsesStreamRepairs{MixedIndexedAndUnindexedDoneItems,
// MultilineCompletedOutputAsSSEDataLines}; expected JSON is a literal fixture,
// not the oracle's deliberately unmodelled repairFrame output.
#[test]
fn terminal_repair_matches_upstream_mixed_output_fixture() {
    let chunks: &[&[u8]] = &[
        b"data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc-1\",\"call_id\":\"call-1\",\"name\":\"shell\",\"arguments\":\"{}\",\"status\":\"completed\"}}",
        b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"msg-1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}",
        b"data: {\"type\":\"response.completed\",\ndata: \"response\":{\"id\":\"resp-1\",\"output\":[]}}\n\n",
        COMPLETED.as_bytes(),
    ];
    let expected = json!({"type":"response.completed","response":{"id":"resp-1","output":[
        {"type":"function_call","id":"fc-1","call_id":"call-1","name":"shell","arguments":"{}","status":"completed"},
        {"type":"message","id":"msg-1","role":"assistant","content":[{"type":"output_text","text":"done"}]}
    ]}});
    for codex in [false, true] {
        let (out, framer) = run(codex, chunks);
        assert_upstream_frames(codex, chunks, &framer);
        assert_eq!(framer.terminal_event, "response.completed");
        assert_eq!(framer.data_frames, 3);
        let frames: Vec<_> = out
            .trim_end()
            .split("\n\n")
            .map(|frame| frame.as_bytes().to_vec())
            .collect();
        assert_eq!(frames.len(), 3, "{out}");
        assert!(
            frames[2]
                .split(|&byte| byte == b'\n')
                .all(|line| line.starts_with(b"data: "))
        );
        let events = frame_fields(&frames);
        let completed: Value = serde_json::from_slice(events[2].data.as_ref().unwrap()).unwrap();
        assert_eq!(completed, expected);
    }
}

fn client_frames(out: &str) -> Vec<Vec<u8>> {
    out.replace("\r\n", "\n")
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| frame.as_bytes().to_vec())
        .collect()
}

// Additional delta streams use the same exact upstream differential contract.
#[test]
fn complete_delta_streams_match_upstream_for_every_partition() {
    let delta = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n\n";
    let streams = [
        "event: delta\ndata: {\"delta\":\"a\"}\n\n".to_owned(),
        delta.to_owned(),
        "event: response.output_text.delta\ndata: {\"delta\":\"雪🌉\"}\n\n".to_owned(),
        format!(": keepalive\nid: seven\nretry: 500\n{delta}"),
        format!("{delta}{delta}{COMPLETED}"),
    ];
    for wire in &streams {
        for codex in [false, true] {
            for_each_partition(wire.as_bytes(), |chunks| {
                let (_, framer) = run(codex, chunks);
                assert_upstream_frames(codex, chunks, &framer);
            });
        }
    }
}

fn framing_matches(framer: &Framer, oracle: &upstream::Upstream) -> bool {
    framer.frames == oracle.frames && framer.terminal_event == oracle.terminal_event
}

#[test]
fn every_partition_matches_upstream_framing_exactly() {
    let started = std::time::Instant::now();
    let streams = partition_streams();
    let mut partitions = 0;
    let mut mismatches = Vec::new();
    for (i, wire) in streams.iter().enumerate() {
        for codex in [false, true] {
            for_each_partition(wire, |chunks| {
                let (_, framer) = run(codex, chunks);
                let mut oracle = upstream::Upstream::new(codex);
                for chunk in chunks {
                    oracle.write_chunk(chunk);
                }
                oracle.flush();
                if !framing_matches(&framer, &oracle) {
                    mismatches.push((wire.len(), format!(
                            "stream={i} codex={codex} lengths={:?} chunks={:?}\ngot={:?} terminal={:?}\nwant={:?} terminal={:?}",
                            chunks.iter().map(|c| c.len()).collect::<Vec<_>>(),
                            chunks.iter().map(|c| String::from_utf8_lossy(c)).collect::<Vec<_>>(),
                            framer.frames.iter().map(|c| String::from_utf8_lossy(c)).collect::<Vec<_>>(), framer.terminal_event,
                            oracle.frames.iter().map(|c| String::from_utf8_lossy(c)).collect::<Vec<_>>(), oracle.terminal_event
                        )));
                }
                partitions += 1;
            });
        }
    }
    eprintln!(
        "differential invariant: {} streams, {partitions} cases, {} mismatches, {:?}",
        streams.len(),
        mismatches.len(),
        started.elapsed()
    );
    mismatches.sort();
    assert!(
        mismatches.is_empty(),
        "{PARTITION_INVARIANT}\nsmallest mismatches: {:#?}",
        &mismatches[..mismatches.len().min(8)]
    );
}

fn assert_r3_split_fields(index: usize) {
    let fixtures: &[&[&[u8]]] = &[
        &[
            b"data",
            b": {\"response\":{\"id\":\"r\"}}\nevent:",
            b" response.completed\n\n",
        ],
        &[
            b"data",
            b": {\"response\":{\"id\":\"r\"}}\nevent: respon",
            b"se.completed\n\n",
        ],
        &[
            b"data",
            b": {\"marker\":\"private\"}\nevent: response.",
            b"metadata\n\n",
        ],
        &[
            b"event: response.completed\ndata",
            b": {\"type\":\"response.completed\"}",
            b"\nid: terminal\nretry: 1000\n\n",
        ],
        &[b": keepalive\nid: seven\nretry: 500\ndata", b": {}\r\n\r\n"],
    ];
    let chunks = fixtures[index];
    for codex in [false, true] {
        let (_, framer) = run(codex, chunks);
        assert_upstream_frames(codex, chunks, &framer);
    }
}

#[test]
fn r3_already_visible_malformed_private_type_matches_upstream() {
    let fixtures: &[(&[u8], &[u8])] = &[
        (b"data: {\"type\":\"response.metadata\",\"bad\"", b":\n\n"),
        (b"data: {\"type\":\" response.metadata \",\"bad\"", b":\n\n"),
        (
            b"event: response.metadata\ndata: {\"type\"",
            b":\"response.metadata\",\"bad\":\n\n",
        ),
    ];
    for &(prefix, suffix) in fixtures {
        let chunks: &[&[u8]] = &[prefix, suffix];
        for codex in [false, true] {
            let (_, framer) = run(codex, chunks);
            assert_upstream_frames(codex, chunks, &framer);
        }
    }
}

#[test]
fn malformed_private_type_keeps_upstream_bytes_for_both_clients() {
    let chunks: &[&[u8]] = &[b"data: {\"type\"", b":\"response.metadata\",\"bad\":}\n\n"];
    for codex in [false, true] {
        let (out, framer) = run(codex, chunks);
        assert_upstream_frames(codex, chunks, &framer);
        assert_eq!(out.as_bytes(), upstream::wire(&framer.frames).as_ref());
    }
}

#[test]
fn r3_long_partitions_cover_field_boundaries() {
    let wire = &partition_streams()[16];
    let mut found = false;
    let a = wire.windows(5).position(|w| w == b"data:").unwrap() + 4;
    let b = wire.windows(3).position(|w| w == b"}\ni").unwrap() + 1;
    for_each_partition(wire, |chunks| {
        found |= chunks.len() == 3 && chunks[0].len() == a && chunks[1].len() == b - a;
    });
    assert!(found);
    let streams = partition_streams();
    let unique: std::collections::HashSet<_> = streams.iter().collect();
    assert_eq!(streams.len(), unique.len());
}

#[test]
fn r3_comparison_preserves_all_frame_bytes_and_terminal_decision() {
    let input = b"event: a\nevent: b\ndata: {}\n\n";
    let (_, framer) = run(false, &[input]);
    for changed in [
        &b"event: a\nevent: c\ndata: {}\n\n"[..],
        &b"event: a\r\nevent: b\r\ndata: {}\r\n\r\n"[..],
        &b"event: a\nevent: b\ndata:  {}\n\n"[..],
        &b"event: a\ndata: {}\nevent: b\n\n"[..],
    ] {
        let mut oracle = upstream::Upstream::new(false);
        oracle.write_chunk(changed);
        oracle.flush();
        assert!(!framing_matches(&framer, &oracle));
    }
    let mut oracle = upstream::Upstream::new(false);
    oracle.write_chunk(input);
    oracle.flush();
    assert!(framing_matches(&framer, &oracle));
    oracle.terminal_event = "response.completed".into();
    assert!(!framing_matches(&framer, &oracle));
}

#[test]
fn r3_split_empty_event_completion_matches_upstream() {
    assert_r3_split_fields(0);
}

#[test]
fn r3_split_partial_event_completion_matches_upstream() {
    assert_r3_split_fields(1);
}

#[test]
fn r3_split_private_event_matches_upstream() {
    assert_r3_split_fields(2);
}

#[test]
fn r3_split_terminal_trailing_fields_matches_upstream() {
    assert_r3_split_fields(3);
}

#[test]
fn r3_split_crlf_matches_upstream() {
    assert_r3_split_fields(4);
}

#[test]
fn empty_data_before_bare_word_matches_upstream_frame_split() {
    let chunks: &[&[u8]] = &[
        b"event: a\ndata:",
        b"\nfoo\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
    ];
    for codex in [false, true] {
        let (_, framer) = run(codex, chunks);
        assert_upstream_frames(codex, chunks, &framer);
        assert_eq!(framer.frames.len(), 2);
        assert_eq!(framer.frames[0], b"event: a\ndata:");
        assert_eq!(framer.terminal_event, "response.completed");
    }
}

#[test]
fn empty_completed_data_before_scalar_matches_upstream_no_terminal() {
    let chunks: &[&[u8]] = &[b"event: response.completed\ndata:", b"1\n\n"];
    for codex in [false, true] {
        let (_, framer) = run(codex, chunks);
        assert_upstream_frames(codex, chunks, &framer);
        assert_eq!(framer.frames.len(), 2);
        assert!(framer.terminal_event.is_empty());
    }
}
