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
    assert!(!got.contains("response.metadata") && !got.contains("private"), "{chunks:?}: {got}");
    assert!(!got.contains("event: error"), "{chunks:?}: {got}");
    assert!(got.contains("response.completed"), "{chunks:?}: {got}");
    assert_eq!(framer.terminal_event, "response.completed", "{chunks:?}: {got}");
}

#[test]
fn metadata_filter_checks_event_and_payload_even_when_malformed() {
    for private in ["response.metadata", "codex.response.metadata", "codex.rate_limits"] {
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
                let (got, _) = run(codex, &[&frame.as_bytes()[..split], &frame.as_bytes()[split..], COMPLETED.as_bytes()]);
                assert_eq!(got.contains(payload), codex, "split={split} codex={codex}: {got}");
                assert!(got.contains(COMPLETED));
            }
        }
    }
}

#[test]
fn empty_metadata_before_fragmented_fields_and_data_only_completion() {
    for field in ["event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n", "data: {\"type\":\"response.completed\"}\n\n", "id: event-42\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n", "retry: 1000\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n", "x-extra: value\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n"] {
        let colon = field.find(':').unwrap();
        for split in 1..=colon + 1 {
            public_completion(&[EMPTY_METADATA, &field[..split], &field[split..]]);
        }
    }
    for prefix in ["\n", "\r\n", ": keepalive\n", "id: event-42\n", "retry: 1000\n", "x-extra: value\n"] {
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
    for field in ["id: event-42", "retry: 1000", ": keepalive", "x-extra: value"] {
        for split in 1..=field.len() {
            public_completion(&[EMPTY_METADATA, DATA_FIRST, &field[..split], &field[split..], event]);
        }
    }
    for field in ["", "id: event-42\n", ": keepalive\n"] {
        for split in 1..6 {
            public_completion(&[EMPTY_METADATA, &format!("{DATA_FIRST}\n"), field, &event[..split], &event[split..]]);
            public_completion(&[EMPTY_METADATA, &format!("{DATA_FIRST}\n{field}{}", &event[..split]), &event[split..]]);
        }
    }
}

#[test]
fn preserves_empty_custom_event_until_its_delimiter() {
    let mut framer = Framer::new(false);
    let mut out = BytesMut::new();
    framer.write_chunk(&mut out, b"event: custom\ndata:").unwrap();
    assert!(out.is_empty());
    framer.write_chunk(&mut out, b"\n\n").unwrap();
    assert_eq!(&out[..], b"event: custom\ndata:\n\n");
}

#[test]
fn field_prefixes_in_json_strings_are_not_sse_fields() {
    for marker in ["data: private", "event: private", "id: private", "retry: private", ": private", "escaped \" data: private", "escaped \\ event: private"] {
        let payload = json!({"type":"response.metadata","metadata":{"marker":marker}}).to_string();
        let frame = format!("data: {payload}\n\n");
        // Try every split, including escaped quote/backslash boundaries.
        for split in 1..frame.len() {
            for codex in [false, true] {
                let (got, _) = run(codex, &[&frame.as_bytes()[..split], &frame.as_bytes()[split..], COMPLETED.as_bytes()]);
                assert_eq!(got.contains(&payload), codex, "marker={marker:?} split={split} codex={codex}: {got}");
                assert!(got.contains(COMPLETED));
            }
        }
    }
}

fn recovery_fixtures() -> Vec<Vec<&'static str>> {
    vec![
        vec![r#"data: {"type""#, ":\"response.metadata\",\ndata: \"metadata\":{\"marker\":\"private\"}}\n\n"],
        vec![r#"data: {"type""#, r#":"response.metadata","metadata""#, ":{\"marker\":\"private\"}}\n\n"],
        vec![r#"data: {"type""#, ":\"response.metadata\",\n: keepalive\ndata: \"metadata\":{\"marker\":\"private\"}}\n\n"],
        vec!["data", ": {\"type\":\"response.metadata\",\"metadata\":{\"marker\":\"private\"}}\n\n"],
        vec![r#"data: {"type""#, ":\"response.metadata\",\n: keepalive\ndata: \"metadata\"", ":{\"marker\":\"private\"}}\n\n"],
        vec![r#"data: {"type""#, ": keepalive", r#"data: :"response.metadata","metadata""#, ":{\"marker\":\"private\"}}\n\n"],
        vec![r#"data: {"type":"response.metadata","metadata":{"marker""#, ":\"", "data: private\"}}\n\n"],
        vec![r#"data: {"type""#, ":", "data: :\"response.metadata\",\"metadata\":{\"marker\":\"private\"}}\n\n"],
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
                let value: Value = serde_json::from_slice(&payload).unwrap_or_else(|error| panic!("fixture={i}: {error}: {got}"));
                assert_eq!(value["type"], "response.metadata", "fixture={i}: {got}");
                assert_eq!(value["metadata"]["marker"], if i == 6 { "data: private" } else { "private" });
                if fixture.iter().any(|chunk| chunk.contains(": keepalive")) {
                    assert!(got.contains(": keepalive"), "fixture={i}: {got}");
                }
            } else {
                assert!(!got.contains("response.metadata") && !got.contains("private"), "fixture={i}: {got}");
            }
            assert!(got.contains(COMPLETED), "fixture={i}: {got}");
        }
    }
}

// The invariant: transport chunk boundaries do not change the ordered SSE events
// received by either client class (event names and joined data, not wire formatting).
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
    ];
    for fixture in recovery_fixtures() {
        let bytes: Vec<_> = fixture.iter().map(|chunk| chunk.as_bytes()).collect();
        streams.push(run(true, &bytes).0.into_bytes());
    }
    fn events(out: &str) -> Vec<(String, Vec<u8>)> {
        out.replace("\r\n", "\n").split("\n\n").filter_map(|frame| {
            let data = data_payload(frame.as_bytes())?;
            Some((event_name(frame.as_bytes()), data))
        }).collect()
    }
    let mut splits = 0;
    for (i, wire) in streams.iter().enumerate() {
        for codex in [false, true] {
            let want = events(&run(codex, &[wire]).0);
            for a in 0..=wire.len() {
                let got = events(&run(codex, &[&wire[..a], &wire[a..]]).0);
                assert_eq!(got, want, "stream={i} codex={codex} split={a}");
                splits += 1;
                if wire.len() <= 60 {
                    for b in a..=wire.len() {
                        let got = events(&run(codex, &[&wire[..a], &wire[a..b], &wire[b..]]).0);
                        assert_eq!(got, want, "stream={i} codex={codex} splits={a},{b}");
                        splits += 1;
                    }
                }
            }
        }
    }
    eprintln!("partition invariant: {} streams, {splits} partitions, {:?}", streams.len(), started.elapsed());
}
