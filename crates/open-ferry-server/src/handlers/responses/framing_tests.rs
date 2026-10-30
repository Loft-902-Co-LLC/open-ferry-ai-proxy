// Ported from CLIProxyAPI openai_responses_framing_metadata_test.go and
// openai_responses_framing_recovery_test.go (v8.0.47, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use serde_json::{Value, json};

const COMPLETED: &str = "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
const EMPTY_METADATA: &str = "event: response.metadata\ndata:";
const DATA_FIRST: &str = "data: {\"response\":{\"id\":\"resp-1\",\"status\":\"completed\"}}";
const SPLIT_FIELD: &str = "responses-sse-split-standard-field";
const BARE_WORD: &str = "responses-sse-bare-word-data-continuation";
const PRIVATE_RECOVERY: &str = "responses-sse-malformed-private-recovery";

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
fn fragmented_standard_data_field_preserves_malformed_payload() {
    for codex in [false, true] {
        let (got, _) = run(codex, &[b"data", b": not-json\n\n"]);
        assert_eq!(got, "data: not-json\n\n");
    }
}

// Exact expected differences from the literal v8.0.47 oracle. No catch-all
// exception: each retained difference has a concrete corrupted/lost event.
#[test]
fn upstream_expected_framing_differences() {
    struct Difference {
        name: &'static str,
        reason: &'static str,
        chunks: &'static [&'static [u8]],
        upstream: &'static [&'static [u8]],
        retained: &'static [&'static [u8]],
    }
    let differences = [
        Difference {
            name: "responses-sse-split-standard-field",
            reason: "upstream turns the data-field colon into a comment, losing the data event",
            chunks: &[b"data", b": not-json\n\n"],
            upstream: &[b"data\n: not-json\n\n"],
            retained: &[b"data: not-json\n\n"],
        },
        Difference {
            name: "responses-sse-bare-word-data-continuation",
            reason: "upstream emits empty custom data and separates its malformed payload into a no-data frame",
            chunks: &[b"event: custom\ndata:", b"bad\n\n"],
            upstream: &[b"event: custom\ndata:", b"bad\n\n"],
            retained: &[b"event: custom\ndata:bad\n\n"],
        },
        Difference {
            name: "responses-sse-malformed-private-recovery",
            reason: "upstream inserts a comment before the type value and leaks private metadata; Codex receives corrupted bytes",
            chunks: &[b"data: {\"type\"", b":\"response.metadata\",\"bad\":}\n\n"],
            upstream: &[b"data: {\"type\"\n:\"response.metadata\",\"bad\":}\n\n"],
            retained: &[b"data: {\"type\":\"response.metadata\",\"bad\":}\n\n"],
        },
    ];
    for difference in differences {
        for codex in [false, true] {
            let mut oracle = upstream::Upstream::new(codex);
            for chunk in difference.chunks {
                oracle.write_chunk(chunk);
            }
            oracle.flush();
            let (out, framer) = run(codex, difference.chunks);
            assert_eq!(
                oracle.frames, difference.upstream,
                "{}: {}",
                difference.name, difference.reason
            );
            assert_eq!(
                framer.frames, difference.retained,
                "{}: {}",
                difference.name, difference.reason
            );
            assert_ne!(oracle.frames, framer.frames, "{}", difference.name);
            if difference.name == "responses-sse-malformed-private-recovery" {
                assert_eq!(out.is_empty(), !codex);
                if codex {
                    assert_eq!(out.as_bytes(), difference.retained[0]);
                }
                assert!(
                    String::from_utf8_lossy(&upstream::wire(&oracle.frames))
                        .contains("response.metadata")
                );
            } else {
                assert_eq!(out.as_bytes(), upstream::wire(&framer.frames).as_ref());
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

const PARTITION_INVARIANT: &str = "Every partition matches the independent v8.0.47 oracle's recovered pre-repair frames (event, data, id, retry, comments and no-data frames) and terminal decision, except explicitly listed fixtures under the three named differences.";

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
        streams.push(wire.as_bytes().to_vec());
    }
    streams
}

#[derive(Debug, PartialEq, Eq)]
struct FrameFields {
    event: String,
    data: Option<Vec<u8>>,
    other: Vec<String>,
}

// A framing-level view only: no repair/filter helpers are called. Keep every
// frame, including ones without data, and preserve id/retry/comments verbatim.
fn frame_fields(frames: &[Vec<u8>]) -> Vec<FrameFields> {
    frames
        .iter()
        .map(|frame| {
            let mut fields = FrameFields {
                event: String::new(),
                data: None,
                other: Vec::new(),
            };
            let mut event_seen = false;
            for line in frame.split(|&byte| byte == b'\n') {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                let trimmed = go::trim_space(line);
                if let Some(event) = trimmed.strip_prefix(b"event:") {
                    if !event_seen {
                        fields.event = String::from_utf8_lossy(go::trim_space(event)).into_owned();
                        event_seen = true;
                    }
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
    for a in 0..=wire.len() {
        check(&[&wire[..a], &wire[a..]]);
        if wire.len() <= 60 {
            for b in a..=wire.len() {
                check(&[&wire[..a], &wire[a..b], &wire[b..]]);
            }
        }
    }
}

struct PartitionDifference {
    names: &'static [&'static str],
    fixture: String,
    stream: usize,
    upstream: Vec<Vec<u8>>,
    retained: Vec<Vec<u8>>,
}

// Exact trace pairs, scoped to individual family fixtures. This is not a list
// of streams to skip: every exceptional partition must match both snapshots
// and both independently calculated terminal decisions. No oracle mode changes.
fn partition_differences(streams: &[Vec<u8>]) -> Vec<PartitionDifference> {
    let mut differences = Vec::new();
    let mut add = |names: &'static [&'static str], stream, fixture: String, upstream, retained| {
        differences.push(PartitionDifference {
            names,
            fixture,
            stream,
            upstream,
            retained,
        });
    };
    // Field-colon splits. The masks enumerate only the named field occurrences
    // in each fixture; both single-field and bytewise combinations are explicit.
    for (stream, fields) in [
        (1, &["event", "data"][..]),
        (2, &["id", "retry"][..]),
        (5, &["event", "data"][..]),
        (7, &["event"][..]),
        (11, &["event"][..]),
        (12, &["id", "retry"][..]),
        (13, &["event"][..]),
        (14, &["event"][..]),
        (16, &["event"][..]),
    ] {
        for mask in 1..1 << fields.len() {
            let mut broken = String::from_utf8(streams[stream].clone()).unwrap();
            for (i, field) in fields.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    broken = broken.replacen(&format!("{field}:"), &format!("{field}\n:"), 1);
                }
            }
            let mut endings = vec![broken.as_bytes().to_vec()];
            if stream == 12 {
                endings.push(broken.replace("\r\n", "\n").into_bytes());
            }
            for (ending, upstream) in endings.into_iter().enumerate() {
                add(
                    &[SPLIT_FIELD],
                    stream,
                    format!("family-{stream}/field-mask-{mask}/ending-{ending}"),
                    vec![upstream],
                    vec![streams[stream].clone()],
                );
            }
        }
    }
    // A split data colon delays upstream emission until its recovery at the
    // delimiter. Retaining the field can emit sooner, including an empty frame.
    let wire = &streams[4];
    let broken = String::from_utf8(wire.clone())
        .unwrap()
        .replacen("event:", "event\n:", 1)
        .into_bytes();
    for ending in 0..=2 {
        add(
            &[SPLIT_FIELD],
            4,
            format!("codex-event/early-ending-{ending}"),
            vec![broken.clone()],
            vec![wire[..wire.len() - 2 + ending].to_vec()],
        );
    }
    for (recovery, upstream) in [broken, wire.clone()].into_iter().enumerate() {
        add(
            &[SPLIT_FIELD],
            4,
            format!("codex-event/early-plus-blank/recovery-{recovery}"),
            vec![upstream],
            vec![wire[..wire.len() - 2].to_vec(), b"\n\n".to_vec()],
        );
    }
    // Data-first completion: splitting either its data or event field changes
    // when upstream can emit. Enumerate every prefix of this particular name,
    // rather than accepting arbitrary partial event names or terminal losses.
    let wire = &streams[10];
    let text = String::from_utf8(wire.clone()).unwrap();
    let event_colon = text.find("event:").unwrap() + "event:".len();
    let event_end = wire.len() - 2;
    let broken_event = text.replacen("event:", "event\n:", 1).into_bytes();
    let recovered_data =
        b"event: response.completed\ndata: {\"response\":{\"id\":\"r\"}}\n\n".to_vec();
    for cut in event_colon..=event_end + 1 {
        let retained = if cut < event_end {
            vec![wire[..cut].to_vec(), wire[cut..].to_vec()]
        } else {
            vec![wire[..cut].to_vec()]
        };
        add(
            &[SPLIT_FIELD],
            10,
            format!("data-first/split-event-prefix-{cut}"),
            vec![broken_event.clone()],
            retained.clone(),
        );
        if cut < event_end {
            add(
                &[SPLIT_FIELD],
                10,
                format!("data-first/split-data-prefix-{cut}"),
                vec![recovered_data.clone()],
                retained,
            );
        }
    }
    add(
        &[SPLIT_FIELD],
        10,
        "data-first/split-event-whole".into(),
        vec![broken_event],
        vec![wire.clone()],
    );
    let broken_both =
        b"event\n: response.completed\ndata: {\"response\":{\"id\":\"r\"}}\n\n".to_vec();
    for (order, retained) in [wire.clone(), recovered_data].into_iter().enumerate() {
        add(
            &[SPLIT_FIELD],
            10,
            format!("data-first/split-both-fields/order-{order}"),
            vec![broken_both.clone()],
            vec![retained],
        );
    }
    // Empty metadata data followed by a bare malformed word, with/without a
    // leading space. Both are the existing bare-word-continuation difference.
    for space in ["", " "] {
        add(
            &[BARE_WORD],
            5,
            format!("malformed-metadata/bare-word-space-{}", space.len()),
            vec![
                format!("event: response.metadata\ndata:{space}").into_bytes(),
                format!("{}malformed\n\n", if space.is_empty() { " " } else { "" }).into_bytes(),
            ],
            vec![streams[5].clone()],
        );
    }
    // Malformed private JSON: the two JSON colons and the data field colon
    // are the only repair sites. A combined fixture names both differences.
    for mask in 1..8 {
        let mut broken = String::from_utf8(streams[15].clone()).unwrap();
        for (bit, from, to) in [
            (1, "data:", "data\n:"),
            (2, "\"type\":", "\"type\"\n:"),
            (4, "\"bad\":", "\"bad\"\n:"),
        ] {
            if mask & bit != 0 {
                broken = broken.replacen(from, to, 1);
            }
        }
        let names: &'static [&'static str] = if mask == 1 {
            &[SPLIT_FIELD]
        } else if mask & 1 == 0 {
            &[PRIVATE_RECOVERY]
        } else {
            &[SPLIT_FIELD, PRIVATE_RECOVERY]
        };
        add(
            names,
            15,
            format!("malformed-private/colon-mask-{mask}"),
            vec![broken.into_bytes()],
            vec![streams[15].clone()],
        );
    }
    // Bytewise versions of the three multiline recovered-wire fixtures. Their
    // broken data-field colon prevents upstream's otherwise-valid JSON recovery.
    for stream in [17, 19, 21] {
        let comment = if stream == 17 { "" } else { ": keepalive\n" };
        let broken = format!(
            "{comment}data\n: {{\"type\"\n:\"response.metadata\",\n:{{\"marker\"\n:\"private\"}}}}\ndata: \"metadata\"\n\n"
        );
        add(
            &[SPLIT_FIELD],
            stream,
            format!("recovered-wire-{stream}/bytewise"),
            vec![broken.into_bytes()],
            vec![streams[stream].clone()],
        );
    }
    differences
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

// This separate unsplit-equality check is intentionally limited to event-first
// delta streams with no fields after their complete data payload. It compares
// post-repair client events, ignoring only empty delimiter-only separators.
#[test]
fn complete_delta_streams_without_trailing_fields_are_partition_invariant() {
    let delta = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n\n";
    let streams = [
        "event: delta\ndata: {\"delta\":\"a\"}\n\n".to_owned(),
        delta.to_owned(),
        "event: response.output_text.delta\ndata: {\"delta\":\"雪🌉\"}\n\n".to_owned(),
        format!(": keepalive\nid: seven\nretry: 500\n{delta}"),
        format!("{delta}{delta}{COMPLETED}"),
    ];
    let client_events = |out: &str| frame_fields(&client_frames(out));
    for wire in &streams {
        for codex in [false, true] {
            let expected = client_events(&run(codex, &[wire.as_bytes()]).0);
            for_each_partition(wire.as_bytes(), |chunks| {
                assert_eq!(
                    client_events(&run(codex, chunks).0),
                    expected,
                    "codex={codex} chunks={chunks:?}"
                );
            });
        }
    }
}

fn terminal_for_frames(codex: bool, frames: &[Vec<u8>]) -> String {
    let mut oracle = upstream::Upstream::new(codex);
    oracle.write_chunk(&upstream::wire(frames));
    oracle.flush();
    oracle.terminal_event
}

#[test]
fn every_partition_matches_upstream_framing_except_named_fixtures() {
    let started = std::time::Instant::now();
    let streams = partition_streams();
    let differences = partition_differences(&streams);
    let mut fixture_names = std::collections::HashSet::new();
    for difference in &differences {
        assert!(fixture_names.insert(&difference.fixture));
        assert!(!difference.names.is_empty());
        assert!(
            difference
                .names
                .iter()
                .all(|name| { [SPLIT_FIELD, BARE_WORD, PRIVATE_RECOVERY].contains(name) })
        );
    }
    let mut hits = vec![0; differences.len()];
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
                if frame_fields(&framer.frames) != frame_fields(&oracle.frames)
                    || framer.terminal_event != oracle.terminal_event
                {
                    let expected = differences.iter().position(|difference| {
                        difference.stream == i
                            && difference.upstream == oracle.frames
                            && difference.retained == framer.frames
                            && terminal_for_frames(codex, &difference.upstream)
                                == oracle.terminal_event
                            && terminal_for_frames(codex, &difference.retained)
                                == framer.terminal_event
                    });
                    if let Some(index) = expected {
                        hits[index] += 1;
                    } else {
                        mismatches.push((wire.len(), format!(
                            "stream={i} codex={codex} lengths={:?} chunks={:?}\ngot={:?} terminal={:?}\nwant={:?} terminal={:?}",
                            chunks.iter().map(|c| c.len()).collect::<Vec<_>>(),
                            chunks.iter().map(|c| String::from_utf8_lossy(c)).collect::<Vec<_>>(),
                            framer.frames.iter().map(|c| String::from_utf8_lossy(c)).collect::<Vec<_>>(), framer.terminal_event,
                            oracle.frames.iter().map(|c| String::from_utf8_lossy(c)).collect::<Vec<_>>(), oracle.terminal_event
                        )));
                    }
                }
                partitions += 1;
            });
        }
    }
    eprintln!(
        "differential invariant: {} streams, {partitions} partitions, {} mismatches, {} named-exception partitions, {:?}",
        streams.len(),
        mismatches.len(),
        hits.iter().sum::<usize>(),
        started.elapsed()
    );
    mismatches.sort();
    assert!(
        mismatches.is_empty(),
        "{PARTITION_INVARIANT}\nsmallest mismatches: {:#?}",
        &mismatches[..mismatches.len().min(8)]
    );
    let unused: Vec<_> = differences
        .iter()
        .zip(&hits)
        .filter(|(_, hits)| **hits == 0)
        .map(|(difference, _)| &difference.fixture)
        .collect();
    assert!(
        unused.is_empty(),
        "unexercised exception fixtures: {unused:?}"
    );
}
