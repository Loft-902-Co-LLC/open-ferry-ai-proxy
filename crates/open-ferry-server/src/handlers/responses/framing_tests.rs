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

// The desired invariant includes no-data frames, id, retry and comments, not
// merely event names/data. Upstream early emission violates this invariant;
// collecting all mismatches keeps the limitation measurable without a new hold.
// Boundary-dependent repair fixtures above are also represented by their recovered
// wire form here; a synthetic newline is not a genuine byte in that wire form.
#[test]
fn every_byte_partition_preserves_client_events() {
    let started = std::time::Instant::now();
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
    for fixture in recovery_fixtures() {
        let bytes: Vec<_> = fixture.iter().map(|chunk| chunk.as_bytes()).collect();
        streams.push(run(true, &bytes).0.into_bytes());
    }
    fn events(out: &str) -> Vec<(String, Option<Vec<u8>>, Vec<String>)> {
        out.replace("\r\n", "\n")
            .split("\n\n")
            .filter(|frame| !frame.trim().is_empty())
            .map(|frame| {
                let fields = frame
                    .lines()
                    .filter(|line| {
                        !line.starts_with("event:")
                            && !line.starts_with("data:")
                            && !line.is_empty()
                    })
                    .map(str::to_owned)
                    .collect();
                (
                    event_name(frame.as_bytes()),
                    data_payload(frame.as_bytes()),
                    fields,
                )
            })
            .collect()
    }
    let mut splits = 0;
    let mut mismatches = Vec::new();
    for (i, wire) in streams.iter().enumerate() {
        for codex in [false, true] {
            let want = events(&run(codex, &[wire]).0);
            let bytes: Vec<_> = wire.chunks(1).collect();
            if events(&run(codex, &bytes).0) != want {
                mismatches.push(format!("stream={i} codex={codex} bytewise"));
            }
            splits += 1;
            for a in 0..=wire.len() {
                let got = events(&run(codex, &[&wire[..a], &wire[a..]]).0);
                if got != want {
                    mismatches.push(format!("stream={i} codex={codex} split={a}"));
                }
                splits += 1;
                if wire.len() <= 60 {
                    for b in a..=wire.len() {
                        let got = events(&run(codex, &[&wire[..a], &wire[a..b], &wire[b..]]).0);
                        if got != want {
                            mismatches.push(format!("stream={i} codex={codex} splits={a},{b}"));
                        }
                        splits += 1;
                    }
                }
            }
        }
    }
    eprintln!(
        "partition invariant: {} streams, {splits} partitions, {} mismatches, {:?}",
        streams.len(),
        mismatches.len(),
        started.elapsed()
    );
    assert!(
        mismatches.is_empty(),
        "first mismatches: {:?}",
        &mismatches[..mismatches.len().min(8)]
    );
}
