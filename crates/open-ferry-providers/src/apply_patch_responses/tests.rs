// Ported from CLIProxyAPI internal/runtime/executor/helps/apply_patch_responses_test.go
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// TestApplyPatchResponsesHelperDispatcherLifecycle also checks that the state
// copies the events it keeps, by overwriting them after each call. Here the
// state takes them borrowed, so that step is dropped.

use open_ferry_translate::go::json_string as q;

use super::*;

const PATCH: &str = r#"{"tools":[{"type":"custom","name":"apply_patch"}]}"#;
const NAMESPACED: &str = r#"{"tools":[{"type":"namespace","name":"n","tools":[{"type":"custom","name":"apply_patch"}]}]}"#;
const NAMESPACED_LOOKUP: &str = r#"{"tools":[{"type":"namespace","name":"n","tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"}]}]}"#;

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn state(source: &Format, original: &str, declarations: &str) -> State {
    State::new(source, &json(original), &json(declarations))
}

/// A state for `request` from a Responses client, with `n` dispatching into
/// the namespace `n`.
fn dispatching(request: &str) -> State {
    let mut s = state(&Format::OPENAI_RESPONSE, request, request);
    s.add_dispatcher("n", "n");
    s
}

/// gjson `GetBytes(event, path).String()`.
fn text(event: &[u8], path: &str) -> String {
    str_at(&parse(event), path)
}

fn shown(events: &[Vec<u8>]) -> String {
    events
        .iter()
        .map(|event| String::from_utf8_lossy(event).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The bytes and events the state holds, counted from what it holds.
fn recount(s: &State) -> (usize, usize) {
    let mut bytes: usize = s.by_key.keys().map(|key| key.len() + ENTRY_COST).sum();
    let mut events = 0;
    for call in &s.records {
        bytes += CALL_COST
            + held_size(&call.events)
            + held_size(&call.originals)
            + held_size(&call.snapshots)
            + call.source.len()
            + call.name.len()
            + call.arguments.len();
        events += call.events.len() + call.snapshots.len();
    }
    (bytes, events)
}

/// The state's own count of what it holds is what it holds.
fn assert_counted(s: &State) {
    assert_eq!((s.held_bytes, s.held_events), recount(s));
}

/// Transforms an event that must not fail.
fn send(s: &mut State, event: &str) -> Vec<Vec<u8>> {
    let (out, error) = s.transform(event.as_bytes());
    assert!(error.is_none(), "transform({event}): {error:?}");
    assert_counted(s);
    out
}

/// Remembers and transforms an event that must not fail, as the xAI
/// executor passes each one.
fn remember_and_send(s: &mut State, event: &str) -> Vec<Vec<u8>> {
    s.remember_dispatcher_event(event.as_bytes());
    assert_counted(s);
    send(s, event)
}

fn assert_failed(out: &[Vec<u8>], error: Option<&Error>, case: &str) {
    assert!(error.is_some(), "{case}: {}", shown(out));
    assert_eq!(out.len(), 1, "{case}: {}", shown(out));
    assert_eq!(text(&out[0], "type"), "response.failed", "{case}");
}

/// The call a key names.
fn call<'s>(s: &'s State, key: &str) -> Option<&'s DispatcherCall> {
    s.by_key.get(key).map(|&call| &s.records[call])
}

#[test]
fn native_sse_bytes() {
    let mut s = state(&Format::CODEX, PATCH, PATCH);
    for line in [
        "event: response.output_item.done",
        r#"data:   { "type":"response.output_item.done", "output_index":0, "item":{"type":"custom_tool_call","id":"a","name":"apply_patch","input":"raw"}}  "#,
        "",
        "event: response.completed",
        r#"data:  { "type":"response.completed", "sequence_number":8,"response":{"output":[{"type":"custom_tool_call","id":"a","name":"apply_patch","input":"raw"}]}} "#,
    ] {
        let (out, error) = s.stream(line.as_bytes());
        assert!(error.is_none(), "{error:?}");
        if line.starts_with("event:") {
            continue;
        }
        assert!(
            out.last().is_some_and(|last| last == line.as_bytes()),
            "native framing changed: {}",
            shown(&out)
        );
    }
}

#[test]
fn dispatcher_keys_and_final() {
    for key in [
        r#""output_index":0"#,
        r#""call_id":"c""#,
        r#""item_id":"a""#,
    ] {
        for terminal_only in [false, true] {
            let case = format!("{key}/{terminal_only}");
            let mut s = dispatching(NAMESPACED_LOOKUP);
            send(
                &mut s,
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n","namespace":"n","arguments":""}}"#,
            );
            send(
                &mut s,
                &format!(
                    r#"{{"type":"response.function_call_arguments.delta",{key},"delta":"{{\"name\":\"apply_patch\",\"arguments\":{{\"input\":\"p\"}}}}"}}"#
                ),
            );
            let item = r#"{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","namespace":"n","arguments":"{\"input\":\"p\"}"}"#;
            let out = if terminal_only {
                send(
                    &mut s,
                    &format!(r#"{{"type":"response.completed","response":{{"output":[{item}]}}}}"#),
                )
            } else {
                send(
                    &mut s,
                    &format!(
                        r#"{{"type":"response.output_item.done","output_index":0,"item":{item}}}"#
                    ),
                )
            };
            let kinds: Vec<String> = out.iter().map(|event| text(event, "type")).collect();
            assert!(
                !kinds
                    .iter()
                    .any(|kind| kind == "response.custom_tool_call_input.delta"),
                "{case}: fabricated dispatcher preview: {}",
                shown(&out)
            );
            assert!(
                kinds
                    .iter()
                    .any(|kind| kind == "response.custom_tool_call_input.done"),
                "{case}: dispatcher bypass: {}",
                shown(&out)
            );
            // Completed input is valid whether or not the response closed.
            assert_eq!(s.bridge.finish(), Ok(()), "{case}");
            if !terminal_only {
                assert!(
                    s.finish().is_err(),
                    "{case}: completed dispatcher input substituted for source completion"
                );
            }
        }
    }
}

#[test]
fn chat_function_preference() {
    let original = r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","function":{"name":"apply_patch"}}]}"#;
    let declarations = r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"apply_patch"}]}"#;
    let mut s = state(&Format::OPENAI, original, declarations);
    let raw = r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","name":"apply_patch","arguments":"ordinary"}}"#;
    let (out, error) = s.transform(raw.as_bytes());
    assert!(error.is_none(), "{error:?}");
    assert_eq!(out, [raw.as_bytes()], "Chat function preference");
}

#[test]
fn request_chat_preference() {
    let original = json(
        r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","function":{"name":"apply_patch","parameters":{"type":"object","properties":{"x":{"type":"integer"}}}}}]}"#,
    );
    let mut body = json(
        r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"apply_patch","parameters":{"type":"object","properties":{"x":{"type":"integer"}}}}]}"#,
    );
    normalize_request(&mut body, Some(&original)).unwrap();
    assert!(
        get(&body, "tools.0.parameters.properties.x").is_some(),
        "request ordinary preference lost: {body}"
    );
    assert!(
        get(&body, "tools.0.parameters.properties.input").is_none(),
        "request ordinary preference lost: {body}"
    );
}

#[test]
fn dispatcher_omitted_arguments() {
    let request = r#"{"tools":[{"type":"namespace","name":"n","tools":[{"type":"custom","name":"apply_patch"}]}]}"#;
    for late_name in ["n", "apply_patch"] {
        let mut s = dispatching(request);
        send(
            &mut s,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n","arguments":""}}"#,
        );
        send(
            &mut s,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"a","delta":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}"#,
        );
        let out = send(
            &mut s,
            &format!(
                r#"{{"type":"response.output_item.done","output_index":0,"item":{{"type":"function_call","id":"a","call_id":"c","name":{},"namespace":"n"}}}}"#,
                q(late_name)
            ),
        );
        assert!(
            out.last()
                .is_some_and(|last| text(last, "item.input") == "p"),
            "{late_name}: sourced dispatcher completion lost: {}",
            shown(&out)
        );
    }
}

#[test]
fn closed_response() {
    let mut s = dispatching(NAMESPACED);
    send(
        &mut s,
        r#"{"type":"response.completed","response":{"output":[{"type":"function_call","id":"a","call_id":"c","namespace":"n","name":"apply_patch","arguments":"{\"input\":\"p\"}"}]}}"#,
    );
    let (out, error) = s.transform(
        br#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"changed","name":"n","arguments":""}}"#,
    );
    assert!(
        out.is_empty() && error.is_none(),
        "closed response mutated: {} {error:?}",
        shown(&out)
    );
    assert_eq!(s.finish(), Ok(()));
}

#[test]
fn transport_terminal() {
    let complete = br#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":"{\"input\":\"p\"}"}}"#;
    for json_terminal in [false, true] {
        let mut s = state(&Format::OPENAI_RESPONSE, PATCH, PATCH);
        let (_, error) = s.stream(complete);
        assert!(error.is_none(), "{json_terminal}: {error:?}");
        if json_terminal {
            let (_, error) =
                s.stream(br#"data: {"type":"response.completed","response":{"output":[]}}"#);
            assert!(error.is_none(), "{json_terminal}: {error:?}");
        }
        let marker = b"data:   [DONE]  ";
        let (out, error) = s.stream(marker);
        if json_terminal {
            assert!(
                error.is_none() && out == [marker],
                "first legitimate source sentinel changed: {} {error:?}",
                shown(&out)
            );
        } else {
            assert!(
                error.is_some()
                    && out.len() == 1
                    && String::from_utf8_lossy(&out[0]).contains(r#""type":"response.failed""#),
                "premature source sentinel accepted: {} {error:?}",
                shown(&out)
            );
        }
        for line in [
            &complete[..],
            b"event: response.completed",
            b"",
            b": keepalive",
            marker,
        ] {
            let (out, error) = s.stream(line);
            assert!(
                error.is_none() && out.is_empty(),
                "{json_terminal}: post-terminal output: {} {error:?}",
                shown(&out)
            );
        }
        let (out, error) =
            s.transform(br#"{"type":"response.completed","response":{"output":[]}}"#);
        assert!(
            error.is_none() && out.is_empty(),
            "{json_terminal}: transport terminal reopened by JSON: {} {error:?}",
            shown(&out)
        );
    }
}

#[test]
fn failed_transport() {
    let mut s = state(&Format::OPENAI_RESPONSE, PATCH, PATCH);
    let (_, error) = s.stream(
        br#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"apply_patch","arguments":""}}"#,
    );
    assert!(error.is_none(), "{error:?}");
    let (out, error) = s.stream(b"data: [DONE]");
    assert!(
        error.is_some()
            && out.len() == 1
            && String::from_utf8_lossy(&out[0]).contains(r#""type":"response.failed""#),
        "incomplete call did not fail before DONE: {} {error:?}",
        shown(&out)
    );
    for line in [
        &b"data: [DONE]"[..],
        br#"data: {"type":"response.completed","response":{"output":[]}}"#,
        b"event: response.completed",
    ] {
        let (out, error) = s.stream(line);
        assert!(
            out.is_empty() && error.is_none(),
            "failure repeated or success published after failure: {} {error:?}",
            shown(&out)
        );
    }
}

#[test]
fn inactive_transport_bytes() {
    let request = r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#;
    let mut s = state(&Format::OPENAI_RESPONSE, request, request);
    for line in [
        &b"data: [DONE]"[..],
        br#"data:  { "type":"response.completed", "response":{"output":[]}} "#,
        b"event: response.completed",
        b"",
        b"data: [DONE]",
    ] {
        let (out, error) = s.stream(line);
        assert!(
            error.is_none() && out == [line],
            "inactive bridge changed source bytes: {} {error:?}",
            shown(&out)
        );
    }
}

#[test]
fn retained_dispatcher_provenance() {
    let patch = r#"{"name":"apply_patch","arguments":{"input":"p"}}"#;
    let ordinary = r#"{"name":"lookup","arguments":{"input":"p"}}"#;
    let nested =
        r#"{"name":"lookup","arguments":{"name":"apply_patch","arguments":{"input":"not patch"}}}"#;
    let conflicting = r#"{"name":"apply_patch","arguments":{"input":"q"}}"#;
    for (case, delta, wrappers, final_name, should_fail) in [
        ("patch_then_ordinary", "", vec![patch, ordinary], "n", true),
        ("ordinary_then_patch", "", vec![ordinary, patch], "n", true),
        (
            "conflicting_inputs",
            "",
            vec![patch, conflicting],
            "n",
            true,
        ),
        (
            "full_source_conflicts_with_snapshot",
            patch,
            vec![ordinary],
            "n",
            true,
        ),
        (
            "full_source_conflicts_with_child",
            patch,
            vec![],
            "lookup",
            true,
        ),
        (
            "ordinary_child_is_not_patch",
            "",
            vec![ordinary],
            "n",
            false,
        ),
        (
            "ordinary_arguments_are_not_dispatcher_provenance",
            "",
            vec![nested],
            "n",
            false,
        ),
    ] {
        let mut s = dispatching(NAMESPACED_LOOKUP);
        send(
            &mut s,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n","arguments":""}}"#,
        );
        if !delta.is_empty() {
            send(
                &mut s,
                &format!(
                    r#"{{"type":"response.function_call_arguments.delta","item_id":"a","delta":{}}}"#,
                    q(delta)
                ),
            );
        }
        for wrapper in &wrappers {
            s.remember_dispatcher_arguments(
                format!(
                    r#"{{"type":"response.function_call_arguments.done","item_id":"a","arguments":{}}}"#,
                    q(wrapper)
                )
                .as_bytes(),
            );
            // The real restorer removes the wrapper before the event reaches
            // transform.
            let inner = lenient_get(wrapper, "arguments").unwrap();
            send(
                &mut s,
                &format!(
                    r#"{{"type":"response.function_call_arguments.done","item_id":"a","arguments":{}}}"#,
                    q(inner.text())
                ),
            );
        }
        let last = format!(
            r#"{{"type":"response.output_item.done","output_index":0,"item":{{"type":"function_call","id":"a","call_id":"late","name":{},"namespace":"n"}}}}"#,
            q(final_name)
        );
        let (out, error) = s.transform(last.as_bytes());
        if should_fail {
            assert_failed(&out, error.as_ref(), case);
            continue;
        }
        assert!(error.is_none(), "{case}: {error:?}");
        let last = out.last().unwrap_or_else(|| panic!("{case}: no events"));
        let expected = lenient_get(wrappers[wrappers.len() - 1], "arguments").unwrap();
        assert_eq!(text(last, "item.name"), "lookup", "{case}: {}", shown(&out));
        assert_eq!(text(last, "item.namespace"), "n", "{case}: {}", shown(&out));
        assert_eq!(
            text(last, "item.arguments"),
            expected.text(),
            "{case}: {}",
            shown(&out)
        );
        assert!(
            !shown(&out).contains("custom_tool_call"),
            "{case}: ordinary dispatcher child changed: {}",
            shown(&out)
        );
    }
}

#[test]
fn dispatcher_snapshots_all_matched() {
    for discover in 0..3 {
        let mut s = dispatching(NAMESPACED);
        for i in 0..3 {
            send(
                &mut s,
                &format!(
                    r#"{{"type":"response.output_item.added","output_index":{i},"item":{{"type":"function_call","id":"i{i}","call_id":"c{i}","name":"n","arguments":""}}}}"#
                ),
            );
        }
        let original = br#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"i1","call_id":"c2","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}"#;
        s.remember_dispatcher_arguments(original);
        for key in ["item:i0", "item:i1", "item:i2"] {
            let snapshots = call(&s, key).map(|call| call.snapshots.clone());
            assert_eq!(
                snapshots,
                Some(vec![original.to_vec()]),
                "{discover}: original source not retained on matched record {key}"
            );
        }
        send(
            &mut s,
            r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"i1","call_id":"c2","arguments":"{\"input\":\"p\"}"}"#,
        );
        let (out, error) = s.transform(
            format!(
                r#"{{"type":"response.output_item.done","output_index":{discover},"item":{{"type":"function_call","id":"i{discover}","call_id":"c{discover}","name":"n"}}}}"#
            )
            .as_bytes(),
        );
        assert_failed(
            &out,
            error.as_ref(),
            &format!("conflicting all-key evidence lost on record {discover}"),
        );
    }
}

#[test]
fn ordinary_progress() {
    let mut s = dispatching(NAMESPACED_LOOKUP);
    for raw in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"lookup","namespace":"n","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.delta","item_id":"a","delta":"{\"x\":1}"}"#,
        r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"x\":1}"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"lookup","namespace":"n","arguments":"{\"x\":1}"}}"#,
    ] {
        let out = remember_and_send(&mut s, raw);
        assert_eq!(
            out,
            [raw.as_bytes()],
            "ordinary progress delayed or rewritten: {}",
            shown(&out)
        );
    }
}

#[test]
fn dispatcher_lifecycle() {
    for close_at in ["response", "sentinel", "upstream_failure", "local_failure"] {
        let mut s = dispatching(NAMESPACED);
        for raw in [
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"a"}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}"#,
        ] {
            let out = remember_and_send(&mut s, raw);
            assert!(
                out.is_empty() && call(&s, "item:a").is_some_and(|call| call.namespace.is_empty()),
                "{close_at}: wrapper prematurely acquired dispatcher provenance: {}",
                shown(&out)
            );
        }
        let out = send(
            &mut s,
            r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"a","call_id":"c","name":"n"}}"#,
        );
        assert!(
            out.last()
                .is_some_and(|last| text(last, "item.input") == "p"),
            "{close_at}: retained source not acquired: {}",
            shown(&out)
        );
        let held = s.by_key.get("item:a").copied();
        let record = held.map(|held| &s.records[held]);
        assert!(
            record.is_some_and(|call| call.completed
                && call.namespace == "n"
                && call.index == 2
                && call.snapshots.len() == 1)
                && s.by_key.get("call:c").copied() == held
                && s.by_key.get("index:2").copied() == held,
            "{close_at}: completed aliases/source evidence expired"
        );
        assert_eq!(s.bridge.finish(), Ok(()), "{close_at}");
        assert!(
            s.finish().is_err() && s.by_key.get("item:a").copied() == held,
            "{close_at}: argument validation must not close completed provenance"
        );
        match close_at {
            "response" => {
                let out = send(
                    &mut s,
                    r#"{"type":"response.completed","response":{"output":[{"type":"function_call","id":"a","call_id":"c","name":"n"}]}}"#,
                );
                assert!(
                    out.len() == 1 && text(&out[0], "response.output.0.input") == "p",
                    "sparse terminal lost completed child or replayed progress: {}",
                    shown(&out)
                );
            }
            "sentinel" => {
                let (out, error) = s.stream(b"data: [DONE]");
                assert!(
                    error.is_some()
                        && out.len() == 1
                        && String::from_utf8_lossy(&out[0]).contains(r#""type":"response.failed""#),
                    "premature sentinel accepted: {} {error:?}",
                    shown(&out)
                );
            }
            "upstream_failure" => {
                send(
                    &mut s,
                    r#"{"type":"response.failed","response":{"output":[]}}"#,
                );
            }
            _ => {
                let (_, error) = s.transform(
                    br#"{"type":"response.output_item.done","output_index":3,"item":{"type":"function_call","id":"a","name":"n"}}"#,
                );
                assert!(error.is_some(), "identity conflict did not fail");
            }
        }
        assert!(
            s.by_key.is_empty() && s.records.is_empty() && s.upstream.is_none(),
            "{close_at}: closed response retained dispatcher source/aliases"
        );
        let (out, error) = s.transform(
            br#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"a","name":"n"}}"#,
        );
        assert!(
            error.is_none() && out.is_empty(),
            "{close_at}: closed dispatcher reopened: {} {error:?}",
            shown(&out)
        );
    }
}

#[test]
fn late_child_namespace() {
    let mut s = dispatching(NAMESPACED);
    let mut out = Vec::new();
    for raw in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch"}}"#,
    ] {
        out.extend(remember_and_send(&mut s, raw));
    }
    assert!(
        out.last().is_some_and(
            |last| text(last, "item.namespace") == "n" && text(last, "item.input") == "p"
        ),
        "sourced late child lost its namespace/input: {}",
        shown(&out)
    );
}

#[test]
fn terminal_source_identity() {
    let mut s = dispatching(NAMESPACED);
    for raw in [
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"a","name":"n","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"a","call_id":"c","name":"n"}}"#,
    ] {
        remember_and_send(&mut s, raw);
    }
    // Provider filtering can remove an earlier item after the original
    // snapshot is copied.
    s.remember_dispatcher_event(
        br#"{"type":"response.completed","response":{"output":[{"type":"message","id":"removed"},{"type":"function_call","id":"a","call_id":"c","name":"n","namespace":"other"}]}}"#,
    );
    let (out, error) = s.transform(
        br#"{"type":"response.completed","response":{"output":[{"type":"function_call","id":"a","call_id":"c","name":"n","namespace":"n"}]}}"#,
    );
    assert_failed(
        &out,
        error.as_ref(),
        "terminal source matched by position, losing contradictory namespace",
    );
}

#[test]
fn index_only_terminal() {
    let mut s = dispatching(NAMESPACED);
    for raw in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call"}}"#,
        r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}"#,
        // IDs are real source evidence; the later sparse terminal still
        // matches by index.
        r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","call_id":"c","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}"#,
    ] {
        remember_and_send(&mut s, raw);
    }
    let out = remember_and_send(
        &mut s,
        r#"{"type":"response.completed","response":{"output":[{"type":"function_call","name":"n"}]}}"#,
    );
    assert!(
        out.last()
            .is_some_and(|last| text(last, "response.output.0.input") == "p"),
        "index-only provenance bypassed: {}",
        shown(&out)
    );
}

#[test]
fn completed_ordinary_delta() {
    let mut s = dispatching(NAMESPACED_LOOKUP);
    for raw in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n"}}"#,
        r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"lookup\",\"arguments\":{\"x\":1}}"}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n"}}"#,
    ] {
        remember_and_send(&mut s, raw);
    }
    let raw =
        r#"{"type":"response.function_call_arguments.delta","item_id":"a","delta":"ordinary"}"#;
    let out = remember_and_send(&mut s, raw);
    assert_eq!(
        out,
        [raw.as_bytes()],
        "ordinary post-completion progress acquired patch validation: {}",
        shown(&out)
    );
}

#[test]
fn source_terminal_required() {
    for mode in ["empty", "arguments", "item"] {
        let mut s = state(&Format::CODEX, PATCH, PATCH);
        if mode != "empty" {
            send(
                &mut s,
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"apply_patch","arguments":""}}"#,
            );
            let event = if mode == "item" {
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","name":"apply_patch","arguments":"{\"input\":\"p\"}"}}"#
            } else {
                r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"input\":\"p\"}"}"#
            };
            send(&mut s, event);
        }
        let (out, error) = s.finish_stream();
        assert!(
            error.is_some()
                && out.len() == 1
                && String::from_utf8_lossy(&out[0]).contains(r#""type":"response.failed""#),
            "{mode}: source EOF silently accepted: {} {error:?}",
            shown(&out)
        );
        let (out, error) = s.finish_stream();
        assert!(
            out.is_empty() && error.is_none(),
            "{mode}: EOF failure repeated: {} {error:?}",
            shown(&out)
        );
    }
}

#[test]
fn inactive_eof_and_done() {
    for request in [
        "{}",
        r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#,
    ] {
        let mut s = state(&Format::CODEX, request, request);
        let (out, error) = s.finish_stream();
        assert!(
            out.is_empty() && error.is_none(),
            "inactive EOF changed: {} {error:?}",
            shown(&out)
        );
        let line = b"data: [DONE]";
        let (out, error) = s.stream(line);
        assert!(
            error.is_none() && out == [line],
            "ordinary DONE changed: {} {error:?}",
            shown(&out)
        );
    }
}

#[test]
fn malformed_wrapper_is_read_as_gjson_reads_it() {
    // gjson finds the name in the cut-off wrapper, so it is evidence of a
    // patch child, and an invalid one.
    let mut s = dispatching(NAMESPACED);
    for raw in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n","arguments":""}}"#,
        r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}"}"#,
    ] {
        remember_and_send(&mut s, raw);
    }
    let (out, error) = s.transform(
        br#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n","arguments":"{\"input\":\"p\"}"}}"#,
    );
    assert_failed(&out, error.as_ref(), "malformed wrapper");
}

#[test]
fn chat_preference_writes_tools_back() {
    // Once the Chat request declares a function, tools is written back, even
    // when nothing is dropped.
    let original = json(r#"{"tools":[{"type":"function","function":{"name":"lookup"}}]}"#);
    let mut declarations = json(r#"{"tools":{"type":"function","name":"lookup"}}"#);
    prefer_chat_function_patch_tools(&original, &mut declarations);
    assert_eq!(
        declarations,
        json(r#"{"tools":[{"type":"function","name":"lookup"}]}"#)
    );
}

const EMPTY_DELTA: &str =
    r#"{"type":"response.function_call_arguments.delta","item_id":"a","delta":""}"#;

/// The state failed once, with the limit's error, and holds nothing.
fn assert_limit_failure(s: &mut State, out: &[Vec<u8>], error: Option<&Error>, case: &str) {
    assert_failed(out, error, case);
    assert_eq!(
        text(&out[0], "response.error.code"),
        "invalid_tool_arguments",
        "{case}"
    );
    assert_eq!(
        error.map(ToString::to_string).as_deref(),
        Some(LIMIT_MESSAGE),
        "{case}"
    );
    assert!(s.records.is_empty() && s.by_key.is_empty(), "{case}");
    assert_eq!((s.held_bytes, s.held_events), (0, 0), "{case}");
    // Nothing else comes out, and the failure is what the stream ends with.
    let (out, error) = s.transform(EMPTY_DELTA.as_bytes());
    assert!(out.is_empty() && error.is_none(), "{case}: {}", shown(&out));
    let (out, error) = s.finish_stream();
    assert!(out.is_empty() && error.is_none(), "{case}: {}", shown(&out));
    assert_eq!(s.finish().unwrap_err().to_string(), LIMIT_MESSAGE, "{case}");
}

/// Not upstream's: a regression test, from the review's case. A declared
/// dispatcher gets empty argument deltas for one item and never a terminal
/// event. The state holds each event twice, and with Go's version no number
/// of them stops it. Here the event limit does, at its default.
#[test]
fn held_events_are_bounded() {
    let mut s = dispatching(NAMESPACED);
    for held in 0..Limits::DEFAULT.events {
        let (out, error) = s.transform(EMPTY_DELTA.as_bytes());
        assert!(
            out.is_empty() && error.is_none(),
            "event {held}: {} {error:?}",
            shown(&out)
        );
    }
    assert_eq!(s.held_events, Limits::DEFAULT.events);
    assert_counted(&s);
    assert!(
        s.held_bytes < Limits::DEFAULT.bytes / 2,
        "the byte limit stopped it first"
    );
    // The next event is one too many.
    let (out, error) = s.transform(EMPTY_DELTA.as_bytes());
    assert_limit_failure(&mut s, &out, error.as_ref(), "the event limit");
}

/// Not upstream's: the byte limit counts events however many there are of
/// them, and what each one carries.
#[test]
fn held_bytes_are_bounded() {
    let mut s = dispatching(NAMESPACED);
    s.limits.bytes = 64 << 10;
    let delta = format!(
        r#"{{"type":"response.function_call_arguments.delta","item_id":"a","delta":"{}"}}"#,
        "x".repeat(1000)
    );
    let mut held = 0;
    let (out, error) = loop {
        let (out, error) = s.transform(delta.as_bytes());
        if error.is_some() {
            break (out, error);
        }
        assert!(out.is_empty(), "{}", shown(&out));
        assert_counted(&s);
        assert!(s.held_bytes <= s.limits.bytes);
        held += 1;
        assert!(held < 1000, "nothing stopped {held} events");
    };
    // Each event is held twice and its delta once, about 3 KB in all.
    assert!((15..=25).contains(&held), "{held} events held");
    assert_limit_failure(&mut s, &out, error.as_ref(), "the byte limit");

    // One event of more than the limit fails at once.
    let mut s = dispatching(NAMESPACED);
    s.limits.bytes = 64 << 10;
    let big = format!(
        r#"{{"type":"response.function_call_arguments.delta","item_id":"a","delta":"{}"}}"#,
        "x".repeat(40 << 10)
    );
    let (out, error) = s.transform(big.as_bytes());
    assert_limit_failure(&mut s, &out, error.as_ref(), "one large event");
}

/// Not upstream's: an event that names no call still records one. Without a
/// bound on the calls, such events fill `records` however little each holds.
#[test]
fn recorded_calls_are_bounded() {
    let mut s = dispatching(NAMESPACED);
    s.limits.calls = 50;
    let nameless = r#"{"type":"response.function_call_arguments.delta","delta":""}"#;
    for held in 0..50 {
        let (out, error) = s.transform(nameless.as_bytes());
        assert!(
            out.is_empty() && error.is_none(),
            "call {held}: {} {error:?}",
            shown(&out)
        );
    }
    assert_eq!(s.records.len(), 50);
    assert_counted(&s);
    let (out, error) = s.transform(nameless.as_bytes());
    assert_limit_failure(&mut s, &out, error.as_ref(), "the call limit");

    // The same through the keys of the events: each names a new call.
    let mut s = dispatching(NAMESPACED);
    s.limits.calls = 50;
    for item in 0..51 {
        let delta = format!(
            r#"{{"type":"response.function_call_arguments.delta","item_id":"i{item}","delta":""}}"#
        );
        let (out, error) = s.transform(delta.as_bytes());
        if item < 50 {
            assert!(out.is_empty() && error.is_none(), "item {item}");
        } else {
            assert_limit_failure(&mut s, &out, error.as_ref(), "the call limit by key");
        }
    }
}

/// Not upstream's: the snapshots the executor hands over are held too, and it
/// can't be told when they pass a limit. The next event, or the end of the
/// stream, fails.
#[test]
fn held_snapshots_are_bounded() {
    let done = r#"{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{}"}"#;
    for next in ["event", "finish", "stream end"] {
        let mut s = dispatching(NAMESPACED);
        s.limits.events = 10;
        for held in 0..10 {
            s.remember_dispatcher_event(done.as_bytes());
            assert_eq!(s.held_events, held + 1);
            assert_counted(&s);
        }
        assert!(s.finish().is_err(), "{next}: the response hasn't ended");
        s.remember_dispatcher_event(done.as_bytes());
        // Nothing more is held, and nothing fails yet.
        s.remember_dispatcher_arguments(done.as_bytes());
        assert_eq!(s.held_events, 10, "{next}");
        assert!(!s.failed, "{next}");
        match next {
            "event" => {
                let (out, error) = s.transform(EMPTY_DELTA.as_bytes());
                assert_limit_failure(&mut s, &out, error.as_ref(), next);
            }
            "finish" => {
                assert_eq!(s.finish().unwrap_err().to_string(), LIMIT_MESSAGE);
                let (out, error) = s.transform(b"[DONE]");
                assert_limit_failure(&mut s, &out, error.as_ref(), next);
            }
            _ => {
                let (events, error) = s.finish_stream();
                assert_eq!(events.len(), 1, "{}", shown(&events));
                assert_eq!(
                    str_at(&parse(events[0].strip_prefix(b"data: ").unwrap()), "type"),
                    "response.failed"
                );
                assert_eq!(error.map(|e| e.to_string()).as_deref(), Some(LIMIT_MESSAGE));
            }
        }
    }
}

/// A long dispatcher call, far below the limits, completes as it does
/// without them, and what it held is counted until the response ends.
#[test]
fn a_long_dispatcher_call_completes_within_the_limits() {
    let patch = "*** Begin Patch\n*** End Patch\n".repeat(4000);
    let wrapper = json!({"name": "apply_patch", "arguments": {"input": patch}}).to_string();
    let mut s = dispatching(NAMESPACED);
    send(
        &mut s,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n","arguments":""}}"#,
    );
    let chunks: Vec<&str> = wrapper
        .as_bytes()
        .chunks(16)
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect();
    assert!(chunks.len() > 7000, "{} chunks", chunks.len());
    for chunk in &chunks {
        let delta = json!({
            "type": DELTA, "output_index": 0, "item_id": "a", "delta": chunk,
        });
        assert!(send(&mut s, &delta.to_string()).is_empty());
    }
    assert_eq!(s.held_events, chunks.len() + 1);
    assert!(s.held_bytes < s.limits.bytes / 10, "{}", s.held_bytes);
    let out = send(
        &mut s,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n","namespace":"n"}}"#,
    );
    assert!(
        out.last()
            .is_some_and(|last| text(last, "item.input") == patch),
        "the long call was lost: {} events",
        out.len()
    );
    // What the call keeps for the response's end is counted.
    assert_eq!(s.held_events, chunks.len() + 2);
    send(
        &mut s,
        r#"{"type":"response.completed","response":{"output":[]}}"#,
    );
    assert_eq!((s.held_bytes, s.held_events), (0, 0));
}

/// Events an ordinary function's call held back are released with it, and
/// not counted again.
#[test]
fn released_events_are_not_counted() {
    let mut s = dispatching(NAMESPACED);
    for item in ["a", "b", "c"] {
        let out = send(
            &mut s,
            &format!(
                r#"{{"type":"response.function_call_arguments.delta","item_id":"{item}","delta":"{{}}"}}"#
            ),
        );
        assert!(out.is_empty());
    }
    assert_eq!(s.held_events, 3);
    // Their late names say they're ordinary.
    for item in ["a", "b", "c"] {
        let out = send(
            &mut s,
            &format!(
                r#"{{"type":"response.output_item.done","item":{{"type":"function_call","id":"{item}","name":"lookup","arguments":"{{}}"}}}}"#
            ),
        );
        assert_eq!(out.len(), 2, "{}", shown(&out));
    }
    assert_eq!(s.held_events, 0);
    // What stays is the calls and their keys, and no events.
    assert!(s.records.iter().all(|call| call.events.is_empty()));
}

// Not upstream's: `requested` and `initialize_stream`, from
// helps/apply_patch.go, which the executors' apply_patch tests drive end to
// end (see the crate's `apply_patch_integration_tests`).

#[test]
fn requested_reads_the_winning_custom_declarations() {
    assert!(requested(&json(PATCH)));
    assert!(requested(&json(NAMESPACED)));
    assert!(!requested(&json(
        r#"{"tools":[{"type":"function","name":"apply_patch"}]}"#
    )));
    assert!(!requested(&json(
        r#"{"tools":[{"type":"function","name":"apply_patch"},{"type":"custom","name":"apply_patch"}]}"#
    )));
    assert!(!requested(&json("{}")));
    // A wrapped request is read from inside, if it holds a model, input or
    // tools.
    assert!(requested(&json(&format!(r#"{{"request":{PATCH}}}"#))));
    assert!(!requested(&json(
        r#"{"request":{"model":"m"},"tools":[{"type":"custom","name":"apply_patch"}]}"#
    )));
    assert!(requested(&json(
        r#"{"request":{"other":1},"tools":[{"type":"custom","name":"apply_patch"}]}"#
    )));
}

#[test]
fn an_initialized_stream_fails_a_requested_patch_without_a_chunk() {
    use open_ferry_translate::registry::{Registry, ResponseContext};

    let stream = |original: &Value| {
        Registry::global().response_stream(
            &Format::OPENAI,
            &Format::OPENAI_RESPONSE,
            &ResponseContext {
                model: "m",
                original_request: original,
                request: original,
            },
        )
    };
    let original = json(PATCH);

    let mut initialized = stream(&original);
    initialize_stream(&mut initialized, &Format::OPENAI_RESPONSE, &original);
    assert!(shown(&initialized.finish()).contains("response.failed"));
    assert!(initialized.tool_input_error().is_some());

    // A translator never given a chunk has nothing to finish.
    let mut untouched = stream(&original);
    assert!(untouched.finish().is_empty());
    assert!(untouched.tool_input_error().is_none());

    // Only a Responses client's stream is initialized.
    let mut other = stream(&original);
    initialize_stream(&mut other, &Format::CLAUDE, &original);
    assert!(other.finish().is_empty());
    assert!(other.tool_input_error().is_none());
}
