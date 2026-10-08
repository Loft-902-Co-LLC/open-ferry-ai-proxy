// Ported from CLIProxyAPI internal/translator/common/apply_patch_responses_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use std::collections::HashMap;

use serde_json::Value;

use super::*;
use crate::apply_patch::{unwrap_input, wrap_input};
use crate::go::json_string as q;

const REQUEST: &str = r#"{"tools":[{"type":"custom","name":"apply_patch"}]}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"output":[]}}"#;

fn bridge(request: &str) -> Bridge {
    Bridge::new(&serde_json::from_str(request).unwrap())
}

/// `patchItem`.
fn item(kind: &str, id: &str, call: &str, name: &str, arguments: &str) -> String {
    format!(
        r#"{{"type":{},"id":{},"call_id":{},"name":{},"arguments":{}}}"#,
        q(kind),
        q(id),
        q(call),
        q(name),
        q(arguments)
    )
}

/// `patchEvent`.
fn event(kind: &str, index: i64, item: &str) -> String {
    format!(
        r#"{{"type":{},"output_index":{index},"item":{item}}}"#,
        q(kind)
    )
}

/// `patchSend`: transforms an event that must not fail.
fn send(bridge: &mut Bridge, event: &str) -> Vec<Vec<u8>> {
    let (out, error) = bridge.transform(event.as_bytes());
    assert!(error.is_none(), "transform({event}): {error:?}");
    out
}

/// gjson `Get` for a dotted path, array indexes included.
fn get<'v>(value: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(value, |value, key| match value {
        Value::Array(items) => key.parse::<usize>().ok().and_then(|i| items.get(i)),
        _ => value.get(key),
    })
}

fn parse(event: &[u8]) -> Value {
    serde_json::from_slice(event).unwrap_or(Value::Null)
}

/// gjson `GetBytes(event, path).String()`.
fn text(event: &[u8], path: &str) -> String {
    str_of(get(&parse(event), path)).into_owned()
}

fn exists(event: &[u8], path: &str) -> bool {
    get(&parse(event), path).is_some()
}

fn shown(events: &[Vec<u8>]) -> String {
    events
        .iter()
        .map(|event| String::from_utf8_lossy(event).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_failed(out: &[Vec<u8>], error: Option<&Error>, case: &str) {
    assert!(error.is_some(), "{case}: {}", shown(out));
    assert_eq!(out.len(), 1, "{case}: {}", shown(out));
    assert_eq!(text(&out[0], "type"), "response.failed", "{case}");
}

/// Raw source fragments must survive decoding without inventing a final
/// snapshot.
#[test]
fn delta_and_four_completions() {
    let u = "\\u";
    let args =
        format!(r#"{{"input":"*** Begin Patch\n+中文{u}D83D{u}DE00 \"\\\n*** End Patch\n"}}"#);
    let want = unwrap_input(&args).unwrap();
    for late in [false, true] {
        let mut b = bridge(REQUEST);
        let name = if late { "" } else { "apply_patch" };
        let mut events = Vec::new();
        events.extend(send(
            &mut b,
            &event(
                "response.output_item.added",
                0,
                &item("function_call", "", "", name, ""),
            ),
        ));
        for part in [&args[..18], &args[18..35], &args[35..41], &args[41..]] {
            let delta = format!(
                r#"{{"type":"response.function_call_arguments.delta","output_index":0,"delta":{}}}"#,
                q(part)
            );
            events.extend(send(&mut b, &delta));
        }
        let done = item("function_call", "fc1", "c1", "apply_patch", &args);
        events.extend(send(&mut b, &event("response.output_item.done", 0, &done)));
        events.extend(send(
            &mut b,
            &format!(
                r#"{{"type":"response.completed","response":{{"id":"r1","output":[{done}]}}}}"#
            ),
        ));

        let mut delta = String::new();
        let mut counts = HashMap::<String, usize>::new();
        let mut last = -1;
        for event in &events {
            let kind = text(event, "type");
            *counts.entry(kind.clone()).or_default() += 1;
            let sequence = get(&parse(event), "sequence_number").map_or(0, int_of);
            assert!(
                sequence > last,
                "late={late}: sequence not increasing: {}",
                shown(&events)
            );
            last = sequence;
            match kind.as_str() {
                "response.custom_tool_call_input.delta" => delta.push_str(&text(event, "delta")),
                "response.custom_tool_call_input.done" => {
                    assert_eq!(text(event, "input"), want);
                    assert_eq!(text(event, "item_id"), "fc1");
                    assert_eq!(text(event, "call_id"), "c1");
                }
                "response.output_item.done" => {
                    assert_eq!(text(event, "item.input"), want);
                    assert!(!exists(event, "item.arguments"));
                }
                "response.completed" => {
                    assert_eq!(text(event, "response.output.0.input"), want);
                }
                _ => {}
            }
        }
        assert_eq!(delta, want, "late={late}");
        for kind in [
            "response.custom_tool_call_input.done",
            "response.output_item.done",
            "response.completed",
        ] {
            assert_eq!(counts.get(kind), Some(&1), "late={late}: {counts:?}");
        }
        assert_eq!(b.finish(), Ok(()));
        assert!(send(&mut b, COMPLETED).is_empty(), "duplicate terminal");
    }
}

#[test]
fn passthrough() {
    for request in [
        REQUEST,
        r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#,
        "{}",
    ] {
        let mut b = bridge(request);
        for event in [
            r#"{ "type":"response.output_item.added", "output_index":0,"item":{"type":"custom_tool_call","id":"native","name":"apply_patch","input":""}}"#.to_owned(),
            r#"{ "type":"response.custom_tool_call_input.delta", "item_id":"native","delta":"raw patch"}"#.to_owned(),
            event(
                "response.output_item.done",
                1,
                &item("function_call", "ordinary", "ordinary", "lookup", r#"{"x":1}"#),
            ),
        ] {
            let out = send(&mut b, &event);
            assert_eq!(out, [event.as_bytes()], "{request}");
        }
    }
}

#[test]
fn identity_and_snapshot_evidence() {
    let added = |index, id, call, name, args| {
        event(
            "response.output_item.added",
            index,
            &item("function_call", id, call, name, args),
        )
    };
    let cases: [(&str, Vec<String>, String); 8] = [
        (
            "index-item",
            vec![
                added(0, "a", "ca", "apply_patch", ""),
                added(1, "b", "cb", "lookup", ""),
            ],
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"item_id":"a","delta":"{}"}"#.to_owned(),
        ),
        (
            "call-item",
            vec![added(0, "a", "ca", "apply_patch", "")],
            r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","call_id":"other","arguments":"{\"input\":\"p\"}"}"#.to_owned(),
        ),
        (
            "type-before-name",
            vec![event(
                "response.output_item.added",
                0,
                &item("message", "a", "ca", "", ""),
            )],
            event(
                "response.output_item.done",
                0,
                &item("function_call", "a", "ca", "apply_patch", r#"{"input":"p"}"#),
            ),
        ),
        (
            "invalid-before-name",
            vec![added(0, "a", "ca", "", r#"{"input":"p","extra":1}"#)],
            event(
                "response.output_item.done",
                0,
                &item("function_call", "a", "ca", "apply_patch", r#"{"input":"p"}"#),
            ),
        ),
        (
            "partial-snapshot",
            vec![],
            added(0, "a", "ca", "apply_patch", r#"{"input":"p"#),
        ),
        (
            "invalid-final-only",
            vec![],
            r#"{"type":"response.completed","response":{"output":[{"type":"function_call","name":"apply_patch","arguments":"{}"}]}}"#.to_owned(),
        ),
        (
            "old-patch-new-type",
            vec![added(0, "a", "ca", "apply_patch", "")],
            r#"{"type":"response.completed","response":{"output":[{"type":"message","id":"a","content":[]}]}}"#.to_owned(),
        ),
        (
            "pending-id-evidence",
            vec![added(0, "a", "ca", "", "")],
            event(
                "response.output_item.done",
                0,
                &item("function_call", "a", "changed", "apply_patch", r#"{"input":"p"}"#),
            ),
        ),
    ];
    for (name, before, after) in cases {
        let mut b = bridge(REQUEST);
        for event in &before {
            send(&mut b, event);
        }
        let (out, error) = b.transform(after.as_bytes());
        assert_failed(&out, error.as_ref(), name);
        assert!(b.tool_input_error().is_some(), "{name}");
        let (more, error) = b.transform(COMPLETED.as_bytes());
        assert!(
            more.is_empty() && error.is_none(),
            "{name}: post-failure output"
        );
    }
}

#[test]
fn stages_and_finish() {
    let mut b = bridge(REQUEST);
    send(
        &mut b,
        &event(
            "response.output_item.added",
            0,
            &item("function_call", "a", "ca", "apply_patch", ""),
        ),
    );
    assert!(b.finish().is_err(), "incomplete call accepted");

    let mut b = bridge(REQUEST);
    for i in 0..2 {
        let (id, call) = (format!("a{i}"), format!("c{i}"));
        send(
            &mut b,
            &event(
                "response.output_item.added",
                i,
                &item("function_call", &id, &call, "apply_patch", ""),
            ),
        );
        let args = wrap_input(&format!("patch{i}"));
        let out = send(
            &mut b,
            &format!(
                r#"{{"type":"response.function_call_arguments.done","output_index":{i},"arguments":{}}}"#,
                q(&args)
            ),
        );
        assert_eq!(out.len(), 1, "missing input.done: {}", shown(&out));
        let done = event(
            "response.output_item.done",
            i,
            &item("function_call", &id, &call, "apply_patch", &args),
        );
        let out = send(&mut b, &done);
        assert_eq!(out.len(), 1, "duplicate arguments: {}", shown(&out));
        assert_eq!(text(&out[0], "item.input"), format!("patch{i}"));
        assert!(send(&mut b, &done).is_empty(), "duplicate done");
    }
    assert_eq!(b.finish(), Ok(()));
}

#[test]
fn request_history_and_winners() {
    for request in [
        r#"{"tools":[{"type":"custom","name":"apply_patch"}],"input":[]}"#,
        r#"{"tools":[{"type":"function","name":"apply_patch"}],"input":[]}"#,
        r#"{"input":[]}"#,
    ] {
        let raw = request.replacen(
            r#""input":[]"#,
            r#""input":[{"type":"custom_tool_call","call_id":"old","name":"apply_patch","input":"{\"input\":\"raw\"}"},{"type":"custom_tool_call_output","call_id":"old","output":"ok"},{"type":"function_call","call_id":"fn","name":"apply_patch","arguments":"{\"input\":\"existing\"}"}]"#,
            1,
        );
        let mut out: Value = serde_json::from_str(&raw).unwrap();
        normalize_request(&mut out).unwrap();
        assert_eq!(
            str_of(get(&out, "input.0.arguments")),
            wrap_input(r#"{"input":"raw"}"#),
            "{out}"
        );
        assert_eq!(str_of(get(&out, "input.1.type")), "function_call_output");
        assert_eq!(
            str_of(get(&out, "input.2.arguments")),
            r#"{"input":"existing"}"#
        );
        if request.contains(r#""type":"function""#) {
            assert!(get(&out, "tools.0.parameters").is_none(), "{out}");
        }
    }
    for request in [
        r#"{"tools":[{"type":"function","name":"apply_patch","description":"ordinary"}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
        r#"{"tools":[{"type":"namespace","name":"n","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"n__apply_patch","description":"ordinary"}]}"#,
    ] {
        let mut normalized: Value = serde_json::from_str(request).unwrap();
        normalize_request(&mut normalized).unwrap();
        let mut winners = bridge(request);
        let name = if request.contains(r#""namespace""#) {
            "n__apply_patch"
        } else {
            "apply_patch"
        };
        let done = event(
            "response.output_item.done",
            0,
            &item("function_call", "a", "c", name, r#"{"x":1}"#),
        );
        let got = send(&mut winners, &done);
        assert_eq!(got.len(), 1, "{}", shown(&got));
        assert_eq!(
            text(&got[0], "item.type"),
            "function_call",
            "loser stole ordinary identity, normalized={normalized}"
        );
    }
}

#[test]
fn namespace_mixed_non_stream() {
    let mut b = bridge(
        r#"{"tools":[{"type":"namespace","name":"n","tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"}]}]}"#,
    );
    let raw = format!(
        r#"{{"id":"r","output":[{},{}]}}"#,
        item(
            "function_call",
            "a",
            "c",
            "n__apply_patch",
            &wrap_input("p")
        ),
        item("function_call", "b", "d", "n__lookup", r#"{"x":1}"#)
    );
    let out = b.transform_non_stream(raw.as_bytes()).unwrap();
    assert_eq!(text(&out, "output.0.type"), "custom_tool_call");
    assert_eq!(text(&out, "output.0.name"), "apply_patch");
    assert_eq!(text(&out, "output.0.namespace"), "n");
    assert_eq!(text(&out, "output.1.name"), "lookup");
    assert_eq!(text(&out, "output.1.namespace"), "n");
    assert_eq!(text(&out, "output.1.arguments"), r#"{"x":1}"#);
}

#[test]
fn continuation_missing_snapshots() {
    for source in ["deltas", "arguments.done"] {
        let mut b = bridge(REQUEST);
        send(
            &mut b,
            &event(
                "response.output_item.added",
                0,
                &item("function_call", "a", "c", "apply_patch", ""),
            ),
        );
        let (kind, field) = if source == "deltas" {
            ("response.function_call_arguments.delta", "delta")
        } else {
            ("response.function_call_arguments.done", "arguments")
        };
        send(
            &mut b,
            &format!(
                r#"{{"type":{},"item_id":"a",{}:{}}}"#,
                q(kind),
                q(field),
                q(&wrap_input("p"))
            ),
        );
        let out = send(
            &mut b,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch"}}"#,
        );
        let last = out.last().unwrap();
        assert_eq!(text(last, "item.input"), "p", "{source}: missing snapshot");
        let out = send(&mut b, COMPLETED);
        let last = out.last().unwrap();
        assert_eq!(
            text(last, "response.output.0.input"),
            "p",
            "{source}: omitted completed item"
        );
    }
}

#[test]
fn continuation_native_terminal_bytes() {
    let mut b = bridge(REQUEST);
    for event in [
        r#"{ "type":"response.output_item.done", "output_index":0, "item":{"type":"custom_tool_call","id":"n","name":"apply_patch","input":"raw"}}"#,
        r#"{ "type":"response.completed", "sequence_number":71, "response": {"output":[{"type":"custom_tool_call","id":"n","name":"apply_patch","input":"raw"}]}}"#,
    ] {
        assert_eq!(send(&mut b, event), [event.as_bytes()]);
    }
}

#[test]
fn continuation_root_late_name() {
    let mut b = bridge(REQUEST);
    send(
        &mut b,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a"}}"#,
    );
    let out = send(
        &mut b,
        r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","call_id":"c","name":"apply_patch","arguments":"{\"input\":\"p\"}"}"#,
    );
    let last = out.last().expect("late root name");
    assert_eq!(text(last, "type"), "response.custom_tool_call_input.done");
    assert_eq!(b.finish(), Ok(()));
}

#[test]
fn continuation_no_invented_preview() {
    let mut b = bridge(REQUEST);
    let out = send(
        &mut b,
        &event(
            "response.output_item.done",
            0,
            &item("function_call", "a", "c", "apply_patch", &wrap_input("p")),
        ),
    );
    for event in &out {
        assert_ne!(
            text(event, "type"),
            "response.custom_tool_call_input.delta",
            "invented progress: {}",
            shown(&out)
        );
    }
}

#[test]
fn continuation_all_matched_provenance() {
    // Every key can select patch provenance, including a record other than
    // the first match.
    let keys = ["output_index", "item_id", "call_id"];
    for patch_key in keys {
        for first_key in keys {
            if first_key == patch_key {
                continue;
            }
            let mut b = bridge(REQUEST);
            send(
                &mut b,
                &event(
                    "response.output_item.added",
                    0,
                    &item("function_call", "a", "ca", "lookup", ""),
                ),
            );
            send(
                &mut b,
                &event(
                    "response.output_item.added",
                    1,
                    &item("function_call", "b", "cb", "apply_patch", ""),
                ),
            );
            let mut values = HashMap::from([
                ("output_index", "0"),
                ("item_id", r#""a""#),
                ("call_id", r#""ca""#),
            ]);
            let patch_values = HashMap::from([
                ("output_index", "1"),
                ("item_id", r#""b""#),
                ("call_id", r#""cb""#),
            ]);
            values.insert(patch_key, patch_values[patch_key]);
            let delta = format!(
                r#"{{"type":"response.function_call_arguments.delta","output_index":{},"item_id":{},"call_id":{},"delta":"{{}}"}}"#,
                values["output_index"], values["item_id"], values["call_id"]
            );
            let (out, error) = b.transform(delta.as_bytes());
            assert_failed(&out, error.as_ref(), &format!("{patch_key}-{first_key}"));
        }
    }
    for discover in 0..3 {
        let mut b = bridge(REQUEST);
        for i in 0..3 {
            send(
                &mut b,
                &event(
                    "response.output_item.added",
                    i,
                    &item("function_call", &format!("i{i}"), &format!("c{i}"), "", ""),
                ),
            );
        }
        send(
            &mut b,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"i1","call_id":"c2","delta":""}"#,
        );
        let done = event(
            "response.output_item.done",
            discover,
            &item(
                "function_call",
                &format!("i{discover}"),
                &format!("c{discover}"),
                "apply_patch",
                &wrap_input("p"),
            ),
        );
        let (out, error) = b.transform(done.as_bytes());
        assert!(
            error.is_some() && out.len() == 1,
            "pending-{discover}: evidence lost: {}",
            shown(&out)
        );
    }
}

#[test]
fn continuation_completed_window() {
    for terminal in [false, true] {
        let mut b = bridge(REQUEST);
        let done = item("function_call", "a", "c", "apply_patch", &wrap_input("p"));
        send(&mut b, &event("response.output_item.done", 0, &done));
        if terminal {
            send(
                &mut b,
                &format!(r#"{{"type":"response.completed","response":{{"output":[{done}]}}}}"#),
            );
        }
        let different = event(
            "response.output_item.done",
            0,
            &item(
                "function_call",
                "a",
                "c",
                "apply_patch",
                &wrap_input("different"),
            ),
        );
        let (out, error) = b.transform(different.as_bytes());
        if terminal {
            assert!(
                out.is_empty() && error.is_none(),
                "closed response: {}",
                shown(&out)
            );
        } else {
            assert!(
                error.is_some(),
                "contradiction after item.done: {}",
                shown(&out)
            );
        }
    }
}

#[test]
fn continuation_omitted_mixed_completed_items() {
    let patch = event(
        "response.output_item.done",
        0,
        &item("function_call", "p", "cp", "apply_patch", &wrap_input("p")),
    );
    let ordinary = r#"{"type":"message","id":"m","content":[{"type":"output_text","text":"ok"}]}"#;

    let mut b = bridge(REQUEST);
    send(&mut b, &patch);
    send(&mut b, &event("response.output_item.done", 1, ordinary));
    let out = send(&mut b, COMPLETED);
    let last = out.last().unwrap();
    assert_eq!(
        text(last, "response.output.0.input"),
        "p",
        "{}",
        shown(&out)
    );
    assert_eq!(text(last, "response.output.1.id"), "m", "{}", shown(&out));

    let mut b = bridge(REQUEST);
    send(&mut b, &patch);
    let out = send(
        &mut b,
        &format!(r#"{{"type":"response.completed","response":{{"output":[{ordinary}]}}}}"#),
    );
    let last = out.last().unwrap();
    assert_eq!(
        text(last, "response.output.0.input"),
        "p",
        "{}",
        shown(&out)
    );
    assert_eq!(text(last, "response.output.1.id"), "m", "{}", shown(&out));
}

#[test]
fn continuation_unmatched_identity_evidence() {
    for known in [false, true] {
        for key in [
            r#""item_id":"b""#,
            r#""call_id":"cb""#,
            r#""output_index":1"#,
        ] {
            let mut b = bridge(REQUEST);
            if known {
                send(
                    &mut b,
                    &event(
                        "response.output_item.added",
                        0,
                        &item("function_call", "a", "ca", "lookup", ""),
                    ),
                );
            }
            send(
                &mut b,
                r#"{"type":"response.output_item.added","output_index":1,"item_id":"a","call_id":"ca","item":{"type":"function_call","id":"b","call_id":"cb","name":""}}"#,
            );
            let done = format!(
                r#"{{"type":"response.function_call_arguments.done",{key},"name":"apply_patch","arguments":"{{\"input\":\"p\"}}"}}"#
            );
            let (out, error) = b.transform(done.as_bytes());
            assert_failed(&out, error.as_ref(), &format!("known={known}/{key}"));
        }
    }
}

#[test]
fn continuation_mixed_sequence() {
    let mut b = bridge(REQUEST);
    let mut events = Vec::new();
    for raw in [
        r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":""}}"#,
        r#"{"type":"response.output_item.added","sequence_number":2,"output_index":1,"item":{"type":"function_call","id":"b","name":"lookup","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.done","sequence_number":3,"output_index":0,"item_id":"a","arguments":"{\"input\":\"p\"}"}"#,
        r#"{"type":"response.output_item.done","sequence_number":4,"output_index":1,"item":{"type":"function_call","id":"b","name":"lookup","arguments":"{\"x\":1}"}}"#,
        r#"{"type":"response.completed","sequence_number":5,"response":{"output":[]}}"#,
    ] {
        events.extend(send(&mut b, raw));
    }
    let mut last = -1;
    for event in &events {
        let sequence = get(&parse(event), "sequence_number").map_or(0, int_of);
        assert!(
            sequence > last,
            "nonmonotonic mixed sequence: {}",
            shown(&events)
        );
        last = sequence;
    }
}

#[test]
fn continuation_ordinary_root_name_passthrough() {
    let mut b = bridge(
        r#"{"tools":[{"type":"namespace","name":"n","tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"}]}]}"#,
    );
    let raw = r#"{ "type":"response.function_call_arguments.done", "output_index":0,"name":"lookup","namespace":"n","arguments":"{}"}"#;
    assert_eq!(send(&mut b, raw), [raw.as_bytes()]);
}

#[test]
fn continuation_mixed_native_bytes() {
    let mut b = bridge(REQUEST);
    send(
        &mut b,
        r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"a","name":"apply_patch","arguments":""}}"#,
    );
    let native = r#"  { "type":"response.output_item.added", "sequence_number":2, "output_index":1,"item":{"type":"custom_tool_call","id":"n","name":"apply_patch","input":""}}  "#;
    assert_eq!(send(&mut b, native), [native.as_bytes()]);
}

// Not upstream's.

#[test]
fn normalize_declares_the_function_and_drops_losers() {
    let mut request: Value = serde_json::from_str(
        r#"{"tools":[{"type":"custom","name":"apply_patch","description":"Edit.","format":{"type":"grammar"}},{"type":"function","name":"apply_patch"},{"type":"function","name":"lookup"}],"tool_choice":{"type":"allowed_tools","tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"}]}}"#,
    )
    .unwrap();
    normalize_request(&mut request).unwrap();
    let tools = request["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2, "{request}");
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["parameters"], parameters());
    assert!(
        tools[0]["description"]
            .as_str()
            .unwrap()
            .starts_with("Edit.")
    );
    assert!(tools[0].get("format").is_none());
    assert_eq!(tools[1]["name"], "lookup");
    assert_eq!(request["tool_choice"]["tools"][0]["type"], "function");
    assert_eq!(request["tool_choice"]["tools"][1]["type"], "function");
}

#[test]
fn history_input_must_be_a_string() {
    let raw = r#"{"tools":[{"type":"custom","name":"apply_patch"}],"input":[{"type":"custom_tool_call","name":"apply_patch","input":{"patch":1}}]}"#;
    let mut request: Value = serde_json::from_str(raw).unwrap();
    let error = normalize_request(&mut request).unwrap_err();
    assert_eq!(
        error.to_string(),
        "apply_patch history input must be a string"
    );
    assert_eq!(request, serde_json::from_str::<Value>(raw).unwrap());
}

#[test]
fn unreadable_event_fails_and_other_text_passes() {
    let mut b = bridge(REQUEST);
    assert_eq!(send(&mut b, ""), [b""]);
    assert_eq!(send(&mut b, "event: x"), [b"event: x"]);
    let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
    let (out, error) = b.transform(deep.as_bytes());
    assert_failed(&out, error.as_ref(), "deep");
    assert_eq!(b.finish().unwrap_err().to_string(), UNREADABLE);
}

#[test]
fn inactive_bridge_passes_everything() {
    let mut b = bridge("{}");
    assert!(!b.active());
    let raw = event(
        "response.output_item.done",
        0,
        &item("function_call", "a", "c", "apply_patch", "{}"),
    );
    assert_eq!(send(&mut b, &raw), [raw.as_bytes()]);
    assert_eq!(b.transform_non_stream(b"x").unwrap(), b"x");
    assert_eq!(b.finish(), Ok(()));
}

/// Events whose root supplies a late identity for a patch call: a delta that
/// names the call, then the end of its arguments, then the terminal event.
fn late_identity(call_id: &str) -> [String; 3] {
    [
        format!(
            r#"{{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"p","call_id":{call_id},"name":"apply_patch","delta":"{{\"input\":\"abc\"}}"}}"#
        ),
        r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"p","arguments":"{\"input\":\"abc\"}"}"#.to_owned(),
        r#"{"type":"response.completed","response":{"id":"r","output":[]}}"#.to_owned(),
    ]
}

/// Not upstream's: a regression test. The late identity is built with gjson
/// `Value()`, which reads a number as a float64, and sjson writes it back
/// with `FormatFloat(v, 'f', -1, 64)`. A number the float64 can't hold exactly
/// therefore no longer agrees with the same number at the root of the event,
/// and the call fails. Go v8.0.10 was run on every case here.
#[test]
fn late_numeric_call_id_above_2_pow_53_conflicts_with_the_root() {
    let mut b = bridge(REQUEST);
    let [delta, done, completed] = late_identity("9007199254740993");
    let (out, error) = b.transform(delta.as_bytes());
    assert_failed(&out, error.as_ref(), "the first event");
    assert_eq!(text(&out[0], "sequence_number"), "1");
    assert_eq!(
        text(&out[0], "response.error.code"),
        "invalid_tool_arguments"
    );
    assert_eq!(
        error.expect("a failure").to_string(),
        "conflicting apply_patch call identity"
    );
    // After a failure nothing else comes out, and the error is kept.
    assert!(send(&mut b, &done).is_empty());
    assert!(send(&mut b, &completed).is_empty());
    assert_eq!(
        b.finish().unwrap_err().to_string(),
        "conflicting apply_patch call identity"
    );

    // Repeating the number at the root of every event doesn't help: each
    // event's identity is rebuilt from the root.
    let mut b = bridge(REQUEST);
    let (out, error) = b.transform(
        br#"{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"p","call_id":9007199254740993,"name":"apply_patch","delta":"{}"}"#,
    );
    assert_failed(&out, error.as_ref(), "the number repeated");
}

/// Not upstream's: a regression test. Numbers a float64 holds exactly still
/// agree, and are written the way `FormatFloat(v, 'f', -1, 64)` writes them
/// (1e2 is 100). Go v8.0.10 gave the same results.
#[test]
fn late_numeric_call_id_that_a_float64_holds_is_kept() {
    for (written, id) in [
        ("12", "12"),
        ("1e2", "100"),
        ("1.5", "1.5"),
        (r#""c""#, "c"),
    ] {
        let mut b = bridge(REQUEST);
        let [delta, done, completed] = late_identity(written);
        let first = send(&mut b, &delta);
        assert_eq!(first.len(), 2, "{written}: {}", shown(&first));
        assert_eq!(
            text(&first[0], "type"),
            "response.output_item.added",
            "{written}"
        );
        assert_eq!(text(&first[0], "item.call_id"), id, "{written}");
        assert_eq!(text(&first[1], "call_id"), id, "{written}");
        assert_eq!(text(&first[1], "delta"), "abc", "{written}");
        let second = send(&mut b, &done);
        assert_eq!(second.len(), 1, "{written}");
        assert_eq!(text(&second[0], "input"), "abc", "{written}");
        let last = send(&mut b, &completed);
        assert_eq!(last.len(), 2, "{written}: {}", shown(&last));
        assert_eq!(text(&last[0], "item.call_id"), id, "{written}");
        assert_eq!(text(&last[1], "response.output.0.call_id"), id, "{written}");
        assert_eq!(b.finish(), Ok(()), "{written}");
    }
}

/// Not upstream's: a regression test. The same coercion applies to a numeric
/// `name` and `namespace` at the root of an event. A number is not the name
/// `apply_patch`, so the first leaves the event alone; the second, a
/// namespace the request doesn't declare, doesn't name the patch tool either.
/// Go v8.0.10 passed both through unchanged.
#[test]
fn late_numeric_name_and_namespace_pass_through() {
    let mut b = bridge(REQUEST);
    let name = r#"{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"p","call_id":"c","name":9007199254740993,"delta":"{}"}"#;
    assert_eq!(send(&mut b, name), [name.as_bytes()]);
    assert_eq!(send(&mut b, COMPLETED), [COMPLETED.as_bytes()]);
    assert_eq!(b.finish(), Ok(()));

    let mut b = bridge(REQUEST);
    let namespace = r#"{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"p","call_id":"c","name":"apply_patch","namespace":9007199254740993,"delta":"{\"input\":\"abc\"}"}"#;
    let done = r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"p","arguments":"{\"input\":\"abc\"}"}"#;
    assert_eq!(send(&mut b, namespace), [namespace.as_bytes()]);
    assert_eq!(send(&mut b, done), [done.as_bytes()]);
    assert_eq!(send(&mut b, COMPLETED), [COMPLETED.as_bytes()]);
    assert_eq!(b.finish(), Ok(()));
}

/// Not upstream's: a regression test. A numeric `call_id` inside an output
/// item is read as text, not through `Value()`, so a non-stream response keeps
/// every digit. Go v8.0.10 gave the same.
#[test]
fn non_stream_numeric_call_id_keeps_every_digit() {
    let mut b = bridge(REQUEST);
    let response = br#"{"id":"r","output":[{"type":"function_call","id":"p","call_id":9007199254740993,"name":"apply_patch","arguments":"{\"input\":\"abc\"}","status":"completed"}]}"#;
    let out = b.transform_non_stream(response).expect("a patch call");
    assert_eq!(text(&out, "output.0.type"), "custom_tool_call");
    assert_eq!(text(&out, "output.0.call_id"), "9007199254740993");
    assert_eq!(text(&out, "output.0.input"), "abc");
}

/// Not upstream's: a regression test. Go's `int` is 64 bits and wraps, and
/// upstream supplies `sequence_number`. An event at the largest value used to
/// panic with "attempt to add with overflow" when the bridge numbered the
/// event it converted. Go v8.0.10 wrapped.
#[test]
fn maximum_sequence_number_wraps_like_go() {
    let mut b = bridge(REQUEST);
    let added = r#"{"type":"response.output_item.added","output_index":0,"sequence_number":9223372036854775807,"item":{"type":"function_call","name":"apply_patch","id":"p","call_id":"c","arguments":""}}"#;
    let out = send(&mut b, added);
    assert_eq!(out.len(), 1, "{}", shown(&out));
    assert_eq!(text(&out[0], "type"), "response.output_item.added");
    assert_eq!(text(&out[0], "sequence_number"), "-9223372036854775807");
    assert_eq!(text(&out[0], "item.type"), "custom_tool_call");

    // The rest of the call, each event at the same largest number.
    let mut b = bridge(REQUEST);
    let events = [
        added.to_owned(),
        r#"{"type":"response.function_call_arguments.delta","output_index":0,"sequence_number":9223372036854775807,"item_id":"p","delta":"{\"input\":\"abc\"}"}"#.to_owned(),
        r#"{"type":"response.function_call_arguments.done","output_index":0,"sequence_number":9223372036854775807,"item_id":"p","arguments":"{\"input\":\"abc\"}"}"#.to_owned(),
        r#"{"type":"response.completed","sequence_number":9223372036854775807,"response":{"id":"r","output":[]}}"#.to_owned(),
    ];
    let mut numbers = Vec::new();
    for event in &events {
        for out in send(&mut b, event) {
            numbers.push((text(&out, "type"), text(&out, "sequence_number")));
        }
    }
    let numbers: Vec<_> = numbers
        .iter()
        .map(|(kind, number)| (kind.as_str(), number.as_str()))
        .collect();
    assert_eq!(
        numbers,
        [
            ("response.output_item.added", "-9223372036854775807"),
            (
                "response.custom_tool_call_input.delta",
                "-9223372036854775807"
            ),
            (
                "response.custom_tool_call_input.done",
                "-9223372036854775807"
            ),
            ("response.output_item.done", "-9223372036854775806"),
            ("response.completed", "-9223372036854775805"),
        ]
    );
    assert_eq!(b.finish(), Ok(()));
}

/// Not upstream's: a regression test. An output index of the largest value
/// is upstream-supplied too. Finding a free index for an item the snapshot
/// adds counts one past the highest taken index, which wraps in Go and used to
/// panic here. Go v8.0.10 returned the response unchanged.
#[test]
fn maximum_output_index_wraps_when_a_free_index_is_found() {
    let mut b = bridge(REQUEST);
    for raw in [
        event(
            "response.output_item.added",
            0,
            &item("function_call", "a", "ca", "apply_patch", ""),
        ),
        event(
            "response.output_item.added",
            i64::MAX,
            &item("function_call", "b", "cb", "apply_patch", ""),
        ),
    ] {
        let out = send(&mut b, &raw);
        assert_eq!(out.len(), 1, "{}", shown(&out));
        assert_eq!(text(&out[0], "item.type"), "custom_tool_call");
    }
    let response = br#"{"id":"r","output":[{"type":"function_call","id":"c","call_id":"cc","name":"other","arguments":"{}","status":"completed"}]}"#;
    let out = b.transform_non_stream(response).expect("an unknown item");
    assert_eq!(out, response);
}
