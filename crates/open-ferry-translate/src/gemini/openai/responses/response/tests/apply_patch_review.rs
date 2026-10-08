// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/apply_patch_review_test.go (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Calls whose part index and ID point at two different earlier calls: the
//! response fails if either is an `apply_patch` call, and an ordinary call
//! keeps what it first matched.
//!
//! Dropped or changed tests: none.

use super::*;

const MIXED_REQUEST: &str =
    r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"}]}"#;

/// `geminiPatchReviewPart`
fn review_part(index_key: &str, index: i64, id: &str, name: &str, args: &str) -> String {
    format!(
        r#"{{"{index_key}":{index},"functionCall":{{"id":{},"name":{},"args":{args}}}}}"#,
        go::quote(id),
        go::quote(name)
    )
}

/// `geminiPatchReviewFrame`
fn review_frame(parts: &[String], terminal: bool) -> String {
    let finish = if terminal {
        r#","finishReason":"STOP""#
    } else {
        ""
    };
    format!(
        r#"{{"candidates":[{{"content":{{"parts":[{}]}}{finish}}}]}}"#,
        parts.join(",")
    )
}

/// The data of each event one chunk gives, as `geminiPatchReviewEvents`
/// keeps them.
fn send(stream: &mut GeminiToOpenAIResponsesStream, raw: &str) -> Vec<Value> {
    translate(stream, raw)
        .into_iter()
        .map(|(_, data)| data)
        .collect()
}

/// How many events there are of each `type`.
fn counts(events: &[Value]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for event in events {
        *counts.entry(text(event, "type")).or_insert(0) += 1;
    }
    counts
}

/// Go's `counts[kind]`: zero for a kind never seen.
fn count(counts: &HashMap<String, usize>, kind: &str) -> usize {
    counts.get(kind).copied().unwrap_or(0)
}

#[test]
fn gemini_apply_patch_cross_key_evidence_conflict() {
    let request = parse(MIXED_REQUEST);
    for index_key in ["partIndex", "index"] {
        for order in ["ordinary first", "patch first"] {
            for direction in ["ordinary index patch ID", "patch index ordinary ID"] {
                for name in ["lookup", "apply_patch", ""] {
                    for mode in [
                        "stream separate frames",
                        "stream single frame",
                        "non-stream",
                    ] {
                        let case = [index_key, order, direction, name, mode].join("/");
                        let ordinary =
                            review_part(index_key, 3, "ordinary", "lookup", r#"{"x":1}"#);
                        let patch =
                            review_part(index_key, 2, "patch", "apply_patch", r#"{"input":"p"}"#);
                        let mut parts = vec![ordinary.clone(), patch.clone()];
                        if order == "patch first" {
                            parts = vec![patch.clone(), ordinary];
                        }
                        let (index, id) = if direction == "patch index ordinary ID" {
                            (2, "ordinary")
                        } else {
                            (3, "patch")
                        };
                        let args = if name == "apply_patch" {
                            r#"{"input":"secret"}"#
                        } else {
                            r#"{"x":1}"#
                        };
                        let conflict = review_part(index_key, index, id, name, args);
                        parts.push(conflict.clone());

                        if mode == "non-stream" {
                            let frame = review_frame(&parts, true);
                            let out = convert_gemini_response_to_openai_responses_non_stream(
                                &request,
                                &Value::Null,
                                frame.as_bytes(),
                            );
                            assert!(out.is_none(), "{case}: patch provenance discarded: {out:?}");
                            assert!(
                                non_stream(&request, &Value::Null, frame.as_bytes()).is_err(),
                                "{case}: missing non-stream tool input error"
                            );
                            continue;
                        }

                        let mut stream =
                            GeminiToOpenAIResponsesStream::new("gemini", &request, &Value::Null);
                        let mut events = Vec::new();
                        if mode == "stream single frame" {
                            events = send(&mut stream, &review_frame(&parts, true));
                        } else {
                            for part in &parts[..2] {
                                events.extend(send(
                                    &mut stream,
                                    &review_frame(std::slice::from_ref(part), false),
                                ));
                            }
                            events.extend(send(
                                &mut stream,
                                &review_frame(std::slice::from_ref(&conflict), true),
                            ));
                        }

                        for event in &events {
                            let kind = text(event, "type");
                            let raw = event.to_string();
                            assert!(
                                kind != "response.failed"
                                    || (text(event, "response.error.code")
                                        == "invalid_tool_arguments"
                                        && !raw.contains("secret")
                                        && !raw.contains(r#"\"input\""#)),
                                "{case}: unsanitized failure: {raw}"
                            );
                            assert!(
                                !(kind == "response.custom_tool_call_input.delta"
                                    && text(event, "delta") != "p"
                                    || kind == "response.custom_tool_call_input.done"
                                        && text(event, "input") != "p"),
                                "{case}: collision leaked or replayed patch input: {raw}"
                            );
                        }
                        let counts = counts(&events);
                        assert!(
                            count(&counts, "response.failed") == 1
                                && count(&counts, "response.completed") == 0
                                && count(&counts, "response.output_item.added") == 2
                                && count(&counts, "response.output_item.done") == 2
                                && count(&counts, "response.custom_tool_call_input.delta") == 1
                                && count(&counts, "response.custom_tool_call_input.done") == 1
                                && count(&counts, "response.function_call_arguments.done") == 1,
                            "{case}: collision added a call or completed response: counts={counts:?} events={events:?}"
                        );

                        let by_key = &stream.evidence.by_key;
                        let (Some(&patch_index), Some(&ordinary_index)) =
                            (by_key.get("part:2"), by_key.get("part:3"))
                        else {
                            panic!(
                                "{case}: collision changed established aliases/provenance: {by_key:?}"
                            );
                        };
                        let patch_evidence = &stream.evidence.items[patch_index];
                        let ordinary_evidence = &stream.evidence.items[ordinary_index];
                        assert!(
                            stream.next_index == 2
                                && by_key.len() == 4
                                && patch_index != ordinary_index
                                && by_key.get("id:patch") == Some(&patch_index)
                                && by_key.get("id:ordinary") == Some(&ordinary_index)
                                && patch_evidence.apply_patch
                                && patch_evidence.patch_call.is_some()
                                && patch_evidence.raw_name == "apply_patch"
                                && !ordinary_evidence.apply_patch
                                && ordinary_evidence.raw_name == "lookup",
                            "{case}: collision changed established aliases/provenance: {by_key:?}"
                        );

                        let error_input = stream.tool_input_error().map(ToString::to_string);
                        assert!(
                            error_input
                                .as_deref()
                                .is_some_and(|message| message.contains("indexes")),
                            "{case}: missing cross-key tool input error: {error_input:?}"
                        );
                        // Go compares the error values themselves.
                        let error_state = format!("{:?}", stream.error);
                        for raw in [
                            "[DONE]".to_owned(),
                            review_frame(&[], true),
                            review_frame(&parts, true),
                            review_frame(std::slice::from_ref(&patch), false),
                        ] {
                            let more = send(&mut stream, &raw);
                            assert!(
                                more.is_empty() && format!("{:?}", stream.error) == error_state,
                                "{case}: failed response reopened or lost error: {more:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn gemini_apply_patch_cross_key_ordinary_legacy() {
    let requests = [
        (
            "ordinary only in patch-enabled request",
            r#"{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","name":"lookup"},{"type":"function","name":"second"}]}"#,
            "second",
        ),
        (
            "same-name function winner",
            r#"{"tools":[{"type":"function","name":"apply_patch"},{"type":"function","name":"lookup"},{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"}]}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}"#,
            "apply_patch",
        ),
    ];
    for (request_name, request, second) in requests {
        let request = parse(request);
        for index_key in ["partIndex", "index"] {
            for order in ["first second", "second first"] {
                for direction in ["first index second ID", "second index first ID"] {
                    let case = [request_name, index_key, order, direction].join("/");
                    let mut parts = vec![
                        review_part(index_key, 3, "first", "lookup", r#"{"x":1}"#),
                        review_part(index_key, 2, "second", second, r#"{"x":2}"#),
                    ];
                    let mut names = vec!["lookup", second];
                    let mut arguments = vec![r#"{"x":1}"#, r#"{"x":2}"#];
                    if order == "second first" {
                        parts.swap(0, 1);
                        names.swap(0, 1);
                        arguments.swap(0, 1);
                    }
                    let (index, id, name) = if direction == "second index first ID" {
                        (2, "first", second)
                    } else {
                        (3, "second", "lookup")
                    };
                    parts.push(review_part(index_key, index, id, name, r#"{"x":3}"#));
                    names.push(name);
                    arguments.push(r#"{"x":3}"#);

                    let mut stream =
                        GeminiToOpenAIResponsesStream::new("gemini", &request, &Value::Null);
                    let mut events = Vec::new();
                    for (i, part) in parts.iter().enumerate() {
                        events.extend(send(
                            &mut stream,
                            &review_frame(std::slice::from_ref(part), i == parts.len() - 1),
                        ));
                    }
                    events.extend(send(&mut stream, "[DONE]"));
                    let counts = counts(&events);
                    let last = events
                        .iter()
                        .rev()
                        .find(|event| text(event, "type") == "response.completed")
                        .and_then(|event| at(event, "response"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    assert!(
                        count(&counts, "response.failed") == 0
                            && count(&counts, "response.completed") == 1
                            && count(&counts, "response.output_item.done") == 3
                            && count(&counts, "response.custom_tool_call_input.done") == 0
                            && stream.tool_input_error().is_none(),
                        "{case}: ordinary cross-key legacy behavior changed: {events:?}"
                    );

                    let frame = review_frame(&parts, true);
                    let out = convert_gemini_response_to_openai_responses_non_stream(
                        &request,
                        &Value::Null,
                        frame.as_bytes(),
                    )
                    .unwrap_or(Value::Null);
                    for output in [&last, &out] {
                        assert_eq!(
                            list(output, "output").len(),
                            3,
                            "{case}: ordinary calls consolidated or rejected: {output}"
                        );
                        for (i, item) in list(output, "output").iter().enumerate() {
                            assert!(
                                text(item, "type") == "function_call"
                                    && text(item, "name") == names[i]
                                    && text(item, "arguments") == arguments[i],
                                "{case}: ordinary winner changed: {item}"
                            );
                        }
                    }
                    assert!(
                        non_stream(&request, &Value::Null, frame.as_bytes()).is_ok(),
                        "{case}: ordinary non-stream call received patch error"
                    );
                }
            }
        }
    }
}
